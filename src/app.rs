use std::{
    collections::BTreeMap, collections::BTreeSet, collections::HashMap, collections::VecDeque, env,
    fs::OpenOptions, io::Write, path::Path, path::PathBuf,
};

use crate::{
    broker::{
        AccountPosition, AccountSnapshot, BrokerClient, OrderExecution, OrderRequest, OrderSide,
        OrderStatus, PaperBroker,
    },
    config::{Config, Mode, LIVE_CONFIRMATION},
    errors::AppError,
    iol_client::{AccountMovement, AccountProfile, CostCalibration, IolClient, IolRealtimeEvent},
    learning::{
        trading_regressed, AuthorizationRequest, EvidenceBundle, EvidenceSource,
        ExecutionAuthorization, GateRequirements, LearningReport, LearningState, LiveStage,
        StrategyManifest, ValidationContext, ValidationTrade, AUTHORIZATION_SCHEMA_VERSION,
        EVIDENCE_SCHEMA_VERSION,
    },
    learning_model::SignalFeatures,
    market::{
        select_option_with_criteria, MarketDataProvider, MarketFrame, OptionKind,
        OptionSelectionCriteria, ReplayMarket,
    },
    number_format::{decimal, integer},
    pattern::{Direction, PriceSample, Trend, TrendCriteria, TrendDetector},
    persistence::{
        load_snapshot, read_events, save_snapshot, Journal, JournalEvent, JournalEventKind,
        RuntimeSnapshot, Snapshot,
    },
    portfolio::{Portfolio, PortfolioMetrics},
    risk::{RiskLimits, RiskManager},
    trading::{
        build_position_economics, calculate_pnl_with_contract_multiplier, calculate_position_pnl,
        EntryContext, ExitReason, Pnl, Position, PositionKind, TradingEngine,
    },
};

enum MarketSource {
    Replay(ReplayMarket),
    Iol(Box<IolClient>),
}

struct InstanceLease {
    path: PathBuf,
}

impl InstanceLease {
    fn acquire(path: PathBuf) -> Result<Self, AppError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        match OpenOptions::new().create_new(true).write(true).open(&path) {
            Ok(mut file) => {
                writeln!(file, "{}", std::process::id())?;
                file.sync_all()?;
                Ok(Self { path })
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                let owner = std::fs::read_to_string(&path).unwrap_or_default();
                let active = owner
                    .trim()
                    .parse::<u32>()
                    .ok()
                    .is_some_and(process_is_active);
                if active {
                    Err(AppError::Recovery(format!(
                        "ya existe una instancia activa para este modo (PID {})",
                        owner.trim()
                    )))
                } else {
                    std::fs::remove_file(&path)?;
                    Self::acquire(path)
                }
            }
            Err(error) => Err(error.into()),
        }
    }
}

#[cfg(target_os = "linux")]
fn process_is_active(pid: u32) -> bool {
    PathBuf::from(format!("/proc/{pid}")).exists()
}

#[cfg(not(target_os = "linux"))]
fn process_is_active(_pid: u32) -> bool {
    true
}

impl Drop for InstanceLease {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

pub(crate) struct LogEntry {
    pub timestamp_secs: i64,
    pub message: String,
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
    pub account_profile: Option<AccountProfile>,
    pub cost_calibration: Option<CostCalibration>,
    pub realtime_status: String,
    pub connection_operational: bool,
    pub last_movement: Option<AccountMovement>,
    pub live_stage: LiveStage,
    pub learning_state: LearningState,
    pub trading_performance: Vec<ValidationTrade>,
    logs: VecDeque<LogEntry>,
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
    startup_context_loaded: bool,
    return_to_learning_pending: bool,
    cooldown_until_secs: i64,
    learning_report_path: PathBuf,
    evidence_bundle_path: PathBuf,
    strategy_manifest: StrategyManifest,
    authorization_request_path: PathBuf,
    real_account_clear: bool,
    last_account_reconciliation_secs: i64,
    dataset_ids: Vec<String>,
    _instance_lease: InstanceLease,
}

impl TradingApp {
    pub fn new(mut config: Config) -> Result<Self, AppError> {
        let replay_path = config.replay_path.clone();
        let synthetic_test_source = cfg!(test) && config.iol_base_url == "https://example.invalid";
        let mut source = if let Some(path) = &replay_path {
            MarketSource::Replay(ReplayMarket::from_jsonl(path)?)
        } else if synthetic_test_source {
            MarketSource::Replay(ReplayMarket::synthetic(&config.ticker))
        } else {
            let username = env::var("IOL_USERNAME")
                .map_err(|_| AppError::External("IOL_USERNAME ausente".into()))?;
            let encrypted_password = env::var("IOL_PASSWORD")
                .map_err(|_| AppError::External("IOL_PASSWORD ausente".into()))?;
            let refresh_token = env::var("IOL_REFRESH_TOKEN").unwrap_or_default();
            let client = IolClient::new(
                &config.iol_base_url,
                username,
                encrypted_password,
                refresh_token,
            )
            .map_err(|error| AppError::External(error.to_string()))?
            .with_catalog_cache_ttl(config.cache_ttl_secs)
            .with_websocket_url(&config.iol_websocket_url);
            MarketSource::Iol(Box::new(client))
        };
        let dataset_ids = replay_path
            .as_ref()
            .map(|path| dataset_id(path))
            .transpose()?
            .into_iter()
            .collect();

        let mode_name = format!("{:?}", config.mode).to_ascii_lowercase();
        let instance_lease =
            InstanceLease::acquire(config.data_dir.join(&mode_name).join("instance.lock"))?;
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
        let mut detector = TrendDetector::new_robust(
            config.history_capacity(),
            config.min_samples_for_trend,
            TrendCriteria {
                warmup_samples: config.history_capacity(),
                deadband_percentage: config.trend_deadband_percentage,
                min_slope_percent_per_minute: config.min_trend_slope_percent_per_minute,
                min_r_squared: config.min_trend_r_squared,
                min_move_volatility_ratio: config.min_trend_move_volatility_ratio,
            },
        );
        let mut current_frame = None;
        let mut current_trend = None;
        let mut current_pnl = None;
        let mut last_market_timestamp = None;
        let mut operation_counter = 0;
        let mut last_traded_signal = None;
        let mut ticks = 0;
        let mut selected_option = None;
        let mut fingerprint = strategy_fingerprint(&config);
        let mut live_stage = LiveStage::Learning;
        let mut learning_state = LearningState::new(fingerprint.clone());
        let mut trading_performance = Vec::new();
        let mut return_to_learning_pending = false;
        let mut cooldown_until_secs = 0;
        let mut cost_calibration = None;

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
            live_stage = snapshot.runtime.live_stage;
            learning_state = snapshot.runtime.learning_state;
            trading_performance = snapshot.runtime.trading_performance;
            return_to_learning_pending = snapshot.runtime.return_to_learning_pending;
            cooldown_until_secs = snapshot.runtime.cooldown_until_secs;
            cost_calibration = snapshot.runtime.cost_calibration;
            if let Some(calibration) = &cost_calibration {
                config.commission_percentage = calibration.commission_percentage;
                config.vat_percentage = calibration.vat_percentage;
                config.other_fees_percentage = calibration.other_fees_percentage;
                fingerprint = strategy_fingerprint(&config);
            }
            if learning_state.strategy_fingerprint != fingerprint {
                learning_state.reset(fingerprint.clone());
                live_stage = LiveStage::Learning;
                return_to_learning_pending = false;
            }
            if let (Some(timestamp), MarketSource::Replay(replay)) =
                (last_market_timestamp, &mut source)
            {
                replay.resume_after(timestamp);
            }
            let events = journal.events_after(snapshot.last_sequence)?;
            for event in &events {
                apply_recovery_event(
                    &mut engine,
                    &mut portfolio,
                    &mut risk,
                    &mut learning_state,
                    &mut trading_performance,
                    event,
                )?;
                if let JournalEventKind::LiveStageChanged { to, .. } = event.event {
                    live_stage = to;
                }
            }
            learning_state.approved = learning_state
                .report(gate_requirements_for_config(&config))
                .eligible;
            // Los límites de riesgo vigentes siempre provienen de la configuración actual,
            // no de un snapshot potencialmente antiguo.
            risk.limits = configured_limits;
            recovery_message = Some(format!(
                "estado recuperado desde secuencia {} y {} eventos",
                integer(snapshot.last_sequence),
                integer(events.len())
            ));
        }

        let strategy_manifest = strategy_manifest(&config);
        fingerprint = strategy_manifest.fingerprint.clone();
        if learning_state.strategy_fingerprint != fingerprint {
            learning_state.reset(fingerprint.clone());
            live_stage = LiveStage::Learning;
            trading_performance.clear();
            return_to_learning_pending = false;
        }
        let evidence_dir = config.data_dir.join("evidence").join(&fingerprint);
        let learning_report_path = evidence_dir.join("learning-eligibility.json");
        let evidence_bundle_path = evidence_dir.join("evidence-bundle.json");
        let authorization_request_path = config
            .data_dir
            .join("live")
            .join("live-authorization-request.json");
        if config.recover_state && evidence_bundle_path.exists() {
            match load_evidence_bundle(&evidence_bundle_path) {
                Ok(bundle)
                    if bundle.is_compatible(
                        &strategy_manifest,
                        gate_requirements_for_config(&config),
                    ) =>
                {
                    let mut imported = 0_u64;
                    for trade in bundle.learning_state.trades {
                        imported += u64::from(learning_state.record(trade));
                    }
                    learning_state.approved = learning_state
                        .report(gate_requirements_for_config(&config))
                        .eligible;
                    if imported > 0 {
                        recovery_message = Some(format!(
                            "se importaron {} operaciones compatibles del bundle de evidencia",
                            integer(imported)
                        ));
                    }
                }
                Ok(_) => {
                    recovery_message = Some(
                        "el bundle de evidencia no coincide con estrategia, build o gate; se ignora"
                            .into(),
                    );
                }
                Err(error) => return Err(error),
            }
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

        let realtime_status = if matches!(source, MarketSource::Replay(_)) {
            "Fuente replay interna para pruebas".into()
        } else {
            "Conectando con IOL".into()
        };
        let mut app = Self {
            detector,
            paper_broker: PaperBroker::new(config.learning_slippage_bps),
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
            account_profile: None,
            cost_calibration,
            realtime_status,
            connection_operational: matches!(source, MarketSource::Replay(_)),
            last_movement: None,
            live_stage,
            learning_state,
            trading_performance,
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
            startup_context_loaded: false,
            return_to_learning_pending,
            cooldown_until_secs,
            learning_report_path,
            evidence_bundle_path,
            strategy_manifest,
            authorization_request_path,
            real_account_clear: false,
            last_account_reconciliation_secs: 0,
            dataset_ids,
            _instance_lease: instance_lease,
        };
        if let Some(message) = recovery_message {
            app.push_log(message);
        }
        app.push_log(format!(
            "Programa listo para seguir el precio de {}",
            app.config.ticker
        ));
        Ok(app)
    }

    pub async fn step(&mut self) -> Result<bool, AppError> {
        if self.completed {
            return Ok(false);
        }
        self.initialize_iol_context().await?;
        self.sync_realtime_events();
        let frame = match self.next_frame().await? {
            Some(frame) => frame,
            None => {
                self.completed = true;
                self.status = "Prueba terminada".into();
                self.push_log("Se terminaron los precios de prueba".into());
                return Ok(false);
            }
        };
        frame.validate(self.last_market_timestamp)?;
        if self.config.capture_market_data && !matches!(self.source, MarketSource::Replay(_)) {
            capture_market_frame(&self.config, &frame)?;
        }
        self.last_market_timestamp = Some(frame.underlying.timestamp_secs);
        self.ticks = self.ticks.saturating_add(1);
        let timestamp = frame.underlying.timestamp_secs;
        self.current_frame = Some(frame);
        self.risk.rollover(timestamp);
        self.sync_realtime_events();

        self.reconcile_startup(timestamp).await?;
        if self.reconciliation_blocked {
            self.snapshot()?;
            return Ok(true);
        }

        if self.config.mode == Mode::Live
            && matches!(self.source, MarketSource::Iol(_))
            && timestamp.saturating_sub(self.last_account_reconciliation_secs) >= 60
        {
            self.refresh_live_account(timestamp).await?;
            if self.reconciliation_blocked {
                self.snapshot()?;
                return Ok(true);
            }
        }
        if self.live_stage != LiveStage::Learning
            && !self.return_to_learning_pending
            && !self.has_fresh_option_calibration(timestamp)
        {
            self.return_to_learning_pending = true;
            self.push_log(
                "La calibración de opciones venció; se volverá a Learning al quedar plano".into(),
            );
            self.apply_pending_learning_return(timestamp)?;
        }
        if self.config.mode == Mode::Live
            && matches!(self.live_stage, LiveStage::Canary | LiveStage::Live)
            && !self.return_to_learning_pending
            && !self.config.live_ordering_ready()
        {
            self.return_to_learning_pending = true;
            self.push_log(
                "Live perdió la autorización de órdenes; se volverá a Learning al quedar plano"
                    .into(),
            );
            self.apply_pending_learning_return(timestamp)?;
        }

        if !matches!(self.source, MarketSource::Replay(_)) {
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
                "Precio {} · {} · {}",
                decimal(
                    self.current_frame
                        .as_ref()
                        .map_or(0.0, |frame| frame.underlying.last),
                    2
                ),
                simple_direction(trend.direction),
                if trend.confirmed {
                    "señal lista"
                } else if !trend.warmed_up {
                    "calentando histórico"
                } else {
                    "esperando más precios"
                }
            );
        }

        if self.engine.position.is_some() {
            self.evaluate_exit(timestamp).await?;
        }
        self.apply_pending_learning_return(timestamp)?;
        self.maybe_promote_live(timestamp).await?;
        if !self.paused
            && self.engine.position.is_none()
            && trend.confirmed
            && !self.risk.state.kill_switch
            && timestamp >= self.cooldown_until_secs
            && self.last_traded_signal != Some(trend.direction)
        {
            self.evaluate_entry(timestamp, trend.direction).await?;
        }
        self.snapshot()?;
        Ok(true)
    }

    /// Completa el preflight de IOL antes de que la interfaz tome control de la terminal.
    pub async fn connect(&mut self) -> Result<(), AppError> {
        self.initialize_iol_context().await
    }

    pub fn mark_connection_retry(&mut self, attempt: u32, total: u32, error: &AppError) {
        self.realtime_status =
            format!("Reconectando con IOL: intento {attempt} de {total} después de: {error}");
        self.status = "Esperando que IOL vuelva a estar disponible".into();
        self.push_log(self.realtime_status.clone());
    }

    pub fn mark_connection_restored(&mut self) {
        self.connection_operational = true;
        self.realtime_status = "Conexión operativa con IOL".into();
        self.push_log("La conexión con IOL se restableció; el motor vuelve a operar".into());
    }

    pub fn mark_connection_not_operational(
        &mut self,
        attempts: u32,
        error: &AppError,
    ) -> Result<(), AppError> {
        self.connection_operational = false;
        self.paused = true;
        self.risk.engage_operational_halt(format!(
            "conexión con IOL agotada después de {attempts} reintentos: {error}"
        ));
        self.engine.halt();
        self.realtime_status = "NO OPERATIVO · SIN CONEXIÓN CON IOL".into();
        self.status = format!(
            "NO OPERATIVO: se perdió la conexión con IOL después de {attempts} reintentos. No se procesan precios ni órdenes. Si hay una posición, revísela manualmente en IOL. Último error: {error}"
        );
        self.push_log(self.status.clone());
        self.snapshot()
    }

    async fn next_frame(&mut self) -> Result<Option<MarketFrame>, AppError> {
        match &mut self.source {
            MarketSource::Replay(market) => market.next_frame(),
            MarketSource::Iol(client) => client
                .market_frame_with_retry(&self.config.ticker, 1)
                .await
                .map(Some)
                .map_err(|error| AppError::Connection(error.to_string())),
        }
    }

    async fn evaluate_entry(
        &mut self,
        timestamp: i64,
        direction: Direction,
    ) -> Result<(), AppError> {
        if self.is_real_trading() {
            self.refresh_live_account(timestamp).await?;
            if !self.real_account_clear {
                return Ok(());
            }
        }
        if !self.engine.consider_entry(direction) {
            return Ok(());
        }
        let option_kind = match direction {
            Direction::Up => OptionKind::Call,
            Direction::Down => OptionKind::Put,
            Direction::Neutral => return Ok(()),
        };
        let quality_now = if matches!(self.source, MarketSource::Replay(_)) {
            timestamp
        } else {
            unix_now()
        };
        let max_spread = self
            .config
            .max_option_spread_percentage
            .min(self.config.stop_loss_percentage / 2.0);
        let option = self
            .current_frame
            .as_ref()
            .and_then(|frame| {
                select_option_with_criteria(
                    frame,
                    option_kind,
                    OptionSelectionCriteria {
                        min_expiry_days: self.config.option_expiry_days,
                        target_expiry_days: self.config.option_target_expiry_days,
                        max_expiry_days: self.config.option_max_expiry_days,
                        min_volume: self.config.min_option_volume,
                        max_spread_percentage: max_spread,
                        max_moneyness_distance_percentage: self
                            .config
                            .max_option_moneyness_distance_percentage,
                        now_secs: quality_now,
                        max_age_secs: self.config.max_market_data_age_secs,
                        operating_cost_percentage: self.config.operating_cost_percentage(),
                        slippage_bps: self.execution_slippage_bps(),
                    },
                )
            })
            .cloned();
        let Some(option) = option else {
            self.engine.resume();
            self.push_log(format!(
                "No encontré una opción con buenos precios para una {}",
                simple_option_direction(option_kind)
            ));
            return Ok(());
        };
        if self.live_stage != LiveStage::Learning {
            let assessment = self
                .learning_state
                .report(self.gate_requirements())
                .meta_filter;
            if assessment.recommended {
                if let (Some(model), Some(trend), Some(frame), Some(spread), Some(r_squared)) = (
                    assessment.model,
                    self.current_trend.as_ref(),
                    self.current_frame.as_ref(),
                    option.spread_percentage(),
                    self.current_trend
                        .as_ref()
                        .and_then(|trend| trend.r_squared),
                ) {
                    let underlying = frame.underlying.last;
                    let features = SignalFeatures([
                        spread,
                        option.volume as f64,
                        option.expiry_days as f64,
                        ((option.strike - underlying).abs() / underlying) * 100.0,
                        trend.confidence,
                        r_squared,
                        trend.slope_percent_per_minute.abs(),
                    ]);
                    if !model.allows(features) {
                        self.engine.resume();
                        self.last_traded_signal = Some(direction);
                        self.push_log(format!(
                            "Meta-filtro rechazó {}: probabilidad {:.1}% bajo umbral {:.1}%",
                            option.symbol,
                            model.probability(features) * 100.0,
                            model.threshold * 100.0
                        ));
                        return Ok(());
                    }
                }
            }
        }
        self.selected_option = Some(option.symbol.clone());
        let market_price = option
            .executable_buy_price()
            .ok_or_else(|| AppError::InvalidMarketData("ask de opcion ausente".into()))?;
        let limit_price = market_price * 1.005;
        let (max_investment_amount, max_loss_per_trade, max_position_size) =
            self.execution_risk_limits();
        let cash_quantity = affordable_contracts(
            max_investment_amount,
            limit_price,
            self.config.contract_multiplier,
            self.config.operating_cost_percentage(),
            max_position_size,
        );
        let risk_per_contract = build_position_economics(
            limit_price,
            1,
            self.config.contract_multiplier,
            self.config.operating_cost_percentage(),
            self.config.tax_percentage,
            self.execution_slippage_bps(),
            self.config.stop_loss_percentage,
            max_loss_per_trade,
            self.config.min_profit_multiplier,
            self.config.min_reward_risk_ratio,
        )
        .map_or(f64::INFINITY, |economics| economics.max_net_loss);
        let risk_quantity = if risk_per_contract.is_finite() && risk_per_contract > 0.0 {
            (max_loss_per_trade / risk_per_contract)
                .floor()
                .clamp(0.0, u32::MAX as f64) as u32
        } else {
            0
        };
        let quantity = cash_quantity.min(risk_quantity);
        if quantity == 0 {
            let reason = format!(
                "presupuesto {} insuficiente para un contrato de {}",
                decimal(max_investment_amount, 2),
                option.symbol
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
            self.config.operating_cost_percentage(),
        );
        if let Err(reason) = self.risk.allow_entry_at(timestamp, maximum_cash) {
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
        let fill_price = execution.fill_price.unwrap_or(market_price);
        let economics = build_position_economics(
            fill_price,
            execution.filled_quantity,
            self.config.contract_multiplier,
            self.config.operating_cost_percentage(),
            self.config.tax_percentage,
            self.execution_slippage_bps(),
            self.config.stop_loss_percentage,
            max_loss_per_trade,
            self.config.min_profit_multiplier,
            self.config.min_reward_risk_ratio,
        )
        .ok_or_else(|| AppError::OrderRejected("economía de posición inválida".into()))?;
        let position = Position {
            operation_id: operation_id.clone(),
            option_symbol: option.symbol.clone(),
            kind: PositionKind::from(option_kind),
            entry_price: fill_price,
            contracts: execution.filled_quantity,
            contract_multiplier: self.config.contract_multiplier,
            opened_at_secs: timestamp,
            economics: Some(economics),
            entry_context: Some(EntryContext {
                spread_percentage: option.spread_percentage(),
                option_volume: option.volume,
                days_to_expiry: option.expiry_days,
                moneyness_distance_percentage: ((option.strike
                    - self
                        .current_frame
                        .as_ref()
                        .map_or(option.strike, |frame| frame.underlying.last))
                .abs()
                    / self
                        .current_frame
                        .as_ref()
                        .map_or(option.strike, |frame| frame.underlying.last)
                        .max(f64::EPSILON))
                    * 100.0,
                trend_confidence: self
                    .current_trend
                    .as_ref()
                    .map_or(0.0, |trend| trend.confidence),
                trend_r_squared: self
                    .current_trend
                    .as_ref()
                    .and_then(|trend| trend.r_squared),
                trend_slope_percent_per_minute: self
                    .current_trend
                    .as_ref()
                    .map_or(0.0, |trend| trend.slope_percent_per_minute),
            }),
        };
        self.journal.append(
            timestamp,
            Some(operation_id),
            JournalEventKind::PositionOpened {
                position: position.clone(),
            },
        )?;
        if !self.engine.open_position(position.clone()) || !self.portfolio.open(position.clone()) {
            self.risk
                .engage_operational_halt("inconsistencia al registrar posicion ejecutada");
            self.engine.halt();
            return Err(AppError::Recovery(
                "inconsistencia al registrar posicion ejecutada".into(),
            ));
        }
        self.last_traded_signal = Some(direction);
        let buy_message = if self.is_real_trading() {
            "Compra real enviada"
        } else if self.config.mode == Mode::Readonly {
            "READONLY · debería COMPRAR; no se envió una orden a IOL"
        } else {
            "LEARNING · compra simulada; no se envió una orden a IOL"
        };
        self.push_log(format!(
            "{buy_message}: {} · {} contrato(s) · precio estimado {}",
            position.option_symbol,
            integer(position.contracts),
            decimal(position.entry_price, 4)
        ));
        Ok(())
    }

    async fn reconcile_startup(&mut self, timestamp: i64) -> Result<(), AppError> {
        if self.startup_reconciled {
            return Ok(());
        }
        if !self.local_pending_orders.is_empty() && self.config.mode == Mode::Readonly {
            self.startup_reconciled = true;
            return self.block_reconciliation(
                timestamp,
                format!(
                    "hay {} orden(es) locales sin estado final: {}",
                    integer(self.local_pending_orders.len()),
                    self.local_pending_orders.join(", ")
                ),
            );
        }

        if !self.local_pending_orders.is_empty() {
            self.push_log(format!(
                "{} orden(es) locales requieren confirmacion contra IOL",
                integer(self.local_pending_orders.len())
            ));
        }

        if self.config.mode == Mode::Readonly {
            self.startup_reconciled = true;
            self.push_log("La información guardada fue revisada y está en orden".into());
            return Ok(());
        }

        let account_result = match &mut self.source {
            MarketSource::Iol(client) => client.account_snapshot().await,
            MarketSource::Replay(_) => {
                self.startup_reconciled = true;
                self.real_account_clear = true;
                self.push_log("Replay aislado: no se consultó ni se operará una cuenta IOL".into());
                return Ok(());
            }
        };
        match account_result {
            Ok(account) => {
                self.startup_reconciled = true;
                self.apply_account_snapshot(timestamp, account)
            }
            Err(error) => Err(AppError::Connection(format!(
                "no se pudo consultar cartera/órdenes IOL: {error}"
            ))),
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

        if self.is_learning() {
            self.real_account_clear = option_positions.is_empty() && pending_options.is_empty();
            if !self.real_account_clear {
                return self.block_reconciliation(
                    timestamp,
                    "Learning requiere una cuenta IOL sin posiciones ni órdenes de opciones".into(),
                );
            }
            self.last_account_reconciliation_secs = timestamp;
            self.push_log("Cuenta IOL limpia; Learning continúa sin dinero real".into());
            return Ok(());
        }

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
                    integer(option_positions.len())
                ),
            );
        }

        match (self.engine.position.clone(), option_positions.first()) {
            (None, None) => {
                self.real_account_clear = true;
                self.last_account_reconciliation_secs = timestamp;
                self.push_log("IOL confirma que no hay opciones ni órdenes activas".into());
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
                            local.option_symbol,
                            integer(local.contracts),
                            remote.symbol,
                            integer(remote.quantity)
                        ),
                    );
                }
                self.engine.resume();
                self.real_account_clear = false;
                self.last_account_reconciliation_secs = timestamp;
                self.push_log(format!(
                    "posicion {} x{} confirmada contra cartera IOL",
                    local.option_symbol,
                    integer(local.contracts)
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
            economics: build_position_economics(
                entry_price,
                remote.quantity,
                self.config.contract_multiplier,
                self.config.operating_cost_percentage(),
                self.config.tax_percentage,
                self.execution_slippage_bps(),
                self.config.stop_loss_percentage,
                self.config.max_loss_per_trade,
                self.config.min_profit_multiplier,
                self.config.min_reward_risk_ratio,
            ),
            entry_context: None,
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
            self.risk.engage_operational_halt(
                "la exposición recuperada supera el presupuesto configurado",
            );
            self.push_log(format!(
                "exposicion recuperada {} supera presupuesto {}; nuevas entradas bloqueadas",
                decimal(position.notional(), 2),
                decimal(self.config.max_investment_amount, 2)
            ));
        }
        self.push_log(format!(
            "posicion reconstruida desde IOL: {:?} {} x{} @ {}",
            position.kind,
            position.option_symbol,
            integer(position.contracts),
            decimal(position.entry_price, 4)
        ));
        Ok(())
    }

    fn block_reconciliation(&mut self, timestamp: i64, reason: String) -> Result<(), AppError> {
        self.reconciliation_blocked = true;
        self.risk
            .engage_operational_halt(format!("reconciliación bloqueada: {reason}"));
        self.engine.halt();
        self.status = format!("Detenido: los datos guardados no coinciden con IOL: {reason}");
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
        self.risk
            .engage_operational_halt(format!("riesgo de mercado: {reason}"));
        self.engine.halt();
        self.status = format!("Detenido para evitar una operación insegura: {reason}");
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
        let quality_now = if matches!(self.source, MarketSource::Replay(_)) {
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
        let pnl = if position.economics.is_some() {
            calculate_position_pnl(&position, market_price)
        } else {
            calculate_pnl_with_contract_multiplier(
                position.entry_price,
                market_price,
                position.contracts,
                position.contract_multiplier,
                self.config.operating_cost_percentage(),
                self.config.tax_percentage,
                self.config.min_profit_multiplier,
            )
        };
        self.current_pnl = Some(pnl);
        let opposite = self
            .detector
            .robust_opposite_confirmed(position.direction(), self.config.trend_change_samples);
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
                    "La diferencia entre compra y venta es {}% y supera el límite de {}%; igualmente se vende para reducir el riesgo",
                    decimal(option.spread_percentage().unwrap_or_default(), 2),
                    decimal(self.config.max_option_spread_percentage, 2)
                ));
            }
            self.close_position(timestamp, market_price, pnl, reason)
                .await?;
            if reason == ExitReason::TrendReversal {
                self.cooldown_until_secs =
                    timestamp.saturating_add(self.config.reversal_cooldown_secs as i64);
                self.detector.reset_confirmation();
            }
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
        let pnl = if position.economics.is_some() {
            calculate_position_pnl(&position, fill_price)
        } else {
            calculate_pnl_with_contract_multiplier(
                position.entry_price,
                fill_price,
                position.contracts,
                position.contract_multiplier,
                self.config.operating_cost_percentage(),
                self.config.tax_percentage,
                self.config.min_profit_multiplier,
            )
        };
        let validation_trade =
            self.build_validation_trade(&position, pnl, fill_price, timestamp, reason);
        self.journal.append(
            timestamp,
            Some(position.operation_id.clone()),
            JournalEventKind::PositionClosed {
                operation_id: position.operation_id.clone(),
                exit_price: fill_price,
                net_pnl: pnl.net,
                reason,
                stage: self.live_stage,
                validation_trade: Some(validation_trade.clone()),
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
        self.record_stage_trade(validation_trade)?;
        self.risk.record_close_at(timestamp, pnl.net);
        self.current_pnl = Some(pnl);
        let sell_message = if self.is_real_trading() {
            "Venta real enviada"
        } else if self.config.mode == Mode::Readonly {
            "READONLY · debería VENDER; no se envió una orden a IOL"
        } else {
            "LEARNING · venta simulada; no se envió una orden a IOL"
        };
        self.push_log(format!(
            "{sell_message}: {} · precio estimado {} · {} · resultado neto {} (cotizado {})",
            position.option_symbol,
            decimal(fill_price, 4),
            simple_exit_reason(reason),
            decimal(pnl.net, 2),
            decimal(quoted_pnl.net, 2)
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
        let execution = if self.is_real_trading() {
            let order_path =
                self.config.iol_order_path.as_deref().ok_or_else(|| {
                    AppError::External("IOL_ORDER_PATH ausente en modo live".into())
                })?;
            let MarketSource::Iol(client) = &mut self.source else {
                return Err(AppError::External("cliente IOL no inicializado".into()));
            };
            match client.submit_order(order_path, request).await {
                Ok(execution) => execution,
                Err(error) => {
                    let reason = error.to_string();
                    self.journal.append(
                        timestamp,
                        Some(request.operation_id.clone()),
                        JournalEventKind::OrderUnknown {
                            request: request.clone(),
                            reason: reason.clone(),
                        },
                    )?;
                    self.journal.sync()?;
                    if !self.local_pending_orders.contains(&request.operation_id) {
                        self.local_pending_orders.push(request.operation_id.clone());
                    }
                    self.reconciliation_blocked = true;
                    self.real_account_clear = false;
                    self.risk.engage_operational_halt(format!(
                        "resultado desconocido para la orden {}: {reason}",
                        request.operation_id
                    ));
                    self.engine.halt();
                    self.status = format!(
                        "Orden {} sin confirmación; reconciliar manualmente con IOL",
                        request.operation_id
                    );
                    return Err(AppError::External(format!(
                        "orden enviada con resultado desconocido: {reason}"
                    )));
                }
            }
        } else {
            self.paper_broker.submit_limit(request.clone())?
        };
        if self.is_real_trading()
            && execution.status == OrderStatus::Executed
            && execution
                .broker_order_id
                .as_deref()
                .is_none_or(str::is_empty)
        {
            let reason = "IOL informó ejecución sin broker_order_id".to_string();
            self.journal.append(
                timestamp,
                Some(request.operation_id.clone()),
                JournalEventKind::OrderUnknown {
                    request: request.clone(),
                    reason: reason.clone(),
                },
            )?;
            self.journal.sync()?;
            if !self.local_pending_orders.contains(&request.operation_id) {
                self.local_pending_orders.push(request.operation_id.clone());
            }
            self.reconciliation_blocked = true;
            self.real_account_clear = false;
            self.risk.engage_operational_halt(&reason);
            self.engine.halt();
            return Err(AppError::Recovery(reason));
        }
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
        if self.is_real_trading()
            && matches!(
                execution.status,
                OrderStatus::Pending | OrderStatus::PartiallyExecuted
            )
        {
            self.risk
                .engage_operational_halt("orden real pendiente o parcialmente ejecutada");
            self.engine.halt();
            self.status =
                "Una orden con dinero real quedó sin confirmar; revisarla manualmente en IOL"
                    .into();
        } else {
            self.engine.resume();
        }
    }

    pub fn toggle_pause(&mut self) {
        self.paused = !self.paused;
        self.push_log(if self.paused {
            "Programa pausado".into()
        } else {
            "Programa reanudado".into()
        });
    }

    pub fn toggle_kill_switch(&mut self) -> Result<(), AppError> {
        if self.risk.state.kill_switch {
            if self.reconciliation_blocked {
                self.push_log(
                    "El freno de emergencia sigue activo porque los datos aún no coinciden con IOL"
                        .into(),
                );
                return Ok(());
            }
            self.risk.resume().map_err(AppError::OrderRejected)?;
            self.engine.resume();
            self.push_log("Freno de emergencia apagado".into());
        } else {
            self.risk.engage_kill_switch();
            if self.engine.position.is_none() {
                self.engine.halt();
            }
            self.push_log("Freno de emergencia activado".into());
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
            self.push_log("No hay ninguna compra para vender".into());
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
        let pnl = if position.economics.is_some() {
            calculate_position_pnl(&position, market_price)
        } else {
            calculate_pnl_with_contract_multiplier(
                position.entry_price,
                market_price,
                position.contracts,
                position.contract_multiplier,
                self.config.operating_cost_percentage(),
                self.config.tax_percentage,
                self.config.min_profit_multiplier,
            )
        };
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

    pub(crate) fn price_history(&self) -> &VecDeque<PriceSample> {
        self.detector.samples()
    }

    pub(crate) fn logs(&self) -> &VecDeque<LogEntry> {
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
                live_stage: self.live_stage,
                learning_state: self.learning_state.clone(),
                trading_performance: self.trading_performance.clone(),
                return_to_learning_pending: self.return_to_learning_pending,
                cooldown_until_secs: self.cooldown_until_secs,
                cost_calibration: self.cost_calibration.clone(),
            },
        );
        save_snapshot(&self.snapshot_path, &snapshot)
    }

    pub async fn shutdown(&mut self) -> Result<(), AppError> {
        self.shutdown_with_status(true).await
    }

    pub async fn shutdown_with_status(&mut self, requested_clean: bool) -> Result<(), AppError> {
        self.refresh_iol_costs().await;
        if let MarketSource::Iol(client) = &mut self.source {
            client.shutdown().await;
        }
        let timestamp = self
            .current_frame
            .as_ref()
            .map_or_else(unix_now, |frame| frame.underlying.timestamp_secs);
        let clean = requested_clean
            && self.connection_operational
            && !self.risk.state.kill_switch
            && self.engine.position.is_none()
            && !self.reconciliation_blocked
            && self.local_pending_orders.is_empty();
        self.journal.append(
            timestamp,
            self.engine
                .position
                .as_ref()
                .map(|position| position.operation_id.clone()),
            JournalEventKind::Shutdown { clean },
        )?;
        self.snapshot()?;
        self.journal.sync()
    }

    pub fn environment_suggestion(&self) -> Option<String> {
        let calibration = self.cost_calibration.as_ref()?;
        Some(format!(
            "# Aranceles observados en operación IOL {} (no se modificó .env)\nCOMMISSION_PERCENTAGE={:.6}\nVAT_PERCENTAGE={:.6}\nOTHER_FEES_PERCENTAGE={:.6}\n# Costo operativo efectivo: {:.6}%\n# Impuesto estimado sobre la ganancia positiva; no surge de los aranceles IOL\nTAX_PERCENTAGE={:.6}",
            calibration.operation_number,
            calibration.commission_percentage,
            calibration.vat_percentage,
            calibration.other_fees_percentage,
            calibration.total_cost_percentage,
            self.config.tax_percentage,
        ))
    }

    async fn initialize_iol_context(&mut self) -> Result<(), AppError> {
        if self.startup_context_loaded {
            return Ok(());
        }
        let context = match &mut self.source {
            MarketSource::Replay(_) => {
                self.startup_context_loaded = true;
                self.connection_operational = true;
                return Ok(());
            }
            MarketSource::Iol(client) => client
                .startup_context()
                .await
                .map_err(|error| AppError::Connection(error.to_string()))?,
        };
        self.startup_context_loaded = true;
        self.connection_operational = true;
        self.realtime_status = "Conexión inicial con IOL confirmada".into();
        if let Some(profile) = context.profile {
            self.push_log(format!(
                "cuenta IOL {} · {}",
                profile.account_number,
                profile.full_name()
            ));
            self.account_profile = Some(profile);
        }
        if let Some(calibration) = context.calibration {
            self.apply_cost_calibration(calibration);
        } else {
            self.push_log(format!(
                "IOL no tiene una operación terminada para calcular los costos; se usa el valor configurado: {}%",
                decimal(self.config.operating_cost_percentage(), 6)
            ));
        }
        for warning in context.warnings {
            self.push_log(warning);
        }
        Ok(())
    }

    async fn refresh_iol_costs(&mut self) {
        let result = match &mut self.source {
            MarketSource::Replay(_) => return,
            MarketSource::Iol(client) => client.latest_cost_calibration().await,
        };
        match result {
            Ok(Some(calibration)) => self.apply_cost_calibration(calibration),
            Ok(None) => self.push_log(
                "IOL no tiene una operación terminada para volver a calcular los costos".into(),
            ),
            Err(error) => self.push_log(format!(
                "No se pudieron actualizar los costos al finalizar: {error}"
            )),
        }
    }

    fn apply_cost_calibration(&mut self, calibration: CostCalibration) {
        if !calibration.instrument_is_option {
            self.push_log(format!(
                "La operación {} no está identificada como opción; se ignora para calibrar costos",
                calibration.operation_number
            ));
            return;
        }
        let previous_fingerprint = strategy_fingerprint(&self.config);
        self.config.commission_percentage = calibration.commission_percentage;
        self.config.vat_percentage = calibration.vat_percentage;
        self.config.other_fees_percentage = calibration.other_fees_percentage;
        let calibrated_fingerprint = strategy_fingerprint(&self.config);
        if calibrated_fingerprint != previous_fingerprint {
            if self.live_stage == LiveStage::Live {
                self.return_to_learning_pending = true;
                self.push_log(
                    "La calibración de costos cambió; Live volverá a Learning al quedar plano"
                        .into(),
                );
            } else if self.learning_state.trades.is_empty() {
                self.learning_state.strategy_fingerprint = calibrated_fingerprint;
                self.learning_state.approved = false;
            } else {
                self.learning_state.reset(calibrated_fingerprint);
                self.push_log(
                    "La calibración de costos cambió; comenzó un nuevo epoch de Learning".into(),
                );
            }
        }
        self.refresh_strategy_identity();
        self.push_log(format!(
            "Costos calculados con la operación {} de IOL: total {}% ({} cargos)",
            calibration.operation_number,
            decimal(calibration.total_cost_percentage, 6),
            integer(calibration.components.len())
        ));
        self.cost_calibration = Some(calibration);
    }

    fn refresh_strategy_identity(&mut self) {
        let manifest = strategy_manifest(&self.config);
        let evidence_dir = self
            .config
            .data_dir
            .join("evidence")
            .join(&manifest.fingerprint);
        self.learning_report_path = evidence_dir.join("learning-eligibility.json");
        self.evidence_bundle_path = evidence_dir.join("evidence-bundle.json");
        self.strategy_manifest = manifest;
    }

    fn has_fresh_option_calibration(&self, now_secs: i64) -> bool {
        self.cost_calibration.as_ref().is_some_and(|calibration| {
            calibration.instrument_is_option
                && now_secs.saturating_sub(calibration.observed_at_secs) <= 30 * 86_400
        })
    }

    fn sync_realtime_events(&mut self) {
        let events = match &mut self.source {
            MarketSource::Replay(_) => return,
            MarketSource::Iol(client) => client.drain_realtime_events(),
        };
        for event in events {
            match event {
                IolRealtimeEvent::Status(status) => {
                    self.realtime_status = status.clone();
                    self.push_log(status);
                }
                IolRealtimeEvent::Movement(movement) => {
                    self.push_log(format!(
                        "Movimiento recibido de IOL: {} · {} · {} · {}",
                        movement.tipo,
                        movement.estado,
                        movement.simbolo,
                        decimal(movement.monto.or(movement.cantidad).unwrap_or_default(), 2)
                    ));
                    self.last_movement = Some(movement);
                }
            }
        }
    }

    fn is_real_trading(&self) -> bool {
        self.config.mode == Mode::Live
            && matches!(self.live_stage, LiveStage::Canary | LiveStage::Live)
            && matches!(self.source, MarketSource::Iol(_))
    }

    fn is_learning(&self) -> bool {
        self.live_stage == LiveStage::Learning
    }

    fn execution_slippage_bps(&self) -> f64 {
        if self.is_learning() || self.config.mode == Mode::Live {
            self.config.learning_slippage_bps
        } else {
            self.config.readonly_slippage_bps
        }
    }

    fn execution_risk_limits(&self) -> (f64, f64, u32) {
        if self.live_stage == LiveStage::Canary {
            (
                self.config.canary_max_investment_amount,
                self.config.canary_max_loss_per_trade,
                self.config.canary_max_position_size,
            )
        } else {
            (
                self.config.max_investment_amount,
                self.config.max_loss_per_trade,
                self.config.max_position_size,
            )
        }
    }

    fn gate_requirements(&self) -> GateRequirements {
        gate_requirements_for_config(&self.config)
    }

    fn build_validation_trade(
        &self,
        position: &Position,
        pnl: Pnl,
        fill_price: f64,
        timestamp: i64,
        reason: ExitReason,
    ) -> ValidationTrade {
        let units = position.contracts as f64 * position.contract_multiplier as f64;
        let additional_slippage = fill_price * units * (self.execution_slippage_bps() / 10_000.0);
        let stressed_net_pnl = pnl.net - pnl.commission - additional_slippage;
        let max_net_loss = position
            .economics
            .map_or(self.config.max_loss_per_trade, |economics| {
                economics.max_net_loss
            });
        ValidationTrade {
            kind: position.kind,
            net_pnl: pnl.net,
            stressed_net_pnl,
            closed_at_secs: timestamp,
            context: ValidationContext {
                trade_id: position.operation_id.clone(),
                source: if matches!(self.source, MarketSource::Replay(_)) {
                    EvidenceSource::HistoricalOutOfSample
                } else {
                    match self.live_stage {
                        LiveStage::Learning | LiveStage::Eligible | LiveStage::Armed => {
                            EvidenceSource::Shadow
                        }
                        LiveStage::Canary => EvidenceSource::Canary,
                        LiveStage::Live => EvidenceSource::Live,
                    }
                },
                option_symbol: position.option_symbol.clone(),
                opened_at_secs: position.opened_at_secs,
                entry_price: position.entry_price,
                exit_price: fill_price,
                contracts: position.contracts,
                max_net_loss,
                r_multiple: pnl.net / max_net_loss.max(f64::EPSILON),
                stressed_r_multiple: stressed_net_pnl / max_net_loss.max(f64::EPSILON),
                exit_reason: Some(format!("{reason:?}").to_ascii_lowercase()),
                entry_spread_percentage: position
                    .entry_context
                    .and_then(|context| context.spread_percentage),
                option_volume: position.entry_context.map(|context| context.option_volume),
                days_to_expiry: position
                    .entry_context
                    .map(|context| i64::from(context.days_to_expiry)),
                moneyness_distance_percentage: position
                    .entry_context
                    .map(|context| context.moneyness_distance_percentage),
                trend_confidence: position
                    .entry_context
                    .map(|context| context.trend_confidence),
                trend_r_squared: position
                    .entry_context
                    .and_then(|context| context.trend_r_squared),
                trend_slope_percent_per_minute: position
                    .entry_context
                    .map(|context| context.trend_slope_percent_per_minute),
            },
        }
    }

    fn record_stage_trade(&mut self, trade: ValidationTrade) -> Result<(), AppError> {
        if self.live_stage == LiveStage::Learning {
            self.learning_state.record(trade);
            let report = self.learning_state.report(self.gate_requirements());
            self.learning_state.approved = report.eligible;
            self.save_learning_report(&report)?;
        } else {
            let trade_net_pnl = trade.net_pnl;
            self.trading_performance.push(trade);
            let daily_loss_after_close =
                self.risk.state.realized_pnl + trade_net_pnl <= -self.config.max_daily_loss;
            if daily_loss_after_close
                || trading_regressed(
                    &self.trading_performance,
                    self.config.live_regression_window_trades,
                    self.config.live_max_consecutive_losses,
                    self.config.max_daily_loss * 2.0,
                )
            {
                self.return_to_learning_pending = true;
                self.push_log(
                    "El rendimiento de la etapa Live dejó de cumplir los límites; se volverá a Learning al quedar plano"
                        .into(),
                );
            }
        }
        Ok(())
    }

    fn save_learning_report(&self, report: &LearningReport) -> Result<(), AppError> {
        write_json_atomic(&self.learning_report_path, report)?;
        let bundle = EvidenceBundle {
            schema_version: EVIDENCE_SCHEMA_VERSION,
            manifest: self.strategy_manifest.clone(),
            gate_policy: self.gate_requirements(),
            learning_state: self.learning_state.clone(),
            report: report.clone(),
            dataset_ids: self.dataset_ids.clone(),
            updated_at_secs: report.generated_at_secs,
        };
        write_json_atomic(&self.evidence_bundle_path, &bundle)?;
        Ok(())
    }

    fn ensure_authorization_request(
        &self,
        timestamp: i64,
    ) -> Result<Option<AuthorizationRequest>, AppError> {
        let Some(profile) = &self.account_profile else {
            return Ok(None);
        };
        let report = self.learning_state.report(self.gate_requirements());
        let report_sha256 = digest_hex(&serde_json::to_vec(&report)?);
        let mut desired = AuthorizationRequest {
            schema_version: AUTHORIZATION_SCHEMA_VERSION,
            account_number: profile.account_number.clone(),
            epoch: self.learning_state.epoch,
            strategy_fingerprint: self.strategy_manifest.fingerprint.clone(),
            build_hash: self.strategy_manifest.build_hash.clone(),
            report_sha256,
            canary_max_position_size: self.config.canary_max_position_size,
            canary_max_investment_amount: self.config.canary_max_investment_amount,
            canary_max_loss_per_trade: self.config.canary_max_loss_per_trade,
            canary_max_daily_loss: self.config.canary_max_daily_loss,
            generated_at_secs: timestamp,
        };
        if self.authorization_request_path.exists() {
            let existing: AuthorizationRequest =
                serde_json::from_slice(&std::fs::read(&self.authorization_request_path)?)?;
            desired.generated_at_secs = existing.generated_at_secs;
            if existing == desired {
                return Ok(Some(existing));
            }
            desired.generated_at_secs = timestamp;
        }
        write_json_atomic(&self.authorization_request_path, &desired)?;
        Ok(Some(desired))
    }

    fn authorization_is_valid(
        &self,
        request: &AuthorizationRequest,
        timestamp: i64,
    ) -> Result<bool, AppError> {
        if !self.config.live_ordering_ready() {
            return Ok(false);
        }
        let Some(path) = self.config.live_authorization_path.as_deref() else {
            return Ok(false);
        };
        let bytes = match std::fs::read(path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error.into()),
        };
        let authorization: ExecutionAuthorization = serde_json::from_slice(&bytes)?;
        let valid_lifetime = authorization.issued_at_secs <= timestamp
            && authorization.expires_at_secs > timestamp
            && authorization
                .expires_at_secs
                .saturating_sub(authorization.issued_at_secs)
                <= 15 * 60;
        Ok(authorization.schema_version == AUTHORIZATION_SCHEMA_VERSION
            && authorization.request == *request
            && authorization.confirmation == LIVE_CONFIRMATION
            && valid_lifetime)
    }

    fn consume_authorization(&self, timestamp: i64) -> Result<(), AppError> {
        let path = self
            .config
            .live_authorization_path
            .as_deref()
            .ok_or_else(|| AppError::External("LIVE_AUTHORIZATION_PATH ausente".into()))?;
        let parent = path.parent().unwrap_or_else(|| Path::new("."));
        let consumed = parent.join(format!(
            "live-authorization-consumed-epoch-{}-{timestamp}.json",
            self.learning_state.epoch
        ));
        std::fs::rename(path, consumed)?;
        Ok(())
    }

    fn canary_eligible(&self) -> bool {
        let trades = self
            .trading_performance
            .iter()
            .filter(|trade| trade.context.source == EvidenceSource::Canary)
            .collect::<Vec<_>>();
        let calls = trades
            .iter()
            .filter(|trade| trade.kind == PositionKind::Call)
            .count() as u64;
        let puts = trades
            .iter()
            .filter(|trade| trade.kind == PositionKind::Put)
            .count() as u64;
        let sessions = trades
            .iter()
            .map(|trade| argentina_day(trade.closed_at_secs))
            .collect::<BTreeSet<_>>()
            .len();
        let stressed_total = trades
            .iter()
            .map(|trade| trade.stressed_net_pnl)
            .sum::<f64>();
        let stressed_calls = trades
            .iter()
            .filter(|trade| trade.kind == PositionKind::Call)
            .map(|trade| trade.stressed_net_pnl)
            .sum::<f64>();
        let stressed_puts = trades
            .iter()
            .filter(|trade| trade.kind == PositionKind::Put)
            .map(|trade| trade.stressed_net_pnl)
            .sum::<f64>();
        trades.len() as u64 >= self.config.canary_min_trades
            && calls >= self.config.canary_min_call_trades
            && puts >= self.config.canary_min_put_trades
            && sessions >= self.config.canary_min_sessions
            && stressed_total > 0.0
            && stressed_calls > 0.0
            && stressed_puts > 0.0
            && !trading_regressed(
                &trades.into_iter().cloned().collect::<Vec<_>>(),
                self.config.live_regression_window_trades,
                self.config.live_max_consecutive_losses,
                self.config.canary_max_daily_loss * 2.0,
            )
    }

    async fn maybe_promote_live(&mut self, timestamp: i64) -> Result<(), AppError> {
        if self.engine.position.is_some()
            || self.reconciliation_blocked
            || self.risk.state.kill_switch
        {
            return Ok(());
        }
        match self.live_stage {
            LiveStage::Learning => {
                if !self.learning_state.approved
                    || !self.has_fresh_option_calibration(timestamp)
                    || !self
                        .current_trend
                        .as_ref()
                        .is_some_and(|trend| trend.warmed_up)
                {
                    return Ok(());
                }
                let report = self.learning_state.report(self.gate_requirements());
                if !report.eligible {
                    self.learning_state.approved = false;
                    return Ok(());
                }
                self.transition_live_stage(
                    timestamp,
                    LiveStage::Eligible,
                    "evidencia estadística aprobada",
                )
            }
            LiveStage::Eligible => {
                if self.config.mode == Mode::Readonly {
                    return Ok(());
                }
                self.refresh_live_account(timestamp).await?;
                if !self.real_account_clear {
                    return Ok(());
                }
                let Some(request) = self.ensure_authorization_request(timestamp)? else {
                    return Ok(());
                };
                if !self.authorization_is_valid(&request, timestamp)? {
                    return Ok(());
                }
                self.consume_authorization(timestamp)?;
                self.transition_live_stage(
                    timestamp,
                    LiveStage::Armed,
                    "autorización efímera validada y consumida",
                )
            }
            LiveStage::Armed => {
                self.refresh_live_account(timestamp).await?;
                if !self.real_account_clear {
                    return Ok(());
                }
                self.transition_live_stage(
                    timestamp,
                    LiveStage::Canary,
                    "preflight aprobado; exposición canary",
                )
            }
            LiveStage::Canary => {
                if self.canary_eligible() {
                    self.transition_live_stage(
                        timestamp,
                        LiveStage::Live,
                        "canary reconciliado y métricas aprobadas",
                    )?;
                    self.trading_performance.clear();
                }
                Ok(())
            }
            LiveStage::Live => Ok(()),
        }
    }

    async fn refresh_live_account(&mut self, timestamp: i64) -> Result<(), AppError> {
        if self.config.mode != Mode::Live
            || (timestamp.saturating_sub(self.last_account_reconciliation_secs) < 60
                && self.real_account_clear)
        {
            return Ok(());
        }
        let account = match &mut self.source {
            MarketSource::Iol(client) => client
                .account_snapshot()
                .await
                .map_err(|error| AppError::Connection(error.to_string()))?,
            MarketSource::Replay(_) => return Ok(()),
        };
        self.last_account_reconciliation_secs = timestamp;
        self.apply_account_snapshot(timestamp, account)
    }

    fn apply_pending_learning_return(&mut self, timestamp: i64) -> Result<(), AppError> {
        if self.live_stage != LiveStage::Learning
            && self.return_to_learning_pending
            && self.engine.position.is_none()
        {
            self.transition_live_stage(
                timestamp,
                LiveStage::Learning,
                "degradación del rendimiento de la etapa Live",
            )?;
            let fingerprint = strategy_fingerprint(&self.config);
            self.learning_state.reset(fingerprint);
            self.trading_performance.clear();
            self.return_to_learning_pending = false;
        }
        Ok(())
    }

    fn transition_live_stage(
        &mut self,
        timestamp: i64,
        to: LiveStage,
        reason: &str,
    ) -> Result<(), AppError> {
        let from = self.live_stage;
        if from == to {
            return Ok(());
        }
        self.journal.append(
            timestamp,
            None,
            JournalEventKind::LiveStageChanged {
                from,
                to,
                reason: reason.to_string(),
                epoch: self.learning_state.epoch,
            },
        )?;
        self.journal.sync()?;
        self.live_stage = to;
        self.risk.limits = if to == LiveStage::Canary {
            RiskLimits {
                max_notional: self.config.canary_max_investment_amount,
                max_loss_per_trade: self.config.canary_max_loss_per_trade,
                max_daily_loss: self.config.canary_max_daily_loss,
                max_trades_per_day: self.config.canary_max_trades_per_day,
            }
        } else {
            RiskLimits {
                max_notional: self.config.max_investment_amount,
                max_loss_per_trade: self.config.max_loss_per_trade,
                max_daily_loss: self.config.max_daily_loss,
                max_trades_per_day: self.config.max_trades_per_day,
            }
        };
        self.push_log(format!("Etapa cambió de {:?} a {:?}: {reason}", from, to));
        self.snapshot()?;
        Ok(())
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
        tracing::info!(message = %message, "evento operativo");
        if self.logs.len() == 100 {
            self.logs.pop_front();
        }
        self.logs.push_back(LogEntry {
            timestamp_secs: unix_now(),
            message,
        });
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

fn strategy_fingerprint(config: &Config) -> String {
    strategy_manifest(config).fingerprint
}

fn strategy_manifest(config: &Config) -> StrategyManifest {
    let mut parameters = BTreeMap::new();
    parameters.insert("ticker".into(), config.ticker.clone());
    macro_rules! add_parameter {
        ($field:ident) => {
            parameters.insert(stringify!($field).into(), config.$field.to_string());
        };
    }
    add_parameter!(check_interval_secs);
    add_parameter!(price_history_minutes);
    add_parameter!(min_samples_for_trend);
    add_parameter!(trend_change_samples);
    add_parameter!(trend_deadband_percentage);
    add_parameter!(min_trend_slope_percent_per_minute);
    add_parameter!(min_trend_r_squared);
    add_parameter!(min_trend_move_volatility_ratio);
    add_parameter!(reversal_cooldown_secs);
    parameters.insert(
        "operating_cost_percentage_bucket_5bps".into(),
        format!(
            "{:.4}",
            bucket_percentage(config.operating_cost_percentage(), 0.05)
        ),
    );
    add_parameter!(tax_percentage);
    add_parameter!(min_profit_multiplier);
    add_parameter!(option_expiry_days);
    add_parameter!(option_target_expiry_days);
    add_parameter!(option_max_expiry_days);
    add_parameter!(max_position_size);
    add_parameter!(position_timeout_mins);
    add_parameter!(max_investment_amount);
    add_parameter!(max_loss_per_trade);
    add_parameter!(max_daily_loss);
    add_parameter!(max_trades_per_day);
    add_parameter!(stop_loss_percentage);
    add_parameter!(contract_multiplier);
    add_parameter!(readonly_slippage_bps);
    add_parameter!(learning_slippage_bps);
    add_parameter!(max_market_data_age_secs);
    add_parameter!(max_option_spread_percentage);
    add_parameter!(min_option_volume);
    add_parameter!(max_option_moneyness_distance_percentage);
    add_parameter!(min_reward_risk_ratio);
    add_parameter!(live_learning_min_trades);
    add_parameter!(live_learning_min_call_trades);
    add_parameter!(live_learning_min_put_trades);
    add_parameter!(live_learning_min_sessions);
    add_parameter!(live_learning_min_profit_factor);
    add_parameter!(live_regression_window_trades);
    add_parameter!(live_max_consecutive_losses);
    add_parameter!(canary_min_trades);
    add_parameter!(canary_min_call_trades);
    add_parameter!(canary_min_put_trades);
    add_parameter!(canary_min_sessions);
    add_parameter!(canary_max_position_size);
    add_parameter!(canary_max_investment_amount);
    add_parameter!(canary_max_loss_per_trade);
    add_parameter!(canary_max_daily_loss);
    add_parameter!(canary_max_trades_per_day);

    let build_hash = strategy_build_hash();
    let encoded = serde_json::to_vec(&(
        EVIDENCE_SCHEMA_VERSION,
        &build_hash,
        &parameters,
        gate_requirements_for_config(config),
    ))
    .expect("strategy manifest is serializable");
    StrategyManifest {
        schema_version: EVIDENCE_SCHEMA_VERSION,
        fingerprint: digest_hex(&encoded),
        build_hash,
        package_version: env!("CARGO_PKG_VERSION").into(),
        parameters,
    }
}

fn strategy_build_hash() -> String {
    let mut context = ring::digest::Context::new(&ring::digest::SHA256);
    for source in [
        include_str!("app.rs"),
        include_str!("learning.rs"),
        include_str!("market.rs"),
        include_str!("pattern.rs"),
        include_str!("risk.rs"),
        include_str!("trading.rs"),
    ] {
        context.update(source.as_bytes());
    }
    context
        .finish()
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn digest_hex(bytes: &[u8]) -> String {
    ring::digest::digest(&ring::digest::SHA256, bytes)
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn argentina_day(timestamp_secs: i64) -> i64 {
    timestamp_secs
        .saturating_sub(3 * 60 * 60)
        .div_euclid(86_400)
}

fn bucket_percentage(value: f64, bucket_size: f64) -> f64 {
    if !value.is_finite() || bucket_size <= 0.0 {
        value
    } else {
        (value / bucket_size).round() * bucket_size
    }
}

fn load_evidence_bundle(path: &Path) -> Result<EvidenceBundle, AppError> {
    Ok(serde_json::from_slice(&std::fs::read(path)?)?)
}

fn write_json_atomic<T: serde::Serialize>(path: &Path, value: &T) -> Result<(), AppError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let temporary = path.with_extension("json.tmp");
    let mut file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&temporary)?;
    file.write_all(&serde_json::to_vec_pretty(value)?)?;
    file.sync_all()?;
    std::fs::rename(&temporary, path)?;
    if let Some(parent) = path.parent() {
        std::fs::File::open(parent)?.sync_all()?;
    }
    Ok(())
}

fn gate_requirements_for_config(config: &Config) -> GateRequirements {
    GateRequirements {
        min_trades: config.live_learning_min_trades,
        min_call_trades: config.live_learning_min_call_trades,
        min_put_trades: config.live_learning_min_put_trades,
        min_sessions: config.live_learning_min_sessions,
        min_profit_factor: config.live_learning_min_profit_factor,
        max_daily_drawdown: config.max_daily_loss,
        max_total_drawdown: config.max_daily_loss * 2.0,
    }
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
            JournalEventKind::OrderUnknown { request, .. } => {
                pending.insert(request.operation_id.clone(), true);
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
    learning_state: &mut LearningState,
    trading_performance: &mut Vec<ValidationTrade>,
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
            stage,
            validation_trade,
        } => {
            if portfolio.contains(operation_id) {
                portfolio.close(
                    operation_id,
                    *exit_price,
                    *net_pnl,
                    event.timestamp_secs,
                    *reason,
                );
                risk.record_close_at(event.timestamp_secs, *net_pnl);
            }
            if let Some(trade) = validation_trade {
                if *stage == LiveStage::Learning {
                    learning_state.record(trade.clone());
                } else if !trading_performance.iter().any(|existing| {
                    !trade.context.trade_id.is_empty()
                        && existing.context.trade_id == trade.context.trade_id
                }) {
                    trading_performance.push(trade.clone());
                }
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

fn simple_direction(direction: Direction) -> &'static str {
    match direction {
        Direction::Up => "parece subir",
        Direction::Down => "parece bajar",
        Direction::Neutral => "sin dirección clara",
    }
}

fn simple_option_direction(kind: OptionKind) -> &'static str {
    match kind {
        OptionKind::Call => "posible suba",
        OptionKind::Put => "posible baja",
    }
}

fn simple_exit_reason(reason: ExitReason) -> &'static str {
    match reason {
        ExitReason::ProfitTarget => "se alcanzó la ganancia buscada",
        ExitReason::StopLoss => "se alcanzó el límite de pérdida",
        ExitReason::TrendReversal => "el precio cambió de dirección",
        ExitReason::Timeout => "se cumplió el tiempo máximo",
        ExitReason::RiskLimit => "se alcanzó un límite de seguridad",
        ExitReason::Manual => "venta pedida manualmente",
        ExitReason::Defensive => "venta preventiva por datos dudosos",
    }
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or_default()
}

fn dataset_id(path: &Path) -> Result<String, AppError> {
    let bytes = std::fs::read(path)?;
    Ok(format!("sha256:{}", digest_hex(&bytes)))
}

fn capture_market_frame(config: &Config, frame: &MarketFrame) -> Result<(), AppError> {
    let argentina_day = frame
        .underlying
        .timestamp_secs
        .saturating_sub(3 * 60 * 60)
        .div_euclid(86_400);
    let directory = config.data_dir.join("market").join(&config.ticker);
    std::fs::create_dir_all(&directory)?;
    let path = directory.join(format!("{argentina_day}.jsonl"));
    let mut file = OpenOptions::new().create(true).append(true).open(path)?;
    serde_json::to_writer(&mut file, frame)?;
    file.write_all(b"\n")?;
    file.sync_data()?;
    Ok(())
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
            mode: Mode::Readonly,
            ticker: "GAL".into(),
            check_interval_secs: 60,
            price_history_minutes: 1,
            min_samples_for_trend: 3,
            trend_change_samples: 3,
            trend_deadband_percentage: 0.10,
            min_trend_slope_percent_per_minute: 0.0,
            min_trend_r_squared: 0.0,
            min_trend_move_volatility_ratio: 0.0,
            reversal_cooldown_secs: 300,
            commission_percentage: 0.19,
            vat_percentage: 21.0,
            other_fees_percentage: 0.0,
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
            capture_market_data: false,
            connection_retry_attempts: 3,
            connection_retry_delay_secs: 1,
            max_investment_amount: 100_000.0,
            max_loss_per_trade: 5_000.0,
            max_daily_loss: 10_000.0,
            max_trades_per_day: 20,
            stop_loss_percentage: 15.0,
            contract_multiplier: 1,
            readonly_slippage_bps: 5.0,
            max_market_data_age_secs: 15,
            max_option_spread_percentage: 20.0,
            min_option_volume: 1,
            option_target_expiry_days: 1,
            option_max_expiry_days: 365,
            max_option_moneyness_distance_percentage: 100.0,
            min_reward_risk_ratio: 1.25,
            learning_slippage_bps: 25.0,
            live_learning_min_trades: 200,
            live_learning_min_call_trades: 75,
            live_learning_min_put_trades: 75,
            live_learning_min_sessions: 20,
            live_learning_min_profit_factor: 1.25,
            live_regression_window_trades: 30,
            live_max_consecutive_losses: 3,
            canary_min_trades: 20,
            canary_min_call_trades: 5,
            canary_min_put_trades: 5,
            canary_min_sessions: 5,
            canary_max_position_size: 1,
            canary_max_investment_amount: 10_000.0,
            canary_max_loss_per_trade: 500.0,
            canary_max_daily_loss: 1_000.0,
            canary_max_trades_per_day: 5,
            iol_base_url: "https://example.invalid".into(),
            iol_websocket_url: "wss://example.invalid".into(),
            iol_order_path: None,
            live_authorization_path: None,
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

    #[test]
    fn a_second_instance_for_the_same_mode_is_rejected() {
        let config = replay_config();
        let first = TradingApp::new(config.clone()).unwrap();
        let second = TradingApp::new(config);
        assert!(matches!(second, Err(AppError::Recovery(_))));
        drop(first);
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

    #[test]
    fn exhausted_connection_retries_make_the_engine_clearly_inoperative() {
        let mut app = TradingApp::new(replay_config()).unwrap();
        app.connection_operational = true;
        let error = AppError::Connection("timeout".into());

        app.mark_connection_not_operational(3, &error).unwrap();

        assert!(!app.connection_operational);
        assert!(app.paused);
        assert!(app.risk.state.kill_switch);
        assert_eq!(app.engine.state, TradingState::Halted);
        assert!(app.status.contains("NO OPERATIVO"));
        assert!(app.status.contains("3 reintentos"));
        assert!(app.status.contains("manualmente en IOL"));
    }

    #[tokio::test]
    async fn restart_restores_runtime_and_resumes_replay_cursor() {
        let config = replay_config();
        let mut app = TradingApp::new(config.clone()).unwrap();
        for _ in 0..12 {
            app.step().await.unwrap();
        }
        app.shutdown().await.unwrap();
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
        app.live_stage = LiveStage::Live;
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
    async fn pause_blocks_entries_but_still_closes_existing_risk() {
        let mut app = TradingApp::new(replay_config()).unwrap();
        app.step().await.unwrap();
        let frame = app.current_frame.clone().unwrap();
        let quote = frame.options.first().unwrap().clone();
        app.live_stage = LiveStage::Live;
        app.apply_account_snapshot(
            frame.underlying.timestamp_secs,
            AccountSnapshot {
                positions: vec![AccountPosition {
                    symbol: quote.symbol,
                    quantity: 1,
                    average_price: Some(0.01),
                    kind: Some(PositionKind::from(quote.kind)),
                    is_option: true,
                }],
                pending_orders: Vec::new(),
            },
        )
        .unwrap();
        app.paused = true;

        app.step().await.unwrap();

        assert!(app.engine.position.is_none());
        assert_eq!(app.engine.last_exit_reason, Some(ExitReason::ProfitTarget));
    }

    #[tokio::test]
    async fn reconstructed_position_is_immediately_checked_for_stop_loss() {
        let mut app = TradingApp::new(replay_config()).unwrap();
        app.step().await.unwrap();
        app.live_stage = LiveStage::Live;
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
    async fn wide_spread_blocks_entry_without_engaging_kill_switch() {
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
        assert!(!app.risk.state.kill_switch);
        assert_eq!(app.engine.state, TradingState::Idle);
    }

    #[tokio::test]
    async fn readonly_transitions_both_ways_but_never_becomes_real_trading() {
        let mut config = replay_config();
        config.live_learning_min_trades = 4;
        config.live_learning_min_call_trades = 2;
        config.live_learning_min_put_trades = 2;
        config.live_learning_min_sessions = 1;
        config.live_learning_min_profit_factor = 1.0;
        let mut app = TradingApp::new(config).unwrap();
        app.learning_state.record(ValidationTrade {
            kind: PositionKind::Call,
            net_pnl: 10.0,
            stressed_net_pnl: 5.0,
            closed_at_secs: 4 * 60 * 60,
            context: ValidationContext::default(),
        });
        app.learning_state.record(ValidationTrade {
            kind: PositionKind::Put,
            net_pnl: 10.0,
            stressed_net_pnl: 5.0,
            closed_at_secs: 86_400 + 4 * 60 * 60,
            context: ValidationContext::default(),
        });
        app.learning_state.record(ValidationTrade {
            kind: PositionKind::Call,
            net_pnl: 10.0,
            stressed_net_pnl: 5.0,
            closed_at_secs: 2 * 86_400 + 4 * 60 * 60,
            context: ValidationContext::default(),
        });
        app.learning_state.record(ValidationTrade {
            kind: PositionKind::Put,
            net_pnl: 10.0,
            stressed_net_pnl: 5.0,
            closed_at_secs: 3 * 86_400 + 4 * 60 * 60,
            context: ValidationContext::default(),
        });
        app.learning_state.approved = true;
        app.cost_calibration = Some(CostCalibration {
            operation_number: "option-1".into(),
            operation_amount: 1_000.0,
            commission_percentage: 0.1,
            vat_percentage: 21.0,
            other_fees_percentage: 0.05,
            total_cost_percentage: 0.1815,
            components: Vec::new(),
            observed_at_secs: 1,
            instrument_is_option: true,
        });
        app.current_trend = Some(Trend {
            direction: Direction::Up,
            confirmed: true,
            warmed_up: true,
            samples: 30,
            sma: 100.0,
            slope: 1.0,
            slope_percent_per_minute: 0.1,
            volatility: 1.0,
            r_squared: Some(1.0),
            confidence: 1.0,
        });

        app.maybe_promote_live(2).await.unwrap();
        assert_eq!(app.live_stage, LiveStage::Eligible);
        assert!(!app.is_real_trading());

        app.return_to_learning_pending = true;
        app.apply_pending_learning_return(3).unwrap();
        assert_eq!(app.live_stage, LiveStage::Learning);
    }

    #[test]
    fn replay_never_routes_real_orders_even_when_configured_live() {
        let mut config = replay_config();
        config.mode = Mode::Live;
        config.live_confirmed = true;
        config.iol_order_path = Some("/verified-orders".into());
        config.live_authorization_path = Some(config.data_dir.join("authorization.json"));
        let mut app = TradingApp::new(config).unwrap();
        assert_eq!(app.live_stage, LiveStage::Learning);
        assert!(!app.is_real_trading());
        app.live_stage = LiveStage::Live;
        assert!(!app.is_real_trading());
    }

    #[test]
    fn purchase_quantity_never_exceeds_cash_budget_including_commission() {
        let quantity = affordable_contracts(100_000.0, 20_000.0, 1, 0.19, 10);
        assert_eq!(quantity, 4);
        assert!(purchase_cash_required(20_000.0, quantity, 1, 0.19) <= 100_000.0);
        assert!(purchase_cash_required(20_000.0, quantity + 1, 1, 0.19) > 100_000.0);
    }

    #[test]
    fn environment_suggestion_separates_iol_fees_from_profit_tax() {
        let mut app = TradingApp::new(replay_config()).unwrap();
        app.cost_calibration = Some(CostCalibration {
            operation_number: "123".into(),
            operation_amount: 10_000.0,
            commission_percentage: 0.099970,
            vat_percentage: 20.999978,
            other_fees_percentage: 0.049985,
            total_cost_percentage: 0.181445,
            components: Vec::new(),
            observed_at_secs: unix_now(),
            instrument_is_option: true,
        });

        let suggestion = app.environment_suggestion().unwrap();

        assert!(suggestion.contains("OTHER_FEES_PERCENTAGE=0.049985"));
        assert!(suggestion.contains("Costo operativo efectivo: 0.181445%"));
        assert!(suggestion.contains("TAX_PERCENTAGE=35.000000"));
        assert!(suggestion.contains("no surge de los aranceles IOL"));
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

    #[test]
    fn startup_treats_unknown_order_result_as_pending() {
        let request = OrderRequest {
            operation_id: "unknown-1".into(),
            symbol: "GAL-C-100".into(),
            quantity: 1,
            market_price: 2.0,
            limit_price: 2.1,
            side: OrderSide::Buy,
        };
        let events = vec![JournalEvent {
            sequence: 1,
            timestamp_secs: 1,
            operation_id: Some(request.operation_id.clone()),
            event: JournalEventKind::OrderUnknown {
                request,
                reason: "timeout".into(),
            },
        }];
        assert_eq!(unresolved_local_orders(&events), vec!["unknown-1"]);
    }
}
