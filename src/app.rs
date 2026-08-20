use std::{collections::HashMap, collections::VecDeque, env, path::PathBuf};

use crate::{
    broker::{
        AccountPosition, AccountSnapshot, BrokerClient, OrderExecution, OrderRequest, OrderSide,
        OrderStatus, PaperBroker,
    },
    config::{Config, Mode},
    errors::AppError,
    iol_client::IolClient,
    market::{select_option, MarketDataProvider, MarketFrame, OptionKind, ReplayMarket},
    pattern::{Direction, PriceSample, Trend, TrendDetector},
    persistence::{
        load_snapshot, read_events, save_snapshot, Journal, JournalEvent, JournalEventKind,
        RuntimeSnapshot, Snapshot,
    },
    portfolio::{Portfolio, PortfolioMetrics},
    risk::{RiskLimits, RiskManager},
    trading::{
        calculate_pnl_with_contract_multiplier, ExitReason, Pnl, Position, PositionKind,
        TradingEngine,
    },
};

enum MarketSource {
    Replay(ReplayMarket),
    Iol(Box<IolClient>),
}

pub struct TradingApp {
    pub config: Config,
    pub engine: TradingEngine,
    pub portfolio: Portfolio,
    pub risk: RiskManager,
    pub current_frame: Option<MarketFrame>,
    pub current_trend: Option<Trend>,
    pub current_pnl: Option<Pnl>,
    pub selected_option: Option<String>,
    pub paused: bool,
    pub completed: bool,
    pub status: String,
    pub ticks: u64,
    logs: VecDeque<String>,
    detector: TrendDetector,
    source: MarketSource,
    paper_broker: PaperBroker,
    journal: Journal,
    snapshot_path: PathBuf,
    last_market_timestamp: Option<i64>,
    operation_counter: u64,
    last_traded_signal: Option<Direction>,
    startup_reconciled: bool,
    reconciliation_blocked: bool,
    local_pending_orders: Vec<String>,
}

impl TradingApp {
    pub fn new(config: Config) -> Result<Self, AppError> {
        let mut source = if config.mode == Mode::Replay {
            let replay = if let Some(path) = &config.replay_path {
                ReplayMarket::from_jsonl(path)?
            } else {
                ReplayMarket::synthetic(&config.ticker)
            };
            MarketSource::Replay(replay)
        } else {
            let username = env::var("IOL_USERNAME")
                .map_err(|_| AppError::External("IOL_USERNAME ausente".into()))?;
            let password = env::var("IOL_PASSWORD")
                .map_err(|_| AppError::External("IOL_PASSWORD ausente".into()))?;
            let refresh_token = env::var("IOL_REFRESH_TOKEN").unwrap_or_default();
            let client = IolClient::new(&config.iol_base_url, username, password, refresh_token)
                .map_err(|error| AppError::External(error.to_string()))?;
            MarketSource::Iol(Box::new(client))
        };

        let mode_name = format!("{:?}", config.mode).to_ascii_lowercase();
        let journal_path = config.data_dir.join(&mode_name).join("journal.jsonl");
        let snapshot_path = config.data_dir.join(&mode_name).join("state.json");
        let mut journal = Journal::open(&journal_path)?;
        let mut engine = TradingEngine::new();
        let mut portfolio = Portfolio::default();
        let configured_limits = RiskLimits {
            max_notional: config.max_investment_amount,
            max_loss_per_trade: config.max_loss_per_trade,
            max_daily_loss: config.max_daily_loss,
            max_trades_per_day: config.max_trades_per_day,
        };
        let mut risk = RiskManager::new(configured_limits.clone());
        let mut recovery_message = None;
        let mut detector =
            TrendDetector::new(config.history_capacity(), config.min_samples_for_trend);
        let mut current_frame = None;
        let mut current_trend = None;
        let mut current_pnl = None;
        let mut last_market_timestamp = None;
        let mut operation_counter = 0;
        let mut last_traded_signal = None;
        let mut ticks = 0;
        let mut selected_option = None;

        if config.recover_state && snapshot_path.exists() {
            let snapshot = load_snapshot(&snapshot_path)?;
            engine = snapshot.engine;
            portfolio = snapshot.portfolio;
            risk = snapshot.risk;
            detector = snapshot.runtime.detector;
            current_frame = snapshot.runtime.current_frame;
            current_trend = snapshot.runtime.current_trend;
            current_pnl = snapshot.runtime.current_pnl;
            last_market_timestamp = snapshot.runtime.last_market_timestamp;
            operation_counter = snapshot.runtime.operation_counter;
            last_traded_signal = snapshot.runtime.last_traded_signal;
            ticks = snapshot.runtime.ticks;
            selected_option = snapshot.runtime.selected_option;
            if let (Some(timestamp), MarketSource::Replay(replay)) =
                (last_market_timestamp, &mut source)
            {
                replay.resume_after(timestamp);
            }
            let events = journal.events_after(snapshot.last_sequence)?;
            for event in &events {
                apply_recovery_event(&mut engine, &mut portfolio, &mut risk, event)?;
            }
            // Los límites de riesgo vigentes siempre provienen de la configuración actual,
            // no de un snapshot potencialmente antiguo.
            risk.limits = configured_limits;
            recovery_message = Some(format!(
                "estado recuperado desde secuencia {} y {} eventos",
                snapshot.last_sequence,
                events.len()
            ));
        }

        let local_pending_orders = unresolved_local_orders(&read_events(&journal_path)?);

        let started_at = unix_now();
        journal.append(
            started_at,
            None,
            JournalEventKind::Started {
                mode: mode_name,
                ticker: config.ticker.clone(),
            },
        )?;

        let mut app = Self {
            detector,
            paper_broker: PaperBroker::new(config.paper_slippage_bps),
            config,
            engine,
            portfolio,
            risk,
            current_frame,
            current_trend,
            current_pnl,
            selected_option,
            paused: false,
            completed: false,
            status: "inicializado".into(),
            ticks,
            logs: VecDeque::with_capacity(100),
            source,
            journal,
            snapshot_path,
            last_market_timestamp,
            operation_counter,
            last_traded_signal,
            startup_reconciled: false,
            reconciliation_blocked: false,
            local_pending_orders,
        };
        if let Some(message) = recovery_message {
            app.push_log(message);
        }
        app.push_log(format!(
            "modo {:?} listo para {}",
            app.config.mode, app.config.ticker
        ));
        Ok(app)
    }

    pub async fn step(&mut self) -> Result<bool, AppError> {
        if self.paused || self.completed {
            return Ok(!self.completed);
        }
        let frame = match self.next_frame().await? {
            Some(frame) => frame,
            None => {
                self.completed = true;
                self.status = "replay completado".into();
                self.push_log("dataset de replay agotado".into());
                return Ok(false);
            }
        };
        frame.validate(self.last_market_timestamp)?;
        self.last_market_timestamp = Some(frame.underlying.timestamp_secs);
        self.ticks = self.ticks.saturating_add(1);
        let timestamp = frame.underlying.timestamp_secs;
        self.current_frame = Some(frame);

        self.reconcile_startup(timestamp).await?;
        if self.reconciliation_blocked {
            self.snapshot()?;
            return Ok(true);
        }

        if self.config.mode != Mode::Replay {
            let freshness = self
                .current_frame
                .as_ref()
                .expect("current frame was just assigned")
                .underlying
                .validate_freshness(unix_now(), self.config.max_market_data_age_secs);
            if let Err(error) = freshness {
                self.halt_for_market_risk(unix_now(), error.to_string())?;
                self.snapshot()?;
                return Ok(true);
            }
        }

        let trend = self
            .detector
            .push(PriceSample {
                price: self
                    .current_frame
                    .as_ref()
                    .expect("current frame was just assigned")
                    .underlying
                    .last,
                timestamp_secs: timestamp,
            })
            .ok_or_else(|| AppError::InvalidMarketData("muestra rechazada".into()))?;
        self.current_trend = Some(trend.clone());
        if !trend.confirmed {
            self.last_traded_signal = None;
        }
        if !self.reconciliation_blocked {
            self.status = format!(
                "precio {:.2} · {:?}{}",
                self.current_frame
                    .as_ref()
                    .map_or(0.0, |frame| frame.underlying.last),
                trend.direction,
                if trend.confirmed { " confirmada" } else { "" }
            );
        }

        if self.engine.position.is_some() {
            self.evaluate_exit(timestamp).await?;
        } else if trend.confirmed
            && !self.risk.state.kill_switch
            && self.last_traded_signal != Some(trend.direction)
        {
            self.evaluate_entry(timestamp, trend.direction).await?;
        }
        self.snapshot()?;
        Ok(true)
    }

    async fn next_frame(&mut self) -> Result<Option<MarketFrame>, AppError> {
        match &mut self.source {
            MarketSource::Replay(market) => market.next_frame(),
            MarketSource::Iol(client) => client
                .market_frame_with_retry(&self.config.ticker, 3)
                .await
                .map(Some)
                .map_err(|error| AppError::External(error.to_string())),
        }
    }

    async fn evaluate_entry(
        &mut self,
        timestamp: i64,
        direction: Direction,
    ) -> Result<(), AppError> {
        if !self.engine.consider_entry(direction) {
            return Ok(());
        }
        let option_kind = match direction {
            Direction::Up => OptionKind::Call,
            Direction::Down => OptionKind::Put,
            Direction::Neutral => return Ok(()),
        };
        let option = self
            .current_frame
            .as_ref()
            .and_then(|frame| select_option(frame, option_kind, self.config.option_expiry_days))
            .cloned();
        let Some(option) = option else {
            self.engine.resume();
            self.push_log(format!("sin opcion liquida para {option_kind:?}"));
            return Ok(());
        };
        self.selected_option = Some(option.symbol.clone());
        let market_price = option
            .executable_buy_price()
            .ok_or_else(|| AppError::InvalidMarketData("ask de opcion ausente".into()))?;
        let quality_now = if self.config.mode == Mode::Replay {
            option.timestamp_secs
        } else {
            unix_now()
        };
        if let Err(error) = option.validate_entry_quality(
            quality_now,
            self.config.max_market_data_age_secs,
            self.config.max_option_spread_percentage,
        ) {
            self.halt_for_market_risk(timestamp, error.to_string())?;
            return Ok(());
        }
        let limit_price = market_price * 1.005;
        let quantity = affordable_contracts(
            self.config.max_investment_amount,
            limit_price,
            self.config.contract_multiplier,
            self.config.commission_percentage,
            self.config.max_position_size,
        );
        if quantity == 0 {
            let reason = format!(
                "presupuesto {:.2} insuficiente para un contrato de {}",
                self.config.max_investment_amount, option.symbol
            );
            self.journal.append(
                timestamp,
                None,
                JournalEventKind::RiskRejected {
                    reason: reason.clone(),
                },
            )?;
            self.engine.resume();
            self.push_log(format!("entrada bloqueada: {reason}"));
            return Ok(());
        }
        let maximum_cash = purchase_cash_required(
            limit_price,
            quantity,
            self.config.contract_multiplier,
            self.config.commission_percentage,
        );
        if let Err(reason) = self.risk.allow_entry(maximum_cash) {
            self.journal.append(
                timestamp,
                None,
                JournalEventKind::RiskRejected {
                    reason: reason.clone(),
                },
            )?;
            self.engine.resume();
            self.push_log(format!("entrada bloqueada: {reason}"));
            return Ok(());
        }

        let operation_id = self.next_operation_id(timestamp, "buy");
        let request = OrderRequest {
            operation_id: operation_id.clone(),
            symbol: option.symbol.clone(),
            quantity,
            market_price,
            limit_price,
            side: OrderSide::Buy,
        };
        self.engine.mark_buying();
        let execution = self.execute_order(timestamp, &request).await?;
        if execution.status != OrderStatus::Executed
            || execution.filled_quantity != request.quantity
            || execution.fill_price.is_none()
        {
            self.handle_unfilled_order(&execution);
            return Ok(());
        }
        let position = Position {
            operation_id: operation_id.clone(),
            option_symbol: option.symbol,
            kind: PositionKind::from(option_kind),
            entry_price: execution.fill_price.unwrap_or(market_price),
            contracts: execution.filled_quantity,
            contract_multiplier: self.config.contract_multiplier,
            opened_at_secs: timestamp,
        };
        self.journal.append(
            timestamp,
            Some(operation_id),
            JournalEventKind::PositionOpened {
                position: position.clone(),
            },
        )?;
        if !self.engine.open_position(position.clone()) || !self.portfolio.open(position.clone()) {
            self.risk.engage_kill_switch();
            self.engine.halt();
            return Err(AppError::Recovery(
                "inconsistencia al registrar posicion ejecutada".into(),
            ));
        }
        self.last_traded_signal = Some(direction);
        self.push_log(format!(
            "BUY {} x{} @ {:.4}",
            position.option_symbol, position.contracts, position.entry_price
        ));
        Ok(())
    }

    async fn reconcile_startup(&mut self, timestamp: i64) -> Result<(), AppError> {
        if self.startup_reconciled {
            return Ok(());
        }
        self.startup_reconciled = true;

        if !self.local_pending_orders.is_empty() && self.config.mode != Mode::Live {
            return self.block_reconciliation(
                timestamp,
                format!(
                    "hay {} orden(es) locales sin estado final: {}",
                    self.local_pending_orders.len(),
                    self.local_pending_orders.join(", ")
                ),
            );
        }

        if !self.local_pending_orders.is_empty() {
            self.push_log(format!(
                "{} orden(es) locales requieren confirmacion contra IOL",
                self.local_pending_orders.len()
            ));
        }

        if self.config.mode != Mode::Live {
            self.push_log("reconciliacion inicial local completada".into());
            return Ok(());
        }

        let account_result = match &mut self.source {
            MarketSource::Iol(client) => client.account_snapshot().await,
            MarketSource::Replay(_) => unreachable!("live siempre usa IOL"),
        };
        match account_result {
            Ok(account) => self.apply_account_snapshot(timestamp, account),
            Err(error) => self.block_reconciliation(
                timestamp,
                format!("no se pudo consultar cartera/ordenes IOL: {error}"),
            ),
        }
    }

    fn apply_account_snapshot(
        &mut self,
        timestamp: i64,
        account: AccountSnapshot,
    ) -> Result<(), AppError> {
        let Some(frame) = self.current_frame.as_ref() else {
            return self.block_reconciliation(timestamp, "mercado inicial ausente".into());
        };
        let option_positions: Vec<AccountPosition> = account
            .positions
            .into_iter()
            .filter(|position| position.is_option || frame.option(&position.symbol).is_some())
            .collect();
        let pending_options: Vec<_> = account
            .pending_orders
            .into_iter()
            .filter(|order| order.is_option || frame.option(&order.symbol).is_some())
            .collect();

        if !pending_options.is_empty() {
            let orders = pending_options
                .iter()
                .map(|order| format!("{}:{}", order.broker_order_id, order.symbol))
                .collect::<Vec<_>>()
                .join(", ");
            return self.block_reconciliation(
                timestamp,
                format!("hay orden(es) de opciones pendientes en IOL: {orders}"),
            );
        }
        if option_positions.len() > 1 {
            return self.block_reconciliation(
                timestamp,
                format!(
                    "IOL informa {} posiciones de opciones; el motor admite una",
                    option_positions.len()
                ),
            );
        }

        match (self.engine.position.clone(), option_positions.first()) {
            (None, None) => {
                self.push_log("cartera y ordenes IOL reconciliadas: sin opciones activas".into());
                Ok(())
            }
            (Some(_), None) => self.block_reconciliation(
                timestamp,
                "el estado local tiene una opcion que no existe en la cartera IOL".into(),
            ),
            (Some(local), Some(remote)) => {
                if local.option_symbol != remote.symbol || local.contracts != remote.quantity {
                    return self.block_reconciliation(
                        timestamp,
                        format!(
                            "posicion local {} x{} no coincide con IOL {} x{}",
                            local.option_symbol, local.contracts, remote.symbol, remote.quantity
                        ),
                    );
                }
                self.engine.resume();
                self.push_log(format!(
                    "posicion {} x{} confirmada contra cartera IOL",
                    local.option_symbol, local.contracts
                ));
                Ok(())
            }
            (None, Some(remote)) => self.reconstruct_account_position(timestamp, remote.clone()),
        }
    }

    fn reconstruct_account_position(
        &mut self,
        timestamp: i64,
        remote: AccountPosition,
    ) -> Result<(), AppError> {
        let quote = self
            .current_frame
            .as_ref()
            .and_then(|frame| frame.option(&remote.symbol));
        let kind = remote
            .kind
            .or_else(|| quote.map(|quote| PositionKind::from(quote.kind)));
        let (Some(kind), Some(entry_price)) = (kind, remote.average_price) else {
            return self.block_reconciliation(
                timestamp,
                format!(
                    "no se puede reconstruir {}: tipo o precio promedio ausente",
                    remote.symbol
                ),
            );
        };
        if remote.quantity == 0 || quote.is_none() {
            return self.block_reconciliation(
                timestamp,
                format!(
                    "la opcion {} de IOL no aparece en el mercado actual",
                    remote.symbol
                ),
            );
        }
        let position = Position {
            operation_id: format!(
                "recovered-{}-{timestamp}",
                remote.symbol.to_ascii_lowercase()
            ),
            option_symbol: remote.symbol,
            kind,
            entry_price,
            contracts: remote.quantity,
            contract_multiplier: self.config.contract_multiplier,
            opened_at_secs: timestamp,
        };
        self.journal.append(
            timestamp,
            Some(position.operation_id.clone()),
            JournalEventKind::PositionOpened {
                position: position.clone(),
            },
        )?;
        if !self.engine.open_position(position.clone()) || !self.portfolio.open(position.clone()) {
            return self.block_reconciliation(
                timestamp,
                "fallo al reconstruir la posicion informada por IOL".into(),
            );
        }
        self.selected_option = Some(position.option_symbol.clone());
        self.last_traded_signal = Some(position.direction());
        if position.notional() > self.config.max_investment_amount {
            self.risk.engage_kill_switch();
            self.push_log(format!(
                "exposicion recuperada {:.2} supera presupuesto {:.2}; nuevas entradas bloqueadas",
                position.notional(),
                self.config.max_investment_amount
            ));
        }
        self.push_log(format!(
            "posicion reconstruida desde IOL: {:?} {} x{} @ {:.4}",
            position.kind, position.option_symbol, position.contracts, position.entry_price
        ));
        Ok(())
    }

    fn block_reconciliation(&mut self, timestamp: i64, reason: String) -> Result<(), AppError> {
        self.reconciliation_blocked = true;
        self.risk.engage_kill_switch();
        self.engine.halt();
        self.status = format!("reconciliacion bloqueada: {reason}");
        self.push_log(self.status.clone());
        self.journal.append(
            timestamp,
            None,
            JournalEventKind::Recovery {
                message: self.status.clone(),
            },
        )?;
        Ok(())
    }

    fn halt_for_market_risk(&mut self, timestamp: i64, reason: String) -> Result<(), AppError> {
        self.risk.engage_kill_switch();
        self.engine.halt();
        self.status = format!("riesgo de mercado: {reason}");
        self.push_log(self.status.clone());
        self.journal.append(
            timestamp,
            None,
            JournalEventKind::RiskRejected {
                reason: self.status.clone(),
            },
        )?;
        Ok(())
    }

    async fn evaluate_exit(&mut self, timestamp: i64) -> Result<(), AppError> {
        let Some(position) = self.engine.position.clone() else {
            return Ok(());
        };
        let option = self
            .current_frame
            .as_ref()
            .and_then(|frame| frame.option(&position.option_symbol))
            .cloned();
        let Some(option) = option else {
            self.status = format!("sin precio para {}", position.option_symbol);
            self.push_log(self.status.clone());
            return Ok(());
        };
        let quality_now = if self.config.mode == Mode::Replay {
            option.timestamp_secs
        } else {
            unix_now()
        };
        if let Err(error) =
            option.validate_freshness(quality_now, self.config.max_market_data_age_secs)
        {
            self.halt_for_market_risk(timestamp, error.to_string())?;
            return Ok(());
        }
        let market_price = option
            .executable_sell_price()
            .ok_or_else(|| AppError::InvalidMarketData("bid de opcion ausente".into()))?;
        let pnl = calculate_pnl_with_contract_multiplier(
            position.entry_price,
            market_price,
            position.contracts,
            position.contract_multiplier,
            self.config.commission_percentage,
            self.config.tax_percentage,
            self.config.min_profit_multiplier,
        );
        self.current_pnl = Some(pnl);
        let opposite = self
            .detector
            .opposite_confirmed(position.direction(), self.config.trend_change_samples);
        let reason = self.engine.should_exit(
            market_price,
            pnl,
            opposite,
            timestamp,
            (self.config.position_timeout_mins * 60) as i64,
            self.risk.limits.max_loss_per_trade,
            self.config.stop_loss_percentage,
        );
        if let Some(reason) = reason {
            if option
                .spread_percentage()
                .is_some_and(|spread| spread > self.config.max_option_spread_percentage)
            {
                self.push_log(format!(
                    "spread de salida {:.2}% excede {:.2}%; se permite vender para reducir riesgo",
                    option.spread_percentage().unwrap_or_default(),
                    self.config.max_option_spread_percentage
                ));
            }
            self.close_position(timestamp, market_price, pnl, reason)
                .await?;
        }
        Ok(())
    }

    async fn close_position(
        &mut self,
        timestamp: i64,
        market_price: f64,
        quoted_pnl: Pnl,
        reason: ExitReason,
    ) -> Result<(), AppError> {
        let Some(position) = self.engine.position.clone() else {
            return Ok(());
        };
        let operation_id = format!("{}-close", position.operation_id);
        let request = OrderRequest {
            operation_id: operation_id.clone(),
            symbol: position.option_symbol.clone(),
            quantity: position.contracts,
            market_price,
            limit_price: market_price * 0.995,
            side: OrderSide::Sell,
        };
        self.engine.mark_selling();
        let execution = self.execute_order(timestamp, &request).await?;
        if execution.status != OrderStatus::Executed
            || execution.filled_quantity != request.quantity
            || execution.fill_price.is_none()
        {
            self.handle_unfilled_order(&execution);
            return Ok(());
        }
        let fill_price = execution.fill_price.unwrap_or(market_price);
        let pnl = calculate_pnl_with_contract_multiplier(
            position.entry_price,
            fill_price,
            position.contracts,
            position.contract_multiplier,
            self.config.commission_percentage,
            self.config.tax_percentage,
            self.config.min_profit_multiplier,
        );
        self.journal.append(
            timestamp,
            Some(position.operation_id.clone()),
            JournalEventKind::PositionClosed {
                operation_id: position.operation_id.clone(),
                exit_price: fill_price,
                net_pnl: pnl.net,
                reason,
            },
        )?;
        self.portfolio.close(
            &position.operation_id,
            fill_price,
            pnl.net,
            timestamp,
            reason,
        );
        self.engine.close(reason);
        self.risk.record_close(pnl.net);
        self.current_pnl = Some(pnl);
        self.push_log(format!(
            "SELL {} @ {:.4} · {:?} · net {:.2} (cotizado {:.2})",
            position.option_symbol, fill_price, reason, pnl.net, quoted_pnl.net
        ));
        if self.risk.state.kill_switch {
            self.engine.halt();
        }
        Ok(())
    }

    async fn execute_order(
        &mut self,
        timestamp: i64,
        request: &OrderRequest,
    ) -> Result<OrderExecution, AppError> {
        self.journal.append(
            timestamp,
            Some(request.operation_id.clone()),
            JournalEventKind::OrderSubmitted {
                symbol: request.symbol.clone(),
                side: request.side,
                quantity: request.quantity,
                limit_price: request.limit_price,
            },
        )?;
        let execution = match self.config.mode {
            Mode::Replay | Mode::Paper => self.paper_broker.submit_limit(request.clone())?,
            Mode::Live => {
                let order_path = self.config.iol_order_path.as_deref().ok_or_else(|| {
                    AppError::External("IOL_ORDER_PATH ausente en modo live".into())
                })?;
                let MarketSource::Iol(client) = &mut self.source else {
                    return Err(AppError::External("cliente IOL no inicializado".into()));
                };
                client
                    .submit_order(order_path, request)
                    .await
                    .map_err(|error| AppError::External(error.to_string()))?
            }
        };
        self.journal.append(
            timestamp,
            Some(request.operation_id.clone()),
            JournalEventKind::OrderUpdated {
                execution: execution.clone(),
            },
        )?;
        Ok(execution)
    }

    fn handle_unfilled_order(&mut self, execution: &OrderExecution) {
        self.push_log(format!(
            "orden {} no ejecutada: {:?}",
            execution.operation_id, execution.status
        ));
        if self.config.mode == Mode::Live
            && matches!(
                execution.status,
                OrderStatus::Pending | OrderStatus::PartiallyExecuted
            )
        {
            self.risk.engage_kill_switch();
            self.engine.halt();
            self.status = "orden live no resuelta; intervencion manual requerida".into();
        } else {
            self.engine.resume();
        }
    }

    pub fn toggle_pause(&mut self) {
        self.paused = !self.paused;
        self.push_log(if self.paused {
            "procesamiento pausado".into()
        } else {
            "procesamiento reanudado".into()
        });
    }

    pub fn toggle_kill_switch(&mut self) -> Result<(), AppError> {
        if self.risk.state.kill_switch {
            if self.reconciliation_blocked {
                self.push_log(
                    "kill switch no se puede desactivar: reconciliacion pendiente".into(),
                );
                return Ok(());
            }
            self.risk.resume().map_err(AppError::OrderRejected)?;
            self.engine.resume();
            self.push_log("kill switch desactivado".into());
        } else {
            self.risk.engage_kill_switch();
            if self.engine.position.is_none() {
                self.engine.halt();
            }
            self.push_log("kill switch activado".into());
        }
        let timestamp = self
            .current_frame
            .as_ref()
            .map_or_else(unix_now, |frame| frame.underlying.timestamp_secs);
        self.journal.append(
            timestamp,
            None,
            JournalEventKind::KillSwitch {
                active: self.risk.state.kill_switch,
            },
        )?;
        Ok(())
    }

    pub async fn manual_close(&mut self) -> Result<(), AppError> {
        if self.reconciliation_blocked {
            return Err(AppError::OrderRejected(
                "cierre bloqueado: existe una orden o inconsistencia sin reconciliar".into(),
            ));
        }
        let Some(position) = self.engine.position.clone() else {
            self.push_log("no hay posicion para cerrar".into());
            return Ok(());
        };
        let Some(frame) = &self.current_frame else {
            return Ok(());
        };
        let option = frame
            .option(&position.option_symbol)
            .ok_or_else(|| AppError::InvalidMarketData("opcion activa no cotiza".into()))?;
        let market_price = option
            .executable_sell_price()
            .ok_or_else(|| AppError::InvalidMarketData("bid de opcion ausente".into()))?;
        let pnl = calculate_pnl_with_contract_multiplier(
            position.entry_price,
            market_price,
            position.contracts,
            position.contract_multiplier,
            self.config.commission_percentage,
            self.config.tax_percentage,
            self.config.min_profit_multiplier,
        );
        self.close_position(
            frame.underlying.timestamp_secs,
            market_price,
            pnl,
            ExitReason::Manual,
        )
        .await
    }

    pub fn metrics(&self) -> PortfolioMetrics {
        self.portfolio.metrics()
    }

    pub fn logs(&self) -> &VecDeque<String> {
        &self.logs
    }

    pub fn snapshot(&mut self) -> Result<(), AppError> {
        let timestamp = self
            .current_frame
            .as_ref()
            .map_or_else(unix_now, |frame| frame.underlying.timestamp_secs);
        let snapshot = Snapshot::new(
            timestamp,
            self.journal.last_sequence(),
            self.engine.clone(),
            self.portfolio.clone(),
            self.risk.clone(),
            RuntimeSnapshot {
                detector: self.detector.clone(),
                current_frame: self.current_frame.clone(),
                current_trend: self.current_trend.clone(),
                current_pnl: self.current_pnl,
                last_market_timestamp: self.last_market_timestamp,
                operation_counter: self.operation_counter,
                last_traded_signal: self.last_traded_signal,
                ticks: self.ticks,
                selected_option: self.selected_option.clone(),
            },
        );
        save_snapshot(&self.snapshot_path, &snapshot)
    }

    pub fn shutdown(&mut self) -> Result<(), AppError> {
        let timestamp = self
            .current_frame
            .as_ref()
            .map_or_else(unix_now, |frame| frame.underlying.timestamp_secs);
        self.journal.append(
            timestamp,
            self.engine
                .position
                .as_ref()
                .map(|position| position.operation_id.clone()),
            JournalEventKind::Shutdown { clean: true },
        )?;
        self.snapshot()?;
        self.journal.sync()
    }

    fn next_operation_id(&mut self, timestamp: i64, action: &str) -> String {
        self.operation_counter = self.operation_counter.saturating_add(1);
        format!(
            "{:?}-{}-{timestamp}-{action}-{}",
            self.config.mode, self.config.ticker, self.operation_counter
        )
        .to_ascii_lowercase()
    }

    fn push_log(&mut self, message: String) {
        if self.logs.len() == 100 {
            self.logs.pop_front();
        }
        self.logs.push_back(message);
    }
}

fn purchase_cash_required(
    price: f64,
    contracts: u32,
    contract_multiplier: u32,
    commission_percentage: f64,
) -> f64 {
    let premium = price * contracts as f64 * contract_multiplier as f64;
    premium * (1.0 + commission_percentage.max(0.0) / 100.0)
}

fn affordable_contracts(
    budget: f64,
    limit_price: f64,
    contract_multiplier: u32,
    commission_percentage: f64,
    max_position_size: u32,
) -> u32 {
    if !budget.is_finite()
        || budget <= 0.0
        || !limit_price.is_finite()
        || limit_price <= 0.0
        || contract_multiplier == 0
    {
        return 0;
    }
    let per_contract =
        purchase_cash_required(limit_price, 1, contract_multiplier, commission_percentage);
    ((budget / per_contract)
        .floor()
        .max(0.0)
        .min(u32::MAX as f64) as u32)
        .min(max_position_size)
}

fn unresolved_local_orders(events: &[JournalEvent]) -> Vec<String> {
    let mut pending = HashMap::<String, bool>::new();
    for event in events {
        match &event.event {
            JournalEventKind::OrderSubmitted { .. } => {
                if let Some(operation_id) = &event.operation_id {
                    pending.insert(operation_id.clone(), true);
                }
            }
            JournalEventKind::OrderUpdated { execution } => {
                if matches!(
                    execution.status,
                    OrderStatus::Pending | OrderStatus::PartiallyExecuted
                ) {
                    pending.insert(execution.operation_id.clone(), true);
                } else {
                    pending.remove(&execution.operation_id);
                }
            }
            _ => {}
        }
    }
    let mut operation_ids: Vec<_> = pending.into_keys().collect();
    operation_ids.sort();
    operation_ids
}

fn apply_recovery_event(
    engine: &mut TradingEngine,
    portfolio: &mut Portfolio,
    risk: &mut RiskManager,
    event: &JournalEvent,
) -> Result<(), AppError> {
    match &event.event {
        JournalEventKind::PositionOpened { position } => {
            if engine.position.is_none() {
                engine.open_position(position.clone());
            }
            if !portfolio.contains(&position.operation_id) {
                portfolio.open(position.clone());
            }
        }
        JournalEventKind::PositionClosed {
            operation_id,
            exit_price,
            net_pnl,
            reason,
        } => {
            if portfolio.contains(operation_id) {
                portfolio.close(
                    operation_id,
                    *exit_price,
                    *net_pnl,
                    event.timestamp_secs,
                    *reason,
                );
                risk.record_close(*net_pnl);
            }
            if engine
                .position
                .as_ref()
                .is_some_and(|position| position.operation_id == *operation_id)
            {
                engine.close(*reason);
            }
        }
        JournalEventKind::KillSwitch { active } => {
            if *active {
                risk.engage_kill_switch();
                if engine.position.is_none() {
                    engine.halt();
                }
            } else {
                let _ = risk.resume();
                engine.resume();
            }
        }
        _ => {}
    }
    if engine
        .position
        .as_ref()
        .is_some_and(|position| !portfolio.contains(&position.operation_id))
    {
        return Err(AppError::Recovery(
            "snapshot/journal dejaron motor y portfolio inconsistentes".into(),
        ));
    }
    Ok(())
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::trading::TradingState;

    fn replay_config() -> Config {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        Config {
            mode: Mode::Replay,
            ticker: "GAL".into(),
            check_interval_secs: 1,
            price_history_minutes: 1,
            min_samples_for_trend: 3,
            trend_change_samples: 3,
            commission_percentage: 0.19,
            tax_percentage: 35.0,
            min_profit_multiplier: 2.0,
            option_expiry_days: 1,
            max_position_size: 5,
            position_timeout_mins: 60,
            max_concurrent_requests: 10,
            cache_ttl_secs: 60,
            log_level: "info".into(),
            tui_enabled: false,
            recover_state: false,
            data_dir: std::env::temp_dir()
                .join(format!("options-app-test-{}-{unique}", std::process::id())),
            replay_path: None,
            max_investment_amount: 100_000.0,
            max_loss_per_trade: 5_000.0,
            max_daily_loss: 10_000.0,
            max_trades_per_day: 20,
            stop_loss_percentage: 15.0,
            contract_multiplier: 1,
            paper_slippage_bps: 5.0,
            max_market_data_age_secs: 15,
            max_option_spread_percentage: 20.0,
            iol_base_url: "https://example.invalid".into(),
            iol_order_path: None,
            live_confirmed: false,
        }
    }

    #[tokio::test]
    async fn replay_opens_and_closes_option_positions() {
        let mut app = TradingApp::new(replay_config()).unwrap();
        while app.step().await.unwrap() {}
        assert!(app.metrics().trades > 0);
        assert!(app
            .portfolio
            .closed_trades()
            .iter()
            .all(|trade| trade.position.entry_price < 10.0));
    }

    #[tokio::test]
    async fn kill_switch_blocks_new_entries() {
        let mut app = TradingApp::new(replay_config()).unwrap();
        app.toggle_kill_switch().unwrap();
        for _ in 0..10 {
            app.step().await.unwrap();
        }
        assert_eq!(app.metrics().trades, 0);
        assert_eq!(app.engine.state, TradingState::Halted);
    }

    #[tokio::test]
    async fn restart_restores_runtime_and_resumes_replay_cursor() {
        let config = replay_config();
        let mut app = TradingApp::new(config.clone()).unwrap();
        for _ in 0..12 {
            app.step().await.unwrap();
        }
        app.shutdown().unwrap();
        let expected_metrics = app.metrics();
        let expected_timestamp = app.last_market_timestamp;
        drop(app);

        let mut recovered_config = config;
        recovered_config.recover_state = true;
        let mut recovered = TradingApp::new(recovered_config).unwrap();
        assert_eq!(recovered.metrics(), expected_metrics);
        assert_eq!(recovered.last_market_timestamp, expected_timestamp);
        recovered.step().await.unwrap();
        assert!(recovered.last_market_timestamp > expected_timestamp);
    }

    #[tokio::test]
    async fn reconstructed_position_is_immediately_checked_for_profit_exit() {
        let mut app = TradingApp::new(replay_config()).unwrap();
        app.step().await.unwrap();
        let frame = app.current_frame.clone().unwrap();
        let quote = frame
            .options
            .iter()
            .find(|quote| quote.executable_sell_price().is_some())
            .unwrap()
            .clone();
        let bid = quote.executable_sell_price().unwrap();
        app.apply_account_snapshot(
            frame.underlying.timestamp_secs,
            AccountSnapshot {
                positions: vec![AccountPosition {
                    symbol: quote.symbol.clone(),
                    quantity: 1,
                    average_price: Some(bid * 0.25),
                    kind: Some(PositionKind::from(quote.kind)),
                    is_option: true,
                }],
                pending_orders: Vec::new(),
            },
        )
        .unwrap();
        assert!(app.engine.position.is_some());

        app.evaluate_exit(frame.underlying.timestamp_secs)
            .await
            .unwrap();

        assert!(app.engine.position.is_none());
        assert_eq!(app.engine.last_exit_reason, Some(ExitReason::ProfitTarget));
    }

    #[tokio::test]
    async fn reconstructed_position_is_immediately_checked_for_stop_loss() {
        let mut app = TradingApp::new(replay_config()).unwrap();
        app.step().await.unwrap();
        let frame = app.current_frame.clone().unwrap();
        let quote = frame
            .options
            .iter()
            .find(|quote| quote.executable_sell_price().is_some())
            .unwrap()
            .clone();
        let bid = quote.executable_sell_price().unwrap();
        app.apply_account_snapshot(
            frame.underlying.timestamp_secs,
            AccountSnapshot {
                positions: vec![AccountPosition {
                    symbol: quote.symbol.clone(),
                    quantity: 1,
                    average_price: Some(bid * 2.0),
                    kind: Some(PositionKind::from(quote.kind)),
                    is_option: true,
                }],
                pending_orders: Vec::new(),
            },
        )
        .unwrap();
        let option = app
            .current_frame
            .as_mut()
            .unwrap()
            .options
            .iter_mut()
            .find(|option| option.symbol == quote.symbol)
            .unwrap();
        option.ask = Some(bid * 2.0);

        app.evaluate_exit(frame.underlying.timestamp_secs)
            .await
            .unwrap();

        assert!(app.engine.position.is_none());
        assert_eq!(app.engine.last_exit_reason, Some(ExitReason::StopLoss));
    }

    #[tokio::test]
    async fn wide_spread_blocks_entry_and_engages_kill_switch() {
        let mut app = TradingApp::new(replay_config()).unwrap();
        app.step().await.unwrap();
        let timestamp = app
            .current_frame
            .as_ref()
            .unwrap()
            .underlying
            .timestamp_secs;
        for option in &mut app.current_frame.as_mut().unwrap().options {
            if option.kind == OptionKind::Call {
                option.bid = Some(1.0);
                option.ask = Some(2.0);
            }
        }

        app.evaluate_entry(timestamp, Direction::Up).await.unwrap();

        assert!(app.engine.position.is_none());
        assert!(app.risk.state.kill_switch);
        assert_eq!(app.engine.state, TradingState::Halted);
    }

    #[test]
    fn purchase_quantity_never_exceeds_cash_budget_including_commission() {
        let quantity = affordable_contracts(100_000.0, 20_000.0, 1, 0.19, 10);
        assert_eq!(quantity, 4);
        assert!(purchase_cash_required(20_000.0, quantity, 1, 0.19) <= 100_000.0);
        assert!(purchase_cash_required(20_000.0, quantity + 1, 1, 0.19) > 100_000.0);
    }

    #[test]
    fn startup_detects_orders_without_a_terminal_update() {
        let events = vec![
            JournalEvent {
                sequence: 1,
                timestamp_secs: 1,
                operation_id: Some("pending-1".into()),
                event: JournalEventKind::OrderSubmitted {
                    symbol: "GAL-C-100".into(),
                    side: OrderSide::Buy,
                    quantity: 1,
                    limit_price: 2.0,
                },
            },
            JournalEvent {
                sequence: 2,
                timestamp_secs: 2,
                operation_id: Some("done-1".into()),
                event: JournalEventKind::OrderSubmitted {
                    symbol: "GAL-P-100".into(),
                    side: OrderSide::Buy,
                    quantity: 1,
                    limit_price: 2.0,
                },
            },
            JournalEvent {
                sequence: 3,
                timestamp_secs: 3,
                operation_id: Some("done-1".into()),
                event: JournalEventKind::OrderUpdated {
                    execution: OrderExecution {
                        operation_id: "done-1".into(),
                        status: OrderStatus::Executed,
                        filled_quantity: 1,
                        fill_price: Some(2.0),
                        broker_order_id: Some("42".into()),
                        message: None,
                    },
                },
            },
        ];
        assert_eq!(unresolved_local_orders(&events), vec!["pending-1"]);
    }
}
