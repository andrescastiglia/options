use std::{
    collections::BTreeMap,
    collections::BTreeSet,
    collections::HashMap,
    collections::VecDeque,
    env,
    io::{Read, Seek, Write},
    path::Path,
    path::PathBuf,
    time::Duration,
};

use fs2::FileExt;

use crate::{
    analytics::{
        append_candidates, append_execution, baseline_report, candidate_observations,
        ExecutionObservation, ANALYTICS_SCHEMA_VERSION,
    },
    broker::{
        validate_order_execution, validate_order_transition, AccountFunds, AccountPosition,
        AccountSnapshot, BrokerClient, OrderExecution, OrderRequest, OrderSide, OrderStatus,
        PaperBroker,
    },
    config::{Config, Mode, LIVE_CONFIRMATION},
    errors::AppError,
    experiments::{
        run_temporal_experiment, ExperimentManifest, ExperimentVariant, EXPERIMENT_SCHEMA_VERSION,
    },
    iol_client::{
        AccountMovement, AccountProfile, CostCalibration, IolClient, IolClientError,
        IolRealtimeEvent, OrderTrackingMetrics, WebsocketConnectionState,
    },
    iv_rank::{
        append_observation as append_iv_observation, append_telemetry as append_iv_telemetry,
        compare_iv_filters_walk_forward, load_history as load_iv_history, point_in_time_iv_rank,
        IvObservation, IvRankPolicy, IvRankResult, IvRankTelemetry, IV_HISTORY_SCHEMA_VERSION,
    },
    learning::{
        trading_regressed, AuthorizationRequest, EvidenceBundle, EvidenceSource,
        ExecutionAuthorization, GateRequirements, LearningReport, LearningState, LiveStage,
        StrategyManifest, ValidationContext, ValidationTrade, AUTHORIZATION_SCHEMA_VERSION,
        EVIDENCE_SCHEMA_VERSION,
    },
    learning_model::{MetaFilterPolicy, SignalFeatures},
    market::{
        moneyness_distance_percentage, option_friction_percentage, select_option_with_criteria,
        CapturedMarketFrame, ContractMetadataSource, MarketDataProvider, MarketFrame, OptionKind,
        OptionSelectionCriteria, ReplayMarket, VixFreshnessState, VixObservation,
    },
    market_calendar::{MarketCalendar, MarketCalendarPolicy, MarketScheduleStatus},
    multileg::{append_research as append_multileg_research, research_vertical_spread},
    number_format::{decimal, integer},
    option_analytics::{
        analyze_american_option, intrinsic_value, validate_pricer, PointInTimeMarketInputs,
    },
    pattern::{Direction, PriceSample, Trend, TrendCriteria, TrendDetector},
    persistence::{
        load_snapshot, record_order_accepted, record_order_intent, record_order_terminal,
        save_snapshot, Journal, JournalEvent, JournalEventKind, RuntimeSnapshot, Snapshot,
    },
    portfolio::{Portfolio, PortfolioMetrics},
    release_readiness::{digest_hex as readiness_digest_hex, ReleaseReadiness},
    risk::{RiskLimits, RiskManager},
    secrets::{uses_live_key_format, verify_authorization_payload_from},
    secure_fs::{
        ensure_private_dir, open_limited_read, open_private_append_bounded,
        open_private_read_write, read_private_limited, reject_symlink, write_atomic,
    },
    storage::{
        daily_jsonl_path, prune_expired_market_captures, require_capacity, StorageCapacity,
        StorageLimits,
    },
    time_reference::{ClockObservation, ClockReferenceClient},
    trading::{
        build_position_economics, calculate_pnl_with_contract_multiplier, calculate_position_pnl,
        EntryContext, ExitReason, IvRankMissingReason, Pnl, Position, PositionKind, TradingEngine,
    },
    vix::VixClient,
};

enum MarketSource {
    Replay(ReplayMarket),
    Iol(Box<IolClient>),
}

const STORAGE_STEP_RESERVE_BYTES: u64 = 8 * 1024 * 1024;

#[derive(Debug, Clone, Copy, serde::Serialize)]
struct StoragePressureObservation {
    observed_at_secs: i64,
    used_bytes: u64,
    max_total_bytes: u64,
    available_bytes: u64,
    min_free_bytes: u64,
    safe: bool,
}

struct InstanceLease {
    file: std::fs::File,
}

impl InstanceLease {
    fn acquire(path: PathBuf) -> Result<Self, AppError> {
        let mut file = open_private_read_write(&path)?;
        file.try_lock_exclusive().map_err(|error| {
            AppError::Recovery(format!(
                "ya existe una instancia activa para esta ruta de estado o no se pudo tomar el lock: {error}"
            ))
        })?;
        file.set_len(0)?;
        file.rewind()?;
        writeln!(file, "{}", std::process::id())?;
        file.sync_all()?;
        Ok(Self { file })
    }
}

impl Drop for InstanceLease {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.file);
    }
}

pub(crate) struct LogEntry {
    pub timestamp_secs: i64,
    pub message: String,
    pub repetitions: u64,
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct LunchQuoteState {
    bid: Option<f64>,
    ask: Option<f64>,
    volume: u64,
}

#[derive(Debug)]
struct LunchQuoteActivity {
    first_seen_secs: i64,
    last_seen_secs: i64,
    state: LunchQuoteState,
    updates: VecDeque<i64>,
}

#[derive(Debug, Default)]
struct LunchLiquidityMonitor {
    by_symbol: HashMap<String, LunchQuoteActivity>,
}

impl LunchLiquidityMonitor {
    fn observe(&mut self, frame: &MarketFrame, window_secs: i64) {
        let timestamp = frame.underlying.timestamp_secs;
        let cutoff = timestamp.saturating_sub(window_secs);
        for quote in &frame.options {
            let state = LunchQuoteState {
                bid: quote.bid,
                ask: quote.ask,
                volume: quote.volume,
            };
            let activity = self
                .by_symbol
                .entry(quote.symbol.clone())
                .or_insert_with(|| LunchQuoteActivity {
                    first_seen_secs: timestamp,
                    last_seen_secs: timestamp,
                    state,
                    updates: VecDeque::new(),
                });
            if timestamp.saturating_sub(activity.last_seen_secs) > window_secs {
                activity.first_seen_secs = timestamp;
                activity.state = state;
                activity.updates.clear();
            } else if activity.state != state {
                activity.state = state;
                activity.updates.push_back(timestamp);
            }
            activity.last_seen_secs = timestamp;
            while activity
                .updates
                .front()
                .is_some_and(|observed| *observed < cutoff)
            {
                activity.updates.pop_front();
            }
        }
        self.by_symbol
            .retain(|_, activity| activity.last_seen_secs >= cutoff);
    }

    fn sufficient_updates(
        &self,
        symbol: &str,
        now_secs: i64,
        window_secs: i64,
        minimum_updates: usize,
    ) -> bool {
        self.update_count(symbol, now_secs, window_secs)
            .is_some_and(|updates| updates >= minimum_updates)
    }

    fn update_count(&self, symbol: &str, now_secs: i64, window_secs: i64) -> Option<usize> {
        self.by_symbol.get(symbol).and_then(|activity| {
            (now_secs.saturating_sub(activity.first_seen_secs) >= window_secs).then(|| {
                activity
                    .updates
                    .iter()
                    .filter(|observed| **observed >= now_secs.saturating_sub(window_secs))
                    .count()
            })
        })
    }
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
    pub websocket_status: WebsocketConnectionState,
    pub connection_operational: bool,
    pub market_open: bool,
    pub market_entries_allowed: bool,
    pub market_force_pre_break_exit: bool,
    pub market_expiry_exit_due: bool,
    pub market_next_session_days: u32,
    pub lunch_slowdown: bool,
    pub lunch_reconfirming: bool,
    pub market_status: String,
    pub market_status_detail: String,
    pub last_movement: Option<AccountMovement>,
    pub live_stage: LiveStage,
    pub learning_state: LearningState,
    pub trading_performance: Vec<ValidationTrade>,
    pub storage_capacity: StorageCapacity,
    pub clock_synchronized: bool,
    pub clock_observation: Option<ClockObservation>,
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
    authorized_readiness_sha256: Option<String>,
    cooldown_until_secs: i64,
    learning_report_path: PathBuf,
    evidence_bundle_path: PathBuf,
    strategy_manifest: StrategyManifest,
    authorization_request_path: PathBuf,
    real_account_clear: bool,
    account_funds: Option<AccountFunds>,
    last_account_reconciliation_secs: i64,
    dataset_ids: Vec<String>,
    vix_client: Option<VixClient>,
    clock_reference_client: Option<ClockReferenceClient>,
    vix_missing_logged: bool,
    market_calendar: MarketCalendar,
    last_market_open: Option<bool>,
    last_lunch_slowdown: bool,
    lunch_liquidity: LunchLiquidityMonitor,
    lunch_liquidity_missing_logged: bool,
    iv_history: Vec<IvObservation>,
    _storage_lease: InstanceLease,
    _instance_lease: InstanceLease,
}

impl TradingApp {
    pub fn new(config: Config) -> Result<Self, AppError> {
        Self::new_with_source(config, None)
    }

    #[cfg(test)]
    pub(crate) fn new_for_test(config: Config) -> Result<Self, AppError> {
        let ticker = config.ticker.clone();
        Self::new_with_source(
            config,
            Some(MarketSource::Replay(ReplayMarket::synthetic(&ticker))),
        )
    }

    fn new_with_source(
        mut config: Config,
        source_override: Option<MarketSource>,
    ) -> Result<Self, AppError> {
        ensure_private_dir(&config.data_dir)?;
        // Serializa mantenimiento, cuota y escrituras compartidas entre readonly/live.
        // Una sola instancia puede usar un DATA_DIR dado; use raíces distintas para
        // procesos independientes.
        let storage_lease = InstanceLease::acquire(config.data_dir.join("storage.lock"))?;
        let removed_captures = prune_expired_market_captures(
            &config.data_dir,
            unix_now(),
            config.market_capture_retention_days,
        )?;
        let storage_capacity = require_capacity(
            &config.data_dir,
            storage_limits(&config),
            STORAGE_STEP_RESERVE_BYTES,
        )?;
        let replay_path = config.replay_path.clone();
        let mut source = if let Some(path) = &replay_path {
            MarketSource::Replay(ReplayMarket::from_jsonl(path)?)
        } else if let Some(source) = source_override {
            source
        } else {
            let username = env::var("IOL_USERNAME")
                .map_err(|_| AppError::External("IOL_USERNAME ausente".into()))?;
            let encrypted_password = env::var("IOL_PASSWORD")
                .map_err(|_| AppError::External("IOL_PASSWORD ausente".into()))?;
            if config.mode == Mode::Live && !uses_live_key_format(&encrypted_password) {
                return Err(AppError::External(
                    "live exige IOL_PASSWORD v3 derivado de OPTIONS_MASTER_KEY_PATH; vuelva a cifrarlo con --encrypt-password"
                        .into(),
                ));
            }
            let refresh_token = env::var("IOL_REFRESH_TOKEN").unwrap_or_default();
            let client = IolClient::new(
                &config.iol_base_url,
                username,
                encrypted_password,
                refresh_token,
            )
            .map_err(|error| AppError::External(error.to_string()))?
            .with_catalog_cache_ttl(config.cache_ttl_secs)
            .with_catalog_archive_dir(config.data_dir.join("catalog"))
            .with_max_concurrent_requests(config.max_concurrent_requests)
            .with_websocket_url(&config.iol_websocket_url)
            .with_websocket_enabled(config.iol_websocket_enabled);
            MarketSource::Iol(Box::new(client))
        };
        let dataset_ids = replay_path
            .as_ref()
            .map(|path| dataset_id(path))
            .transpose()?
            .into_iter()
            .collect();
        let vix_client = config
            .vix_quote_url
            .as_deref()
            .map(|url| {
                VixClient::new(
                    url,
                    config.vix_refresh_secs,
                    config.vix_max_age_secs,
                    config.vix_previous_close_max_age_secs,
                    env::var("VIX_BEARER_TOKEN").ok().as_deref(),
                )
                .map_err(|error| AppError::External(error.to_string()))
            })
            .transpose()?;
        let clock_reference_client = config
            .time_reference_url
            .as_deref()
            .map(|url| {
                ClockReferenceClient::new(
                    url,
                    config.time_reference_refresh_secs,
                    config.time_reference_max_skew_secs,
                )
                .map_err(|error| AppError::External(error.to_string()))
            })
            .transpose()?;
        let market_calendar = MarketCalendar::new(
            &config.holidays_api_base_url,
            config.data_dir.join("calendar"),
            MarketCalendarPolicy {
                entry_delay_after_open_mins: config.entry_delay_after_open_mins,
                weekend_risk_enabled: config.weekend_risk_enabled,
                pre_break_last_entry_minute: config.pre_break_last_entry_minute,
                pre_break_force_exit_minute: config.pre_break_force_exit_minute,
                expiry_day_force_exit_minute: config.expiry_day_force_exit_minute,
                lunch_slowdown_enabled: config.lunch_slowdown_enabled,
                lunch_slowdown_start_minute: config.lunch_slowdown_start_minute,
                lunch_slowdown_end_minute: config.lunch_slowdown_end_minute,
                post_lunch_confirmation_mins: config.post_lunch_confirmation_mins,
                lunch_position_factor: config.lunch_position_factor,
            },
        )
        .map_err(|error| AppError::External(error.to_string()))?
        .with_exchange_calendar(
            config.market_sessions_path.as_deref(),
            config.mode == Mode::Live,
        )
        .map_err(AppError::External)?;

        let mode_name = format!("{:?}", config.mode).to_ascii_lowercase();
        let instance_lease =
            InstanceLease::acquire(config.data_dir.join(&mode_name).join("instance.lock"))?;
        let journal_path = config.data_dir.join(&mode_name).join("journal.jsonl");
        let snapshot_path = config.data_dir.join(&mode_name).join("state.json");
        let mut journal = if config.mode == Mode::Live {
            let master_key_path = config.master_key_path.as_deref().ok_or_else(|| {
                AppError::Recovery(
                    "falta OPTIONS_MASTER_KEY_PATH; debe apuntar a una clave privada creada con --init-master-key"
                        .into(),
                )
            })?;
            Journal::open_authenticated_with_master_key(&journal_path, master_key_path)?
        } else {
            Journal::open(&journal_path)?
        };
        let mut engine = TradingEngine::new();
        let mut portfolio = Portfolio::default();
        let mut risk = RiskManager::new(risk_limits_for_stage(&config, LiveStage::Learning));
        let mut recovery_message = None;
        let mut detector = trend_detector_for_config(&config);
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
            risk.limits = risk_limits_for_stage(&config, live_stage);
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

        let local_pending_orders = unresolved_local_orders(&journal.events_after(0)?)?;

        let started_at = unix_now();
        journal.append(
            started_at,
            None,
            JournalEventKind::Started {
                mode: mode_name,
                ticker: config.ticker.clone(),
            },
        )?;

        let replay_source = matches!(source, MarketSource::Replay(_));
        if config.option_analytics_enabled {
            let report = validate_pricer(config.option_binomial_steps);
            write_json_atomic(
                &config.data_dir.join("analytics/pricer-validation.json"),
                &report,
            )?;
            if report.failures > 0 {
                return Err(AppError::InvalidMarketData(format!(
                    "el pricer americano falló {} casos de referencia",
                    report.failures
                )));
            }
        }
        let iv_history = load_iv_history(&config.data_dir.join("analytics/iv_history.jsonl"))?;
        let initial_websocket_status = if replay_source || !config.iol_websocket_enabled {
            WebsocketConnectionState::Disabled
        } else {
            WebsocketConnectionState::Connecting
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
            websocket_status: initial_websocket_status,
            connection_operational: matches!(source, MarketSource::Replay(_)),
            market_open: replay_source,
            market_entries_allowed: replay_source,
            market_force_pre_break_exit: false,
            market_expiry_exit_due: false,
            market_next_session_days: 0,
            lunch_slowdown: false,
            lunch_reconfirming: false,
            market_status: if replay_source {
                "ONLINE · REPLAY".into()
            } else {
                "OFFLINE · VERIFICANDO HORARIO".into()
            },
            market_status_detail: if replay_source {
                "El replay no depende del horario real".into()
            } else {
                "Consultando calendario de la rueda argentina".into()
            },
            last_movement: None,
            live_stage,
            learning_state,
            trading_performance,
            storage_capacity,
            clock_synchronized: replay_source || clock_reference_client.is_none(),
            clock_observation: None,
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
            authorized_readiness_sha256: None,
            cooldown_until_secs,
            learning_report_path,
            evidence_bundle_path,
            strategy_manifest,
            authorization_request_path,
            real_account_clear: false,
            account_funds: None,
            last_account_reconciliation_secs: 0,
            dataset_ids,
            vix_client,
            clock_reference_client,
            vix_missing_logged: false,
            market_calendar,
            last_market_open: replay_source.then_some(true),
            last_lunch_slowdown: false,
            lunch_liquidity: LunchLiquidityMonitor::default(),
            lunch_liquidity_missing_logged: false,
            iv_history,
            _storage_lease: storage_lease,
            _instance_lease: instance_lease,
        };
        if let Some(message) = recovery_message {
            app.push_log(message);
        }
        if !removed_captures.is_empty() {
            app.push_log(format!(
                "Retención: se eliminaron {} captures de mercado vencidos",
                integer(removed_captures.len())
            ));
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
        match require_capacity(
            &self.config.data_dir,
            storage_limits(&self.config),
            STORAGE_STEP_RESERVE_BYTES,
        ) {
            Ok(capacity) => self.storage_capacity = capacity,
            Err(error) => {
                let detail = error.to_string();
                self.status = "OFFLINE · ALMACENAMIENTO SIN MARGEN SEGURO".into();
                self.market_status = "OFFLINE · ALMACENAMIENTO".into();
                self.market_status_detail = detail.clone();
                self.engine.halt();
                self.risk.engage_operational_halt(&detail);
                self.push_log(format!(
                    "Procesamiento detenido para no perder estado durable: {detail}"
                ));
                return Ok(true);
            }
        }
        write_json_atomic(
            &self.config.data_dir.join("telemetry/storage-pressure.json"),
            &StoragePressureObservation {
                observed_at_secs: unix_now(),
                used_bytes: self.storage_capacity.used_bytes,
                max_total_bytes: self.config.data_dir_max_bytes,
                available_bytes: self.storage_capacity.available_bytes,
                min_free_bytes: self.config.data_disk_min_free_bytes,
                safe: true,
            },
        )?;
        self.refresh_clock_reference().await;
        self.initialize_iol_context().await?;
        self.sync_realtime_events();
        if !self.refresh_market_schedule().await {
            return Ok(true);
        }
        let mut frame = match self.next_frame().await? {
            Some(frame) => frame,
            None => {
                self.completed = true;
                self.status = "Prueba terminada".into();
                self.push_log("Se terminaron los precios de prueba".into());
                return Ok(false);
            }
        };
        frame.validate(self.last_market_timestamp)?;
        self.refresh_replay_market_risk(frame.underlying.timestamp_secs);
        self.enrich_vix(&mut frame).await?;
        if self.config.capture_market_data && !matches!(self.source, MarketSource::Replay(_)) {
            capture_market_frame(&self.config, &frame)?;
        }
        self.lunch_liquidity.observe(
            &frame,
            i64::from(self.config.lunch_liquidity_window_mins) * 60,
        );
        self.last_market_timestamp = Some(frame.underlying.timestamp_secs);
        self.ticks = self.ticks.saturating_add(1);
        let timestamp = frame.underlying.timestamp_secs;
        if let Some(quality) = frame.option_chain_quality.as_ref().filter(|quality| {
            quality.missing_quote_contracts > 0 || quality.invalid_quote_contracts > 0
        }) {
            self.push_log(format!(
                "Calidad de cadena IOL: {}/{} contratos válidos; {} sin cotización y {} inválidos",
                quality.accepted_contracts,
                quality.catalog_contracts,
                quality.missing_quote_contracts,
                quality.invalid_quote_contracts
            ));
        }
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
        let readiness_still_authorized = self.config.mode != Mode::Live
            || !matches!(self.live_stage, LiveStage::Canary | LiveStage::Live)
            || self.release_readiness_is_still_authorized(timestamp);
        if self.config.mode == Mode::Live
            && matches!(self.live_stage, LiveStage::Canary | LiveStage::Live)
            && !self.return_to_learning_pending
            && (!self.config.live_ordering_ready() || !readiness_still_authorized)
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
            self.status = if self.market_entries_allowed && !self.lunch_slowdown {
                format!(
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
                )
            } else {
                self.market_status_detail.clone()
            };
        }

        if self.engine.position.is_some() {
            self.evaluate_exit(timestamp).await?;
        }
        self.apply_pending_learning_return(timestamp)?;
        self.maybe_promote_live(timestamp).await?;
        if !self.paused
            && self.engine.position.is_none()
            && trend.confirmed
            && self.market_entries_allowed
            && (self.config.mode != Mode::Live || self.clock_synchronized)
            && !self.risk.state.kill_switch
            && timestamp >= self.cooldown_until_secs
            && self.last_traded_signal != Some(trend.direction)
        {
            self.evaluate_entry(timestamp, trend.direction).await?;
        }
        self.snapshot()?;
        Ok(true)
    }

    async fn refresh_clock_reference(&mut self) {
        let Some(client) = &mut self.clock_reference_client else {
            self.clock_synchronized = true;
            return;
        };
        let was_synchronized = self.clock_synchronized;
        match client.verify(unix_now()).await {
            Ok(observation) => {
                self.clock_synchronized = true;
                self.clock_observation = Some(observation);
                if !was_synchronized {
                    self.push_log(
                        "Reloj local verificado contra una fuente independiente; se habilitan entradas"
                            .into(),
                    );
                }
            }
            Err(error) => {
                self.clock_synchronized = false;
                self.clock_observation = None;
                if was_synchronized {
                    self.push_log(format!(
                        "Entradas bloqueadas: no se pudo verificar el reloj local ({error})"
                    ));
                }
            }
        }
    }

    async fn enrich_vix(&mut self, frame: &mut MarketFrame) -> Result<(), AppError> {
        let timestamp = frame.underlying.timestamp_secs;
        if matches!(self.source, MarketSource::Replay(_)) {
            if let Some(vix) = frame.vix {
                if vix.freshness_state(
                    timestamp,
                    self.config.vix_max_age_secs,
                    self.config.vix_previous_close_max_age_secs,
                ) == VixFreshnessState::Stale
                {
                    return Err(AppError::InvalidMarketData("VIX obsoleto en replay".into()));
                }
                if vix
                    .validated_change_percentage(
                        timestamp,
                        self.config.vix_previous_close_max_age_secs,
                    )
                    .is_some()
                {
                    self.vix_missing_logged = false;
                } else {
                    self.log_missing_vix_during_learning();
                }
            } else {
                self.log_missing_vix_during_learning();
            }
            return Ok(());
        }

        let learning = self.is_learning();
        if let Some(client) = &mut self.vix_client {
            match client.observation(timestamp).await {
                Ok(observation) => {
                    frame.vix = Some(observation);
                    if observation
                        .validated_change_percentage(
                            timestamp,
                            self.config.vix_previous_close_max_age_secs,
                        )
                        .is_some()
                    {
                        self.vix_missing_logged = false;
                    } else {
                        self.log_missing_vix_during_learning();
                    }
                    return Ok(());
                }
                Err(error) => {
                    if !self.vix_missing_logged {
                        let consequence = if learning {
                            "Learning continúa con el meta-filtro base"
                        } else {
                            "una política VIX activa bloqueará nuevas entradas"
                        };
                        self.push_log(format!("VIX no disponible ({error}); {consequence}"));
                        self.vix_missing_logged = true;
                    }
                }
            }
        }
        frame.vix = None;
        self.log_missing_vix_during_learning();
        Ok(())
    }

    async fn refresh_market_schedule(&mut self) -> bool {
        if matches!(self.source, MarketSource::Replay(_)) {
            return true;
        }
        let schedule = self.market_calendar.status(unix_now()).await;
        self.apply_market_schedule(schedule)
    }

    fn refresh_replay_market_risk(&mut self, timestamp_secs: i64) {
        let MarketSource::Replay(replay) = &self.source else {
            return;
        };
        let next_session_days = replay.next_session_days_after(timestamp_secs);
        let schedule = self
            .market_calendar
            .replay_risk_status(timestamp_secs, next_session_days);
        self.apply_market_schedule(schedule);
    }

    fn apply_market_schedule(&mut self, schedule: MarketScheduleStatus) -> bool {
        let changed = self.last_market_open != Some(schedule.open)
            || self.market_status_detail != schedule.detail;
        let opening_transition = schedule.open && self.last_market_open != Some(true);
        let lunch_ended = self.last_lunch_slowdown && !schedule.lunch_slowdown;
        self.market_open = schedule.open;
        self.market_entries_allowed = schedule.entries_allowed;
        self.market_force_pre_break_exit = schedule.force_pre_break_exit;
        self.market_expiry_exit_due = schedule.expiry_exit_due;
        self.market_next_session_days = schedule.next_session_days;
        self.lunch_slowdown = schedule.lunch_slowdown;
        self.lunch_reconfirming = schedule.lunch_reconfirming;
        self.market_status = schedule.headline;
        self.market_status_detail = schedule.detail;
        self.last_market_open = Some(schedule.open);
        self.last_lunch_slowdown = schedule.lunch_slowdown;
        if opening_transition {
            self.detector = trend_detector_for_config(&self.config);
            self.current_trend = None;
            self.last_traded_signal = None;
            self.push_log(format!(
                "{} · {}; se reinició el calentamiento de tendencia",
                self.market_status, self.market_status_detail
            ));
        } else if lunch_ended {
            self.detector.reset_confirmation();
            self.last_traded_signal = None;
            self.push_log(format!(
                "Terminó la liquidez reducida de mediodía; {}",
                self.market_status_detail
            ));
        } else if changed {
            self.push_log(format!(
                "{} · {}",
                self.market_status, self.market_status_detail
            ));
        }
        if !schedule.open {
            self.current_frame = None;
            self.current_trend = None;
            self.current_pnl = None;
            self.selected_option = None;
            self.status = format!("{} · {}", self.market_status, self.market_status_detail);
        } else if !schedule.entries_allowed || schedule.lunch_slowdown {
            self.status = self.market_status_detail.clone();
        }
        if !schedule.lunch_slowdown {
            self.lunch_liquidity_missing_logged = false;
        }
        schedule.open
    }

    fn log_missing_vix_during_learning(&mut self) {
        if self.is_learning() && !self.vix_missing_logged {
            self.push_log(
                "Learning continúa sin VIX: la operación se evaluará sólo con el meta-filtro base"
                    .into(),
            );
            self.vix_missing_logged = true;
        }
    }

    /// Completa el preflight de IOL antes de que la interfaz tome control de la terminal.
    pub async fn connect(&mut self) -> Result<(), AppError> {
        self.initialize_iol_context().await
    }

    pub fn mark_connection_retry(&mut self, attempt: u32, total: u32, error: &AppError) {
        self.status = "Esperando que IOL vuelva a estar disponible".into();
        self.push_log(format!(
            "Reconectando con IOL: intento {attempt} de {total} después de: {error}"
        ));
    }

    pub fn mark_connection_restored(&mut self) {
        self.connection_operational = true;
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
        if let Some(quality) = self
            .current_frame
            .as_ref()
            .and_then(|frame| frame.option_chain_quality.as_ref())
            .filter(|quality| {
                !quality.allows_entry_for_tenor(
                    self.config.option_expiry_days,
                    self.config.option_max_expiry_days,
                    self.config.min_option_chain_acceptance_percentage,
                    self.config.min_option_chain_contracts_per_side,
                )
            })
        {
            let (catalog, accepted, calls, puts) = quality
                .tenor_totals(
                    self.config.option_expiry_days,
                    self.config.option_max_expiry_days,
                )
                .unwrap_or((0, 0, 0, 0));
            let acceptance = if catalog == 0 {
                0.0
            } else {
                accepted as f64 / catalog as f64 * 100.0
            };
            self.status = "ENTRADA BLOQUEADA · cadena de opciones incompleta".into();
            self.push_log(format!(
                "Entrada bloqueada por calidad de cadena en tenor {}–{} días: {}% válidas ({}/{}), CALL {}, PUT {}; mínimos {}% y {} por lado",
                self.config.option_expiry_days,
                self.config.option_max_expiry_days,
                decimal(acceptance, 2),
                accepted,
                catalog,
                calls,
                puts,
                decimal(self.config.min_option_chain_acceptance_percentage, 2),
                self.config.min_option_chain_contracts_per_side
            ));
            return Ok(());
        }
        if self.requires_real_order_route() {
            self.refresh_live_account(timestamp).await?;
            if !self.real_account_clear {
                return Ok(());
            }
        }
        if !self.engine.consider_entry(direction) {
            return Ok(());
        }
        let Some(option_kind) = option_kind_for_direction(direction) else {
            return Ok(());
        };
        let quality_now = if matches!(self.source, MarketSource::Replay(_)) {
            timestamp
        } else {
            unix_now()
        };
        let mut max_spread = self
            .config
            .max_option_spread_percentage
            .min(self.config.stop_loss_percentage / 2.0);
        if self.lunch_slowdown {
            max_spread *= self.config.lunch_max_spread_factor;
        }
        if self.config.volatility_normalized_signals_enabled {
            if let (Some(trend), Some(frame)) = (&self.current_trend, &self.current_frame) {
                let observed = trend.volatility / frame.underlying.last.max(f64::EPSILON) * 100.0;
                let regime_factor = (observed
                    / self.config.target_underlying_volatility_percentage)
                    .sqrt()
                    .clamp(0.75, 1.50);
                max_spread *= regime_factor;
            }
        }
        let selection_criteria = OptionSelectionCriteria {
            min_expiry_days: self.config.option_expiry_days.max(
                if self.config.weekend_risk_enabled {
                    self.market_next_session_days
                } else {
                    0
                },
            ),
            target_expiry_days: self.config.option_target_expiry_days,
            max_expiry_days: self.config.option_max_expiry_days,
            min_volume: self.config.min_option_volume,
            max_spread_percentage: max_spread,
            max_moneyness_distance_percentage: self.config.max_option_moneyness_distance_percentage,
            now_secs: quality_now,
            max_age_secs: self.config.max_market_data_age_secs,
            operating_cost_percentage: self.config.operating_cost_percentage(),
            slippage_bps: self.execution_slippage_bps(),
        };
        let option = self
            .current_frame
            .as_ref()
            .and_then(|frame| select_option_with_criteria(frame, option_kind, selection_criteria))
            .cloned();
        if let Some(frame) = &self.current_frame {
            let observations = candidate_observations(
                frame,
                option_kind,
                direction,
                selection_criteria,
                option.as_ref().map(|option| option.symbol.as_str()),
            );
            append_candidates(
                &daily_jsonl_path(
                    &self.config.data_dir,
                    "telemetry",
                    "candidates",
                    quality_now,
                ),
                &observations,
            )?;
        }
        let Some(option) = option else {
            self.engine.resume();
            self.push_log(format!(
                "No encontré una opción con buenos precios para una {}",
                simple_option_direction(option_kind)
            ));
            return Ok(());
        };
        let underlying_price = self
            .current_frame
            .as_ref()
            .map_or(option.strike, |frame| frame.underlying.last);
        let premium = option
            .bid
            .zip(option.ask)
            .map_or(option.last, |(bid, ask)| (bid + ask) / 2.0);
        let intrinsic = intrinsic_value(option.kind, underlying_price, option.strike);
        let extrinsic = (premium - intrinsic).max(0.0);
        let option_analytics = self
            .config
            .option_analytics_enabled
            .then(|| {
                let time_years = option
                    .expiration_timestamp_secs
                    .map(|expiry| expiry.saturating_sub(quality_now).max(1) as f64 / 31_536_000.0)
                    .unwrap_or_else(|| option.expiry_days.max(1) as f64 / 365.0);
                let parameters = PointInTimeMarketInputs {
                    observed_at_secs: self.config.option_market_inputs_observed_at_secs?,
                    valid_for_secs: self.config.option_market_inputs_max_age_secs,
                    risk_free_rate: self.config.option_risk_free_rate,
                    risk_free_source: self.config.option_risk_free_source.clone(),
                    dividend_yield: self.config.option_dividend_yield,
                    dividend_source: self.config.option_dividend_source.clone(),
                }
                .parameters_at(quality_now, self.config.option_binomial_steps)?;
                Some(analyze_american_option(
                    option.kind,
                    underlying_price,
                    option.strike,
                    premium,
                    time_years,
                    parameters,
                ))
            })
            .flatten();
        if self.config.option_analytics_enabled && option_analytics.is_none() {
            self.engine.resume();
            self.push_log(format!(
                "Analítica de opciones rechazó {}: tasa o dividendos no son válidos para este instante",
                option.symbol
            ));
            return Ok(());
        }
        let mut iv_rank_result = IvRankResult {
            rank: None,
            window_sessions: 0,
            observations: 0,
            first_observed_at_secs: None,
            last_observed_at_secs: None,
            missing_reason: Some(if self.config.option_analytics_enabled {
                "iv_no_disponible".into()
            } else {
                "analitica_opciones_desactivada".into()
            }),
        };
        if let Some(iv) = option_analytics.and_then(|analytics| analytics.implied_volatility) {
            let observation = IvObservation {
                schema_version: IV_HISTORY_SCHEMA_VERSION,
                underlying: option.underlying.clone(),
                kind: option.kind,
                tenor_days: comparable_tenor(option.expiry_days),
                observed_at_secs: quality_now,
                implied_volatility: iv,
            };
            let policy = IvRankPolicy {
                window_sessions: self.config.iv_rank_window_sessions,
                min_sessions: self.config.iv_rank_min_sessions,
                min_rank: self.config.iv_rank_min,
                max_rank: self.config.iv_rank_max,
            };
            iv_rank_result = point_in_time_iv_rank(&self.iv_history, &observation, policy);
            append_iv_telemetry(
                &daily_jsonl_path(&self.config.data_dir, "telemetry", "iv_rank", quality_now),
                &IvRankTelemetry {
                    schema_version: IV_HISTORY_SCHEMA_VERSION,
                    evaluated_at_secs: quality_now,
                    underlying: observation.underlying.clone(),
                    kind: observation.kind,
                    tenor_days: observation.tenor_days,
                    current_iv: iv,
                    filter_enabled: self.config.iv_rank_filter_enabled,
                    configured_min: policy.min_rank,
                    configured_max: policy.max_rank,
                    result: iv_rank_result.clone(),
                },
            )?;
            if !self.iv_history.iter().any(|item| {
                item.observed_at_secs == observation.observed_at_secs
                    && item.underlying == observation.underlying
                    && item.kind == observation.kind
                    && item.tenor_days == observation.tenor_days
            }) {
                append_iv_observation(
                    &self.config.data_dir.join("analytics/iv_history.jsonl"),
                    &observation,
                )?;
                self.iv_history.push(observation);
            }
            if self.config.iv_rank_filter_enabled && !iv_rank_result.allows(policy) {
                self.engine.resume();
                self.push_log(format!(
                    "IV Rank rechazó {}: {}",
                    option.symbol,
                    iv_rank_result
                        .missing_reason
                        .clone()
                        .unwrap_or_else(|| format!(
                            "percentil fuera de {:.1}-{:.1}",
                            policy.min_rank, policy.max_rank
                        ))
                ));
                return Ok(());
            }
        } else if self.config.iv_rank_filter_enabled {
            self.engine.resume();
            self.push_log(format!(
                "IV Rank rechazó {}: no hay IV punto-en-tiempo válida",
                option.symbol
            ));
            return Ok(());
        }
        if let Some(analytics) = option_analytics {
            let extrinsic_percentage =
                analytics.extrinsic_value / premium.max(f64::EPSILON) * 100.0;
            let valid = analytics.implied_volatility.is_some_and(|iv| {
                iv >= self.config.option_min_implied_volatility
                    && iv <= self.config.option_max_implied_volatility
            }) && analytics.delta.is_some_and(|delta| {
                delta.abs() >= self.config.option_min_abs_delta
                    && delta.abs() <= self.config.option_max_abs_delta
            }) && extrinsic_percentage <= self.config.option_max_extrinsic_percentage;
            if !valid {
                self.engine.resume();
                self.push_log(format!(
                    "Analítica de opciones rechazó {}: IV o delta fuera de los límites validados",
                    option.symbol
                ));
                return Ok(());
            }
        }
        if self.config.adaptive_entry_filter_enabled {
            let friction = option_friction_percentage(
                &option,
                self.config.operating_cost_percentage(),
                self.execution_slippage_bps(),
            )
            .unwrap_or(f64::INFINITY);
            let maximum = self.config.stop_loss_percentage * self.config.max_friction_stop_ratio;
            if friction > maximum {
                self.engine.resume();
                self.push_log(format!(
                    "Filtro costo/riesgo rechazó {}: fricción {:.2}% supera {:.2}%",
                    option.symbol, friction, maximum
                ));
                return Ok(());
            }
        }
        if self.config.vertical_spread_research_enabled {
            if let Some(spread) = self.current_frame.as_ref().and_then(|frame| {
                research_vertical_spread(
                    frame,
                    option.kind,
                    &option.symbol,
                    self.config.contract_multiplier,
                    self.config.operating_cost_percentage(),
                )
            }) {
                append_multileg_research(
                    &daily_jsonl_path(
                        &self.config.data_dir,
                        "telemetry",
                        "vertical-spreads",
                        timestamp,
                    ),
                    timestamp,
                    &spread,
                )?;
            }
        }
        if self.lunch_slowdown
            && !self.lunch_liquidity.sufficient_updates(
                &option.symbol,
                timestamp,
                i64::from(self.config.lunch_liquidity_window_mins) * 60,
                self.config.lunch_min_quote_updates,
            )
        {
            self.engine.resume();
            if !self.lunch_liquidity_missing_logged {
                self.push_log(format!(
                    "Liquidez de mediodía insuficiente para {}: se requieren {} actualizaciones en {} minutos",
                    option.symbol,
                    self.config.lunch_min_quote_updates,
                    self.config.lunch_liquidity_window_mins
                ));
                self.lunch_liquidity_missing_logged = true;
            }
            return Ok(());
        }
        self.lunch_liquidity_missing_logged = false;
        let lunch_quote_updates = self
            .lunch_slowdown
            .then(|| {
                self.lunch_liquidity.update_count(
                    &option.symbol,
                    timestamp,
                    i64::from(self.config.lunch_liquidity_window_mins) * 60,
                )
            })
            .flatten()
            .map(|updates| updates as u64);
        let mut vix_policy_active = false;
        if self.live_stage != LiveStage::Learning {
            let assessment = self
                .learning_state
                .report(self.gate_requirements())
                .meta_filter;
            if assessment.tree_recommended {
                if let (Some(model), Some(trend), Some(frame), Some(spread), Some(r_squared)) = (
                    assessment.tree_model.as_ref(),
                    self.current_trend.as_ref(),
                    self.current_frame.as_ref(),
                    option.spread_percentage(),
                    self.current_trend
                        .as_ref()
                        .and_then(|trend| trend.r_squared),
                ) {
                    let underlying = frame.underlying.last;
                    let features = SignalFeatures::from([
                        spread,
                        option.volume as f64,
                        option.expiry_days as f64,
                        ((option.strike - underlying).abs() / underlying) * 100.0,
                        trend.confidence,
                        r_squared,
                        trend.slope_percent_per_minute.abs(),
                    ]);
                    let mut effective_threshold = model.threshold;
                    if self.lunch_slowdown {
                        effective_threshold = lunch_adjusted_threshold(
                            effective_threshold,
                            self.config.lunch_signal_threshold_bonus,
                        );
                    }
                    if self.config.volatility_normalized_signals_enabled {
                        let observed = trend.volatility / underlying.max(f64::EPSILON) * 100.0;
                        effective_threshold = volatility_adjusted_threshold(
                            effective_threshold,
                            observed,
                            self.config.target_underlying_volatility_percentage,
                        );
                    }
                    let probability = model.probability(&features).unwrap_or_default();
                    if probability < effective_threshold {
                        self.engine.resume();
                        self.last_traded_signal = Some(direction);
                        self.push_log(format!(
                            "Meta-filtro tree rechazó {}: probabilidad {:.1}% bajo umbral {:.1}%",
                            option.symbol,
                            probability * 100.0,
                            effective_threshold * 100.0
                        ));
                        return Ok(());
                    }
                }
            } else if assessment.recommended {
                vix_policy_active = assessment.uses_vix;
                if let (Some(model), Some(trend), Some(frame), Some(spread), Some(r_squared)) = (
                    assessment.model.as_ref(),
                    self.current_trend.as_ref(),
                    self.current_frame.as_ref(),
                    option.spread_percentage(),
                    self.current_trend
                        .as_ref()
                        .and_then(|trend| trend.r_squared),
                ) {
                    let underlying = frame.underlying.last;
                    let mut features = SignalFeatures::from([
                        spread,
                        option.volume as f64,
                        option.expiry_days as f64,
                        ((option.strike - underlying).abs() / underlying) * 100.0,
                        trend.confidence,
                        r_squared,
                        trend.slope_percent_per_minute.abs(),
                    ]);
                    let mut effective_threshold = model.threshold;
                    if assessment.uses_vix {
                        let Some((level, change)) = frame.vix.and_then(|vix| {
                            (vix.freshness_state(
                                quality_now,
                                self.config.vix_max_age_secs,
                                self.config.vix_previous_close_max_age_secs,
                            ) == VixFreshnessState::Current)
                                .then(|| {
                                    vix.validated_change_percentage(
                                        quality_now,
                                        self.config.vix_previous_close_max_age_secs,
                                    )
                                    .map(|change| (vix.level, change))
                                })
                                .flatten()
                        }) else {
                            self.engine.resume();
                            self.last_traded_signal = Some(direction);
                            self.push_log(format!(
                                "Meta-filtro VIX activo rechazó {}: falta una observación VIX completa",
                                option.symbol
                            ));
                            return Ok(());
                        };
                        features = features.with_vix(level, change);
                        effective_threshold = vix_adjusted_threshold(
                            effective_threshold,
                            change,
                            self.config.vix_spike_change_percentage,
                            self.config.vix_spike_threshold_bonus,
                        );
                    }
                    if self.lunch_slowdown {
                        effective_threshold = lunch_adjusted_threshold(
                            effective_threshold,
                            self.config.lunch_signal_threshold_bonus,
                        );
                    }
                    if self.config.volatility_normalized_signals_enabled {
                        let observed = trend.volatility / underlying.max(f64::EPSILON) * 100.0;
                        effective_threshold = volatility_adjusted_threshold(
                            effective_threshold,
                            observed,
                            self.config.target_underlying_volatility_percentage,
                        );
                    }
                    let probability = model.probability(&features).unwrap_or_default();
                    if probability < effective_threshold {
                        self.engine.resume();
                        self.last_traded_signal = Some(direction);
                        self.push_log(format!(
                            "Meta-filtro rechazó {}: probabilidad {:.1}% bajo umbral {:.1}%",
                            option.symbol,
                            probability * 100.0,
                            effective_threshold * 100.0
                        ));
                        return Ok(());
                    }
                }
            }
        }
        self.selected_option = Some(option.symbol.clone());
        let real_order = self.requires_real_order_route();
        let contract_multiplier = match entry_contract_multiplier(
            option.catalog_contract_multiplier,
            option.catalog_observed_at_secs,
            option.contract_metadata_source,
            self.config.contract_multiplier,
            real_order,
            unix_now(),
            self.config.cache_ttl_secs,
        )
        .filter(|_| {
            !real_order
                || catalog_integrity_is_verified(
                    option.catalog_schema_version,
                    option.catalog_sha256,
                    option.catalog_archived,
                )
        }) {
            Some(multiplier) => multiplier,
            None => {
                self.engine.resume();
                self.status =
                    "ÓRDENES BLOQUEADAS · metadata contractual del instrumento ausente".into();
                self.push_log(format!(
                    "No se envió la entrada de {}: el catálogo IOL no informó su multiplicador contractual",
                    option.symbol
                ));
                return Ok(());
            }
        };
        let market_price = option
            .executable_buy_price()
            .ok_or_else(|| AppError::InvalidMarketData("ask de opcion ausente".into()))?;
        let limit_price = market_price * 1.005;
        let (mut max_investment_amount, max_loss_per_trade, max_position_size) =
            self.execution_risk_limits(vix_policy_active);
        if self.requires_real_order_route() {
            let Some(funds) = self.account_funds.as_ref() else {
                self.engine.resume();
                self.status = "ÓRDENES BLOQUEADAS · saldo operable no verificado".into();
                self.push_log(
                    "No se envió la entrada: falta un estado de cuenta IOL vigente".into(),
                );
                return Ok(());
            };
            max_investment_amount = effective_investment_budget(max_investment_amount, funds)
                .map_err(AppError::InvalidMarketData)?;
        }
        let cash_quantity = affordable_contracts(
            max_investment_amount,
            limit_price,
            contract_multiplier,
            self.config.operating_cost_percentage(),
            max_position_size,
        );
        let risk_per_contract = build_position_economics(
            limit_price,
            1,
            contract_multiplier,
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
            contract_multiplier,
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
            self.handle_unfilled_order(timestamp, request.quantity, &execution)?;
            return Ok(());
        }
        let fill_price = execution.fill_price.unwrap_or(market_price);
        let economics = build_position_economics(
            fill_price,
            execution.filled_quantity,
            contract_multiplier,
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
            contract_multiplier,
            opened_at_secs: timestamp,
            economics: Some(economics),
            entry_context: Some(EntryContext {
                spread_percentage: option.spread_percentage(),
                option_volume: option.volume,
                days_to_expiry: option.expiry_days,
                contract_metadata_observed_at_secs: option.catalog_observed_at_secs,
                contract_metadata_source: option.contract_metadata_source,
                contract_metadata_catalog_schema_version: option.catalog_schema_version,
                contract_metadata_catalog_sha256: option.catalog_sha256,
                contract_metadata_catalog_archived: option.catalog_archived,
                moneyness_distance_percentage: moneyness_distance_percentage(
                    option.strike,
                    self.current_frame
                        .as_ref()
                        .map_or(option.strike, |frame| frame.underlying.last),
                )
                .unwrap_or(0.0),
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
                vix_level: self
                    .current_frame
                    .as_ref()
                    .and_then(|frame| frame.vix.map(|vix| vix.level)),
                vix_change_percentage: self.current_frame.as_ref().and_then(|frame| {
                    frame.vix.and_then(|vix| {
                        vix.validated_change_percentage(
                            frame.underlying.timestamp_secs,
                            self.config.vix_previous_close_max_age_secs,
                        )
                    })
                }),
                lunch_slowdown: self.lunch_slowdown,
                lunch_quote_updates,
                intrinsic_value: Some(intrinsic),
                extrinsic_value: Some(extrinsic),
                implied_volatility: option_analytics
                    .and_then(|analytics| analytics.implied_volatility),
                iv_rank: iv_rank_result.rank,
                iv_rank_window_sessions: Some(iv_rank_result.window_sessions),
                iv_rank_observations: Some(iv_rank_result.observations),
                iv_rank_missing_reason: iv_rank_result.missing_reason.as_ref().map(|reason| {
                    if reason.starts_with("historial_insuficiente") {
                        IvRankMissingReason::InsufficientHistory
                    } else if self.config.option_analytics_enabled {
                        IvRankMissingReason::ImpliedVolatilityUnavailable
                    } else {
                        IvRankMissingReason::AnalyticsDisabled
                    }
                }),
                delta: option_analytics.and_then(|analytics| analytics.delta),
                gamma: option_analytics.and_then(|analytics| analytics.gamma),
                theta_per_day: option_analytics.and_then(|analytics| analytics.theta_per_day),
                vega_per_point: option_analytics.and_then(|analytics| analytics.vega_per_point),
                rho_per_point: option_analytics.and_then(|analytics| analytics.rho_per_point),
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
            Err(IolClientError::InvalidResponse(reason)) => {
                self.startup_reconciled = true;
                self.block_reconciliation(
                    timestamp,
                    format!("IOL informó un estado de cuenta inconsistente: {reason}"),
                )
            }
            Err(error) => Err(AppError::Connection(format!(
                "no se pudo consultar estado de cuenta, cartera u órdenes IOL: {error}"
            ))),
        }
    }

    fn apply_account_snapshot(
        &mut self,
        timestamp: i64,
        account: AccountSnapshot,
    ) -> Result<(), AppError> {
        if self.config.mode == Mode::Live {
            let Some(funds) = account.funds.as_ref() else {
                return self.block_reconciliation(
                    timestamp,
                    "IOL no informó el saldo operable de la cuenta en pesos".into(),
                );
            };
            if let Err(reason) = validate_account_funds(funds) {
                return self.block_reconciliation(timestamp, reason);
            }
            self.account_funds = Some(funds.clone());
        } else {
            self.account_funds = account.funds.clone();
        }
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
                .map(|order| {
                    format!(
                        "{}:{}",
                        masked_identifier(&order.broker_order_id),
                        order.symbol
                    )
                })
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
                if local.option_symbol != remote.symbol
                    || local.contracts != remote.quantity
                    || remote.kind.is_some_and(|kind| kind != local.kind)
                {
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
            .and_then(|frame| frame.option(&remote.symbol))
            .cloned();
        if let (Some(remote_kind), Some(quote)) = (remote.kind, quote.as_ref()) {
            if remote_kind != PositionKind::from(quote.kind) {
                return self.block_reconciliation(
                    timestamp,
                    format!(
                        "IOL informa tipo {:?} para {}, pero el catálogo vigente la clasifica como {:?}",
                        remote_kind,
                        remote.symbol,
                        PositionKind::from(quote.kind)
                    ),
                );
            }
        }
        let kind = remote
            .kind
            .or_else(|| quote.as_ref().map(|quote| PositionKind::from(quote.kind)));
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
        let quote = quote.expect("se comprobó la cotización arriba");
        let verified_catalog_multiplier = entry_contract_multiplier(
            quote.catalog_contract_multiplier,
            quote.catalog_observed_at_secs,
            quote.contract_metadata_source,
            self.config.contract_multiplier,
            true,
            unix_now(),
            self.config.cache_ttl_secs,
        )
        .filter(|_| {
            catalog_integrity_is_verified(
                quote.catalog_schema_version,
                quote.catalog_sha256,
                quote.catalog_archived,
            )
        });
        let contract_multiplier =
            verified_catalog_multiplier.unwrap_or_else(|| self.config.contract_multiplier.max(1));
        let entry_context = verified_catalog_multiplier.map(|_| EntryContext {
            spread_percentage: quote.spread_percentage(),
            option_volume: quote.volume,
            days_to_expiry: quote.expiry_days,
            contract_metadata_observed_at_secs: quote.catalog_observed_at_secs,
            contract_metadata_source: quote.contract_metadata_source,
            contract_metadata_catalog_schema_version: quote.catalog_schema_version,
            contract_metadata_catalog_sha256: quote.catalog_sha256,
            contract_metadata_catalog_archived: quote.catalog_archived,
            moneyness_distance_percentage: self.current_frame.as_ref().map_or(0.0, |frame| {
                moneyness_distance_percentage(quote.strike, frame.underlying.last).unwrap_or(0.0)
            }),
            trend_confidence: 0.0,
            trend_r_squared: None,
            trend_slope_percent_per_minute: 0.0,
            vix_level: None,
            vix_change_percentage: None,
            lunch_slowdown: self.lunch_slowdown,
            lunch_quote_updates: None,
            intrinsic_value: None,
            extrinsic_value: None,
            implied_volatility: None,
            iv_rank: None,
            iv_rank_window_sessions: None,
            iv_rank_observations: None,
            iv_rank_missing_reason: None,
            delta: None,
            gamma: None,
            theta_per_day: None,
            vega_per_point: None,
            rho_per_point: None,
        });
        let position = Position {
            operation_id: format!(
                "recovered-{}-{timestamp}",
                remote.symbol.to_ascii_lowercase()
            ),
            option_symbol: remote.symbol,
            kind,
            entry_price,
            contracts: remote.quantity,
            contract_multiplier,
            opened_at_secs: timestamp,
            economics: build_position_economics(
                entry_price,
                remote.quantity,
                contract_multiplier,
                self.config.operating_cost_percentage(),
                self.config.tax_percentage,
                self.execution_slippage_bps(),
                self.config.stop_loss_percentage,
                self.config.max_loss_per_trade,
                self.config.min_profit_multiplier,
                self.config.min_reward_risk_ratio,
            ),
            entry_context,
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
        if self.config.mode == Mode::Live && verified_catalog_multiplier.is_none() {
            self.risk.engage_operational_halt(
                "la posición recuperada no tiene metadata contractual IOL verificable",
            );
            self.push_log(
                "Posición real recuperada sin metadata contractual verificable; nuevas entradas bloqueadas y cualquier salida exige conciliación segura"
                    .into(),
            );
        }
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
            let expiry_close_required = self.market_expiry_exit_due
                && position
                    .entry_context
                    .is_some_and(|context| context.days_to_expiry == 0);
            if self.market_force_pre_break_exit || expiry_close_required {
                let obligation = if self.market_force_pre_break_exit {
                    "antes de la pausa"
                } else {
                    "antes del límite de vencimiento"
                };
                self.halt_for_market_risk(
                    timestamp,
                    format!(
                        "no se puede cerrar {} {obligation}: la serie no aparece en el mercado",
                        position.option_symbol,
                    ),
                )?;
            }
            return Ok(());
        };
        let forced_reason = if self.market_force_pre_break_exit {
            Some(ExitReason::WeekendRisk)
        } else if self.market_expiry_exit_due && option.expiry_days == 0 {
            Some(ExitReason::ExpiryRisk)
        } else {
            None
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
        let market_price = match option.executable_sell_price() {
            Some(price) => price,
            None if forced_reason.is_some() => {
                self.halt_for_market_risk(
                    timestamp,
                    format!(
                        "no hay bid ejecutable para el cierre obligatorio de {}",
                        position.option_symbol
                    ),
                )?;
                return Ok(());
            }
            None => {
                return Err(AppError::InvalidMarketData("bid de opcion ausente".into()));
            }
        };
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
        let reason = forced_reason.or_else(|| {
            self.engine.should_exit(
                market_price,
                pnl,
                opposite,
                timestamp,
                (self.config.position_timeout_mins * 60) as i64,
                self.risk.limits.max_loss_per_trade,
                self.config.stop_loss_percentage,
            )
        });
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
            self.handle_unfilled_order(timestamp, request.quantity, &execution)?;
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
        let real_route = self.requires_real_order_route();
        if real_route && !self.contract_multiplier_is_verified_for(request.side) {
            self.reconciliation_blocked = true;
            self.real_account_clear = false;
            self.risk
                .engage_operational_halt("multiplicador contractual sin verificar");
            self.engine.halt();
            return Err(AppError::OrderRejected(
                "la operación requiere IOL real, pero el multiplicador contractual no está verificado"
                .into(),
            ));
        }
        if real_route
            && request.side == OrderSide::Buy
            && !self.live_entry_authorization_is_valid(timestamp)
        {
            self.return_to_learning_pending = true;
            self.apply_pending_learning_return(timestamp)?;
            return Err(AppError::OrderRejected(
                "la compra real perdió su autorización o readiness antes del envío".into(),
            ));
        }
        // Write-ahead efectivo: si este sync falla, el POST no se ejecuta.
        record_order_intent(&mut self.journal, timestamp, request, real_route)?;
        let started = std::time::Instant::now();
        let websocket_state = format!("{:?}", self.websocket_status).to_ascii_lowercase();
        let (execution, price_attempts, acceptance_millis, tracking_millis, tracking_metrics) =
            if real_route {
                let order_path = self.config.iol_order_path.as_deref().ok_or_else(|| {
                    AppError::External("IOL_ORDER_PATH ausente en modo live".into())
                })?;
                let MarketSource::Iol(client) = &mut self.source else {
                    return Err(AppError::External("cliente IOL no inicializado".into()));
                };
                let submit_started = std::time::Instant::now();
                match client.submit_order(order_path, request).await {
                    Ok(initial) => {
                        let acceptance_millis = submit_started.elapsed().as_millis();
                        record_order_accepted(
                            &mut self.journal,
                            timestamp,
                            &request.operation_id,
                            &initial,
                        )?;
                        let tracking_started = std::time::Instant::now();
                        match client
                            .track_order_to_terminal(
                                request,
                                initial,
                                Duration::from_secs(self.config.order_tracking_timeout_secs),
                                Duration::from_millis(
                                    self.config.order_status_poll_interval_millis,
                                ),
                                Duration::from_secs(self.config.order_cancel_timeout_secs),
                            )
                            .await
                        {
                            Ok(execution) => (
                                execution,
                                1,
                                acceptance_millis,
                                tracking_started.elapsed().as_millis(),
                                client.order_tracking_metrics(),
                            ),
                            Err(error) => {
                                let reason = error.to_string();
                                self.record_unknown_order(timestamp, request, &reason)?;
                                return Err(AppError::External(format!(
                                    "orden enviada con resultado desconocido: {reason}"
                                )));
                            }
                        }
                    }
                    Err(error) => {
                        let reason = error.to_string();
                        self.record_unknown_order(timestamp, request, &reason)?;
                        return Err(AppError::External(format!(
                            "orden enviada con resultado desconocido: {reason}"
                        )));
                    }
                }
            } else {
                // La ruta operativa paper sigue siendo un límite fijo. El estudio
                // dinámico requiere una secuencia de frames futuros y se ejecuta
                // mediante `simulate_dynamic_limit`; nunca inventa reintentos con
                (
                    self.paper_broker.submit_limit(request.clone())?,
                    1,
                    0,
                    0,
                    OrderTrackingMetrics::default(),
                )
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
        record_order_terminal(
            &mut self.journal,
            timestamp,
            &request.operation_id,
            &execution,
            real_route,
        )?;
        append_execution(
            &daily_jsonl_path(&self.config.data_dir, "telemetry", "executions", timestamp),
            &ExecutionObservation {
                schema_version: ANALYTICS_SCHEMA_VERSION,
                submitted_at_secs: timestamp,
                operation_id: request.operation_id.clone(),
                broker_order_id: execution.broker_order_id.clone(),
                symbol: request.symbol.clone(),
                side: request.side,
                requested_quantity: request.quantity,
                filled_quantity: execution.filled_quantity,
                remaining_quantity: execution.remaining_quantity(request.quantity),
                limit_price: request.limit_price,
                fill_price: execution.fill_price,
                final_status: execution.status,
                elapsed_millis: started.elapsed().as_millis(),
                acceptance_millis,
                tracking_millis,
                rest_polls: tracking_metrics.rest_polls,
                websocket_signals: tracking_metrics.websocket_signals,
                route: if real_route {
                    "iol_rest_ws"
                } else if self.config.dynamic_limit_enabled {
                    "paper_dynamic"
                } else {
                    "paper_limit"
                }
                .into(),
                websocket_state_at_submit: websocket_state,
                price_attempts,
                cancellation_observed: execution.status == OrderStatus::Cancelled,
                cancellation_requested: tracking_metrics.cancellation_requested,
            },
        )?;
        Ok(execution)
    }

    fn handle_unfilled_order(
        &mut self,
        timestamp: i64,
        requested_quantity: u32,
        execution: &OrderExecution,
    ) -> Result<(), AppError> {
        self.push_log(format!(
            "orden {} no ejecutada: {:?}",
            masked_identifier(&execution.operation_id),
            execution.status
        ));
        if execution.filled_quantity > 0 {
            self.journal.append(
                timestamp,
                Some(execution.operation_id.clone()),
                JournalEventKind::PartialFillExposure {
                    execution: execution.clone(),
                    requested_quantity,
                    remaining_quantity: execution.remaining_quantity(requested_quantity),
                },
            )?;
            self.push_log(format!(
                "Exposición parcial: {} ejecutados, {} pendientes/cancelados",
                execution.filled_quantity,
                execution.remaining_quantity(requested_quantity)
            ));
        }
        if self.is_real_trading()
            && (execution.filled_quantity > 0
                || matches!(
                    execution.status,
                    OrderStatus::Pending | OrderStatus::PartiallyExecuted
                ))
        {
            if !self.local_pending_orders.contains(&execution.operation_id) {
                self.local_pending_orders
                    .push(execution.operation_id.clone());
            }
            self.reconciliation_blocked = true;
            self.real_account_clear = false;
            self.risk
                .engage_operational_halt("orden real pendiente o parcialmente ejecutada");
            self.engine.halt();
            self.status =
                "Una orden con dinero real quedó sin confirmar; revisarla manualmente en IOL"
                    .into();
        } else {
            self.engine.resume();
        }
        Ok(())
    }

    fn record_unknown_order(
        &mut self,
        timestamp: i64,
        request: &OrderRequest,
        reason: &str,
    ) -> Result<(), AppError> {
        self.journal.append(
            timestamp,
            Some(request.operation_id.clone()),
            JournalEventKind::OrderUnknown {
                request: request.clone(),
                reason: reason.to_string(),
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
        Ok(())
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

    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn current_fresh_vix(&self) -> Option<VixObservation> {
        let frame = self.current_frame.as_ref()?;
        let vix = frame.vix?;
        let reference_timestamp = if matches!(self.source, MarketSource::Replay(_)) {
            frame.underlying.timestamp_secs
        } else {
            unix_now()
        };
        (vix.freshness_state(
            reference_timestamp,
            self.config.vix_max_age_secs,
            self.config.vix_previous_close_max_age_secs,
        ) == VixFreshnessState::Current)
            .then_some(vix)
    }

    pub(crate) fn current_vix_display(&self) -> Option<(VixObservation, VixFreshnessState)> {
        let frame = self.current_frame.as_ref()?;
        let vix = frame.vix?;
        let reference_timestamp = if matches!(self.source, MarketSource::Replay(_)) {
            frame.underlying.timestamp_secs
        } else {
            unix_now()
        };
        Some((
            vix,
            vix.freshness_state(
                reference_timestamp,
                self.config.vix_max_age_secs,
                self.config.vix_previous_close_max_age_secs,
            ),
        ))
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
            masked_identifier(&calibration.operation_number),
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
        if let Some(profile) = context.profile {
            self.push_log(format!(
                "cuenta IOL {} · {}",
                profile.masked_account_number(),
                profile.redacted_name()
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
                masked_identifier(&calibration.operation_number)
            ));
            return;
        }
        if let Some(observed) = calibration.observed_contract_multiplier {
            if observed != self.config.contract_multiplier {
                self.reconciliation_blocked = true;
                self.return_to_learning_pending = self.live_stage != LiveStage::Learning;
                self.push_log(format!(
                    "SEGURIDAD: IOL implica multiplicador contractual {observed}, pero CONTRACT_MULTIPLIER={}; se bloquean órdenes reales hasta corregirlo",
                    self.config.contract_multiplier
                ));
            } else {
                self.push_log(format!(
                    "Multiplicador contractual {} verificado con la operación {} de IOL",
                    observed,
                    masked_identifier(&calibration.operation_number)
                ));
            }
        } else if !self.config.contract_multiplier_confirmed {
            self.push_log(
                "IOL no informó cantidad y precio suficientes para verificar el multiplicador contractual; las órdenes reales permanecen bloqueadas".into(),
            );
        }
        let previous_fingerprint = strategy_fingerprint(&self.config);
        self.config.commission_percentage = calibration.commission_percentage;
        self.config.vat_percentage = calibration.vat_percentage;
        self.config.other_fees_percentage = calibration.other_fees_percentage;
        let calibrated_fingerprint = strategy_fingerprint(&self.config);
        if calibrated_fingerprint != previous_fingerprint {
            if self.live_stage != LiveStage::Learning {
                self.return_to_learning_pending = true;
                self.push_log(
                    "La calibración de costos cambió; se volverá a Learning al quedar plano".into(),
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
            masked_identifier(&calibration.operation_number),
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
                && calibration.observed_at_secs <= now_secs.saturating_add(300)
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
                IolRealtimeEvent::Status { state, detail } => {
                    self.websocket_status = state;
                    self.push_log(detail);
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
                IolRealtimeEvent::Notice(detail) => self.push_log(detail),
            }
        }
    }

    fn is_real_trading(&self) -> bool {
        let side = if self.engine.position.is_some() {
            OrderSide::Sell
        } else {
            OrderSide::Buy
        };
        self.requires_real_order_route() && self.contract_multiplier_is_verified_for(side)
    }

    fn requires_real_order_route(&self) -> bool {
        self.config.mode == Mode::Live
            && matches!(self.live_stage, LiveStage::Canary | LiveStage::Live)
            && matches!(self.source, MarketSource::Iol(_))
    }

    fn contract_multiplier_is_verified_for(&self, side: OrderSide) -> bool {
        let fresh_catalog_multiplier = self.selected_option.as_deref().and_then(|symbol| {
            self.current_frame
                .as_ref()
                .and_then(|frame| frame.options.iter().find(|option| option.symbol == symbol))
                .and_then(|option| {
                    entry_contract_multiplier(
                        option.catalog_contract_multiplier,
                        option.catalog_observed_at_secs,
                        option.contract_metadata_source,
                        self.config.contract_multiplier,
                        true,
                        unix_now(),
                        self.config.cache_ttl_secs,
                    )
                    .filter(|_| {
                        catalog_integrity_is_verified(
                            option.catalog_schema_version,
                            option.catalog_sha256,
                            option.catalog_archived,
                        )
                    })
                })
        });

        if side == OrderSide::Buy {
            return fresh_catalog_multiplier.is_some();
        }

        let frozen_position_metadata = self.engine.position.as_ref().is_some_and(|position| {
            position.contract_multiplier > 0
                && position.entry_context.as_ref().is_some_and(|context| {
                    context.contract_metadata_source == ContractMetadataSource::IolCatalog
                        && context.contract_metadata_observed_at_secs.is_some()
                        && catalog_integrity_is_verified(
                            context.contract_metadata_catalog_schema_version,
                            context.contract_metadata_catalog_sha256,
                            context.contract_metadata_catalog_archived,
                        )
                })
        });
        frozen_position_metadata
            || fresh_catalog_multiplier.is_some_and(|multiplier| {
                self.engine
                    .position
                    .as_ref()
                    .is_none_or(|position| position.contract_multiplier == multiplier)
            })
            || self.config.contract_multiplier_confirmed
            || self.cost_calibration.as_ref().is_some_and(|calibration| {
                calibration.observed_contract_multiplier == Some(self.config.contract_multiplier)
            })
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

    fn execution_risk_limits(&self, vix_policy_active: bool) -> (f64, f64, u32) {
        let limits = if self.live_stage == LiveStage::Canary {
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
        };
        let elevated = vix_policy_active
            && self
                .current_frame
                .as_ref()
                .and_then(|frame| frame.vix)
                .is_some_and(|vix| vix.level >= self.config.vix_elevated_level);
        let mut factor = 1.0;
        if elevated {
            factor *= self.config.vix_elevated_position_factor;
        }
        if self.lunch_slowdown {
            factor *= self.config.lunch_position_factor;
        }
        if factor >= 1.0 {
            return limits;
        }
        (
            limits.0 * factor,
            limits.1 * factor,
            ((limits.2 as f64 * factor).floor() as u32).max(1),
        )
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
                    EvidenceSource::ResearchReplay
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
                vix_level: position.entry_context.and_then(|context| context.vix_level),
                vix_change_percentage: position
                    .entry_context
                    .and_then(|context| context.vix_change_percentage),
                lunch_slowdown: position
                    .entry_context
                    .is_some_and(|context| context.lunch_slowdown),
                lunch_quote_updates: position
                    .entry_context
                    .and_then(|context| context.lunch_quote_updates),
                intrinsic_value: position
                    .entry_context
                    .and_then(|context| context.intrinsic_value),
                extrinsic_value: position
                    .entry_context
                    .and_then(|context| context.extrinsic_value),
                implied_volatility: position
                    .entry_context
                    .and_then(|context| context.implied_volatility),
                iv_rank: position.entry_context.and_then(|context| context.iv_rank),
                iv_rank_window_sessions: position
                    .entry_context
                    .and_then(|context| context.iv_rank_window_sessions),
                iv_rank_observations: position
                    .entry_context
                    .and_then(|context| context.iv_rank_observations),
                iv_rank_missing_reason: position
                    .entry_context
                    .and_then(|context| context.iv_rank_missing_reason)
                    .map(|reason| format!("{reason:?}").to_ascii_lowercase()),
                delta: position.entry_context.and_then(|context| context.delta),
                gamma: position.entry_context.and_then(|context| context.gamma),
                theta_per_day: position
                    .entry_context
                    .and_then(|context| context.theta_per_day),
                vega_per_point: position
                    .entry_context
                    .and_then(|context| context.vega_per_point),
                rho_per_point: position
                    .entry_context
                    .and_then(|context| context.rho_per_point),
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
                self.risk.state.realized_pnl + trade_net_pnl <= -self.risk.limits.max_daily_loss;
            if daily_loss_after_close
                || trading_regressed(
                    &self.trading_performance,
                    self.config.live_regression_window_trades,
                    self.config.live_max_consecutive_losses,
                    self.risk.limits.max_daily_loss * 2.0,
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
        write_json_atomic(
            &self
                .learning_report_path
                .with_file_name("baseline-report.json"),
            &baseline_report(&self.learning_state.trades, report.generated_at_secs),
        )?;
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
        write_json_atomic(
            &self
                .learning_report_path
                .with_file_name("iv-filter-comparison.json"),
            &compare_iv_filters_walk_forward(
                &self.learning_state.trades,
                (
                    self.config.option_min_implied_volatility,
                    self.config.option_max_implied_volatility,
                ),
                (self.config.iv_rank_min, self.config.iv_rank_max),
                self.config.meta_filter_min_train_examples,
            ),
        )?;
        if self.config.experiment_runner_enabled && self.learning_state.trades.len() >= 20 {
            let mut timestamps = self
                .learning_state
                .trades
                .iter()
                .map(|trade| trade.context.opened_at_secs)
                .collect::<Vec<_>>();
            timestamps.sort_unstable();
            let split = timestamps.len() * 4 / 5;
            let selection_end = timestamps[split.saturating_sub(1)];
            let holdout_start = timestamps[split];
            let result = run_temporal_experiment(
                &self.learning_state.trades,
                ExperimentManifest {
                    schema_version: EXPERIMENT_SCHEMA_VERSION,
                    dataset_ids: self.dataset_ids.clone(),
                    build_hash: self.strategy_manifest.build_hash.clone(),
                    seed: 0x5eed_2026,
                    variants: vec![
                        ExperimentVariant {
                            name: "base".into(),
                            entry_delay_minutes: self.config.entry_delay_after_open_mins,
                            extra_cost_bps: 0.0,
                            max_risk_multiple: 10.0,
                            volatility_normalized: false,
                        },
                        ExperimentVariant {
                            name: "apertura_45m".into(),
                            entry_delay_minutes: 45,
                            extra_cost_bps: 10.0,
                            max_risk_multiple: 2.0,
                            volatility_normalized: false,
                        },
                        ExperimentVariant {
                            name: "volatilidad_normalizada".into(),
                            entry_delay_minutes: self.config.entry_delay_after_open_mins,
                            extra_cost_bps: 25.0,
                            max_risk_multiple: 2.0,
                            volatility_normalized: true,
                        },
                    ],
                    selection_start_secs: None,
                    selection_end_secs: selection_end,
                    final_holdout_start_secs: holdout_start,
                    final_holdout_end_secs: None,
                },
            );
            write_json_atomic(
                &self
                    .learning_report_path
                    .with_file_name("experiment-report.json"),
                &result,
            )?;
        }
        Ok(())
    }

    fn ensure_authorization_request(
        &self,
        timestamp: i64,
        readiness_sha256: &str,
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
            readiness_sha256: readiness_sha256.to_string(),
            report_sha256,
            canary_max_position_size: self.config.canary_max_position_size,
            canary_max_investment_amount: self.config.canary_max_investment_amount,
            canary_max_loss_per_trade: self.config.canary_max_loss_per_trade,
            canary_max_daily_loss: self.config.canary_max_daily_loss,
            generated_at_secs: timestamp,
        };
        if self.authorization_request_path.exists() {
            let existing: AuthorizationRequest = serde_json::from_slice(&read_private_limited(
                &self.authorization_request_path,
                64 * 1024,
            )?)?;
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
        let bytes = match read_private_limited(path, 64 * 1024) {
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
        let master_key_path = self.config.master_key_path.as_deref().ok_or_else(|| {
            AppError::External("OPTIONS_MASTER_KEY_PATH ausente durante autorización".into())
        })?;
        let signature_valid = !authorization.nonce.trim().is_empty()
            && !authorization.signature.trim().is_empty()
            && verify_authorization_payload_from(
                master_key_path,
                &authorization.signing_payload()?,
                &authorization.signature,
            )
            .map_err(|error| AppError::External(error.to_string()))?;
        let already_consumed = authorization_consumed_path(path, &authorization.nonce).exists();
        Ok(authorization.schema_version == AUTHORIZATION_SCHEMA_VERSION
            && authorization.request == *request
            && authorization.confirmation == LIVE_CONFIRMATION
            && valid_lifetime
            && signature_valid
            && !already_consumed)
    }

    fn release_readiness_digest(&self, timestamp: i64) -> Result<String, AppError> {
        let path = self
            .config
            .live_readiness_path
            .as_deref()
            .ok_or_else(|| AppError::External("LIVE_READINESS_PATH ausente".into()))?;
        let master_key_path = self
            .config
            .master_key_path
            .as_deref()
            .ok_or_else(|| AppError::External("OPTIONS_MASTER_KEY_PATH ausente".into()))?;
        let bytes = read_private_limited(path, 256 * 1024)?;
        let readiness: ReleaseReadiness = serde_json::from_slice(&bytes)?;
        readiness
            .verify_with_master_key(
                master_key_path,
                &self.strategy_manifest.build_hash,
                timestamp,
            )
            .map_err(|error| AppError::External(error.to_string()))?;
        Ok(readiness_digest_hex(&bytes))
    }

    fn release_readiness_is_still_authorized(&self, timestamp: i64) -> bool {
        self.release_readiness_digest(timestamp)
            .ok()
            .is_some_and(|digest| {
                self.authorized_readiness_sha256.as_deref() == Some(digest.as_str())
            })
    }

    fn live_entry_authorization_is_valid(&self, timestamp: i64) -> bool {
        self.config.live_ordering_ready() && self.release_readiness_is_still_authorized(timestamp)
    }

    fn consume_authorization(&self, _timestamp: i64) -> Result<(), AppError> {
        let path = self
            .config
            .live_authorization_path
            .as_deref()
            .ok_or_else(|| AppError::External("LIVE_AUTHORIZATION_PATH ausente".into()))?;
        let authorization: ExecutionAuthorization =
            serde_json::from_slice(&read_private_limited(path, 64 * 1024)?)?;
        consume_authorization_file(path, &authorization.nonce)
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
                let blockers = self.config.live_ordering_blockers();
                if !blockers.is_empty() {
                    let blocker_count = blockers.len();
                    let blocker_detail = blockers.join("; ");
                    let status = format!(
                        "NO OPERATIVO PARA ÓRDENES · {} bloqueo(s) de configuración",
                        blocker_count
                    );
                    if self.status != status {
                        self.push_log(format!("Órdenes reales bloqueadas: {blocker_detail}"));
                        self.status = status;
                    }
                    return Ok(());
                }
                let readiness_sha256 = match self.release_readiness_digest(timestamp) {
                    Ok(digest) => digest,
                    Err(error) => {
                        let status = "NO OPERATIVO PARA ÓRDENES · readiness inválido".to_string();
                        if self.status != status {
                            self.push_log(format!(
                                "Órdenes reales bloqueadas: readiness pre-canary no verificable ({error})"
                            ));
                            self.status = status;
                        }
                        return Ok(());
                    }
                };
                self.refresh_live_account(timestamp).await?;
                if !self.real_account_clear {
                    return Ok(());
                }
                let Some(request) =
                    self.ensure_authorization_request(timestamp, &readiness_sha256)?
                else {
                    return Ok(());
                };
                if !self.authorization_is_valid(&request, timestamp)? {
                    return Ok(());
                }
                self.consume_authorization(timestamp)?;
                self.authorized_readiness_sha256 = Some(readiness_sha256);
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
            || (!self.requires_real_order_route()
                && timestamp.saturating_sub(self.last_account_reconciliation_secs) < 60
                && self.real_account_clear)
        {
            return Ok(());
        }
        let account_result = match &mut self.source {
            MarketSource::Iol(client) => client.account_snapshot().await,
            MarketSource::Replay(_) => return Ok(()),
        };
        let account = match account_result {
            Ok(account) => account,
            Err(IolClientError::InvalidResponse(reason)) => {
                return self.block_reconciliation(
                    timestamp,
                    format!("IOL informó un estado de cuenta inconsistente: {reason}"),
                );
            }
            Err(error) => return Err(AppError::Connection(error.to_string())),
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
        if to == LiveStage::Learning {
            self.authorized_readiness_sha256 = None;
        }
        self.risk.limits = risk_limits_for_stage(&self.config, to);
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
        let message = crate::redaction::sanitize_operational_message(&message);
        tracing::info!(message = %message, "evento operativo");
        record_log(&mut self.logs, message, unix_now());
    }
}

fn record_log(logs: &mut VecDeque<LogEntry>, message: String, timestamp_secs: i64) {
    if let Some(previous) = logs.iter_mut().rev().find(|entry| entry.message == message) {
        previous.repetitions = previous.repetitions.saturating_add(1);
        return;
    }
    if logs.len() == 100 {
        logs.pop_front();
    }
    logs.push_back(LogEntry {
        timestamp_secs,
        message,
        repetitions: 1,
    });
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

fn comparable_tenor(days: u32) -> u32 {
    [7_u32, 14, 21, 30, 45, 60, 90, 180, 365]
        .into_iter()
        .min_by_key(|candidate| candidate.abs_diff(days))
        .unwrap_or(days)
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

fn validate_account_funds(funds: &AccountFunds) -> Result<(), String> {
    if funds.currency != "peso_Argentino" {
        return Err(format!(
            "la cuenta operativa tiene una moneda inesperada: {}",
            funds.currency
        ));
    }
    if funds.status != "operable" {
        return Err(format!("la cuenta IOL no está operable: {}", funds.status));
    }
    if !funds.available.is_finite()
        || funds.available < 0.0
        || !funds.immediate_available_to_trade.is_finite()
        || funds.immediate_available_to_trade < 0.0
    {
        return Err("IOL informó un saldo disponible inválido".into());
    }
    Ok(())
}

fn effective_investment_budget(configured: f64, funds: &AccountFunds) -> Result<f64, String> {
    validate_account_funds(funds)?;
    if !configured.is_finite() || configured <= 0.0 {
        return Err("el presupuesto configurado no es válido".into());
    }
    Ok(configured
        .min(funds.available)
        .min(funds.immediate_available_to_trade))
}

fn entry_contract_multiplier(
    catalog_multiplier: Option<u32>,
    catalog_observed_at_secs: Option<i64>,
    metadata_source: ContractMetadataSource,
    configured_fallback: u32,
    real_order: bool,
    now_secs: i64,
    max_age_secs: u64,
) -> Option<u32> {
    let verified_catalog_multiplier = (|| {
        let observed_at = catalog_observed_at_secs?;
        let age = now_secs.checked_sub(observed_at)?;
        (metadata_source == ContractMetadataSource::IolCatalog
            && age >= -crate::market::MAX_SOURCE_CLOCK_SKEW_SECS
            && age <= max_age_secs as i64)
            .then_some(catalog_multiplier?)
            .filter(|multiplier| *multiplier > 0)
    })();
    match verified_catalog_multiplier {
        Some(multiplier) => Some(multiplier),
        None if real_order => None,
        None => Some(catalog_multiplier.unwrap_or(configured_fallback).max(1)),
    }
}

fn catalog_integrity_is_verified(
    schema_version: u32,
    sha256: Option<[u8; 32]>,
    archived: bool,
) -> bool {
    schema_version == 1
        && archived
        && sha256.is_some_and(|digest| digest.iter().any(|byte| *byte != 0))
}

fn strategy_fingerprint(config: &Config) -> String {
    strategy_manifest(config).fingerprint
}

fn risk_limits_for_stage(config: &Config, stage: LiveStage) -> RiskLimits {
    if stage == LiveStage::Canary {
        RiskLimits {
            max_notional: config.canary_max_investment_amount,
            max_loss_per_trade: config.canary_max_loss_per_trade,
            max_daily_loss: config.canary_max_daily_loss,
            max_trades_per_day: config.canary_max_trades_per_day,
        }
    } else {
        RiskLimits {
            max_notional: config.max_investment_amount,
            max_loss_per_trade: config.max_loss_per_trade,
            max_daily_loss: config.max_daily_loss,
            max_trades_per_day: config.max_trades_per_day,
        }
    }
}

fn trend_detector_for_config(config: &Config) -> TrendDetector {
    TrendDetector::new_robust(
        config.history_capacity(),
        config.min_samples_for_trend,
        TrendCriteria {
            warmup_samples: config.history_capacity(),
            deadband_percentage: config.trend_deadband_percentage,
            min_slope_percent_per_minute: config.min_trend_slope_percent_per_minute,
            min_r_squared: config.min_trend_r_squared,
            min_move_volatility_ratio: config.min_trend_move_volatility_ratio,
        },
    )
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
    add_parameter!(entry_delay_after_open_mins);
    add_parameter!(weekend_risk_enabled);
    add_parameter!(pre_break_last_entry_minute);
    add_parameter!(pre_break_force_exit_minute);
    add_parameter!(expiry_day_force_exit_minute);
    add_parameter!(lunch_slowdown_enabled);
    add_parameter!(lunch_slowdown_start_minute);
    add_parameter!(lunch_slowdown_end_minute);
    add_parameter!(lunch_position_factor);
    add_parameter!(lunch_max_spread_factor);
    add_parameter!(lunch_signal_threshold_bonus);
    add_parameter!(post_lunch_confirmation_mins);
    add_parameter!(lunch_liquidity_window_mins);
    add_parameter!(lunch_min_quote_updates);
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
    add_parameter!(contract_multiplier_confirmed);
    add_parameter!(dynamic_limit_enabled);
    add_parameter!(dynamic_limit_steps);
    add_parameter!(dynamic_limit_frame_wait_secs);
    add_parameter!(dynamic_limit_queue_ahead_factor);
    add_parameter!(dynamic_limit_adverse_selection_bps);
    add_parameter!(option_analytics_enabled);
    add_parameter!(option_risk_free_rate);
    add_parameter!(option_dividend_yield);
    parameters.insert(
        "option_market_inputs_observed_at_secs".into(),
        config
            .option_market_inputs_observed_at_secs
            .map_or_else(|| "none".into(), |value| value.to_string()),
    );
    add_parameter!(option_market_inputs_max_age_secs);
    parameters.insert(
        "option_risk_free_source".into(),
        config.option_risk_free_source.clone(),
    );
    parameters.insert(
        "option_dividend_source".into(),
        config.option_dividend_source.clone(),
    );
    add_parameter!(option_binomial_steps);
    add_parameter!(option_min_abs_delta);
    add_parameter!(option_max_abs_delta);
    add_parameter!(option_min_implied_volatility);
    add_parameter!(option_max_implied_volatility);
    add_parameter!(option_max_extrinsic_percentage);
    add_parameter!(iv_rank_filter_enabled);
    add_parameter!(iv_rank_window_sessions);
    add_parameter!(iv_rank_min_sessions);
    add_parameter!(iv_rank_min);
    add_parameter!(iv_rank_max);
    add_parameter!(adaptive_entry_filter_enabled);
    add_parameter!(max_friction_stop_ratio);
    add_parameter!(volatility_normalized_signals_enabled);
    add_parameter!(target_underlying_volatility_percentage);
    add_parameter!(meta_filter_min_examples);
    add_parameter!(meta_filter_min_train_examples);
    add_parameter!(meta_filter_min_accepted_holdout);
    add_parameter!(meta_filter_min_coverage);
    add_parameter!(meta_filter_max_brier_score);
    add_parameter!(meta_filter_min_positive_fold_ratio);
    add_parameter!(meta_filter_max_concentration);
    add_parameter!(nonlinear_meta_filter_enabled);
    add_parameter!(tree_meta_filter_enabled);
    add_parameter!(tree_meta_filter_min_improvement);
    add_parameter!(experiment_runner_enabled);
    add_parameter!(vertical_spread_research_enabled);
    add_parameter!(vertical_atomic_execution_verified);
    add_parameter!(readonly_slippage_bps);
    add_parameter!(learning_slippage_bps);
    add_parameter!(vix_refresh_secs);
    add_parameter!(vix_max_age_secs);
    add_parameter!(vix_previous_close_max_age_secs);
    add_parameter!(vix_elevated_level);
    add_parameter!(vix_spike_change_percentage);
    add_parameter!(vix_elevated_position_factor);
    add_parameter!(vix_spike_threshold_bonus);
    add_parameter!(max_market_data_age_secs);
    add_parameter!(max_option_spread_percentage);
    add_parameter!(min_option_volume);
    add_parameter!(min_option_chain_acceptance_percentage);
    add_parameter!(min_option_chain_contracts_per_side);
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

    let build_hash = crate::build_identity::executable_build_hash();
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

fn digest_hex(bytes: &[u8]) -> String {
    ring::digest::digest(&ring::digest::SHA256, bytes)
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn authorization_consumed_path(authorization_path: &Path, nonce: &str) -> PathBuf {
    authorization_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("consumed-authorizations")
        .join(format!("{}.json", digest_hex(nonce.as_bytes())))
}

fn consume_authorization_file(path: &Path, nonce: &str) -> Result<(), AppError> {
    reject_symlink(path)?;
    let consumed = authorization_consumed_path(path, nonce);
    let consumed_dir = consumed
        .parent()
        .ok_or_else(|| AppError::External("ruta de autorización inválida".into()))?;
    ensure_private_dir(consumed_dir)?;
    std::fs::hard_link(path, &consumed).map_err(|error| {
        AppError::External(format!(
            "la autorización ya fue consumida o no pudo reclamarse atómicamente: {error}"
        ))
    })?;
    std::fs::remove_file(path)?;
    std::fs::File::open(consumed_dir)?.sync_all()?;
    Ok(())
}

fn argentina_day(timestamp_secs: i64) -> i64 {
    crate::time_utils::argentina_session_day(timestamp_secs)
}

fn bucket_percentage(value: f64, bucket_size: f64) -> f64 {
    if !value.is_finite() || bucket_size <= 0.0 {
        value
    } else {
        (value / bucket_size).round() * bucket_size
    }
}

fn load_evidence_bundle(path: &Path) -> Result<EvidenceBundle, AppError> {
    Ok(serde_json::from_slice(&read_private_limited(
        path,
        64 * 1024 * 1024,
    )?)?)
}

fn write_json_atomic<T: serde::Serialize>(path: &Path, value: &T) -> Result<(), AppError> {
    write_atomic(path, &serde_json::to_vec_pretty(value)?)?;
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
        meta_filter_policy: MetaFilterPolicy {
            min_examples: config.meta_filter_min_examples,
            min_train_examples: config.meta_filter_min_train_examples,
            min_accepted_holdout: config.meta_filter_min_accepted_holdout,
            min_coverage: config.meta_filter_min_coverage,
            max_brier_score: config.meta_filter_max_brier_score,
            min_positive_fold_ratio: config.meta_filter_min_positive_fold_ratio,
            max_concentration: config.meta_filter_max_concentration,
            nonlinear_enabled: config.nonlinear_meta_filter_enabled,
            tree_enabled: config.tree_meta_filter_enabled,
            tree_min_stressed_expectancy_improvement: config.tree_meta_filter_min_improvement,
        },
    }
}

fn unresolved_local_orders(events: &[JournalEvent]) -> Result<Vec<String>, AppError> {
    let mut requests = HashMap::<String, OrderRequest>::new();
    let mut latest = HashMap::<String, OrderExecution>::new();
    let mut pending = BTreeSet::<String>::new();
    for event in events {
        match &event.event {
            JournalEventKind::OrderIntentCreated { request } => {
                if requests
                    .get(&request.operation_id)
                    .is_some_and(|existing| existing != request)
                {
                    return Err(AppError::Recovery(format!(
                        "la intención {} cambió durante el replay",
                        request.operation_id
                    )));
                }
                requests.insert(request.operation_id.clone(), request.clone());
                pending.insert(request.operation_id.clone());
            }
            JournalEventKind::OrderSubmitted {
                symbol,
                side,
                quantity,
                limit_price,
            } => {
                let operation_id = event.operation_id.as_ref().ok_or_else(|| {
                    AppError::Recovery(
                        "evento legado order_submitted sin operation_id durante el replay".into(),
                    )
                })?;
                let request = OrderRequest {
                    operation_id: operation_id.clone(),
                    symbol: symbol.clone(),
                    quantity: *quantity,
                    market_price: *limit_price,
                    limit_price: *limit_price,
                    side: *side,
                };
                if requests
                    .get(operation_id)
                    .is_some_and(|existing| existing != &request)
                {
                    return Err(AppError::Recovery(format!(
                        "la intención legada {operation_id} cambió durante el replay"
                    )));
                }
                requests.insert(operation_id.clone(), request);
                pending.insert(operation_id.clone());
            }
            JournalEventKind::OrderAccepted { execution }
            | JournalEventKind::OrderUpdated { execution } => {
                let request = requests.get(&execution.operation_id).ok_or_else(|| {
                    AppError::Recovery(format!(
                        "estado de orden huérfano para {}",
                        execution.operation_id
                    ))
                })?;
                validate_order_execution(request, execution).map_err(|reason| {
                    AppError::Recovery(format!(
                        "estado inválido para {} durante el replay: {reason}",
                        execution.operation_id
                    ))
                })?;
                if let Some(previous) = latest.get(&execution.operation_id) {
                    if previous != execution {
                        validate_order_transition(previous, execution).map_err(|reason| {
                            AppError::Recovery(format!(
                                "transición inválida para {} durante el replay: {reason}",
                                execution.operation_id
                            ))
                        })?;
                    }
                }
                latest.insert(execution.operation_id.clone(), execution.clone());
                if matches!(
                    execution.status,
                    OrderStatus::Executed | OrderStatus::Rejected
                ) || (execution.status == OrderStatus::Cancelled
                    && execution.filled_quantity == 0)
                {
                    pending.remove(&execution.operation_id);
                } else {
                    pending.insert(execution.operation_id.clone());
                }
            }
            JournalEventKind::OrderUnknown { request, .. } => {
                let original = requests.get(&request.operation_id).ok_or_else(|| {
                    AppError::Recovery(format!(
                        "resultado desconocido sin intención previa para {}",
                        request.operation_id
                    ))
                })?;
                if original != request {
                    return Err(AppError::Recovery(format!(
                        "el resultado desconocido cambió la intención {}",
                        request.operation_id
                    )));
                }
                if latest.get(&request.operation_id).is_some_and(|execution| {
                    matches!(
                        execution.status,
                        OrderStatus::Executed | OrderStatus::Rejected | OrderStatus::Cancelled
                    )
                }) {
                    return Err(AppError::Recovery(format!(
                        "resultado desconocido posterior a un estado terminal para {}",
                        request.operation_id
                    )));
                }
                pending.insert(request.operation_id.clone());
            }
            JournalEventKind::PartialFillExposure {
                execution,
                requested_quantity,
                remaining_quantity,
            } => {
                let request = requests.get(&execution.operation_id).ok_or_else(|| {
                    AppError::Recovery(format!(
                        "exposición parcial sin intención tipada para {}",
                        execution.operation_id
                    ))
                })?;
                validate_order_execution(request, execution).map_err(|reason| {
                    AppError::Recovery(format!(
                        "exposición parcial inválida para {}: {reason}",
                        execution.operation_id
                    ))
                })?;
                if request.quantity != *requested_quantity
                    || execution.remaining_quantity(*requested_quantity) != *remaining_quantity
                    || latest.get(&execution.operation_id) != Some(execution)
                {
                    return Err(AppError::Recovery(format!(
                        "cantidades o estado incoherentes en la exposición parcial {}",
                        execution.operation_id
                    )));
                }
                pending.insert(execution.operation_id.clone());
            }
            _ => {}
        }
    }
    Ok(pending.into_iter().collect())
}

fn apply_recovery_event(
    engine: &mut TradingEngine,
    portfolio: &mut Portfolio,
    risk: &mut RiskManager,
    learning_state: &mut LearningState,
    trading_performance: &mut Vec<ValidationTrade>,
    event: &JournalEvent,
) -> Result<(), AppError> {
    validate_recovery_position_state(engine, portfolio)?;
    match &event.event {
        JournalEventKind::PositionOpened { position } => {
            if engine
                .position
                .as_ref()
                .is_some_and(|existing| existing != position)
                || portfolio
                    .position(&position.operation_id)
                    .is_some_and(|existing| existing != position)
            {
                return Err(AppError::Recovery(
                    "position_opened contradice la exposición recuperada".into(),
                ));
            }
            if engine.position.is_none() && !engine.open_position(position.clone()) {
                return Err(AppError::Recovery(
                    "position_opened contiene una posición inválida".into(),
                ));
            }
            if !portfolio.contains(&position.operation_id) && !portfolio.open(position.clone()) {
                return Err(AppError::Recovery(
                    "position_opened no pudo incorporarse al portfolio".into(),
                ));
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
            if engine
                .position
                .as_ref()
                .is_some_and(|position| position.operation_id != *operation_id)
            {
                return Err(AppError::Recovery(
                    "position_closed no corresponde a la exposición recuperada".into(),
                ));
            }
            if engine.position.is_some() {
                portfolio.close(
                    operation_id,
                    *exit_price,
                    *net_pnl,
                    event.timestamp_secs,
                    *reason,
                );
                risk.record_close_at(event.timestamp_secs, *net_pnl);
                engine.close(*reason);
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
        }
        JournalEventKind::KillSwitch { active } => {
            if *active {
                risk.engage_kill_switch();
                if engine.position.is_none() {
                    engine.halt();
                }
            } else {
                if risk.resume().is_ok() {
                    engine.resume();
                } else {
                    engine.halt();
                }
            }
        }
        _ => {}
    }
    validate_recovery_position_state(engine, portfolio)?;
    Ok(())
}

fn validate_recovery_position_state(
    engine: &TradingEngine,
    portfolio: &Portfolio,
) -> Result<(), AppError> {
    let metrics = portfolio.metrics();
    let consistent = match engine.position.as_ref() {
        None => metrics.open_positions == 0,
        Some(position) => {
            metrics.open_positions == 1
                && portfolio.position(&position.operation_id) == Some(position)
        }
    };
    if !consistent {
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

fn masked_identifier(value: &str) -> String {
    crate::redaction::masked_identifier(value)
}

fn option_kind_for_direction(direction: Direction) -> Option<OptionKind> {
    match direction {
        Direction::Up => Some(OptionKind::Call),
        Direction::Down => Some(OptionKind::Put),
        Direction::Neutral => None,
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
        ExitReason::WeekendRisk => "se cerró antes de una pausa prolongada",
        ExitReason::ExpiryRisk => "se cerró antes del horario límite de vencimiento",
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

fn vix_adjusted_threshold(
    base_threshold: f64,
    change_percentage: f64,
    spike_change_percentage: f64,
    spike_bonus: f64,
) -> f64 {
    if change_percentage >= spike_change_percentage {
        (base_threshold + spike_bonus).min(0.95)
    } else {
        base_threshold
    }
}

fn lunch_adjusted_threshold(base_threshold: f64, bonus: f64) -> f64 {
    (base_threshold + bonus).min(0.95)
}

fn volatility_adjusted_threshold(
    base_threshold: f64,
    observed_percentage: f64,
    target_percentage: f64,
) -> f64 {
    if !observed_percentage.is_finite() || observed_percentage <= 0.0 || target_percentage <= 0.0 {
        return base_threshold;
    }
    let regime_distance = (observed_percentage / target_percentage).ln().abs();
    (base_threshold + (regime_distance * 0.03).min(0.10)).min(0.95)
}

fn dataset_id(path: &Path) -> Result<String, AppError> {
    const MAX_DATASET_BYTES: u64 = 1024 * 1024 * 1024;
    let mut file = open_limited_read(path, MAX_DATASET_BYTES)?;
    let mut limited = (&mut file).take(MAX_DATASET_BYTES.saturating_add(1));
    let mut context = ring::digest::Context::new(&ring::digest::SHA256);
    let mut buffer = [0_u8; 64 * 1024];
    let mut total = 0_u64;
    loop {
        let read = limited.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        total = total.saturating_add(read as u64);
        if total > MAX_DATASET_BYTES {
            return Err(AppError::Recovery(format!(
                "dataset creció por encima de {MAX_DATASET_BYTES} bytes durante la lectura"
            )));
        }
        context.update(&buffer[..read]);
    }
    let digest = context
        .finish()
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    Ok(format!("sha256:{digest}"))
}

fn capture_market_frame(config: &Config, frame: &MarketFrame) -> Result<(), AppError> {
    let argentina_day = crate::time_utils::argentina_session_day(frame.underlying.timestamp_secs);
    let directory = config.data_dir.join("market").join(&config.ticker);
    crate::secure_fs::ensure_private_dir(&directory)?;
    let path = directory.join(format!("{argentina_day}.jsonl"));
    let mut file = open_private_append_bounded(&path, 256 * 1024 * 1024)?;
    let capture = CapturedMarketFrame::new("iol_v2_normalized", unix_now(), frame.clone())?;
    serde_json::to_writer(&mut file, &capture)?;
    file.write_all(b"\n")?;
    file.sync_data()?;
    Ok(())
}

fn storage_limits(config: &Config) -> StorageLimits {
    StorageLimits {
        max_total_bytes: config.data_dir_max_bytes,
        min_free_bytes: config.data_disk_min_free_bytes,
        capture_retention_days: config.market_capture_retention_days,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn journal_event(sequence: u64, operation_id: &str, event: JournalEventKind) -> JournalEvent {
        JournalEvent {
            schema_version: crate::persistence::JOURNAL_SCHEMA_VERSION,
            sequence,
            timestamp_secs: sequence as i64,
            operation_id: Some(operation_id.into()),
            previous_hash: String::new(),
            event_hash: String::new(),
            event_hmac: String::new(),
            event,
        }
    }

    fn recovery_position(operation_id: &str, kind: PositionKind) -> Position {
        Position {
            operation_id: operation_id.into(),
            option_symbol: match kind {
                PositionKind::Call => "GGALC100",
                PositionKind::Put => "GGALP100",
            }
            .into(),
            kind,
            entry_price: 2.0,
            contracts: 1,
            contract_multiplier: 100,
            opened_at_secs: 1,
            economics: None,
            entry_context: None,
        }
    }

    fn recovery_risk() -> RiskManager {
        RiskManager::new(RiskLimits {
            max_notional: 100_000.0,
            max_loss_per_trade: 1_000.0,
            max_daily_loss: 10_000.0,
            max_trades_per_day: 10,
        })
    }

    #[test]
    fn equal_logs_are_counted_on_one_line_even_when_interleaved() {
        let mut logs = VecDeque::new();
        for timestamp in 1..=7 {
            record_log(&mut logs, "Conexión en reintento".into(), timestamp);
        }
        assert_eq!(logs.len(), 1);
        assert_eq!(logs[0].repetitions, 7);
        assert_eq!(logs[0].timestamp_secs, 1);
        record_log(&mut logs, "Conexión recuperada".into(), 8);
        record_log(&mut logs, "Conexión en reintento".into(), 9);
        assert_eq!(logs.len(), 2, "cada texto conserva una única línea");
        assert_eq!(logs[0].repetitions, 8);
    }

    #[test]
    fn directional_strategy_always_accompanies_the_underlying_move() {
        assert_eq!(
            option_kind_for_direction(Direction::Up),
            Some(OptionKind::Call)
        );
        assert_eq!(
            option_kind_for_direction(Direction::Down),
            Some(OptionKind::Put)
        );
        assert_eq!(option_kind_for_direction(Direction::Neutral), None);
    }

    #[test]
    fn operational_logs_mask_external_identifiers() {
        assert_eq!(masked_identifier("123456789"), "••••6789");
        assert_eq!(masked_identifier("42"), "••••42");
    }

    #[test]
    fn canary_readiness_is_signed_bound_to_the_complete_build_and_fails_on_change() {
        let mut config = replay_config();
        config.mode = Mode::Live;
        enable_live_test_journal(&mut config);
        let readiness_path = config.data_dir.join("release-readiness.json");
        config.live_readiness_path = Some(readiness_path.clone());
        let mut app = TradingApp::new_for_test(config).unwrap();
        app.config.live_confirmed = true;
        app.config.iol_order_path = Some("/api/v2/operar".into());
        app.config.live_authorization_path = Some(app.config.data_dir.join("authorization.json"));
        app.config.market_sessions_path = Some(app.config.data_dir.join("sessions.json"));
        app.config.time_reference_url = Some("https://clock.example.invalid/time".into());
        let metrics = crate::release_readiness::QualityMetrics {
            lines_percentage: 95.0,
            regions_percentage: 95.0,
            branches_percentage: 90.0,
            mutation_score_percentage: 90.0,
        };
        let timestamp = unix_now();
        let mut readiness = crate::release_readiness::ReleaseReadiness {
            schema_version: crate::release_readiness::RELEASE_READINESS_SCHEMA_VERSION,
            build_hash: app.strategy_manifest.build_hash.clone(),
            commit_hash: "a".repeat(64),
            generated_at_secs: timestamp,
            global: metrics,
            critical_scopes: crate::release_readiness::REQUIRED_CRITICAL_SCOPES
                .iter()
                .map(|scope| ((*scope).into(), metrics))
                .collect(),
            coverage_report_sha256: crate::release_readiness::digest_hex(b"coverage"),
            mutation_report_sha256: crate::release_readiness::digest_hex(b"mutation"),
            fuzz_corpus_sha256: crate::release_readiness::digest_hex(b"corpus"),
            fuzz_campaign_seconds: 1,
            signature: String::new(),
        };
        let master_key_path = app.config.master_key_path.as_deref().unwrap();
        readiness.signature = crate::secrets::sign_release_readiness_payload_from(
            master_key_path,
            &readiness.signing_payload().unwrap(),
        )
        .unwrap();
        write_json_atomic(&readiness_path, &readiness).unwrap();
        let digest = app.release_readiness_digest(timestamp).unwrap();
        app.authorized_readiness_sha256 = Some(digest);
        assert!(app.release_readiness_is_still_authorized(timestamp));
        assert!(app.live_entry_authorization_is_valid(timestamp));
        app.authorized_readiness_sha256 = None;
        assert!(!app.release_readiness_is_still_authorized(timestamp));
        assert!(!app.live_entry_authorization_is_valid(timestamp));
        app.authorized_readiness_sha256 = Some(readiness_digest_hex(
            &read_private_limited(&readiness_path, 256 * 1024).unwrap(),
        ));

        readiness.build_hash = "b".repeat(64);
        readiness.signature = crate::secrets::sign_release_readiness_payload_from(
            master_key_path,
            &readiness.signing_payload().unwrap(),
        )
        .unwrap();
        write_json_atomic(&readiness_path, &readiness).unwrap();
        assert!(app.release_readiness_digest(timestamp).is_err());
        assert!(!app.release_readiness_is_still_authorized(timestamp));
        assert!(!app.live_entry_authorization_is_valid(timestamp));
    }

    #[test]
    fn consumed_authorization_nonce_cannot_be_replayed_from_a_copy() {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory = std::env::temp_dir().join(format!(
            "options-authorization-replay-{}-{unique}",
            std::process::id()
        ));
        let path = directory.join("live-authorization.json");
        crate::secure_fs::write_new(&path, b"signed grant").unwrap();
        consume_authorization_file(&path, "fixed-signed-nonce").unwrap();
        crate::secure_fs::write_new(&path, b"copied signed grant").unwrap();
        assert!(consume_authorization_file(&path, "fixed-signed-nonce").is_err());
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn oversized_replay_dataset_is_rejected_before_hashing() {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "options-oversized-dataset-{}-{unique}.jsonl",
            std::process::id()
        ));
        let file = std::fs::File::create(&path).unwrap();
        file.set_len(1024 * 1024 * 1024 + 1).unwrap();
        drop(file);

        assert!(dataset_id(&path).is_err());
        std::fs::remove_file(path).unwrap();
    }

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
            data_dir_max_bytes: 2_147_483_648,
            data_disk_min_free_bytes: 536_870_912,
            market_capture_retention_days: 30,
            holidays_api_base_url: "https://api.argentinadatos.com/v1/feriados".into(),
            market_sessions_path: None,
            entry_delay_after_open_mins: 45,
            weekend_risk_enabled: true,
            pre_break_last_entry_minute: 15 * 60,
            pre_break_force_exit_minute: 16 * 60 + 30,
            expiry_day_force_exit_minute: 15 * 60 + 15,
            lunch_slowdown_enabled: true,
            lunch_slowdown_start_minute: 12 * 60 + 30,
            lunch_slowdown_end_minute: 14 * 60,
            lunch_position_factor: 0.5,
            lunch_max_spread_factor: 0.75,
            lunch_signal_threshold_bonus: 0.05,
            post_lunch_confirmation_mins: 5,
            lunch_liquidity_window_mins: 5,
            lunch_min_quote_updates: 3,
            connection_retry_attempts: 3,
            connection_retry_delay_secs: 1,
            max_investment_amount: 100_000.0,
            max_loss_per_trade: 5_000.0,
            max_daily_loss: 10_000.0,
            max_trades_per_day: 20,
            stop_loss_percentage: 15.0,
            contract_multiplier: 1,
            contract_multiplier_confirmed: false,
            readonly_slippage_bps: 5.0,
            max_market_data_age_secs: 15,
            max_option_spread_percentage: 20.0,
            min_option_volume: 1,
            min_option_chain_acceptance_percentage: 80.0,
            min_option_chain_contracts_per_side: 1,
            option_target_expiry_days: 1,
            option_max_expiry_days: 365,
            max_option_moneyness_distance_percentage: 100.0,
            min_reward_risk_ratio: 1.25,
            learning_slippage_bps: 25.0,
            vix_quote_url: None,
            vix_refresh_secs: 60,
            vix_max_age_secs: 900,
            vix_previous_close_max_age_secs: 345_600,
            vix_elevated_level: 25.0,
            vix_spike_change_percentage: 10.0,
            vix_elevated_position_factor: 0.5,
            vix_spike_threshold_bonus: 0.10,
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
            time_reference_url: None,
            time_reference_refresh_secs: 300,
            time_reference_max_skew_secs: 30,
            iol_websocket_enabled: false,
            iol_websocket_url: "wss://example.invalid".into(),
            iol_order_path: None,
            order_tracking_timeout_secs: 30,
            order_status_poll_interval_millis: 1_000,
            order_cancel_timeout_secs: 15,
            dynamic_limit_enabled: false,
            dynamic_limit_steps: 4,
            dynamic_limit_frame_wait_secs: 2,
            dynamic_limit_queue_ahead_factor: 1.0,
            dynamic_limit_adverse_selection_bps: 10.0,
            option_analytics_enabled: false,
            option_risk_free_rate: 0.35,
            option_dividend_yield: 0.0,
            option_market_inputs_observed_at_secs: None,
            option_market_inputs_max_age_secs: 86_400,
            option_risk_free_source: "manual_env".into(),
            option_dividend_source: "manual_env".into(),
            option_binomial_steps: 150,
            option_min_abs_delta: 0.15,
            option_max_abs_delta: 0.85,
            option_min_implied_volatility: 0.01,
            option_max_implied_volatility: 3.0,
            option_max_extrinsic_percentage: 100.0,
            iv_rank_filter_enabled: false,
            iv_rank_window_sessions: 252,
            iv_rank_min_sessions: 60,
            iv_rank_min: 0.0,
            iv_rank_max: 100.0,
            adaptive_entry_filter_enabled: false,
            max_friction_stop_ratio: 0.25,
            volatility_normalized_signals_enabled: false,
            target_underlying_volatility_percentage: 1.0,
            meta_filter_min_examples: 100,
            meta_filter_min_train_examples: 60,
            meta_filter_min_accepted_holdout: 20,
            meta_filter_min_coverage: 0.15,
            meta_filter_max_brier_score: 0.25,
            meta_filter_min_positive_fold_ratio: 0.67,
            meta_filter_max_concentration: 0.85,
            nonlinear_meta_filter_enabled: false,
            tree_meta_filter_enabled: false,
            tree_meta_filter_min_improvement: 0.05,
            experiment_runner_enabled: false,
            vertical_spread_research_enabled: false,
            vertical_atomic_execution_verified: false,
            live_readiness_path: None,
            live_authorization_path: None,
            master_key_path: None,
            live_confirmed: false,
        }
    }

    fn enable_live_test_journal(config: &mut Config) {
        let path = config.data_dir.join("test-master.key");
        crate::secure_fs::ensure_private_dir(&config.data_dir).unwrap();
        crate::secrets::initialize_master_key(&path).unwrap();
        config.master_key_path = Some(path);
    }

    fn valid_test_account_funds() -> AccountFunds {
        AccountFunds {
            account_number: "test-account".into(),
            currency: "peso_Argentino".into(),
            status: "operable".into(),
            available: 100_000.0,
            immediate_available_to_trade: 100_000.0,
        }
    }

    #[tokio::test]
    async fn replay_opens_and_closes_option_positions() {
        let mut app = TradingApp::new_for_test(replay_config()).unwrap();
        while app.step().await.unwrap() {}
        assert!(app.metrics().trades > 0);
        assert!(app
            .portfolio
            .closed_trades()
            .iter()
            .all(|trade| trade.position.entry_price < 10.0));
    }

    #[tokio::test]
    async fn closed_market_state_clears_stale_quotes_and_reports_offline() {
        let mut app = TradingApp::new_for_test(replay_config()).unwrap();
        app.step().await.unwrap();
        assert!(app.current_frame.is_some());

        let open = app.apply_market_schedule(MarketScheduleStatus {
            open: false,
            entries_allowed: false,
            force_pre_break_exit: false,
            expiry_exit_due: false,
            pre_break: false,
            next_session_days: 0,
            lunch_slowdown: false,
            lunch_reconfirming: false,
            headline: "OFFLINE · MERCADO CERRADO".into(),
            detail: "Feriado: Día de la Independencia".into(),
        });

        assert!(!open);
        assert!(!app.market_open);
        assert!(app.current_frame.is_none());
        assert!(app.current_trend.is_none());
        assert!(app.status.contains("OFFLINE · MERCADO CERRADO"));
        assert!(app.status.contains("Feriado"));
    }

    #[tokio::test]
    async fn opening_observation_keeps_quotes_but_disables_entries() {
        let mut app = TradingApp::new_for_test(replay_config()).unwrap();
        app.step().await.unwrap();
        assert!(app.current_frame.is_some());

        let open = app.apply_market_schedule(MarketScheduleStatus {
            open: true,
            entries_allowed: false,
            force_pre_break_exit: false,
            expiry_exit_due: false,
            pre_break: false,
            next_session_days: 1,
            lunch_slowdown: false,
            lunch_reconfirming: false,
            headline: "ONLINE · OBSERVANDO APERTURA".into(),
            detail: "Recopilando precios · entradas habilitadas a las 11:15".into(),
        });

        assert!(open);
        assert!(app.market_open);
        assert!(!app.market_entries_allowed);
        assert!(app.current_frame.is_some());
        assert!(app.status.contains("11:15"));
    }

    #[tokio::test]
    async fn learning_without_vix_continues_and_leaves_one_audit_log() {
        let mut app = TradingApp::new_for_test(replay_config()).unwrap();

        app.step().await.unwrap();
        app.step().await.unwrap();

        assert!(!app.completed);
        let messages = app
            .logs()
            .iter()
            .filter(|entry| entry.message.contains("Learning continúa sin VIX"))
            .count();
        assert_eq!(messages, 1);
    }

    #[tokio::test]
    async fn vix_at_entry_is_frozen_in_the_position_context() {
        let mut app = TradingApp::new_for_test(replay_config()).unwrap();
        app.step().await.unwrap();
        let timestamp = app
            .current_frame
            .as_ref()
            .unwrap()
            .underlying
            .timestamp_secs;
        app.current_frame.as_mut().unwrap().vix = Some(VixObservation {
            level: 28.0,
            previous_close: Some(25.0),
            timestamp_secs: timestamp,
            previous_close_timestamp_secs: Some(timestamp.saturating_sub(86_400)),
            value_kind: crate::market::VixValueKind::Current,
        });

        app.evaluate_entry(timestamp, Direction::Up).await.unwrap();

        let context = app
            .engine
            .position
            .as_ref()
            .and_then(|position| position.entry_context)
            .unwrap();
        assert_eq!(context.vix_level, Some(28.0));
        assert_eq!(context.vix_change_percentage, Some(12.0));
    }

    #[tokio::test]
    async fn activated_vix_filter_blocks_a_post_learning_entry_when_vix_is_missing() {
        let mut config = replay_config();
        config.meta_filter_min_examples = 50;
        config.meta_filter_min_train_examples = 30;
        config.meta_filter_min_accepted_holdout = 10;
        config.meta_filter_max_concentration = 1.0;
        config.meta_filter_max_brier_score = 0.30;
        config.meta_filter_min_positive_fold_ratio = 2.0 / 3.0;
        let mut app = TradingApp::new_for_test(config).unwrap();
        app.step().await.unwrap();
        app.live_stage = LiveStage::Eligible;
        app.current_frame.as_mut().unwrap().vix = None;
        let trend = app.current_trend.as_mut().unwrap();
        trend.confidence = 0.8;
        trend.r_squared = Some(0.8);
        trend.slope_percent_per_minute = 0.1;
        for index in 0..120 {
            let quiet = index % 2 == 0;
            app.learning_state.record(ValidationTrade {
                kind: PositionKind::Call,
                net_pnl: if quiet { 100.0 } else { -100.0 },
                stressed_net_pnl: if quiet { 100.0 } else { -100.0 },
                closed_at_secs: index + 1,
                context: ValidationContext {
                    trade_id: index.to_string(),
                    opened_at_secs: index,
                    entry_spread_percentage: Some(1.0),
                    option_volume: Some(100),
                    days_to_expiry: Some(20),
                    moneyness_distance_percentage: Some(1.0),
                    trend_confidence: Some(0.8),
                    trend_r_squared: Some(0.8),
                    trend_slope_percent_per_minute: Some(0.1),
                    vix_level: Some(if quiet { 15.0 } else { 35.0 }),
                    vix_change_percentage: Some(if quiet { -5.0 } else { 15.0 }),
                    ..ValidationContext::default()
                },
            });
        }
        let timestamp = app
            .current_frame
            .as_ref()
            .unwrap()
            .underlying
            .timestamp_secs;

        app.evaluate_entry(timestamp, Direction::Up).await.unwrap();

        assert!(app.engine.position.is_none());
        assert!(app
            .logs()
            .iter()
            .any(|entry| entry.message.contains("Meta-filtro VIX activo rechazó")));
    }

    #[tokio::test]
    async fn elevated_vix_reduces_all_entry_risk_limits_only_when_policy_is_active() {
        let mut app = TradingApp::new_for_test(replay_config()).unwrap();
        app.step().await.unwrap();
        app.current_frame.as_mut().unwrap().vix = Some(VixObservation {
            level: 30.0,
            previous_close: Some(28.0),
            timestamp_secs: 1,
            previous_close_timestamp_secs: None,
            value_kind: crate::market::VixValueKind::Current,
        });

        assert_eq!(app.execution_risk_limits(false), (100_000.0, 5_000.0, 5));
        assert_eq!(app.execution_risk_limits(true), (50_000.0, 2_500.0, 2));
        app.current_frame
            .as_mut()
            .unwrap()
            .vix
            .as_mut()
            .unwrap()
            .level = 20.0;
        assert_eq!(app.execution_risk_limits(true), (100_000.0, 5_000.0, 5));

        app.lunch_slowdown = true;
        assert_eq!(app.execution_risk_limits(false), (50_000.0, 2_500.0, 2));
        app.current_frame
            .as_mut()
            .unwrap()
            .vix
            .as_mut()
            .unwrap()
            .level = 30.0;
        assert_eq!(app.execution_risk_limits(true), (25_000.0, 1_250.0, 1));
    }

    #[test]
    fn canary_uses_its_own_limits_for_recovery_and_regression() {
        let config = replay_config();
        let canary_limits = risk_limits_for_stage(&config, LiveStage::Canary);
        assert_eq!(canary_limits.max_notional, 10_000.0);
        assert_eq!(canary_limits.max_loss_per_trade, 500.0);
        assert_eq!(canary_limits.max_daily_loss, 1_000.0);
        assert_eq!(canary_limits.max_trades_per_day, 5);

        let mut app = TradingApp::new_for_test(config).unwrap();
        app.live_stage = LiveStage::Canary;
        app.risk.limits = canary_limits;
        app.record_stage_trade(ValidationTrade {
            kind: PositionKind::Call,
            net_pnl: -1_000.0,
            stressed_net_pnl: -1_000.0,
            closed_at_secs: 1,
            context: ValidationContext::default(),
        })
        .unwrap();

        assert!(app.return_to_learning_pending);
    }

    #[test]
    fn future_cost_calibration_is_not_treated_as_fresh() {
        let mut app = TradingApp::new_for_test(replay_config()).unwrap();
        app.cost_calibration = Some(CostCalibration {
            operation_number: "future".into(),
            operation_amount: 1_000.0,
            commission_percentage: 0.1,
            vat_percentage: 21.0,
            other_fees_percentage: 0.0,
            total_cost_percentage: 0.121,
            components: Vec::new(),
            observed_at_secs: 10_000,
            instrument_is_option: true,
            observed_contract_multiplier: None,
        });

        assert!(!app.has_fresh_option_calibration(9_699));
        assert!(app.has_fresh_option_calibration(9_700));
    }

    #[test]
    fn changed_cost_calibration_returns_canary_to_learning_when_flat() {
        let mut app = TradingApp::new_for_test(replay_config()).unwrap();
        app.live_stage = LiveStage::Canary;
        app.apply_cost_calibration(CostCalibration {
            operation_number: "changed".into(),
            operation_amount: 1_000.0,
            commission_percentage: 0.5,
            vat_percentage: 21.0,
            other_fees_percentage: 0.0,
            total_cost_percentage: 0.605,
            components: Vec::new(),
            observed_at_secs: 1,
            instrument_is_option: true,
            observed_contract_multiplier: None,
        });

        assert!(app.return_to_learning_pending);
        app.apply_pending_learning_return(2).unwrap();
        assert_eq!(app.live_stage, LiveStage::Learning);
    }

    #[tokio::test]
    async fn tui_vix_accessor_hides_stale_observations() {
        let mut app = TradingApp::new_for_test(replay_config()).unwrap();
        app.step().await.unwrap();
        let timestamp = app
            .current_frame
            .as_ref()
            .unwrap()
            .underlying
            .timestamp_secs;
        app.current_frame.as_mut().unwrap().vix = Some(VixObservation {
            level: 20.0,
            previous_close: Some(19.0),
            timestamp_secs: timestamp,
            previous_close_timestamp_secs: None,
            value_kind: crate::market::VixValueKind::Current,
        });
        assert!(app.current_fresh_vix().is_some());

        app.current_frame
            .as_mut()
            .unwrap()
            .vix
            .as_mut()
            .unwrap()
            .timestamp_secs = timestamp - app.config.vix_max_age_secs as i64 - 1;
        assert!(app.current_fresh_vix().is_none());
    }

    #[test]
    fn vix_spike_raises_probability_threshold_but_normal_change_does_not() {
        assert_eq!(vix_adjusted_threshold(0.55, 5.0, 10.0, 0.10), 0.55);
        assert_eq!(vix_adjusted_threshold(0.55, 10.0, 10.0, 0.10), 0.65);
        assert_eq!(vix_adjusted_threshold(0.90, 20.0, 10.0, 0.10), 0.95);
    }

    #[test]
    fn lunch_bonus_raises_probability_threshold_with_the_same_safety_cap() {
        assert!((lunch_adjusted_threshold(0.55, 0.05) - 0.60).abs() < 1e-12);
        assert_eq!(lunch_adjusted_threshold(0.93, 0.05), 0.95);
    }

    #[tokio::test]
    async fn lunch_liquidity_requires_time_coverage_and_real_quote_updates() {
        let mut app = TradingApp::new_for_test(replay_config()).unwrap();
        app.step().await.unwrap();
        let mut frame = app.current_frame.clone().unwrap();
        let symbol = frame.options[0].symbol.clone();
        let start = frame.underlying.timestamp_secs;
        let mut monitor = LunchLiquidityMonitor::default();
        monitor.observe(&frame, 300);
        assert!(!monitor.sufficient_updates(&symbol, start, 300, 3));

        for offset in [100_i64, 200, 300] {
            frame.underlying.timestamp_secs = start + offset;
            for option in &mut frame.options {
                option.timestamp_secs = start + offset;
            }
            frame.options[0].volume += 1;
            monitor.observe(&frame, 300);
        }

        assert!(monitor.sufficient_updates(&symbol, start + 300, 300, 3));
    }

    #[test]
    fn a_second_instance_for_the_same_mode_is_rejected() {
        let config = replay_config();
        let first = TradingApp::new_for_test(config.clone()).unwrap();
        let second = TradingApp::new_for_test(config);
        assert!(matches!(second, Err(AppError::Recovery(_))));
        drop(first);
    }

    #[tokio::test]
    async fn kill_switch_blocks_new_entries() {
        let mut app = TradingApp::new_for_test(replay_config()).unwrap();
        app.toggle_kill_switch().unwrap();
        for _ in 0..10 {
            app.step().await.unwrap();
        }
        assert_eq!(app.metrics().trades, 0);
        assert_eq!(app.engine.state, TradingState::Halted);
    }

    #[test]
    fn exhausted_connection_retries_make_the_engine_clearly_inoperative() {
        let mut app = TradingApp::new_for_test(replay_config()).unwrap();
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
        let mut app = TradingApp::new_for_test(config.clone()).unwrap();
        for _ in 0..12 {
            app.step().await.unwrap();
        }
        app.shutdown().await.unwrap();
        let expected_metrics = app.metrics();
        let expected_timestamp = app.last_market_timestamp;
        drop(app);

        let mut recovered_config = config;
        recovered_config.recover_state = true;
        let mut recovered = TradingApp::new_for_test(recovered_config).unwrap();
        assert_eq!(recovered.metrics(), expected_metrics);
        assert_eq!(recovered.last_market_timestamp, expected_timestamp);
        recovered.step().await.unwrap();
        assert!(recovered.last_market_timestamp > expected_timestamp);
    }

    #[tokio::test]
    async fn reconstructed_position_is_immediately_checked_for_profit_exit() {
        let mut app = TradingApp::new_for_test(replay_config()).unwrap();
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
                funds: None,
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
    async fn invalid_live_account_funds_are_an_operational_block_not_an_online_status() {
        let mut config = replay_config();
        config.mode = Mode::Live;
        enable_live_test_journal(&mut config);
        let mut app = TradingApp::new_for_test(config).unwrap();
        app.step().await.unwrap();
        let timestamp = app
            .current_frame
            .as_ref()
            .unwrap()
            .underlying
            .timestamp_secs;

        app.apply_account_snapshot(
            timestamp,
            AccountSnapshot {
                positions: Vec::new(),
                pending_orders: Vec::new(),
                funds: Some(AccountFunds {
                    account_number: "2033590".into(),
                    currency: "peso_Argentino".into(),
                    status: "bloqueada".into(),
                    available: 1_000.0,
                    immediate_available_to_trade: 1_000.0,
                }),
            },
        )
        .unwrap();

        assert!(app.reconciliation_blocked);
        assert!(app.status.contains("Detenido"));
        assert!(!app.status.contains("conexión"));
    }

    #[tokio::test]
    async fn recovered_live_position_freezes_verified_catalog_metadata_and_direction() {
        let mut config = replay_config();
        config.mode = Mode::Live;
        enable_live_test_journal(&mut config);
        let mut app = TradingApp::new_for_test(config).unwrap();
        app.step().await.unwrap();
        app.live_stage = LiveStage::Live;
        let observed_at = unix_now();
        let frame = app.current_frame.as_mut().unwrap();
        let quote = &mut frame.options[0];
        quote.catalog_contract_multiplier = Some(100);
        quote.catalog_observed_at_secs = Some(observed_at);
        quote.catalog_schema_version = 1;
        quote.catalog_sha256 = Some([7; 32]);
        quote.catalog_archived = true;
        quote.contract_metadata_source = ContractMetadataSource::IolCatalog;
        let symbol = quote.symbol.clone();
        let kind = PositionKind::from(quote.kind);
        let timestamp = frame.underlying.timestamp_secs;

        app.apply_account_snapshot(
            timestamp,
            AccountSnapshot {
                positions: vec![AccountPosition {
                    symbol,
                    quantity: 2,
                    average_price: Some(2.0),
                    kind: Some(kind),
                    is_option: true,
                }],
                pending_orders: Vec::new(),
                funds: Some(valid_test_account_funds()),
            },
        )
        .unwrap();

        let position = app.engine.position.as_ref().unwrap();
        assert_eq!(position.contract_multiplier, 100);
        assert_eq!(
            position.direction(),
            match kind {
                PositionKind::Call => Direction::Up,
                PositionKind::Put => Direction::Down,
            }
        );
        let context = position.entry_context.as_ref().unwrap();
        assert_eq!(
            context.contract_metadata_source,
            ContractMetadataSource::IolCatalog
        );
        assert_eq!(
            context.contract_metadata_observed_at_secs,
            Some(observed_at)
        );
        assert_eq!(context.contract_metadata_catalog_sha256, Some([7; 32]));
        assert!(context.contract_metadata_catalog_archived);
        assert!(!app.risk.state.kill_switch);

        app.selected_option = None;
        assert!(app.contract_multiplier_is_verified_for(OrderSide::Sell));
        app.engine
            .position
            .as_mut()
            .unwrap()
            .entry_context
            .as_mut()
            .unwrap()
            .contract_metadata_catalog_archived = false;
        assert!(!app.contract_multiplier_is_verified_for(OrderSide::Sell));
    }

    #[tokio::test]
    async fn recovered_live_position_without_verified_metadata_remains_visible_but_halts_entries() {
        let mut config = replay_config();
        config.mode = Mode::Live;
        enable_live_test_journal(&mut config);
        let mut app = TradingApp::new_for_test(config).unwrap();
        app.step().await.unwrap();
        app.live_stage = LiveStage::Live;
        let frame = app.current_frame.as_ref().unwrap();
        let quote = frame.options[0].clone();
        let timestamp = frame.underlying.timestamp_secs;

        app.apply_account_snapshot(
            timestamp,
            AccountSnapshot {
                positions: vec![AccountPosition {
                    symbol: quote.symbol,
                    quantity: 1,
                    average_price: Some(2.0),
                    kind: Some(PositionKind::from(quote.kind)),
                    is_option: true,
                }],
                pending_orders: Vec::new(),
                funds: Some(valid_test_account_funds()),
            },
        )
        .unwrap();

        assert!(app.engine.position.is_some());
        assert!(app
            .engine
            .position
            .as_ref()
            .unwrap()
            .entry_context
            .is_none());
        assert!(app.risk.state.kill_switch);
        assert_eq!(
            app.risk.state.kill_switch_reason,
            Some(crate::risk::KillSwitchReason::Operational)
        );
    }

    #[tokio::test]
    async fn recovery_rejects_a_position_kind_that_contradicts_the_current_catalog() {
        let mut app = TradingApp::new_for_test(replay_config()).unwrap();
        app.step().await.unwrap();
        app.live_stage = LiveStage::Live;
        let frame = app.current_frame.as_ref().unwrap();
        let quote = frame.options[0].clone();
        let timestamp = frame.underlying.timestamp_secs;
        let contradictory_kind = match quote.kind {
            OptionKind::Call => PositionKind::Put,
            OptionKind::Put => PositionKind::Call,
        };

        app.apply_account_snapshot(
            timestamp,
            AccountSnapshot {
                positions: vec![AccountPosition {
                    symbol: quote.symbol,
                    quantity: 1,
                    average_price: Some(2.0),
                    kind: Some(contradictory_kind),
                    is_option: true,
                }],
                pending_orders: Vec::new(),
                funds: None,
            },
        )
        .unwrap();

        assert!(app.engine.position.is_none());
        assert!(app.reconciliation_blocked);
        assert!(app.status.contains("catálogo vigente"));
    }

    #[tokio::test]
    async fn reconciliation_blocks_pending_or_multiple_remote_option_exposures() {
        let mut pending_app = TradingApp::new_for_test(replay_config()).unwrap();
        pending_app.step().await.unwrap();
        pending_app.live_stage = LiveStage::Live;
        let frame = pending_app.current_frame.as_ref().unwrap();
        let first = frame.options[0].clone();
        let timestamp = frame.underlying.timestamp_secs;
        pending_app
            .apply_account_snapshot(
                timestamp,
                AccountSnapshot {
                    positions: Vec::new(),
                    pending_orders: vec![crate::broker::AccountOrder {
                        broker_order_id: "broker-sensitive-1234".into(),
                        symbol: first.symbol,
                        side: Some(OrderSide::Buy),
                        quantity: 1,
                        kind: Some(PositionKind::from(first.kind)),
                        is_option: true,
                    }],
                    funds: None,
                },
            )
            .unwrap();
        assert!(pending_app.reconciliation_blocked);
        assert!(!pending_app.status.contains("broker-sensitive-1234"));

        let mut multiple_app = TradingApp::new_for_test(replay_config()).unwrap();
        multiple_app.step().await.unwrap();
        multiple_app.live_stage = LiveStage::Live;
        let frame = multiple_app.current_frame.as_ref().unwrap();
        let timestamp = frame.underlying.timestamp_secs;
        let quote = frame.options[0].clone();
        let positions = [1_u32, 2]
            .into_iter()
            .map(|quantity| AccountPosition {
                symbol: quote.symbol.clone(),
                quantity,
                average_price: Some(2.0),
                kind: Some(PositionKind::from(quote.kind)),
                is_option: true,
            })
            .collect();
        multiple_app
            .apply_account_snapshot(
                timestamp,
                AccountSnapshot {
                    positions,
                    pending_orders: Vec::new(),
                    funds: None,
                },
            )
            .unwrap();
        assert!(multiple_app.reconciliation_blocked);
        assert!(multiple_app.status.contains("informa 2 posiciones"));
    }

    #[tokio::test]
    async fn reconciliation_accepts_only_an_exact_local_remote_position_match() {
        let mut app = TradingApp::new_for_test(replay_config()).unwrap();
        app.step().await.unwrap();
        app.live_stage = LiveStage::Live;
        let frame = app.current_frame.as_ref().unwrap();
        let quote = frame.options[0].clone();
        let timestamp = frame.underlying.timestamp_secs;
        let remote = AccountPosition {
            symbol: quote.symbol,
            quantity: 1,
            average_price: Some(2.0),
            kind: Some(PositionKind::from(quote.kind)),
            is_option: true,
        };
        app.apply_account_snapshot(
            timestamp,
            AccountSnapshot {
                positions: vec![remote.clone()],
                pending_orders: Vec::new(),
                funds: None,
            },
        )
        .unwrap();
        assert!(!app.reconciliation_blocked);

        app.apply_account_snapshot(
            timestamp + 1,
            AccountSnapshot {
                positions: vec![remote],
                pending_orders: Vec::new(),
                funds: None,
            },
        )
        .unwrap();
        assert!(!app.reconciliation_blocked);
        assert_eq!(app.last_account_reconciliation_secs, timestamp + 1);

        let position = app.engine.position.as_ref().unwrap().clone();
        app.apply_account_snapshot(
            timestamp + 2,
            AccountSnapshot {
                positions: vec![AccountPosition {
                    symbol: position.option_symbol,
                    quantity: position.contracts + 1,
                    average_price: Some(position.entry_price),
                    kind: Some(position.kind),
                    is_option: true,
                }],
                pending_orders: Vec::new(),
                funds: None,
            },
        )
        .unwrap();
        assert!(app.reconciliation_blocked);
        assert!(app.status.contains("no coincide"));
    }

    #[tokio::test]
    async fn pause_blocks_entries_but_still_closes_existing_risk() {
        let mut app = TradingApp::new_for_test(replay_config()).unwrap();
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
                funds: None,
            },
        )
        .unwrap();
        app.paused = true;

        app.step().await.unwrap();

        assert!(app.engine.position.is_none());
        assert_eq!(app.engine.last_exit_reason, Some(ExitReason::ProfitTarget));
    }

    #[tokio::test]
    async fn pre_break_force_exit_closes_an_existing_position() {
        let mut app = TradingApp::new_for_test(replay_config()).unwrap();
        app.step().await.unwrap();
        let frame = app.current_frame.clone().unwrap();
        let quote = frame.options.first().unwrap().clone();
        let bid = quote.executable_sell_price().unwrap();
        app.live_stage = LiveStage::Live;
        app.apply_account_snapshot(
            frame.underlying.timestamp_secs,
            AccountSnapshot {
                positions: vec![AccountPosition {
                    symbol: quote.symbol,
                    quantity: 1,
                    average_price: Some(bid),
                    kind: Some(PositionKind::from(quote.kind)),
                    is_option: true,
                }],
                pending_orders: Vec::new(),
                funds: None,
            },
        )
        .unwrap();
        app.market_force_pre_break_exit = true;

        app.evaluate_exit(frame.underlying.timestamp_secs)
            .await
            .unwrap();

        assert!(app.engine.position.is_none());
        assert_eq!(app.engine.last_exit_reason, Some(ExitReason::WeekendRisk));
    }

    #[tokio::test]
    async fn replay_applies_weekend_risk_from_recorded_session_dates() {
        let mut app = TradingApp::new_for_test(replay_config()).unwrap();
        app.step().await.unwrap();
        let initial = app.current_frame.clone().unwrap();
        let quote = initial.options[0].clone();
        let bid = quote.executable_sell_price().unwrap();
        app.live_stage = LiveStage::Live;
        app.apply_account_snapshot(
            initial.underlying.timestamp_secs,
            AccountSnapshot {
                positions: vec![AccountPosition {
                    symbol: quote.symbol,
                    quantity: 1,
                    average_price: Some(bid),
                    kind: Some(PositionKind::from(quote.kind)),
                    is_option: true,
                }],
                pending_orders: Vec::new(),
                funds: None,
            },
        )
        .unwrap();

        let friday_timestamp = 20_000 * 86_400 + (19 * 60 + 30) * 60;
        let monday_timestamp = friday_timestamp + 3 * 86_400;
        let mut friday = initial.clone();
        friday.underlying.timestamp_secs = friday_timestamp;
        friday.underlying.exchange_timestamp_secs = Some(friday_timestamp);
        friday.underlying.received_at_secs = friday_timestamp;
        for option in &mut friday.options {
            option.timestamp_secs = friday_timestamp;
            option.exchange_timestamp_secs = Some(friday_timestamp);
            option.received_at_secs = friday_timestamp;
        }
        let mut monday = friday.clone();
        monday.underlying.timestamp_secs = monday_timestamp;
        monday.underlying.exchange_timestamp_secs = Some(monday_timestamp);
        monday.underlying.received_at_secs = monday_timestamp;
        for option in &mut monday.options {
            option.timestamp_secs = monday_timestamp;
            option.exchange_timestamp_secs = Some(monday_timestamp);
            option.received_at_secs = monday_timestamp;
        }
        app.source = MarketSource::Replay(ReplayMarket::new(vec![friday, monday]).unwrap());

        app.step().await.unwrap();

        assert!(app.engine.position.is_none());
        assert_eq!(app.engine.last_exit_reason, Some(ExitReason::WeekendRisk));
    }

    #[tokio::test]
    async fn expiry_cutoff_closes_a_series_on_its_last_day() {
        let mut app = TradingApp::new_for_test(replay_config()).unwrap();
        app.step().await.unwrap();
        let mut frame = app.current_frame.clone().unwrap();
        frame.options[0].expiry_days = 0;
        let quote = frame.options[0].clone();
        let bid = quote.executable_sell_price().unwrap();
        app.current_frame = Some(frame.clone());
        app.live_stage = LiveStage::Live;
        app.apply_account_snapshot(
            frame.underlying.timestamp_secs,
            AccountSnapshot {
                positions: vec![AccountPosition {
                    symbol: quote.symbol,
                    quantity: 1,
                    average_price: Some(bid),
                    kind: Some(PositionKind::from(quote.kind)),
                    is_option: true,
                }],
                pending_orders: Vec::new(),
                funds: None,
            },
        )
        .unwrap();
        app.market_expiry_exit_due = true;

        app.evaluate_exit(frame.underlying.timestamp_secs)
            .await
            .unwrap();

        assert!(app.engine.position.is_none());
        assert_eq!(app.engine.last_exit_reason, Some(ExitReason::ExpiryRisk));
    }

    #[tokio::test]
    async fn mandatory_close_without_a_bid_keeps_the_position_and_halts() {
        let mut app = TradingApp::new_for_test(replay_config()).unwrap();
        app.step().await.unwrap();
        let frame = app.current_frame.clone().unwrap();
        let quote = frame.options[0].clone();
        let bid = quote.executable_sell_price().unwrap();
        app.live_stage = LiveStage::Live;
        app.apply_account_snapshot(
            frame.underlying.timestamp_secs,
            AccountSnapshot {
                positions: vec![AccountPosition {
                    symbol: quote.symbol.clone(),
                    quantity: 1,
                    average_price: Some(bid),
                    kind: Some(PositionKind::from(quote.kind)),
                    is_option: true,
                }],
                pending_orders: Vec::new(),
                funds: None,
            },
        )
        .unwrap();
        app.current_frame
            .as_mut()
            .unwrap()
            .options
            .iter_mut()
            .find(|option| option.symbol == quote.symbol)
            .unwrap()
            .bid = None;
        app.market_force_pre_break_exit = true;

        app.evaluate_exit(frame.underlying.timestamp_secs)
            .await
            .unwrap();

        assert!(app.engine.position.is_some());
        assert!(app.risk.state.kill_switch);
        assert_eq!(app.engine.state, TradingState::Halted);
        assert!(app.status.contains("no hay bid ejecutable"));
    }

    #[tokio::test]
    async fn missing_expiring_series_keeps_the_position_and_halts() {
        let mut app = TradingApp::new_for_test(replay_config()).unwrap();
        app.step().await.unwrap();
        let frame = app.current_frame.clone().unwrap();
        let quote = frame.options[0].clone();
        let bid = quote.executable_sell_price().unwrap();
        app.live_stage = LiveStage::Live;
        app.apply_account_snapshot(
            frame.underlying.timestamp_secs,
            AccountSnapshot {
                positions: vec![AccountPosition {
                    symbol: quote.symbol.clone(),
                    quantity: 1,
                    average_price: Some(bid),
                    kind: Some(PositionKind::from(quote.kind)),
                    is_option: true,
                }],
                pending_orders: Vec::new(),
                funds: None,
            },
        )
        .unwrap();
        app.engine.position.as_mut().unwrap().entry_context = Some(EntryContext {
            spread_percentage: quote.spread_percentage(),
            option_volume: quote.volume,
            days_to_expiry: 0,
            contract_metadata_observed_at_secs: None,
            contract_metadata_source: ContractMetadataSource::Legacy,
            contract_metadata_catalog_schema_version: 0,
            contract_metadata_catalog_sha256: None,
            contract_metadata_catalog_archived: false,
            moneyness_distance_percentage: 0.0,
            trend_confidence: 0.0,
            trend_r_squared: None,
            trend_slope_percent_per_minute: 0.0,
            vix_level: None,
            vix_change_percentage: None,
            lunch_slowdown: false,
            lunch_quote_updates: None,
            intrinsic_value: None,
            extrinsic_value: None,
            implied_volatility: None,
            iv_rank: None,
            iv_rank_window_sessions: None,
            iv_rank_observations: None,
            iv_rank_missing_reason: None,
            delta: None,
            gamma: None,
            theta_per_day: None,
            vega_per_point: None,
            rho_per_point: None,
        });
        app.current_frame
            .as_mut()
            .unwrap()
            .options
            .retain(|option| option.symbol != quote.symbol);
        app.market_expiry_exit_due = true;

        app.evaluate_exit(frame.underlying.timestamp_secs)
            .await
            .unwrap();

        assert!(app.engine.position.is_some());
        assert!(app.risk.state.kill_switch);
        assert_eq!(app.engine.state, TradingState::Halted);
        assert!(app.status.contains("límite de vencimiento"));
    }

    #[tokio::test]
    async fn reconstructed_position_is_immediately_checked_for_stop_loss() {
        let mut app = TradingApp::new_for_test(replay_config()).unwrap();
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
                funds: None,
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
        let mut app = TradingApp::new_for_test(replay_config()).unwrap();
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
    async fn degraded_option_chain_blocks_entry_before_order_selection() {
        let mut app = TradingApp::new_for_test(replay_config()).unwrap();
        app.step().await.unwrap();
        let frame = app.current_frame.as_mut().unwrap();
        frame.option_chain_quality = Some(crate::market::OptionChainQuality {
            catalog_contracts: 10,
            quote_rows: 6,
            accepted_contracts: 6,
            missing_quote_contracts: 4,
            invalid_quote_contracts: 0,
            accepted_call_contracts: 3,
            accepted_put_contracts: 3,
            by_expiry: Vec::new(),
        });
        let timestamp = frame.underlying.timestamp_secs;

        app.evaluate_entry(timestamp, Direction::Up).await.unwrap();

        assert!(app.engine.position.is_none());
        assert_eq!(app.engine.state, TradingState::Idle);
        assert!(app.status.contains("cadena de opciones incompleta"));
    }

    #[tokio::test]
    async fn readonly_transitions_both_ways_but_never_becomes_real_trading() {
        let mut config = replay_config();
        config.live_learning_min_trades = 4;
        config.live_learning_min_call_trades = 2;
        config.live_learning_min_put_trades = 2;
        config.live_learning_min_sessions = 1;
        config.live_learning_min_profit_factor = 1.0;
        let mut app = TradingApp::new_for_test(config).unwrap();
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
            observed_contract_multiplier: None,
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
        enable_live_test_journal(&mut config);
        let mut app = TradingApp::new_for_test(config).unwrap();
        assert_eq!(app.live_stage, LiveStage::Learning);
        assert!(!app.is_real_trading());
        app.live_stage = LiveStage::Live;
        assert!(!app.is_real_trading());
    }

    #[tokio::test]
    async fn unverified_multiplier_cannot_fall_back_to_paper_on_a_real_route() {
        let mut config = replay_config();
        config.mode = Mode::Live;
        config.iol_order_path = Some("/orders".into());
        enable_live_test_journal(&mut config);
        let mut app = TradingApp::new_for_test(config).unwrap();
        app.source = MarketSource::Iol(Box::new(
            IolClient::new(
                "https://example.invalid",
                "user".into(),
                crate::secrets::encrypt_legacy_for_test("password"),
                String::new(),
            )
            .unwrap(),
        ));
        app.live_stage = LiveStage::Canary;
        let request = OrderRequest {
            operation_id: "must-not-paper".into(),
            symbol: "GFGC100".into(),
            quantity: 1,
            market_price: 2.0,
            limit_price: 2.01,
            side: OrderSide::Buy,
        };
        let error = app.execute_order(1, &request).await.unwrap_err();
        assert!(matches!(error, AppError::OrderRejected(_)));
        assert!(app.paper_broker.status(&request.operation_id).is_none());
        assert!(app.reconciliation_blocked);
    }

    #[tokio::test]
    async fn missing_readiness_rejects_a_real_buy_before_intent_or_network() {
        let mut config = replay_config();
        config.mode = Mode::Live;
        config.iol_order_path = Some("/api/v2/operar".into());
        enable_live_test_journal(&mut config);
        let mut app = TradingApp::new_for_test(config).unwrap();
        app.step().await.unwrap();
        let option = app
            .current_frame
            .as_mut()
            .and_then(|frame| frame.options.first_mut())
            .unwrap();
        option.catalog_contract_multiplier = Some(100);
        option.catalog_observed_at_secs = Some(unix_now());
        option.contract_metadata_source = ContractMetadataSource::IolCatalog;
        option.catalog_schema_version = 1;
        option.catalog_sha256 = Some([7; 32]);
        option.catalog_archived = true;
        let option_symbol = option.symbol.clone();
        app.selected_option = Some(option_symbol.clone());
        app.source = MarketSource::Iol(Box::new(
            IolClient::new(
                "http://127.0.0.1:9",
                "user".into(),
                crate::secrets::encrypt_legacy_for_test("password"),
                String::new(),
            )
            .unwrap(),
        ));
        app.live_stage = LiveStage::Canary;
        let request = OrderRequest {
            operation_id: "readiness-toctou".into(),
            symbol: option_symbol,
            quantity: 1,
            market_price: 2.0,
            limit_price: 2.01,
            side: OrderSide::Buy,
        };

        let error = app.execute_order(unix_now(), &request).await.unwrap_err();
        assert!(matches!(error, AppError::OrderRejected(message) if message.contains("readiness")));
        assert_eq!(app.live_stage, LiveStage::Learning);
        assert!(app.paper_broker.status(&request.operation_id).is_none());
        assert!(!app
            .journal
            .events_after(0)
            .unwrap()
            .iter()
            .any(|event| matches!(&event.event, JournalEventKind::OrderIntentCreated { request: recorded } if recorded.operation_id == request.operation_id)));
    }

    #[test]
    fn purchase_quantity_never_exceeds_cash_budget_including_commission() {
        let quantity = affordable_contracts(100_000.0, 20_000.0, 1, 0.19, 10);
        assert_eq!(quantity, 4);
        assert!(purchase_cash_required(20_000.0, quantity, 1, 0.19) <= 100_000.0);
        assert!(purchase_cash_required(20_000.0, quantity + 1, 1, 0.19) > 100_000.0);
    }

    #[test]
    fn real_budget_uses_the_most_conservative_verified_fund() {
        let funds = AccountFunds {
            account_number: "2033590".into(),
            currency: "peso_Argentino".into(),
            status: "operable".into(),
            available: 80_000.0,
            immediate_available_to_trade: 45_000.0,
        };
        assert_eq!(effective_investment_budget(100_000.0, &funds), Ok(45_000.0));
        assert_eq!(effective_investment_budget(10_000.0, &funds), Ok(10_000.0));
    }

    #[test]
    fn invalid_account_funds_never_produce_a_real_budget() {
        for (currency, status, available, immediate) in [
            ("dolar_Estadounidense", "operable", 1.0, 1.0),
            ("peso_Argentino", "bloqueada", 1.0, 1.0),
            ("peso_Argentino", "operable", -1.0, 1.0),
            ("peso_Argentino", "operable", 1.0, f64::NAN),
        ] {
            let funds = AccountFunds {
                account_number: "2033590".into(),
                currency: currency.into(),
                status: status.into(),
                available,
                immediate_available_to_trade: immediate,
            };
            assert!(effective_investment_budget(100_000.0, &funds).is_err());
        }
    }

    proptest! {
        #[test]
        fn affordable_quantity_never_spends_more_than_the_declared_budget(
            budget in 1.0_f64..1_000_000.0,
            limit_price in 0.01_f64..10_000.0,
            contract_multiplier in 1_u32..1_000,
            commission_percentage in 0.0_f64..10.0,
            max_position_size in 1_u32..1_000,
        ) {
            let quantity = affordable_contracts(
                budget,
                limit_price,
                contract_multiplier,
                commission_percentage,
                max_position_size,
            );
            let spent = purchase_cash_required(
                limit_price,
                quantity,
                contract_multiplier,
                commission_percentage,
            );
            prop_assert!(spent <= budget + 1e-7 * budget.max(1.0));
            prop_assert!(quantity <= max_position_size);
            if quantity < max_position_size {
                let next = purchase_cash_required(
                    limit_price,
                    quantity + 1,
                    contract_multiplier,
                    commission_percentage,
                );
                prop_assert!(next > budget);
            }
        }
    }

    #[test]
    fn live_uses_instrument_multiplier_and_never_invents_missing_metadata() {
        let now = 1_000;
        assert!(catalog_integrity_is_verified(1, Some([1; 32]), true));
        assert!(!catalog_integrity_is_verified(0, Some([1; 32]), true));
        assert!(!catalog_integrity_is_verified(1, Some([0; 32]), true));
        assert!(!catalog_integrity_is_verified(1, Some([1; 32]), false));
        assert_eq!(
            entry_contract_multiplier(
                Some(100),
                Some(now),
                ContractMetadataSource::IolCatalog,
                1,
                true,
                now,
                60,
            ),
            Some(100)
        );
        assert_eq!(
            entry_contract_multiplier(
                Some(100),
                Some(now - 61),
                ContractMetadataSource::IolCatalog,
                1,
                true,
                now,
                60,
            ),
            None
        );
        assert_eq!(
            entry_contract_multiplier(
                Some(100),
                Some(now + crate::market::MAX_SOURCE_CLOCK_SKEW_SECS + 1),
                ContractMetadataSource::IolCatalog,
                1,
                true,
                now,
                60,
            ),
            None
        );
        assert_eq!(
            entry_contract_multiplier(
                Some(100),
                Some(now),
                ContractMetadataSource::Legacy,
                1,
                true,
                now,
                60,
            ),
            None
        );
        assert_eq!(
            entry_contract_multiplier(None, None, ContractMetadataSource::Legacy, 1, true, now, 60,),
            None
        );
        assert_eq!(
            entry_contract_multiplier(
                None,
                None,
                ContractMetadataSource::Legacy,
                1,
                false,
                now,
                60,
            ),
            Some(1)
        );
    }

    #[test]
    fn environment_suggestion_separates_iol_fees_from_profit_tax() {
        let mut app = TradingApp::new_for_test(replay_config()).unwrap();
        app.cost_calibration = Some(CostCalibration {
            operation_number: "IOL-SECRET-987654".into(),
            operation_amount: 10_000.0,
            commission_percentage: 0.099970,
            vat_percentage: 20.999978,
            other_fees_percentage: 0.049985,
            total_cost_percentage: 0.181445,
            components: Vec::new(),
            observed_at_secs: unix_now(),
            instrument_is_option: true,
            observed_contract_multiplier: None,
        });

        let suggestion = app.environment_suggestion().unwrap();

        assert!(suggestion.contains("OTHER_FEES_PERCENTAGE=0.049985"));
        assert!(suggestion.contains("Costo operativo efectivo: 0.181445%"));
        assert!(suggestion.contains("TAX_PERCENTAGE=35.000000"));
        assert!(suggestion.contains("no surge de los aranceles IOL"));
        assert!(!suggestion.contains("IOL-SECRET-987654"));
        assert!(suggestion.contains("••••7654"));
    }

    #[test]
    fn startup_detects_orders_without_a_terminal_update() {
        let events = vec![
            JournalEvent {
                schema_version: crate::persistence::JOURNAL_SCHEMA_VERSION,
                sequence: 1,
                timestamp_secs: 1,
                operation_id: Some("pending-1".into()),
                previous_hash: String::new(),
                event_hash: String::new(),
                event_hmac: String::new(),
                event: JournalEventKind::OrderSubmitted {
                    symbol: "GAL-C-100".into(),
                    side: OrderSide::Buy,
                    quantity: 1,
                    limit_price: 2.0,
                },
            },
            JournalEvent {
                schema_version: crate::persistence::JOURNAL_SCHEMA_VERSION,
                sequence: 2,
                timestamp_secs: 2,
                operation_id: Some("done-1".into()),
                previous_hash: String::new(),
                event_hash: String::new(),
                event_hmac: String::new(),
                event: JournalEventKind::OrderSubmitted {
                    symbol: "GAL-P-100".into(),
                    side: OrderSide::Buy,
                    quantity: 1,
                    limit_price: 2.0,
                },
            },
            JournalEvent {
                schema_version: crate::persistence::JOURNAL_SCHEMA_VERSION,
                sequence: 3,
                timestamp_secs: 3,
                operation_id: Some("done-1".into()),
                previous_hash: String::new(),
                event_hash: String::new(),
                event_hmac: String::new(),
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
        assert_eq!(unresolved_local_orders(&events).unwrap(), vec!["pending-1"]);
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
        let events = vec![
            JournalEvent {
                schema_version: crate::persistence::JOURNAL_SCHEMA_VERSION,
                sequence: 1,
                timestamp_secs: 1,
                operation_id: Some(request.operation_id.clone()),
                previous_hash: String::new(),
                event_hash: String::new(),
                event_hmac: String::new(),
                event: JournalEventKind::OrderIntentCreated {
                    request: request.clone(),
                },
            },
            JournalEvent {
                schema_version: crate::persistence::JOURNAL_SCHEMA_VERSION,
                sequence: 2,
                timestamp_secs: 2,
                operation_id: Some(request.operation_id.clone()),
                previous_hash: String::new(),
                event_hash: String::new(),
                event_hmac: String::new(),
                event: JournalEventKind::OrderUnknown {
                    request,
                    reason: "timeout".into(),
                },
            },
        ];
        assert_eq!(unresolved_local_orders(&events).unwrap(), vec!["unknown-1"]);
    }

    #[test]
    fn startup_keeps_a_partially_filled_cancelled_order_unresolved() {
        let request = OrderRequest {
            operation_id: "partial-1".into(),
            symbol: "GAL-C-100".into(),
            quantity: 2,
            market_price: 2.0,
            limit_price: 2.1,
            side: OrderSide::Buy,
        };
        let events = vec![
            JournalEvent {
                schema_version: crate::persistence::JOURNAL_SCHEMA_VERSION,
                sequence: 1,
                timestamp_secs: 1,
                operation_id: Some(request.operation_id.clone()),
                previous_hash: String::new(),
                event_hash: String::new(),
                event_hmac: String::new(),
                event: JournalEventKind::OrderIntentCreated {
                    request: request.clone(),
                },
            },
            JournalEvent {
                schema_version: crate::persistence::JOURNAL_SCHEMA_VERSION,
                sequence: 2,
                timestamp_secs: 2,
                operation_id: Some(request.operation_id.clone()),
                previous_hash: String::new(),
                event_hash: String::new(),
                event_hmac: String::new(),
                event: JournalEventKind::OrderUpdated {
                    execution: OrderExecution {
                        operation_id: request.operation_id,
                        status: OrderStatus::Cancelled,
                        filled_quantity: 1,
                        fill_price: Some(2.0),
                        broker_order_id: Some("42".into()),
                        message: None,
                    },
                },
            },
        ];

        assert_eq!(unresolved_local_orders(&events).unwrap(), vec!["partial-1"]);
    }

    #[test]
    fn startup_recovery_rejects_orphan_and_changed_broker_identity() {
        let execution = OrderExecution {
            operation_id: "order-1".into(),
            status: OrderStatus::Pending,
            filled_quantity: 0,
            fill_price: None,
            broker_order_id: Some("42".into()),
            message: None,
        };
        let orphan = JournalEvent {
            schema_version: crate::persistence::JOURNAL_SCHEMA_VERSION,
            sequence: 1,
            timestamp_secs: 1,
            operation_id: Some(execution.operation_id.clone()),
            previous_hash: String::new(),
            event_hash: String::new(),
            event_hmac: String::new(),
            event: JournalEventKind::OrderUpdated {
                execution: execution.clone(),
            },
        };
        assert!(unresolved_local_orders(&[orphan]).is_err());

        let request = OrderRequest {
            operation_id: execution.operation_id.clone(),
            symbol: "GAL-C-100".into(),
            quantity: 1,
            market_price: 2.0,
            limit_price: 2.1,
            side: OrderSide::Buy,
        };
        let terminal = OrderExecution {
            status: OrderStatus::Executed,
            filled_quantity: 1,
            fill_price: Some(2.0),
            broker_order_id: Some("43".into()),
            ..execution.clone()
        };
        let events = vec![
            journal_event(
                1,
                &request.operation_id,
                JournalEventKind::OrderIntentCreated {
                    request: request.clone(),
                },
            ),
            journal_event(
                2,
                &request.operation_id,
                JournalEventKind::OrderAccepted { execution },
            ),
            journal_event(
                3,
                &request.operation_id,
                JournalEventKind::OrderUpdated {
                    execution: terminal,
                },
            ),
        ];
        assert!(unresolved_local_orders(&events).is_err());
    }

    #[test]
    fn startup_recovery_rejects_regressive_fills_and_bad_partial_accounting() {
        let request = OrderRequest {
            operation_id: "partial-invalid".into(),
            symbol: "GAL-P-100".into(),
            quantity: 2,
            market_price: 2.0,
            limit_price: 2.1,
            side: OrderSide::Buy,
        };
        let partial = OrderExecution {
            operation_id: request.operation_id.clone(),
            status: OrderStatus::PartiallyExecuted,
            filled_quantity: 1,
            fill_price: Some(2.0),
            broker_order_id: Some("42".into()),
            message: None,
        };
        let regressed = OrderExecution {
            status: OrderStatus::Cancelled,
            filled_quantity: 0,
            fill_price: None,
            ..partial.clone()
        };
        let regressive_events = vec![
            journal_event(
                1,
                &request.operation_id,
                JournalEventKind::OrderIntentCreated {
                    request: request.clone(),
                },
            ),
            journal_event(
                2,
                &request.operation_id,
                JournalEventKind::OrderAccepted {
                    execution: partial.clone(),
                },
            ),
            journal_event(
                3,
                &request.operation_id,
                JournalEventKind::OrderUpdated {
                    execution: regressed,
                },
            ),
        ];
        assert!(unresolved_local_orders(&regressive_events).is_err());

        let bad_exposure = vec![
            journal_event(
                1,
                &request.operation_id,
                JournalEventKind::OrderIntentCreated {
                    request: request.clone(),
                },
            ),
            journal_event(
                2,
                &request.operation_id,
                JournalEventKind::OrderAccepted {
                    execution: partial.clone(),
                },
            ),
            journal_event(
                3,
                &request.operation_id,
                JournalEventKind::PartialFillExposure {
                    execution: partial,
                    requested_quantity: request.quantity,
                    remaining_quantity: 0,
                },
            ),
        ];
        assert!(unresolved_local_orders(&bad_exposure).is_err());
    }

    #[test]
    fn startup_recovery_resolves_only_a_valid_terminal_for_the_same_intent() {
        let request = OrderRequest {
            operation_id: "done-valid".into(),
            symbol: "GAL-C-100".into(),
            quantity: 1,
            market_price: 2.0,
            limit_price: 2.1,
            side: OrderSide::Buy,
        };
        let terminal = OrderExecution {
            operation_id: request.operation_id.clone(),
            status: OrderStatus::Executed,
            filled_quantity: request.quantity,
            fill_price: Some(2.0),
            broker_order_id: Some("42".into()),
            message: None,
        };
        let events = vec![
            journal_event(
                1,
                &request.operation_id,
                JournalEventKind::OrderIntentCreated {
                    request: request.clone(),
                },
            ),
            journal_event(
                2,
                &request.operation_id,
                JournalEventKind::OrderAccepted {
                    execution: terminal.clone(),
                },
            ),
            journal_event(
                3,
                &request.operation_id,
                JournalEventKind::OrderUpdated {
                    execution: terminal,
                },
            ),
        ];
        assert!(unresolved_local_orders(&events).unwrap().is_empty());
    }

    #[test]
    fn startup_recovery_rejects_changed_intents_and_invalid_unknown_results() {
        let request = OrderRequest {
            operation_id: "identity-1".into(),
            symbol: "GAL-C-100".into(),
            quantity: 1,
            market_price: 2.0,
            limit_price: 2.1,
            side: OrderSide::Buy,
        };
        let mut changed = request.clone();
        changed.symbol = "GAL-P-100".into();

        let duplicate = vec![
            journal_event(
                1,
                &request.operation_id,
                JournalEventKind::OrderIntentCreated {
                    request: request.clone(),
                },
            ),
            journal_event(
                2,
                &request.operation_id,
                JournalEventKind::OrderIntentCreated {
                    request: request.clone(),
                },
            ),
        ];
        assert_eq!(
            unresolved_local_orders(&duplicate).unwrap(),
            vec![request.operation_id.clone()]
        );
        let changed_intent = vec![
            duplicate[0].clone(),
            journal_event(
                2,
                &request.operation_id,
                JournalEventKind::OrderIntentCreated {
                    request: changed.clone(),
                },
            ),
        ];
        assert!(unresolved_local_orders(&changed_intent).is_err());

        let orphan_unknown = journal_event(
            1,
            &request.operation_id,
            JournalEventKind::OrderUnknown {
                request: request.clone(),
                reason: "timeout".into(),
            },
        );
        assert!(unresolved_local_orders(&[orphan_unknown]).is_err());
        let changed_unknown = vec![
            duplicate[0].clone(),
            journal_event(
                2,
                &request.operation_id,
                JournalEventKind::OrderUnknown {
                    request: changed,
                    reason: "timeout".into(),
                },
            ),
        ];
        assert!(unresolved_local_orders(&changed_unknown).is_err());

        let terminal = OrderExecution {
            operation_id: request.operation_id.clone(),
            status: OrderStatus::Rejected,
            filled_quantity: 0,
            fill_price: None,
            broker_order_id: Some("42".into()),
            message: None,
        };
        let unknown_after_terminal = vec![
            duplicate[0].clone(),
            journal_event(
                2,
                &request.operation_id,
                JournalEventKind::OrderUpdated {
                    execution: terminal,
                },
            ),
            journal_event(
                3,
                &request.operation_id,
                JournalEventKind::OrderUnknown {
                    request: request.clone(),
                    reason: "late timeout".into(),
                },
            ),
        ];
        assert!(unresolved_local_orders(&unknown_after_terminal).is_err());
    }

    #[test]
    fn startup_recovery_validates_duplicate_legacy_intents() {
        let legacy = JournalEventKind::OrderSubmitted {
            symbol: "GAL-C-100".into(),
            side: OrderSide::Buy,
            quantity: 1,
            limit_price: 2.0,
        };
        let exact_duplicate = vec![
            journal_event(1, "legacy-1", legacy.clone()),
            journal_event(2, "legacy-1", legacy),
        ];
        assert_eq!(
            unresolved_local_orders(&exact_duplicate).unwrap(),
            vec!["legacy-1"]
        );

        let changed_duplicate = vec![
            exact_duplicate[0].clone(),
            journal_event(
                2,
                "legacy-1",
                JournalEventKind::OrderSubmitted {
                    symbol: "GAL-C-100".into(),
                    side: OrderSide::Buy,
                    quantity: 2,
                    limit_price: 2.0,
                },
            ),
        ];
        assert!(unresolved_local_orders(&changed_duplicate).is_err());

        let mut missing_id = exact_duplicate[0].clone();
        missing_id.operation_id = None;
        assert!(unresolved_local_orders(&[missing_id]).is_err());
    }

    #[test]
    fn startup_recovery_distinguishes_clean_cancel_and_each_partial_mismatch() {
        let request = OrderRequest {
            operation_id: "partial-accounting".into(),
            symbol: "GAL-P-100".into(),
            quantity: 2,
            market_price: 2.0,
            limit_price: 2.1,
            side: OrderSide::Buy,
        };
        let clean_cancel = OrderExecution {
            operation_id: request.operation_id.clone(),
            status: OrderStatus::Cancelled,
            filled_quantity: 0,
            fill_price: None,
            broker_order_id: Some("42".into()),
            message: None,
        };
        let clean_events = vec![
            journal_event(
                1,
                &request.operation_id,
                JournalEventKind::OrderIntentCreated {
                    request: request.clone(),
                },
            ),
            journal_event(
                2,
                &request.operation_id,
                JournalEventKind::OrderUpdated {
                    execution: clean_cancel,
                },
            ),
        ];
        assert!(unresolved_local_orders(&clean_events).unwrap().is_empty());

        let partial = OrderExecution {
            operation_id: request.operation_id.clone(),
            status: OrderStatus::PartiallyExecuted,
            filled_quantity: 1,
            fill_price: Some(2.0),
            broker_order_id: Some("42".into()),
            message: None,
        };
        let prefix = vec![
            clean_events[0].clone(),
            journal_event(
                2,
                &request.operation_id,
                JournalEventKind::OrderUpdated {
                    execution: partial.clone(),
                },
            ),
        ];
        let mut wrong_requested = prefix.clone();
        wrong_requested.push(journal_event(
            3,
            &request.operation_id,
            JournalEventKind::PartialFillExposure {
                execution: partial.clone(),
                requested_quantity: 3,
                remaining_quantity: 2,
            },
        ));
        assert!(unresolved_local_orders(&wrong_requested).is_err());

        let mut wrong_remaining = prefix.clone();
        wrong_remaining.push(journal_event(
            3,
            &request.operation_id,
            JournalEventKind::PartialFillExposure {
                execution: partial.clone(),
                requested_quantity: request.quantity,
                remaining_quantity: 0,
            },
        ));
        assert!(unresolved_local_orders(&wrong_remaining).is_err());

        let mut wrong_latest = prefix;
        wrong_latest.push(journal_event(
            3,
            &request.operation_id,
            JournalEventKind::PartialFillExposure {
                execution: OrderExecution {
                    status: OrderStatus::Cancelled,
                    ..partial
                },
                requested_quantity: request.quantity,
                remaining_quantity: 1,
            },
        ));
        assert!(unresolved_local_orders(&wrong_latest).is_err());
    }

    #[test]
    fn recovery_position_replay_is_exact_idempotent_and_rejects_conflicting_exposure() {
        let position = recovery_position("position-1", PositionKind::Call);
        let opened = journal_event(
            1,
            &position.operation_id,
            JournalEventKind::PositionOpened {
                position: position.clone(),
            },
        );
        let mut engine = TradingEngine::new();
        let mut portfolio = Portfolio::default();
        let mut risk = recovery_risk();
        let mut learning = LearningState::new("strategy".into());
        let mut performance = Vec::new();

        apply_recovery_event(
            &mut engine,
            &mut portfolio,
            &mut risk,
            &mut learning,
            &mut performance,
            &opened,
        )
        .unwrap();
        assert_eq!(engine.position.as_ref(), Some(&position));
        assert_eq!(portfolio.position(&position.operation_id), Some(&position));
        apply_recovery_event(
            &mut engine,
            &mut portfolio,
            &mut risk,
            &mut learning,
            &mut performance,
            &opened,
        )
        .unwrap();
        assert_eq!(portfolio.metrics().open_positions, 1);

        let conflicting = recovery_position("position-2", PositionKind::Put);
        let conflicting_event = journal_event(
            2,
            &conflicting.operation_id,
            JournalEventKind::PositionOpened {
                position: conflicting.clone(),
            },
        );
        assert!(apply_recovery_event(
            &mut engine,
            &mut portfolio,
            &mut risk,
            &mut learning,
            &mut performance,
            &conflicting_event,
        )
        .is_err());

        assert_eq!(engine.position.as_ref(), Some(&position));
        assert_eq!(portfolio.metrics().open_positions, 1);
    }

    #[test]
    fn recovery_position_replay_rejects_invalid_or_preexisting_inconsistent_state() {
        let mut invalid = recovery_position("invalid", PositionKind::Call);
        invalid.entry_price = 0.0;
        let invalid_event = journal_event(
            1,
            &invalid.operation_id,
            JournalEventKind::PositionOpened {
                position: invalid.clone(),
            },
        );
        let mut engine = TradingEngine::new();
        let mut portfolio = Portfolio::default();
        let mut risk = recovery_risk();
        let mut learning = LearningState::new("strategy".into());
        let mut performance = Vec::new();
        assert!(apply_recovery_event(
            &mut engine,
            &mut portfolio,
            &mut risk,
            &mut learning,
            &mut performance,
            &invalid_event,
        )
        .is_err());

        assert!(engine.position.is_none());
        assert_eq!(portfolio.metrics().open_positions, 0);

        let position = recovery_position("orphan", PositionKind::Call);
        assert!(engine.open_position(position));
        let diagnostic = journal_event(
            2,
            "",
            JournalEventKind::Recovery {
                message: "diagnóstico".into(),
            },
        );
        assert!(apply_recovery_event(
            &mut engine,
            &mut portfolio,
            &mut risk,
            &mut learning,
            &mut performance,
            &diagnostic,
        )
        .is_err());

        let first = recovery_position("first", PositionKind::Call);
        let extra = recovery_position("extra", PositionKind::Put);
        let mut engine = TradingEngine::new();
        assert!(engine.open_position(first.clone()));
        let mut portfolio = Portfolio::default();
        assert!(portfolio.open(first));
        assert!(portfolio.open(extra));
        assert!(apply_recovery_event(
            &mut engine,
            &mut portfolio,
            &mut risk,
            &mut learning,
            &mut performance,
            &diagnostic,
        )
        .is_err());
    }

    #[test]
    fn recovery_close_replay_updates_risk_learning_and_performance_once() {
        let position = recovery_position("closed-1", PositionKind::Put);
        let opened = journal_event(
            1,
            &position.operation_id,
            JournalEventKind::PositionOpened {
                position: position.clone(),
            },
        );
        let trade = ValidationTrade {
            kind: position.kind,
            net_pnl: 125.0,
            stressed_net_pnl: 100.0,
            closed_at_secs: 2,
            context: ValidationContext {
                trade_id: "trade-1".into(),
                source: EvidenceSource::Shadow,
                ..ValidationContext::default()
            },
        };
        let closed = journal_event(
            2,
            &position.operation_id,
            JournalEventKind::PositionClosed {
                operation_id: position.operation_id.clone(),
                exit_price: 3.0,
                net_pnl: trade.net_pnl,
                reason: ExitReason::ProfitTarget,
                stage: LiveStage::Learning,
                validation_trade: Some(trade.clone()),
            },
        );
        let mut engine = TradingEngine::new();
        let mut portfolio = Portfolio::default();
        let mut risk = recovery_risk();
        let mut learning = LearningState::new("strategy".into());
        let mut performance = Vec::new();
        apply_recovery_event(
            &mut engine,
            &mut portfolio,
            &mut risk,
            &mut learning,
            &mut performance,
            &opened,
        )
        .unwrap();
        apply_recovery_event(
            &mut engine,
            &mut portfolio,
            &mut risk,
            &mut learning,
            &mut performance,
            &closed,
        )
        .unwrap();
        assert!(engine.position.is_none());
        assert_eq!(engine.last_exit_reason, Some(ExitReason::ProfitTarget));
        assert_eq!(portfolio.metrics().realized_pnl, trade.net_pnl);
        assert_eq!(risk.state.realized_pnl, trade.net_pnl);
        assert_eq!(learning.trades, vec![trade.clone()]);
        assert!(performance.is_empty());

        apply_recovery_event(
            &mut engine,
            &mut portfolio,
            &mut risk,
            &mut learning,
            &mut performance,
            &closed,
        )
        .unwrap();
        assert_eq!(risk.state.trades_today, 1);
        assert_eq!(learning.trades.len(), 1);

        let canary_close = journal_event(
            3,
            &position.operation_id,
            JournalEventKind::PositionClosed {
                operation_id: position.operation_id.clone(),
                exit_price: 3.0,
                net_pnl: trade.net_pnl,
                reason: ExitReason::ProfitTarget,
                stage: LiveStage::Canary,
                validation_trade: Some(trade.clone()),
            },
        );
        apply_recovery_event(
            &mut engine,
            &mut portfolio,
            &mut risk,
            &mut learning,
            &mut performance,
            &canary_close,
        )
        .unwrap();
        apply_recovery_event(
            &mut engine,
            &mut portfolio,
            &mut risk,
            &mut learning,
            &mut performance,
            &canary_close,
        )
        .unwrap();
        assert_eq!(performance, vec![trade.clone()]);

        let second_trade = ValidationTrade {
            context: ValidationContext {
                trade_id: "trade-2".into(),
                source: EvidenceSource::Canary,
                ..ValidationContext::default()
            },
            ..performance[0].clone()
        };
        let distinct_canary_close = journal_event(
            4,
            "closed-2",
            JournalEventKind::PositionClosed {
                operation_id: "closed-2".into(),
                exit_price: 3.0,
                net_pnl: second_trade.net_pnl,
                reason: ExitReason::ProfitTarget,
                stage: LiveStage::Canary,
                validation_trade: Some(second_trade.clone()),
            },
        );
        apply_recovery_event(
            &mut engine,
            &mut portfolio,
            &mut risk,
            &mut learning,
            &mut performance,
            &distinct_canary_close,
        )
        .unwrap();
        assert_eq!(performance, vec![trade, second_trade]);
    }

    #[test]
    fn recovery_close_replay_cannot_close_a_different_active_direction() {
        let position = recovery_position("active-call", PositionKind::Call);
        let mut engine = TradingEngine::new();
        assert!(engine.open_position(position.clone()));
        let mut portfolio = Portfolio::default();
        assert!(portfolio.open(position));
        let mut risk = recovery_risk();
        let mut learning = LearningState::new("strategy".into());
        let mut performance = Vec::new();
        let wrong_close = journal_event(
            1,
            "other-put",
            JournalEventKind::PositionClosed {
                operation_id: "other-put".into(),
                exit_price: 3.0,
                net_pnl: 1.0,
                reason: ExitReason::Manual,
                stage: LiveStage::Learning,
                validation_trade: None,
            },
        );
        assert!(apply_recovery_event(
            &mut engine,
            &mut portfolio,
            &mut risk,
            &mut learning,
            &mut performance,
            &wrong_close,
        )
        .is_err());
        assert_eq!(engine.position.unwrap().direction(), Direction::Up);
    }

    #[test]
    fn recovery_never_resumes_an_unreconciled_operational_halt() {
        let active = journal_event(1, "", JournalEventKind::KillSwitch { active: true });
        let inactive = journal_event(2, "", JournalEventKind::KillSwitch { active: false });
        let mut engine = TradingEngine::new();
        let mut portfolio = Portfolio::default();
        let mut risk = recovery_risk();
        let mut learning = LearningState::new("strategy".into());
        let mut performance = Vec::new();
        apply_recovery_event(
            &mut engine,
            &mut portfolio,
            &mut risk,
            &mut learning,
            &mut performance,
            &active,
        )
        .unwrap();
        assert_eq!(engine.state, crate::trading::TradingState::Halted);
        apply_recovery_event(
            &mut engine,
            &mut portfolio,
            &mut risk,
            &mut learning,
            &mut performance,
            &inactive,
        )
        .unwrap();
        assert_eq!(engine.state, crate::trading::TradingState::Idle);
        assert!(!risk.state.kill_switch);

        risk.engage_operational_halt("conciliación pendiente");
        engine.halt();
        apply_recovery_event(
            &mut engine,
            &mut portfolio,
            &mut risk,
            &mut learning,
            &mut performance,
            &inactive,
        )
        .unwrap();
        assert!(risk.state.kill_switch);
        assert_eq!(engine.state, crate::trading::TradingState::Halted);
    }
}
