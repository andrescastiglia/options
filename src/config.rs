use std::{env, num::ParseFloatError, num::ParseIntError, path::PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;

pub const LIVE_CONFIRMATION: &str = "I_UNDERSTAND_THIS_SENDS_REAL_ORDERS";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Mode {
    Readonly,
    Live,
}

impl Mode {
    fn parse(value: &str) -> Result<Self, ConfigError> {
        match value.to_ascii_lowercase().as_str() {
            "readonly" => Ok(Self::Readonly),
            "live" => Ok(Self::Live),
            other => Err(ConfigError::InvalidValue {
                name: "MODE",
                value: other.to_string(),
            }),
        }
    }

    pub fn uses_iol_market_data(self) -> bool {
        true
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    pub mode: Mode,
    pub ticker: String,
    pub check_interval_secs: u64,
    pub price_history_minutes: u64,
    pub min_samples_for_trend: usize,
    pub trend_change_samples: usize,
    pub trend_deadband_percentage: f64,
    pub min_trend_slope_percent_per_minute: f64,
    pub min_trend_r_squared: f64,
    pub min_trend_move_volatility_ratio: f64,
    pub reversal_cooldown_secs: u64,
    pub commission_percentage: f64,
    pub vat_percentage: f64,
    pub other_fees_percentage: f64,
    pub tax_percentage: f64,
    pub min_profit_multiplier: f64,
    pub option_expiry_days: u32,
    pub max_position_size: u32,
    pub position_timeout_mins: u64,
    pub max_concurrent_requests: usize,
    pub cache_ttl_secs: u64,
    pub log_level: String,
    pub tui_enabled: bool,
    pub recover_state: bool,
    pub data_dir: PathBuf,
    pub replay_path: Option<PathBuf>,
    pub capture_market_data: bool,
    pub data_dir_max_bytes: u64,
    pub data_disk_min_free_bytes: u64,
    pub market_capture_retention_days: u64,
    pub holidays_api_base_url: String,
    pub market_sessions_path: Option<PathBuf>,
    pub entry_delay_after_open_mins: u32,
    pub weekend_risk_enabled: bool,
    pub pre_break_last_entry_minute: u16,
    pub pre_break_force_exit_minute: u16,
    pub expiry_day_force_exit_minute: u16,
    pub lunch_slowdown_enabled: bool,
    pub lunch_slowdown_start_minute: u16,
    pub lunch_slowdown_end_minute: u16,
    pub lunch_position_factor: f64,
    pub lunch_max_spread_factor: f64,
    pub lunch_signal_threshold_bonus: f64,
    pub post_lunch_confirmation_mins: u32,
    pub lunch_liquidity_window_mins: u32,
    pub lunch_min_quote_updates: usize,
    pub connection_retry_attempts: u32,
    pub connection_retry_delay_secs: u64,
    pub max_investment_amount: f64,
    pub max_loss_per_trade: f64,
    pub max_daily_loss: f64,
    pub max_trades_per_day: u32,
    pub stop_loss_percentage: f64,
    pub contract_multiplier: u32,
    pub contract_multiplier_confirmed: bool,
    pub readonly_slippage_bps: f64,
    pub max_market_data_age_secs: u64,
    pub max_option_spread_percentage: f64,
    pub min_option_volume: u64,
    pub min_option_chain_acceptance_percentage: f64,
    pub min_option_chain_contracts_per_side: usize,
    pub option_target_expiry_days: u32,
    pub option_max_expiry_days: u32,
    pub max_option_moneyness_distance_percentage: f64,
    pub min_reward_risk_ratio: f64,
    pub learning_slippage_bps: f64,
    pub vix_quote_url: Option<String>,
    pub vix_refresh_secs: u64,
    pub vix_max_age_secs: u64,
    pub vix_previous_close_max_age_secs: u64,
    pub vix_elevated_level: f64,
    pub vix_spike_change_percentage: f64,
    pub vix_elevated_position_factor: f64,
    pub vix_spike_threshold_bonus: f64,
    pub live_learning_min_trades: u64,
    pub live_learning_min_call_trades: u64,
    pub live_learning_min_put_trades: u64,
    pub live_learning_min_sessions: usize,
    pub live_learning_min_profit_factor: f64,
    pub live_regression_window_trades: usize,
    pub live_max_consecutive_losses: u32,
    pub canary_min_trades: u64,
    pub canary_min_call_trades: u64,
    pub canary_min_put_trades: u64,
    pub canary_min_sessions: usize,
    pub canary_max_position_size: u32,
    pub canary_max_investment_amount: f64,
    pub canary_max_loss_per_trade: f64,
    pub canary_max_daily_loss: f64,
    pub canary_max_trades_per_day: u32,
    pub iol_base_url: String,
    pub time_reference_url: Option<String>,
    pub time_reference_refresh_secs: u64,
    pub time_reference_max_skew_secs: u64,
    pub iol_websocket_enabled: bool,
    pub iol_websocket_url: String,
    pub iol_order_path: Option<String>,
    pub order_tracking_timeout_secs: u64,
    pub order_status_poll_interval_millis: u64,
    pub order_cancel_timeout_secs: u64,
    pub dynamic_limit_enabled: bool,
    pub dynamic_limit_steps: u32,
    pub dynamic_limit_frame_wait_secs: u64,
    pub dynamic_limit_queue_ahead_factor: f64,
    pub dynamic_limit_adverse_selection_bps: f64,
    pub option_analytics_enabled: bool,
    pub option_risk_free_rate: f64,
    pub option_dividend_yield: f64,
    pub option_market_inputs_observed_at_secs: Option<i64>,
    pub option_market_inputs_max_age_secs: u64,
    pub option_risk_free_source: String,
    pub option_dividend_source: String,
    pub option_binomial_steps: u32,
    pub option_min_abs_delta: f64,
    pub option_max_abs_delta: f64,
    pub option_min_implied_volatility: f64,
    pub option_max_implied_volatility: f64,
    pub option_max_extrinsic_percentage: f64,
    pub iv_rank_filter_enabled: bool,
    pub iv_rank_window_sessions: usize,
    pub iv_rank_min_sessions: usize,
    pub iv_rank_min: f64,
    pub iv_rank_max: f64,
    pub adaptive_entry_filter_enabled: bool,
    pub max_friction_stop_ratio: f64,
    pub volatility_normalized_signals_enabled: bool,
    pub target_underlying_volatility_percentage: f64,
    pub meta_filter_min_examples: usize,
    pub meta_filter_min_train_examples: usize,
    pub meta_filter_min_accepted_holdout: usize,
    pub meta_filter_min_coverage: f64,
    pub meta_filter_max_brier_score: f64,
    pub meta_filter_min_positive_fold_ratio: f64,
    pub meta_filter_max_concentration: f64,
    pub nonlinear_meta_filter_enabled: bool,
    pub tree_meta_filter_enabled: bool,
    pub tree_meta_filter_min_improvement: f64,
    pub experiment_runner_enabled: bool,
    pub vertical_spread_research_enabled: bool,
    pub vertical_atomic_execution_verified: bool,
    pub live_readiness_path: Option<PathBuf>,
    pub live_authorization_path: Option<PathBuf>,
    pub master_key_path: Option<PathBuf>,
    pub live_confirmed: bool,
}

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("valor invalido para {name}: {value}")]
    InvalidValue { name: &'static str, value: String },
    #[error("variable requerida ausente: {0}")]
    MissingSecret(&'static str),
    #[error("error numerico en {name}: {source}")]
    Number {
        name: &'static str,
        #[source]
        source: NumericError,
    },
}

#[derive(Debug, Error)]
pub enum NumericError {
    #[error("entero invalido")]
    Int(#[from] ParseIntError),
    #[error("decimal invalido")]
    Float(#[from] ParseFloatError),
}

impl Config {
    pub fn from_env() -> Result<Self, ConfigError> {
        let mode = Mode::parse(&env_or("MODE", "readonly"))?;
        let config = Self {
            mode,
            ticker: env_or("TICKER", "GGAL"),
            check_interval_secs: parse_u64("CHECK_INTERVAL_SECS", 1)?,
            price_history_minutes: parse_u64("PRICE_HISTORY_MINUTES", 30)?,
            min_samples_for_trend: parse_usize("MIN_SAMPLES_FOR_TREND", 5)?,
            trend_change_samples: parse_usize("TREND_CHANGE_SAMPLES", 3)?,
            trend_deadband_percentage: parse_f64("TREND_DEADBAND_PERCENTAGE", 0.10)?,
            min_trend_slope_percent_per_minute: parse_f64(
                "MIN_TREND_SLOPE_PERCENT_PER_MINUTE",
                0.02,
            )?,
            min_trend_r_squared: parse_f64("MIN_TREND_R_SQUARED", 0.60)?,
            min_trend_move_volatility_ratio: parse_f64("MIN_TREND_MOVE_VOLATILITY_RATIO", 1.0)?,
            reversal_cooldown_secs: parse_u64("REVERSAL_COOLDOWN_SECS", 300)?,
            commission_percentage: parse_f64("COMMISSION_PERCENTAGE", 0.19)?,
            vat_percentage: parse_f64("VAT_PERCENTAGE", 21.0)?,
            other_fees_percentage: parse_f64("OTHER_FEES_PERCENTAGE", 0.0)?,
            tax_percentage: parse_f64("TAX_PERCENTAGE", 35.0)?,
            min_profit_multiplier: parse_f64("MIN_PROFIT_MULTIPLIER", 2.0)?,
            option_expiry_days: parse_u32("OPTION_EXPIRY_DAYS", 1)?,
            max_position_size: parse_u32("MAX_POSITION_SIZE", 5)?,
            position_timeout_mins: parse_u64("POSITION_TIMEOUT_MINS", 60)?,
            max_concurrent_requests: parse_usize("MAX_CONCURRENT_REQUESTS", 10)?,
            cache_ttl_secs: parse_u64("CACHE_TTL_SECS", 60)?,
            log_level: env_or("LOG_LEVEL", "info"),
            tui_enabled: parse_bool("TUI_ENABLED", true)?,
            recover_state: parse_bool("RECOVER_STATE", true)?,
            data_dir: PathBuf::from(env_or("DATA_DIR", "data")),
            replay_path: optional_path_env("REPLAY_PATH"),
            capture_market_data: parse_bool("CAPTURE_MARKET_DATA", true)?,
            data_dir_max_bytes: parse_u64("DATA_DIR_MAX_BYTES", 2_147_483_648)?,
            data_disk_min_free_bytes: parse_u64("DATA_DISK_MIN_FREE_BYTES", 536_870_912)?,
            market_capture_retention_days: parse_u64("MARKET_CAPTURE_RETENTION_DAYS", 30)?,
            holidays_api_base_url: env_or(
                "HOLIDAYS_API_BASE_URL",
                "https://api.argentinadatos.com/v1/feriados",
            ),
            market_sessions_path: optional_path_env("MARKET_SESSIONS_PATH"),
            entry_delay_after_open_mins: parse_u32("ENTRY_DELAY_AFTER_OPEN_MINS", 45)?,
            weekend_risk_enabled: parse_bool("WEEKEND_RISK_ENABLED", true)?,
            pre_break_last_entry_minute: parse_market_time("PRE_BREAK_LAST_ENTRY_TIME", "15:00")?,
            pre_break_force_exit_minute: parse_market_time("PRE_BREAK_FORCE_EXIT_TIME", "16:30")?,
            expiry_day_force_exit_minute: parse_market_time("EXPIRY_DAY_FORCE_EXIT_TIME", "15:15")?,
            lunch_slowdown_enabled: parse_bool("LUNCH_SLOWDOWN_ENABLED", true)?,
            lunch_slowdown_start_minute: parse_market_time("LUNCH_SLOWDOWN_START_TIME", "12:30")?,
            lunch_slowdown_end_minute: parse_market_time("LUNCH_SLOWDOWN_END_TIME", "14:00")?,
            lunch_position_factor: parse_f64("LUNCH_POSITION_FACTOR", 0.5)?,
            lunch_max_spread_factor: parse_f64("LUNCH_MAX_SPREAD_FACTOR", 0.75)?,
            lunch_signal_threshold_bonus: parse_f64("LUNCH_SIGNAL_THRESHOLD_BONUS", 0.05)?,
            post_lunch_confirmation_mins: parse_u32("POST_LUNCH_CONFIRMATION_MINS", 5)?,
            lunch_liquidity_window_mins: parse_u32("LUNCH_LIQUIDITY_WINDOW_MINS", 5)?,
            lunch_min_quote_updates: parse_usize("LUNCH_MIN_QUOTE_UPDATES", 3)?,
            connection_retry_attempts: parse_u32("CONNECTION_RETRY_ATTEMPTS", 3)?,
            connection_retry_delay_secs: parse_u64("CONNECTION_RETRY_DELAY_SECS", 5)?,
            max_investment_amount: parse_f64_with_legacy(
                "MAX_INVESTMENT_AMOUNT",
                "MAX_NOTIONAL",
                100_000.0,
            )?,
            max_loss_per_trade: parse_f64("MAX_LOSS_PER_TRADE", 5_000.0)?,
            max_daily_loss: parse_f64("MAX_DAILY_LOSS", 10_000.0)?,
            max_trades_per_day: parse_u32("MAX_TRADES_PER_DAY", 20)?,
            stop_loss_percentage: parse_f64("STOP_LOSS_PERCENTAGE", 15.0)?,
            contract_multiplier: parse_u32("CONTRACT_MULTIPLIER", 1)?,
            contract_multiplier_confirmed: parse_bool("CONTRACT_MULTIPLIER_CONFIRMED", false)?,
            readonly_slippage_bps: parse_f64("READONLY_SLIPPAGE_BPS", 5.0)?,
            max_market_data_age_secs: parse_u64("MAX_MARKET_DATA_AGE_SECS", 15)?,
            max_option_spread_percentage: parse_f64("MAX_OPTION_SPREAD_PERCENTAGE", 3.0)?,
            min_option_volume: parse_u64("MIN_OPTION_VOLUME", 10)?,
            min_option_chain_acceptance_percentage: parse_f64(
                "MIN_OPTION_CHAIN_ACCEPTANCE_PERCENTAGE",
                80.0,
            )?,
            min_option_chain_contracts_per_side: parse_usize(
                "MIN_OPTION_CHAIN_CONTRACTS_PER_SIDE",
                1,
            )?,
            option_target_expiry_days: parse_u32("OPTION_TARGET_EXPIRY_DAYS", 21)?,
            option_max_expiry_days: parse_u32("OPTION_MAX_EXPIRY_DAYS", 45)?,
            max_option_moneyness_distance_percentage: parse_f64(
                "MAX_OPTION_MONEYNESS_DISTANCE_PERCENTAGE",
                10.0,
            )?,
            min_reward_risk_ratio: parse_f64("MIN_REWARD_RISK_RATIO", 1.25)?,
            learning_slippage_bps: parse_f64("LEARNING_SLIPPAGE_BPS", 25.0)?,
            vix_quote_url: optional_string_env("VIX_QUOTE_URL"),
            vix_refresh_secs: parse_u64("VIX_REFRESH_SECS", 60)?,
            vix_max_age_secs: parse_u64("VIX_MAX_AGE_SECS", 900)?,
            vix_previous_close_max_age_secs: parse_u64("VIX_PREVIOUS_CLOSE_MAX_AGE_SECS", 345_600)?,
            vix_elevated_level: parse_f64("VIX_ELEVATED_LEVEL", 25.0)?,
            vix_spike_change_percentage: parse_f64("VIX_SPIKE_CHANGE_PERCENTAGE", 10.0)?,
            vix_elevated_position_factor: parse_f64("VIX_ELEVATED_POSITION_FACTOR", 0.5)?,
            vix_spike_threshold_bonus: parse_f64("VIX_SPIKE_THRESHOLD_BONUS", 0.10)?,
            live_learning_min_trades: parse_u64("LIVE_LEARNING_MIN_TRADES", 200)?,
            live_learning_min_call_trades: parse_u64("LIVE_LEARNING_MIN_CALL_TRADES", 75)?,
            live_learning_min_put_trades: parse_u64("LIVE_LEARNING_MIN_PUT_TRADES", 75)?,
            live_learning_min_sessions: parse_usize("LIVE_LEARNING_MIN_SESSIONS", 20)?,
            live_learning_min_profit_factor: parse_f64("LIVE_LEARNING_MIN_PROFIT_FACTOR", 1.25)?,
            live_regression_window_trades: parse_usize("LIVE_REGRESSION_WINDOW_TRADES", 30)?,
            live_max_consecutive_losses: parse_u32("LIVE_MAX_CONSECUTIVE_LOSSES", 3)?,
            canary_min_trades: parse_u64("CANARY_MIN_TRADES", 20)?,
            canary_min_call_trades: parse_u64("CANARY_MIN_CALL_TRADES", 5)?,
            canary_min_put_trades: parse_u64("CANARY_MIN_PUT_TRADES", 5)?,
            canary_min_sessions: parse_usize("CANARY_MIN_SESSIONS", 5)?,
            canary_max_position_size: parse_u32("CANARY_MAX_POSITION_SIZE", 1)?,
            canary_max_investment_amount: parse_f64("CANARY_MAX_INVESTMENT_AMOUNT", 10_000.0)?,
            canary_max_loss_per_trade: parse_f64("CANARY_MAX_LOSS_PER_TRADE", 500.0)?,
            canary_max_daily_loss: parse_f64("CANARY_MAX_DAILY_LOSS", 1_000.0)?,
            canary_max_trades_per_day: parse_u32("CANARY_MAX_TRADES_PER_DAY", 5)?,
            iol_base_url: env_or("IOL_BASE_URL", "https://api.invertironline.com"),
            time_reference_url: optional_string_env("TIME_REFERENCE_URL"),
            time_reference_refresh_secs: parse_u64("TIME_REFERENCE_REFRESH_SECS", 300)?,
            time_reference_max_skew_secs: parse_u64("TIME_REFERENCE_MAX_SKEW_SECS", 30)?,
            iol_websocket_enabled: parse_bool("IOL_WEBSOCKET_ENABLED", false)?,
            iol_websocket_url: env_or(
                "IOL_WEBSOCKET_URL",
                "wss://websocket-movements.invertironline.com/",
            ),
            iol_order_path: optional_string_env("IOL_ORDER_PATH"),
            order_tracking_timeout_secs: parse_u64("ORDER_TRACKING_TIMEOUT_SECS", 30)?,
            order_status_poll_interval_millis: parse_u64(
                "ORDER_STATUS_POLL_INTERVAL_MILLIS",
                1_000,
            )?,
            order_cancel_timeout_secs: parse_u64("ORDER_CANCEL_TIMEOUT_SECS", 15)?,
            dynamic_limit_enabled: parse_bool("DYNAMIC_LIMIT_ENABLED", false)?,
            dynamic_limit_steps: parse_u32("DYNAMIC_LIMIT_STEPS", 4)?,
            dynamic_limit_frame_wait_secs: parse_u64("DYNAMIC_LIMIT_FRAME_WAIT_SECS", 2)?,
            dynamic_limit_queue_ahead_factor: parse_f64("DYNAMIC_LIMIT_QUEUE_AHEAD_FACTOR", 1.0)?,
            dynamic_limit_adverse_selection_bps: parse_f64(
                "DYNAMIC_LIMIT_ADVERSE_SELECTION_BPS",
                10.0,
            )?,
            option_analytics_enabled: parse_bool("OPTION_ANALYTICS_ENABLED", false)?,
            option_risk_free_rate: parse_f64("OPTION_RISK_FREE_RATE", 0.35)?,
            option_dividend_yield: parse_f64("OPTION_DIVIDEND_YIELD", 0.0)?,
            option_market_inputs_observed_at_secs: parse_optional_i64(
                "OPTION_MARKET_INPUTS_OBSERVED_AT_SECS",
            )?,
            option_market_inputs_max_age_secs: parse_u64(
                "OPTION_MARKET_INPUTS_MAX_AGE_SECS",
                86_400,
            )?,
            option_risk_free_source: env_or("OPTION_RISK_FREE_SOURCE", "manual_env"),
            option_dividend_source: env_or("OPTION_DIVIDEND_SOURCE", "manual_env"),
            option_binomial_steps: parse_u32("OPTION_BINOMIAL_STEPS", 150)?,
            option_min_abs_delta: parse_f64("OPTION_MIN_ABS_DELTA", 0.15)?,
            option_max_abs_delta: parse_f64("OPTION_MAX_ABS_DELTA", 0.85)?,
            option_min_implied_volatility: parse_f64("OPTION_MIN_IMPLIED_VOLATILITY", 0.01)?,
            option_max_implied_volatility: parse_f64("OPTION_MAX_IMPLIED_VOLATILITY", 3.0)?,
            option_max_extrinsic_percentage: parse_f64("OPTION_MAX_EXTRINSIC_PERCENTAGE", 100.0)?,
            iv_rank_filter_enabled: parse_bool("IV_RANK_FILTER_ENABLED", false)?,
            iv_rank_window_sessions: parse_usize("IV_RANK_WINDOW_SESSIONS", 252)?,
            iv_rank_min_sessions: parse_usize("IV_RANK_MIN_SESSIONS", 60)?,
            iv_rank_min: parse_f64("IV_RANK_MIN", 0.0)?,
            iv_rank_max: parse_f64("IV_RANK_MAX", 100.0)?,
            adaptive_entry_filter_enabled: parse_bool("ADAPTIVE_ENTRY_FILTER_ENABLED", false)?,
            max_friction_stop_ratio: parse_f64("MAX_FRICTION_STOP_RATIO", 0.25)?,
            volatility_normalized_signals_enabled: parse_bool(
                "VOLATILITY_NORMALIZED_SIGNALS_ENABLED",
                false,
            )?,
            target_underlying_volatility_percentage: parse_f64(
                "TARGET_UNDERLYING_VOLATILITY_PERCENTAGE",
                1.0,
            )?,
            meta_filter_min_examples: parse_usize("META_FILTER_MIN_EXAMPLES", 100)?,
            meta_filter_min_train_examples: parse_usize("META_FILTER_MIN_TRAIN_EXAMPLES", 60)?,
            meta_filter_min_accepted_holdout: parse_usize("META_FILTER_MIN_ACCEPTED_HOLDOUT", 20)?,
            meta_filter_min_coverage: parse_f64("META_FILTER_MIN_COVERAGE", 0.15)?,
            meta_filter_max_brier_score: parse_f64("META_FILTER_MAX_BRIER_SCORE", 0.25)?,
            meta_filter_min_positive_fold_ratio: parse_f64(
                "META_FILTER_MIN_POSITIVE_FOLD_RATIO",
                0.67,
            )?,
            meta_filter_max_concentration: parse_f64("META_FILTER_MAX_CONCENTRATION", 0.85)?,
            nonlinear_meta_filter_enabled: parse_bool("NONLINEAR_META_FILTER_ENABLED", false)?,
            tree_meta_filter_enabled: parse_bool("TREE_META_FILTER_ENABLED", false)?,
            tree_meta_filter_min_improvement: parse_f64("TREE_META_FILTER_MIN_IMPROVEMENT", 0.05)?,
            experiment_runner_enabled: parse_bool("EXPERIMENT_RUNNER_ENABLED", false)?,
            vertical_spread_research_enabled: parse_bool(
                "VERTICAL_SPREAD_RESEARCH_ENABLED",
                false,
            )?,
            vertical_atomic_execution_verified: parse_bool(
                "VERTICAL_ATOMIC_EXECUTION_VERIFIED",
                false,
            )?,
            live_readiness_path: optional_path_env("LIVE_READINESS_PATH"),
            live_authorization_path: optional_path_env("LIVE_AUTHORIZATION_PATH"),
            master_key_path: optional_path_env("OPTIONS_MASTER_KEY_PATH"),
            live_confirmed: env::var("LIVE_TRADING_CONFIRMATION")
                .is_ok_and(|value| confirmation_is_exact(&value)),
        };
        config.validate()?;
        if config.replay_path.is_none()
            && (!required_secret_is_present("IOL_USERNAME")
                || !required_secret_is_present("IOL_PASSWORD"))
        {
            return Err(ConfigError::MissingSecret("IOL_USERNAME/IOL_PASSWORD"));
        }
        Ok(config)
    }

    pub fn validate(&self) -> Result<(), ConfigError> {
        bounded("CHECK_INTERVAL_SECS", self.check_interval_secs, 1, 60)?;
        bounded(
            "CONNECTION_RETRY_ATTEMPTS",
            self.connection_retry_attempts,
            1,
            20,
        )?;
        bounded(
            "CONNECTION_RETRY_DELAY_SECS",
            self.connection_retry_delay_secs,
            1,
            300,
        )?;
        bounded("PRICE_HISTORY_MINUTES", self.price_history_minutes, 1, 120)?;
        bounded(
            "ENTRY_DELAY_AFTER_OPEN_MINS",
            self.entry_delay_after_open_mins,
            0,
            390,
        )?;
        if self.pre_break_last_entry_minute < 630
            || self.pre_break_force_exit_minute <= self.pre_break_last_entry_minute
            || self.pre_break_force_exit_minute >= 1_020
            || self.expiry_day_force_exit_minute < 630
            || self.expiry_day_force_exit_minute >= 930
        {
            return invalid(
                "WEEKEND_RISK_TIMES",
                "se requiere 10:30 <= corte de entradas < cierre forzoso < 17:00 y cierre de vencimiento < 15:30",
            );
        }
        if self.lunch_slowdown_start_minute < 630
            || self.lunch_slowdown_end_minute <= self.lunch_slowdown_start_minute
            || u32::from(self.lunch_slowdown_end_minute)
                .saturating_add(self.post_lunch_confirmation_mins)
                > 1_020
        {
            return invalid(
                "LUNCH_SLOWDOWN_TIMES",
                "la ventana debe estar dentro de la rueda y dejar terminar la reconfirmación antes de las 17:00",
            );
        }
        bounded_float(
            "LUNCH_POSITION_FACTOR",
            self.lunch_position_factor,
            0.01,
            1.0,
        )?;
        bounded(
            "META_FILTER_MIN_EXAMPLES",
            self.meta_filter_min_examples,
            30,
            100_000,
        )?;
        bounded(
            "META_FILTER_MIN_TRAIN_EXAMPLES",
            self.meta_filter_min_train_examples,
            20,
            100_000,
        )?;
        bounded(
            "META_FILTER_MIN_ACCEPTED_HOLDOUT",
            self.meta_filter_min_accepted_holdout,
            5,
            100_000,
        )?;
        for (name, value) in [
            ("META_FILTER_MIN_COVERAGE", self.meta_filter_min_coverage),
            (
                "META_FILTER_MAX_BRIER_SCORE",
                self.meta_filter_max_brier_score,
            ),
            (
                "META_FILTER_MIN_POSITIVE_FOLD_RATIO",
                self.meta_filter_min_positive_fold_ratio,
            ),
            (
                "META_FILTER_MAX_CONCENTRATION",
                self.meta_filter_max_concentration,
            ),
        ] {
            bounded_float(name, value, 0.0, 1.0)?;
        }
        if self.meta_filter_min_train_examples >= self.meta_filter_min_examples {
            return invalid(
                "META_FILTER_SAMPLE_LIMITS",
                "training debe ser menor que el total mínimo",
            );
        }
        bounded_float(
            "LUNCH_MAX_SPREAD_FACTOR",
            self.lunch_max_spread_factor,
            0.01,
            1.0,
        )?;
        bounded_float(
            "LUNCH_SIGNAL_THRESHOLD_BONUS",
            self.lunch_signal_threshold_bonus,
            0.0,
            0.4,
        )?;
        bounded(
            "POST_LUNCH_CONFIRMATION_MINS",
            self.post_lunch_confirmation_mins,
            0,
            120,
        )?;
        bounded(
            "LUNCH_LIQUIDITY_WINDOW_MINS",
            self.lunch_liquidity_window_mins,
            1,
            60,
        )?;
        bounded(
            "LUNCH_MIN_QUOTE_UPDATES",
            self.lunch_min_quote_updates,
            1,
            1_000,
        )?;
        bounded("MIN_SAMPLES_FOR_TREND", self.min_samples_for_trend, 2, 100)?;
        bounded("TREND_CHANGE_SAMPLES", self.trend_change_samples, 2, 100)?;
        bounded_float(
            "TREND_DEADBAND_PERCENTAGE",
            self.trend_deadband_percentage,
            0.0,
            10.0,
        )?;
        bounded_float(
            "MIN_TREND_SLOPE_PERCENT_PER_MINUTE",
            self.min_trend_slope_percent_per_minute,
            0.0,
            100.0,
        )?;
        bounded_float("MIN_TREND_R_SQUARED", self.min_trend_r_squared, 0.0, 1.0)?;
        bounded_float(
            "MIN_TREND_MOVE_VOLATILITY_RATIO",
            self.min_trend_move_volatility_ratio,
            0.0,
            100.0,
        )?;
        bounded_float(
            "COMMISSION_PERCENTAGE",
            self.commission_percentage,
            0.0,
            10.0,
        )?;
        bounded_float("VAT_PERCENTAGE", self.vat_percentage, 0.0, 100.0)?;
        bounded_float(
            "OTHER_FEES_PERCENTAGE",
            self.other_fees_percentage,
            0.0,
            10.0,
        )?;
        bounded_float("TAX_PERCENTAGE", self.tax_percentage, 0.0, 100.0)?;
        bounded_float(
            "MIN_PROFIT_MULTIPLIER",
            self.min_profit_multiplier,
            1.0,
            10.0,
        )?;
        positive_float("MAX_INVESTMENT_AMOUNT", self.max_investment_amount)?;
        positive_float("MAX_LOSS_PER_TRADE", self.max_loss_per_trade)?;
        positive_float("MAX_DAILY_LOSS", self.max_daily_loss)?;
        bounded_float(
            "STOP_LOSS_PERCENTAGE",
            self.stop_loss_percentage,
            0.1,
            100.0,
        )?;
        bounded_float(
            "READONLY_SLIPPAGE_BPS",
            self.readonly_slippage_bps,
            0.0,
            1_000.0,
        )?;
        bounded(
            "MAX_MARKET_DATA_AGE_SECS",
            self.max_market_data_age_secs,
            1,
            300,
        )?;
        bounded_float(
            "MAX_OPTION_SPREAD_PERCENTAGE",
            self.max_option_spread_percentage,
            0.1,
            100.0,
        )?;
        bounded_float(
            "MIN_OPTION_CHAIN_ACCEPTANCE_PERCENTAGE",
            self.min_option_chain_acceptance_percentage,
            1.0,
            100.0,
        )?;
        bounded(
            "MIN_OPTION_CHAIN_CONTRACTS_PER_SIDE",
            self.min_option_chain_contracts_per_side,
            1,
            10_000,
        )?;
        bounded(
            "DATA_DIR_MAX_BYTES",
            self.data_dir_max_bytes,
            67_108_864,
            1_099_511_627_776,
        )?;
        bounded(
            "DATA_DISK_MIN_FREE_BYTES",
            self.data_disk_min_free_bytes,
            0,
            1_099_511_627_776,
        )?;
        bounded(
            "MARKET_CAPTURE_RETENTION_DAYS",
            self.market_capture_retention_days,
            1,
            3_650,
        )?;
        if self.min_option_volume == 0
            || self.option_target_expiry_days < self.option_expiry_days
            || self.option_max_expiry_days < self.option_target_expiry_days
            || self.live_learning_min_trades == 0
            || directional_minimums_exceed_total(
                self.live_learning_min_call_trades,
                self.live_learning_min_put_trades,
                self.live_learning_min_trades,
            )
            || self.live_learning_min_sessions == 0
            || self.live_regression_window_trades == 0
            || self.live_max_consecutive_losses == 0
            || self.canary_min_trades == 0
            || directional_minimums_exceed_total(
                self.canary_min_call_trades,
                self.canary_min_put_trades,
                self.canary_min_trades,
            )
            || self.canary_min_sessions == 0
            || self.canary_max_position_size == 0
            || self.canary_max_position_size > self.max_position_size
            || self.canary_max_trades_per_day == 0
            || self.canary_max_trades_per_day > self.max_trades_per_day
        {
            return invalid("STRATEGY_LIMITS", "límites de estrategia inconsistentes");
        }
        bounded_float(
            "MAX_OPTION_MONEYNESS_DISTANCE_PERCENTAGE",
            self.max_option_moneyness_distance_percentage,
            0.1,
            100.0,
        )?;
        bounded_float(
            "MIN_REWARD_RISK_RATIO",
            self.min_reward_risk_ratio,
            1.0,
            10.0,
        )?;
        bounded_float(
            "LEARNING_SLIPPAGE_BPS",
            self.learning_slippage_bps,
            0.0,
            1_000.0,
        )?;
        bounded("VIX_REFRESH_SECS", self.vix_refresh_secs, 1, 86_400)?;
        bounded("VIX_MAX_AGE_SECS", self.vix_max_age_secs, 60, 604_800)?;
        bounded(
            "VIX_PREVIOUS_CLOSE_MAX_AGE_SECS",
            self.vix_previous_close_max_age_secs,
            60,
            604_800,
        )?;
        bounded_float("VIX_ELEVATED_LEVEL", self.vix_elevated_level, 5.0, 100.0)?;
        bounded_float(
            "VIX_SPIKE_CHANGE_PERCENTAGE",
            self.vix_spike_change_percentage,
            0.1,
            100.0,
        )?;
        bounded_float(
            "VIX_ELEVATED_POSITION_FACTOR",
            self.vix_elevated_position_factor,
            0.01,
            1.0,
        )?;
        bounded_float(
            "VIX_SPIKE_THRESHOLD_BONUS",
            self.vix_spike_threshold_bonus,
            0.0,
            0.4,
        )?;
        if let Some(url) = self.vix_quote_url.as_deref() {
            validate_https_url("VIX_QUOTE_URL", url, true)?;
        }
        validate_https_url("HOLIDAYS_API_BASE_URL", &self.holidays_api_base_url, false)?;
        bounded_float(
            "LIVE_LEARNING_MIN_PROFIT_FACTOR",
            self.live_learning_min_profit_factor,
            1.0,
            10.0,
        )?;
        positive_float(
            "CANARY_MAX_INVESTMENT_AMOUNT",
            self.canary_max_investment_amount,
        )?;
        positive_float("CANARY_MAX_LOSS_PER_TRADE", self.canary_max_loss_per_trade)?;
        positive_float("CANARY_MAX_DAILY_LOSS", self.canary_max_daily_loss)?;
        if self.canary_max_investment_amount > self.max_investment_amount
            || self.canary_max_loss_per_trade > self.max_loss_per_trade
            || self.canary_max_daily_loss > self.max_daily_loss
        {
            return invalid(
                "CANARY_LIMITS",
                "los límites canary no pueden superar los límites live",
            );
        }
        if self.ticker.is_empty()
            || self.ticker.len() > 12
            || !self
                .ticker
                .bytes()
                .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit() || byte == b'.')
        {
            return invalid("TICKER", &self.ticker);
        }
        validate_https_url("IOL_BASE_URL", &self.iol_base_url, false)?;
        if let Some(url) = self.time_reference_url.as_deref() {
            let reference = validate_https_url("TIME_REFERENCE_URL", url, true)?;
            let broker = validate_https_url("IOL_BASE_URL", &self.iol_base_url, false)?;
            if reference.origin() == broker.origin() {
                return invalid("TIME_REFERENCE_URL", "debe tener un origen distinto de IOL");
            }
        } else if self.mode == Mode::Live {
            return invalid(
                "TIME_REFERENCE_URL",
                "live exige una referencia horaria independiente",
            );
        }
        bounded(
            "TIME_REFERENCE_REFRESH_SECS",
            self.time_reference_refresh_secs,
            30,
            3_600,
        )?;
        bounded(
            "TIME_REFERENCE_MAX_SKEW_SECS",
            self.time_reference_max_skew_secs,
            1,
            300,
        )?;
        if self.iol_websocket_enabled && !self.iol_websocket_url.starts_with("wss://") {
            return invalid(
                "IOL_WEBSOCKET_URL",
                "debe usar wss:// cuando IOL_WEBSOCKET_ENABLED=true",
            );
        }
        if let Some(path) = self.iol_order_path.as_deref() {
            if path != "/api/v2/operar" {
                return invalid(
                    "IOL_ORDER_PATH",
                    "el único endpoint de alta de órdenes autorizado es /api/v2/operar",
                );
            }
        }
        bounded(
            "ORDER_TRACKING_TIMEOUT_SECS",
            self.order_tracking_timeout_secs,
            1,
            300,
        )?;
        bounded(
            "ORDER_STATUS_POLL_INTERVAL_MILLIS",
            self.order_status_poll_interval_millis,
            100,
            10_000,
        )?;
        bounded(
            "ORDER_CANCEL_TIMEOUT_SECS",
            self.order_cancel_timeout_secs,
            1,
            120,
        )?;
        bounded("DYNAMIC_LIMIT_STEPS", self.dynamic_limit_steps, 1, 20)?;
        bounded(
            "DYNAMIC_LIMIT_FRAME_WAIT_SECS",
            self.dynamic_limit_frame_wait_secs,
            1,
            60,
        )?;
        bounded_float(
            "DYNAMIC_LIMIT_QUEUE_AHEAD_FACTOR",
            self.dynamic_limit_queue_ahead_factor,
            0.0,
            100.0,
        )?;
        bounded_float(
            "DYNAMIC_LIMIT_ADVERSE_SELECTION_BPS",
            self.dynamic_limit_adverse_selection_bps,
            0.0,
            1_000.0,
        )?;
        bounded(
            "OPTION_BINOMIAL_STEPS",
            self.option_binomial_steps,
            25,
            2_000,
        )?;
        bounded_float(
            "OPTION_RISK_FREE_RATE",
            self.option_risk_free_rate,
            -0.5,
            5.0,
        )?;
        bounded_float(
            "OPTION_DIVIDEND_YIELD",
            self.option_dividend_yield,
            0.0,
            2.0,
        )?;
        bounded(
            "OPTION_MARKET_INPUTS_MAX_AGE_SECS",
            self.option_market_inputs_max_age_secs,
            1,
            31_536_000,
        )?;
        if self.option_analytics_enabled
            && (self.option_market_inputs_observed_at_secs.is_none()
                || self.option_risk_free_source.trim().is_empty()
                || self.option_dividend_source.trim().is_empty())
        {
            return invalid(
                "OPTION_MARKET_INPUTS",
                "analítica activa requiere timestamp y fuentes explícitas",
            );
        }
        bounded_float("OPTION_MIN_ABS_DELTA", self.option_min_abs_delta, 0.0, 1.0)?;
        bounded_float("OPTION_MAX_ABS_DELTA", self.option_max_abs_delta, 0.0, 1.0)?;
        bounded_float(
            "OPTION_MIN_IMPLIED_VOLATILITY",
            self.option_min_implied_volatility,
            0.0001,
            5.0,
        )?;
        bounded_float(
            "OPTION_MAX_IMPLIED_VOLATILITY",
            self.option_max_implied_volatility,
            0.0001,
            5.0,
        )?;
        bounded_float(
            "OPTION_MAX_EXTRINSIC_PERCENTAGE",
            self.option_max_extrinsic_percentage,
            0.0,
            100.0,
        )?;
        if self.option_min_abs_delta > self.option_max_abs_delta
            || self.option_min_implied_volatility > self.option_max_implied_volatility
        {
            return invalid("OPTION_ANALYTICS_LIMITS", "mínimos mayores que máximos");
        }
        bounded(
            "IV_RANK_WINDOW_SESSIONS",
            self.iv_rank_window_sessions,
            2,
            2_000,
        )?;
        bounded("IV_RANK_MIN_SESSIONS", self.iv_rank_min_sessions, 2, 2_000)?;
        bounded_float("IV_RANK_MIN", self.iv_rank_min, 0.0, 100.0)?;
        bounded_float("IV_RANK_MAX", self.iv_rank_max, 0.0, 100.0)?;
        if self.iv_rank_min_sessions > self.iv_rank_window_sessions
            || self.iv_rank_min > self.iv_rank_max
        {
            return invalid("IV_RANK_POLICY", "ventana o límites inconsistentes");
        }
        bounded_float(
            "TREE_META_FILTER_MIN_IMPROVEMENT",
            self.tree_meta_filter_min_improvement,
            0.0,
            1_000_000.0,
        )?;
        bounded_float(
            "MAX_FRICTION_STOP_RATIO",
            self.max_friction_stop_ratio,
            0.01,
            1.0,
        )?;
        bounded_float(
            "TARGET_UNDERLYING_VOLATILITY_PERCENTAGE",
            self.target_underlying_volatility_percentage,
            0.01,
            100.0,
        )?;
        if self.max_position_size == 0
            || self.position_timeout_mins == 0
            || self.max_trades_per_day == 0
            || self.contract_multiplier == 0
        {
            return invalid("POSITION_LIMITS", "deben ser mayores que cero");
        }
        if self
            .master_key_path
            .as_ref()
            .is_some_and(|path| !path.is_absolute())
        {
            return invalid("OPTIONS_MASTER_KEY_PATH", "debe ser una ruta absoluta");
        }
        bounded(
            "MAX_CONCURRENT_REQUESTS",
            self.max_concurrent_requests,
            1,
            100,
        )?;
        bounded("CACHE_TTL_SECS", self.cache_ttl_secs, 1, 86_400)?;
        if !matches!(self.log_level.as_str(), "debug" | "info" | "warn" | "error") {
            return invalid("LOG_LEVEL", &self.log_level);
        }
        if self.mode == Mode::Live && !self.live_ordering_ready() {
            return invalid(
                "LIVE_ORDERING_GATES",
                self.live_ordering_blockers().join("; "),
            );
        }
        Ok(())
    }

    pub fn history_capacity(&self) -> usize {
        ((self.price_history_minutes * 60) / self.check_interval_secs)
            .max(self.min_samples_for_trend as u64) as usize
    }

    pub fn operating_cost_percentage(&self) -> f64 {
        (self.commission_percentage + self.other_fees_percentage)
            * (1.0 + self.vat_percentage / 100.0)
    }

    pub fn live_ordering_ready(&self) -> bool {
        self.live_ordering_blockers().is_empty()
    }

    pub fn live_ordering_blockers(&self) -> Vec<&'static str> {
        let mut blockers = Vec::new();
        if !self.live_confirmed {
            blockers.push("confirmación explícita de órdenes reales ausente");
        }
        if self.iol_order_path.as_deref() != Some("/api/v2/operar") {
            blockers.push("endpoint contractual de alta de órdenes no confirmado");
        }
        if self.live_authorization_path.is_none() {
            blockers.push("autorización HMAC de operador ausente");
        }
        if self.live_readiness_path.is_none() {
            blockers.push("readiness pre-canary firmado ausente");
        }
        if self.market_sessions_path.is_none() {
            blockers.push("manifiesto bursátil vigente ausente");
        }
        if self.time_reference_url.is_none() {
            blockers.push("referencia horaria independiente ausente");
        }
        if self.master_key_path.is_none() {
            blockers.push("clave maestra externa ausente");
        }
        blockers
    }
}

fn env_or(name: &str, default: &str) -> String {
    env::var(name).unwrap_or_else(|_| default.to_string())
}

fn optional_string_env(name: &str) -> Option<String> {
    env::var(name).ok().filter(|value| !value.trim().is_empty())
}

fn optional_path_env(name: &str) -> Option<PathBuf> {
    optional_string_env(name).map(PathBuf::from)
}

fn required_secret_is_present(name: &str) -> bool {
    env::var(name).is_ok_and(|value| !value.trim().is_empty())
}

fn confirmation_is_exact(value: &str) -> bool {
    value == LIVE_CONFIRMATION
}

fn directional_minimums_exceed_total(call_minimum: u64, put_minimum: u64, total: u64) -> bool {
    call_minimum
        .checked_add(put_minimum)
        .is_none_or(|directional_total| directional_total > total)
}

fn parse_u64(name: &'static str, default: u64) -> Result<u64, ConfigError> {
    parse(name, default)
}

fn parse_u32(name: &'static str, default: u32) -> Result<u32, ConfigError> {
    parse(name, default)
}

fn parse_optional_i64(name: &'static str) -> Result<Option<i64>, ConfigError> {
    env::var(name)
        .ok()
        .filter(|value| !value.trim().is_empty())
        .map(|value| {
            value.parse().map_err(|source| ConfigError::Number {
                name,
                source: NumericError::Int(source),
            })
        })
        .transpose()
}

fn parse_usize(name: &'static str, default: usize) -> Result<usize, ConfigError> {
    parse(name, default)
}

fn parse<T>(name: &'static str, default: T) -> Result<T, ConfigError>
where
    T: std::str::FromStr<Err = ParseIntError> + ToString,
{
    env::var(name)
        .unwrap_or_else(|_| default.to_string())
        .parse()
        .map_err(|source| ConfigError::Number {
            name,
            source: NumericError::Int(source),
        })
}

fn parse_f64(name: &'static str, default: f64) -> Result<f64, ConfigError> {
    env::var(name)
        .unwrap_or_else(|_| default.to_string())
        .parse()
        .map_err(|source| ConfigError::Number {
            name,
            source: NumericError::Float(source),
        })
}

fn parse_f64_with_legacy(
    name: &'static str,
    legacy_name: &'static str,
    default: f64,
) -> Result<f64, ConfigError> {
    env::var(name)
        .or_else(|_| env::var(legacy_name))
        .unwrap_or_else(|_| default.to_string())
        .parse()
        .map_err(|source| ConfigError::Number {
            name,
            source: NumericError::Float(source),
        })
}

fn parse_bool(name: &'static str, default: bool) -> Result<bool, ConfigError> {
    let value = env::var(name).unwrap_or_else(|_| default.to_string());
    match value.to_ascii_lowercase().as_str() {
        "true" | "1" | "yes" | "on" => Ok(true),
        "false" | "0" | "no" | "off" => Ok(false),
        _ => Err(ConfigError::InvalidValue { name, value }),
    }
}

fn parse_market_time(name: &'static str, default: &str) -> Result<u16, ConfigError> {
    let value = env::var(name).unwrap_or_else(|_| default.to_string());
    let (hour, minute) = value
        .split_once(':')
        .ok_or_else(|| ConfigError::InvalidValue {
            name,
            value: value.clone(),
        })?;
    let hour = hour.parse::<u16>().map_err(|_| ConfigError::InvalidValue {
        name,
        value: value.clone(),
    })?;
    let minute = minute
        .parse::<u16>()
        .map_err(|_| ConfigError::InvalidValue {
            name,
            value: value.clone(),
        })?;
    if hour > 23 || minute > 59 {
        return Err(ConfigError::InvalidValue { name, value });
    }
    Ok(hour * 60 + minute)
}

fn bounded<T: PartialOrd + std::fmt::Display>(
    name: &'static str,
    value: T,
    min: T,
    max: T,
) -> Result<(), ConfigError> {
    if value < min || value > max {
        return Err(ConfigError::InvalidValue {
            name,
            value: value.to_string(),
        });
    }
    Ok(())
}

fn bounded_float(name: &'static str, value: f64, min: f64, max: f64) -> Result<(), ConfigError> {
    if !value.is_finite() || value < min || value > max {
        return Err(ConfigError::InvalidValue {
            name,
            value: value.to_string(),
        });
    }
    Ok(())
}

fn positive_float(name: &'static str, value: f64) -> Result<(), ConfigError> {
    if !value.is_finite() || value <= 0.0 {
        return Err(ConfigError::InvalidValue {
            name,
            value: value.to_string(),
        });
    }
    Ok(())
}

fn validate_https_url(
    name: &'static str,
    value: &str,
    allow_loopback_http: bool,
) -> Result<reqwest::Url, ConfigError> {
    let parsed = reqwest::Url::parse(value).map_err(|_| ConfigError::InvalidValue {
        name,
        value: "URL inválida".into(),
    })?;
    if !parsed.username().is_empty() || parsed.password().is_some() || parsed.host_str().is_none() {
        return invalid(name, "URL con autoridad inválida o credenciales embebidas");
    }
    let loopback = parsed.host_str().is_some_and(|host| {
        host.eq_ignore_ascii_case("localhost")
            || host
                .trim_start_matches('[')
                .trim_end_matches(']')
                .parse::<std::net::IpAddr>()
                .is_ok_and(|address| address.is_loopback())
    });
    if parsed.scheme() != "https" && !(allow_loopback_http && parsed.scheme() == "http" && loopback)
    {
        return invalid(name, "debe usar HTTPS; HTTP sólo se admite en loopback");
    }
    Ok(parsed)
}

fn invalid<T>(name: &'static str, value: impl ToString) -> Result<T, ConfigError> {
    Err(ConfigError::InvalidValue {
        name,
        value: value.to_string(),
    })
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    type ConfigMutation = (&'static str, fn(&mut Config));
    static CONFIG_ENVIRONMENT_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    pub(crate) fn config() -> Config {
        Config {
            mode: Mode::Readonly,
            ticker: "GAL".into(),
            check_interval_secs: 5,
            price_history_minutes: 30,
            min_samples_for_trend: 5,
            trend_change_samples: 3,
            trend_deadband_percentage: 0.10,
            min_trend_slope_percent_per_minute: 0.02,
            min_trend_r_squared: 0.60,
            min_trend_move_volatility_ratio: 1.0,
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
            tui_enabled: true,
            recover_state: false,
            data_dir: PathBuf::from("data"),
            replay_path: None,
            capture_market_data: true,
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
            connection_retry_delay_secs: 5,
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
            min_option_volume: 10,
            min_option_chain_acceptance_percentage: 80.0,
            min_option_chain_contracts_per_side: 1,
            option_target_expiry_days: 21,
            option_max_expiry_days: 45,
            max_option_moneyness_distance_percentage: 10.0,
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
            time_reference_url: Some("https://clock.example.invalid".into()),
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

    #[test]
    fn readonly_defaults_are_valid() {
        assert!(config().validate().is_ok());
    }

    #[test]
    fn option_chain_quality_gate_has_closed_valid_boundaries() {
        let mut config = config();
        config.min_option_chain_acceptance_percentage = 1.0;
        config.min_option_chain_contracts_per_side = 1;
        assert!(config.validate().is_ok());

        config.min_option_chain_acceptance_percentage = 0.0;
        assert!(config.validate().is_err());
        config.min_option_chain_acceptance_percentage = 101.0;
        assert!(config.validate().is_err());
        config.min_option_chain_acceptance_percentage = 80.0;
        config.min_option_chain_contracts_per_side = 0;
        assert!(config.validate().is_err());
    }

    #[test]
    fn aggregate_storage_policy_has_closed_safe_boundaries() {
        let mut config = config();
        config.data_dir_max_bytes = 67_108_864;
        config.data_disk_min_free_bytes = 0;
        config.market_capture_retention_days = 1;
        assert!(config.validate().is_ok());

        config.data_dir_max_bytes = 67_108_863;
        assert!(config.validate().is_err());
        config.data_dir_max_bytes = 67_108_864;
        config.market_capture_retention_days = 0;
        assert!(config.validate().is_err());
    }

    #[test]
    fn ticker_is_confined_to_the_documented_market_symbol_alphabet() {
        let mut config = config();
        config.ticker = "ABC.DEF12345".into();
        assert_eq!(config.ticker.len(), 12);
        assert!(config.validate().is_ok());
        config.ticker.push('6');
        assert!(config.validate().is_err());
        config.ticker = "../secret".into();
        assert!(config.validate().is_err());
        config.ticker = "ggal".into();
        assert!(config.validate().is_err());
    }

    #[test]
    fn order_path_is_an_exact_contractual_allowlist() {
        let mut config = config();
        config.iol_order_path = Some("/api/v2/operar".into());
        assert!(config.validate().is_ok());
        config.iol_order_path = Some("/api/v2/../token".into());
        assert!(config.validate().is_err());
        config.iol_order_path = Some("/api/v2/operar?admin=true".into());
        assert!(config.validate().is_err());
        config.iol_order_path = Some("/api/v2/otra-ruta".into());
        assert!(config.validate().is_err());
    }

    #[test]
    fn live_transports_require_https_and_optional_websocket_requires_wss() {
        let mut config = config();
        config.iol_base_url = "http://api.invertironline.com".into();
        assert!(config.validate().is_err());

        config.iol_base_url = "https://api.invertironline.com".into();
        config.iol_websocket_enabled = true;
        config.iol_websocket_url = "ws://websocket-movements.invertironline.com".into();
        assert!(config.validate().is_err());

        config.iol_websocket_url = "wss://websocket-movements.invertironline.com".into();
        assert!(config.validate().is_ok());
    }

    #[test]
    fn vix_http_is_only_allowed_for_a_loopback_adapter() {
        let mut config = config();
        config.vix_quote_url = Some("http://quotes.example/vix".into());
        assert!(config.validate().is_err());

        config.vix_quote_url = Some("http://127.0.0.1:8080/vix".into());
        assert!(config.validate().is_ok());
        config.vix_quote_url = Some("http://localhost:8080/vix".into());
        assert!(config.validate().is_ok());
        config.vix_quote_url = Some("http://[::1]:8080/vix".into());
        assert!(config.validate().is_ok());
    }

    #[test]
    fn urls_reject_each_embedded_authority_ambiguity_independently() {
        assert!(validate_https_url("URL", "https://user@example.com/path", false).is_err());
        assert!(validate_https_url("URL", "https://:secret@example.com/path", false).is_err());
        assert!(matches!(
            validate_https_url("URL", "mailto:test@example.com", false),
            Err(ConfigError::InvalidValue { value, .. }) if value.contains("autoridad")
        ));
        assert!(validate_https_url("URL", "https://example.com/path", false).is_ok());
    }

    #[test]
    fn quantitative_and_execution_experiments_default_to_off() {
        let config = config();
        assert!(!config.option_analytics_enabled);
        assert!(!config.adaptive_entry_filter_enabled);
        assert!(!config.volatility_normalized_signals_enabled);
        assert!(!config.nonlinear_meta_filter_enabled);
        assert!(!config.tree_meta_filter_enabled);
        assert!(!config.iv_rank_filter_enabled);
        assert!(!config.experiment_runner_enabled);
        assert!(!config.dynamic_limit_enabled);
        assert!(!config.vertical_spread_research_enabled);
        assert!(!config.vertical_atomic_execution_verified);
        assert!(!config.contract_multiplier_confirmed);
    }

    #[test]
    fn option_analytics_requires_point_in_time_input_metadata() {
        let mut config = config();
        config.option_analytics_enabled = true;
        assert!(config.validate().is_err());
        config.option_market_inputs_observed_at_secs = Some(1_000);
        assert!(config.validate().is_ok());
        config.option_risk_free_source.clear();
        assert!(config.validate().is_err());
    }

    #[test]
    fn connection_retry_policy_has_safe_bounds() {
        let mut config = config();
        config.connection_retry_attempts = 0;
        assert!(config.validate().is_err());
        config.connection_retry_attempts = 3;
        config.connection_retry_delay_secs = 301;
        assert!(config.validate().is_err());
    }

    #[test]
    fn opening_observation_delay_cannot_exceed_the_trading_session() {
        let mut config = config();
        config.entry_delay_after_open_mins = 390;
        assert!(config.validate().is_ok());
        config.entry_delay_after_open_mins = 391;
        assert!(config.validate().is_err());
    }

    #[test]
    fn strategy_and_canary_limits_accept_exact_equality_only() {
        let mut config = config();
        config.option_expiry_days = 21;
        config.option_target_expiry_days = 21;
        config.option_max_expiry_days = 21;
        config.canary_max_position_size = config.max_position_size;
        config.canary_max_trades_per_day = config.max_trades_per_day;
        config.canary_max_investment_amount = config.max_investment_amount;
        config.canary_max_loss_per_trade = config.max_loss_per_trade;
        config.canary_max_daily_loss = config.max_daily_loss;
        assert!(config.validate().is_ok());

        let mut invalid = config.clone();
        invalid.option_target_expiry_days = 20;
        assert_invalid_name(&invalid, "STRATEGY_LIMITS");
        invalid = config.clone();
        invalid.option_max_expiry_days = 20;
        assert_invalid_name(&invalid, "STRATEGY_LIMITS");
        invalid = config.clone();
        invalid.canary_max_position_size += 1;
        assert_invalid_name(&invalid, "STRATEGY_LIMITS");
        invalid = config.clone();
        invalid.canary_max_trades_per_day += 1;
        assert_invalid_name(&invalid, "STRATEGY_LIMITS");
        invalid = config.clone();
        invalid.canary_max_investment_amount += 1.0;
        assert_invalid_name(&invalid, "CANARY_LIMITS");
        invalid = config.clone();
        invalid.canary_max_loss_per_trade += 1.0;
        assert_invalid_name(&invalid, "CANARY_LIMITS");
        invalid = config;
        invalid.canary_max_daily_loss += 1.0;
        assert_invalid_name(&invalid, "CANARY_LIMITS");
    }

    #[test]
    fn weekend_risk_times_are_ordered_and_inside_their_sessions() {
        let mut config = config();
        config.pre_break_last_entry_minute = 630;
        config.pre_break_force_exit_minute = 631;
        config.expiry_day_force_exit_minute = 630;
        assert!(config.validate().is_ok());

        config.pre_break_last_entry_minute = 629;
        assert!(config.validate().is_err());

        config = self::config();
        config.pre_break_force_exit_minute = config.pre_break_last_entry_minute;
        assert!(config.validate().is_err());

        config = self::config();
        config.pre_break_force_exit_minute = 1_020;
        assert!(config.validate().is_err());

        config = self::config();
        config.expiry_day_force_exit_minute = 930;
        assert!(config.validate().is_err());
    }

    #[test]
    fn market_time_parser_requires_a_valid_24_hour_time() {
        unsafe { env::set_var("TEST_MARKET_TIME", "16:30") };
        assert_eq!(parse_market_time("TEST_MARKET_TIME", "15:00").unwrap(), 990);
        unsafe { env::set_var("TEST_MARKET_TIME", "24:00") };
        assert!(parse_market_time("TEST_MARKET_TIME", "15:00").is_err());
        unsafe { env::set_var("TEST_MARKET_TIME", "23:59") };
        assert_eq!(
            parse_market_time("TEST_MARKET_TIME", "15:00").unwrap(),
            1_439
        );
        unsafe { env::set_var("TEST_MARKET_TIME", "23:60") };
        assert!(parse_market_time("TEST_MARKET_TIME", "15:00").is_err());
        unsafe { env::remove_var("TEST_MARKET_TIME") };
    }

    #[test]
    fn lunch_slowdown_requires_an_ordered_session_and_safe_factors() {
        let mut config = config();
        config.lunch_slowdown_start_minute = 630;
        config.lunch_slowdown_end_minute = 631;
        config.post_lunch_confirmation_mins = 0;
        assert!(config.validate().is_ok());

        config.lunch_slowdown_start_minute = 629;
        assert!(config.validate().is_err());

        config = self::config();
        config.lunch_slowdown_end_minute = 1_019;
        config.post_lunch_confirmation_mins = 1;
        assert!(config.validate().is_ok());
        config.post_lunch_confirmation_mins = 2;
        assert!(config.validate().is_err());

        config = self::config();
        config.lunch_slowdown_end_minute = u16::MAX;
        config.post_lunch_confirmation_mins = u32::MAX;
        assert!(config.validate().is_err());

        config = self::config();
        config.lunch_slowdown_end_minute = config.lunch_slowdown_start_minute;
        assert!(config.validate().is_err());

        config = self::config();
        config.lunch_position_factor = 0.0;
        assert!(config.validate().is_err());
    }

    #[test]
    fn directional_sample_totals_fail_closed_without_integer_overflow() {
        assert!(!directional_minimums_exceed_total(5, 5, 10));
        assert!(directional_minimums_exceed_total(5, 6, 10));
        assert!(directional_minimums_exceed_total(u64::MAX, 1, u64::MAX));

        let mut config = config();
        config.live_learning_min_trades = u64::MAX;
        config.live_learning_min_call_trades = u64::MAX;
        config.live_learning_min_put_trades = u64::MAX;
        assert_invalid_name(&config, "STRATEGY_LIMITS");

        let mut config = self::config();
        config.canary_min_trades = u64::MAX;
        config.canary_min_call_trades = u64::MAX;
        config.canary_min_put_trades = u64::MAX;
        assert_invalid_name(&config, "STRATEGY_LIMITS");
    }

    #[test]
    fn analytics_and_iv_rank_ranges_accept_degenerate_closed_intervals() {
        let mut config = config();
        config.option_min_abs_delta = 0.5;
        config.option_max_abs_delta = 0.5;
        config.option_min_implied_volatility = 0.2;
        config.option_max_implied_volatility = 0.2;
        config.iv_rank_min_sessions = config.iv_rank_window_sessions;
        config.iv_rank_min = 50.0;
        config.iv_rank_max = 50.0;
        assert!(config.validate().is_ok());

        let mut invalid = config.clone();
        invalid.option_min_abs_delta = 0.6;
        assert_invalid_name(&invalid, "OPTION_ANALYTICS_LIMITS");
        invalid = config.clone();
        invalid.option_min_implied_volatility = 0.3;
        assert_invalid_name(&invalid, "OPTION_ANALYTICS_LIMITS");
        invalid = config.clone();
        invalid.iv_rank_min_sessions += 1;
        assert_invalid_name(&invalid, "IV_RANK_POLICY");
        invalid = config;
        invalid.iv_rank_min = 51.0;
        assert_invalid_name(&invalid, "IV_RANK_POLICY");
    }

    #[test]
    fn only_readonly_and_live_are_public_modes() {
        assert_eq!(Mode::parse("readonly").unwrap(), Mode::Readonly);
        assert_eq!(Mode::parse("live").unwrap(), Mode::Live);
        assert!(Mode::parse("paper").is_err());
        assert!(Mode::parse("replay").is_err());
        assert!(Mode::Readonly.uses_iol_market_data());
        assert!(Mode::Live.uses_iol_market_data());
    }

    #[test]
    fn live_ordering_requires_confirmation_and_order_contract() {
        let mut config = config();
        config.mode = Mode::Live;
        config.time_reference_url = None;
        assert!(!config.live_ordering_ready());
        assert_eq!(config.live_ordering_blockers().len(), 7);
        config.live_confirmed = true;
        assert!(!config.live_ordering_ready());
        config.iol_order_path = Some("/api/v2/operar".into());
        assert!(!config.live_ordering_ready());
        config.live_readiness_path = Some("data/live/release-readiness.json".into());
        assert!(!config.live_ordering_ready());
        config.live_authorization_path = Some("data/live/authorization.json".into());
        assert!(!config.live_ordering_ready());
        config.market_sessions_path = Some("data/calendar/byma-sessions.json".into());
        assert!(!config.live_ordering_ready());
        config.time_reference_url = Some("https://clock.example.invalid".into());
        assert!(!config.live_ordering_ready());
        config.master_key_path = Some("/tmp/options-master.key".into());
        assert!(config.live_ordering_ready());
        assert!(config.live_ordering_blockers().is_empty());
    }

    #[test]
    fn every_incomplete_live_ordering_configuration_fails_closed() {
        for mask in 0_u8..=0b111_1111 {
            let mut config = config();
            config.mode = Mode::Live;
            config.live_confirmed = mask & 0b000_0001 != 0;
            config.iol_order_path = (mask & 0b000_0010 != 0).then(|| "/api/v2/operar".into());
            config.live_authorization_path =
                (mask & 0b000_0100 != 0).then(|| "data/live/authorization.json".into());
            config.live_readiness_path =
                (mask & 0b000_1000 != 0).then(|| "data/live/readiness.json".into());
            config.market_sessions_path =
                (mask & 0b001_0000 != 0).then(|| "data/calendar/sessions.json".into());
            config.time_reference_url =
                (mask & 0b010_0000 != 0).then(|| "https://clock.example.invalid".into());
            config.master_key_path =
                (mask & 0b100_0000 != 0).then(|| "/tmp/options-master.key".into());

            assert_eq!(
                config.live_ordering_ready(),
                mask == 0b111_1111,
                "combinación {mask:07b} no falló de forma cerrada"
            );
            assert_eq!(
                config.live_ordering_blockers().len(),
                7 - mask.count_ones() as usize
            );
        }
    }

    #[test]
    fn live_requires_an_independent_clock_origin_with_closed_skew_bounds() {
        let mut config = config();
        config.mode = Mode::Live;
        config.time_reference_url = None;
        assert!(config.validate().is_err());

        config.time_reference_url = Some(format!("{}/time", config.iol_base_url));
        assert!(config.validate().is_err());

        config.time_reference_url = Some("https://clock.example.invalid".into());
        config.live_confirmed = true;
        config.iol_order_path = Some("/api/v2/operar".into());
        config.live_authorization_path = Some("data/live/authorization.json".into());
        config.live_readiness_path = Some("data/live/readiness.json".into());
        config.market_sessions_path = Some("data/calendar/sessions.json".into());
        config.master_key_path = Some("/tmp/options-master.key".into());
        config.time_reference_max_skew_secs = 30;
        assert!(config.validate().is_ok());
        config.time_reference_max_skew_secs = 0;
        assert!(config.validate().is_err());
    }

    #[test]
    fn history_capacity_uses_time_window() {
        assert_eq!(config().history_capacity(), 360);
    }

    #[test]
    fn operating_cost_includes_vat_on_all_net_fees() {
        let mut config = config();
        config.commission_percentage = 0.2;
        config.other_fees_percentage = 0.05;
        config.vat_percentage = 21.0;
        assert!((config.operating_cost_percentage() - 0.3025).abs() < 1e-9);
    }

    fn assert_invalid_name(config: &Config, expected: &'static str) {
        assert!(matches!(
            config.validate(),
            Err(ConfigError::InvalidValue { name, .. }) if name == expected
        ));
    }

    fn assert_invalid_environment_value(name: &'static str, value: &str) {
        let previous = env::var_os(name);
        unsafe { env::set_var(name, value) };
        let result = Config::from_env();
        match previous {
            Some(previous) => unsafe { env::set_var(name, previous) },
            None => unsafe { env::remove_var(name) },
        }
        assert!(
            matches!(
                result,
                Err(ConfigError::InvalidValue { name: actual, .. })
                    | Err(ConfigError::Number { name: actual, .. }) if actual == name
            ),
            "{name} no atribuyó su valor inválido al origen correcto: {result:?}"
        );
    }

    #[test]
    fn every_compound_strategy_limit_fails_closed_independently() {
        let mutations: &[fn(&mut Config)] = &[
            |config| config.min_option_volume = 0,
            |config| config.option_target_expiry_days = 0,
            |config| config.option_max_expiry_days = config.option_target_expiry_days - 1,
            |config| config.live_learning_min_trades = 0,
            |config| {
                config.live_learning_min_call_trades = config.live_learning_min_trades;
                config.live_learning_min_put_trades = 1;
            },
            |config| config.live_learning_min_sessions = 0,
            |config| config.live_regression_window_trades = 0,
            |config| config.live_max_consecutive_losses = 0,
            |config| config.canary_min_trades = 0,
            |config| {
                config.canary_min_call_trades = config.canary_min_trades;
                config.canary_min_put_trades = 1;
            },
            |config| config.canary_min_sessions = 0,
            |config| config.canary_max_position_size = 0,
            |config| config.canary_max_position_size = config.max_position_size + 1,
            |config| config.canary_max_trades_per_day = 0,
            |config| config.canary_max_trades_per_day = config.max_trades_per_day + 1,
        ];

        for mutate in mutations {
            let mut candidate = config();
            mutate(&mut candidate);
            assert_invalid_name(&candidate, "STRATEGY_LIMITS");
        }
    }

    #[test]
    fn canary_and_position_limits_cannot_escape_the_live_envelope() {
        for mutate in [
            |config: &mut Config| {
                config.canary_max_investment_amount = config.max_investment_amount + 1.0
            },
            |config: &mut Config| {
                config.canary_max_loss_per_trade = config.max_loss_per_trade + 1.0
            },
            |config: &mut Config| config.canary_max_daily_loss = config.max_daily_loss + 1.0,
        ] {
            let mut candidate = config();
            mutate(&mut candidate);
            assert_invalid_name(&candidate, "CANARY_LIMITS");
        }

        for mutate in [
            |config: &mut Config| config.max_position_size = 0,
            |config: &mut Config| config.position_timeout_mins = 0,
            |config: &mut Config| config.max_trades_per_day = 0,
            |config: &mut Config| config.contract_multiplier = 0,
        ] {
            let mut candidate = config();
            mutate(&mut candidate);
            assert!(candidate.validate().is_err());
        }
    }

    #[test]
    fn non_finite_financial_inputs_and_inconsistent_analytics_are_rejected() {
        for mutate in [
            |config: &mut Config| config.max_investment_amount = f64::NAN,
            |config: &mut Config| config.max_loss_per_trade = f64::INFINITY,
            |config: &mut Config| config.max_daily_loss = f64::NEG_INFINITY,
            |config: &mut Config| config.commission_percentage = f64::NAN,
        ] {
            let mut candidate = config();
            mutate(&mut candidate);
            assert!(candidate.validate().is_err());
        }

        let mut candidate = config();
        candidate.option_min_abs_delta = 0.9;
        candidate.option_max_abs_delta = 0.8;
        assert_invalid_name(&candidate, "OPTION_ANALYTICS_LIMITS");

        candidate = config();
        candidate.option_min_implied_volatility = 2.0;
        candidate.option_max_implied_volatility = 1.0;
        assert_invalid_name(&candidate, "OPTION_ANALYTICS_LIMITS");

        candidate = config();
        candidate.iv_rank_min_sessions = candidate.iv_rank_window_sessions + 1;
        assert_invalid_name(&candidate, "IV_RANK_POLICY");

        candidate = config();
        candidate.iv_rank_min = 80.0;
        candidate.iv_rank_max = 20.0;
        assert_invalid_name(&candidate, "IV_RANK_POLICY");
    }

    #[test]
    fn urls_reject_credentials_missing_authority_and_unsafe_schemes() {
        let credential_url = ["https://user", ":secret@", "example.invalid"].concat();
        for value in [
            "https://user@example.invalid",
            credential_url.as_str(),
            "https://",
            "file:///tmp/quote",
        ] {
            assert!(validate_https_url("TEST_URL", value, false).is_err());
        }

        assert!(validate_https_url("TEST_URL", "http://localhost:8080/quote", true).is_ok());
        assert!(validate_https_url("TEST_URL", "http://[::1]:8080/quote", true).is_ok());
        assert!(validate_https_url("TEST_URL", "http://example.invalid/quote", true).is_err());
    }

    #[test]
    fn filesystem_and_metadata_controls_fail_closed() {
        let mut candidate = config();
        candidate.master_key_path = Some("relative/master.key".into());
        assert_invalid_name(&candidate, "OPTIONS_MASTER_KEY_PATH");

        candidate = config();
        candidate.log_level = "trace".into();
        assert_invalid_name(&candidate, "LOG_LEVEL");

        candidate = config();
        candidate.meta_filter_min_train_examples = candidate.meta_filter_min_examples;
        assert_invalid_name(&candidate, "META_FILTER_SAMPLE_LIMITS");
    }

    #[test]
    fn primitive_parsers_accept_aliases_and_report_invalid_values() {
        const BOOL_NAME: &str = "OPTIONS_TEST_CONFIG_BOOL";
        const INT_NAME: &str = "OPTIONS_TEST_CONFIG_INT";
        const FLOAT_NAME: &str = "OPTIONS_TEST_CONFIG_FLOAT";
        const TIME_NAME: &str = "OPTIONS_TEST_CONFIG_TIME";

        for (raw, expected) in [
            ("TRUE", true),
            ("1", true),
            ("yes", true),
            ("on", true),
            ("FALSE", false),
            ("0", false),
            ("no", false),
            ("off", false),
        ] {
            unsafe { env::set_var(BOOL_NAME, raw) };
            assert_eq!(parse_bool(BOOL_NAME, !expected).unwrap(), expected);
        }
        unsafe { env::set_var(BOOL_NAME, "perhaps") };
        assert!(parse_bool(BOOL_NAME, false).is_err());

        unsafe { env::set_var(INT_NAME, "not-an-integer") };
        assert!(parse_u64(INT_NAME, 1).is_err());
        assert!(parse_optional_i64(INT_NAME).is_err());
        unsafe { env::set_var(FLOAT_NAME, "not-a-decimal") };
        assert!(parse_f64(FLOAT_NAME, 1.0).is_err());

        for invalid_time in ["1630", "xx:30", "16:xx", "16:60"] {
            unsafe { env::set_var(TIME_NAME, invalid_time) };
            assert!(parse_market_time(TIME_NAME, "15:00").is_err());
        }

        for name in [BOOL_NAME, INT_NAME, FLOAT_NAME, TIME_NAME] {
            unsafe { env::remove_var(name) };
        }
        assert_eq!(parse_optional_i64(INT_NAME).unwrap(), None);
    }

    #[test]
    fn every_numeric_policy_rejects_an_independent_out_of_range_value() {
        let mutations: &[ConfigMutation] = &[
            ("META_FILTER_MIN_EXAMPLES", |c| {
                c.meta_filter_min_examples = 29
            }),
            ("META_FILTER_MIN_TRAIN_EXAMPLES", |c| {
                c.meta_filter_min_train_examples = 19
            }),
            ("META_FILTER_MIN_ACCEPTED_HOLDOUT", |c| {
                c.meta_filter_min_accepted_holdout = 4
            }),
            ("LUNCH_MAX_SPREAD_FACTOR", |c| {
                c.lunch_max_spread_factor = 0.0
            }),
            ("LUNCH_SIGNAL_THRESHOLD_BONUS", |c| {
                c.lunch_signal_threshold_bonus = 0.5
            }),
            ("POST_LUNCH_CONFIRMATION_MINS", |c| {
                c.post_lunch_confirmation_mins = 121
            }),
            ("LUNCH_LIQUIDITY_WINDOW_MINS", |c| {
                c.lunch_liquidity_window_mins = 0
            }),
            ("LUNCH_MIN_QUOTE_UPDATES", |c| c.lunch_min_quote_updates = 0),
            ("MIN_SAMPLES_FOR_TREND", |c| c.min_samples_for_trend = 1),
            ("TREND_CHANGE_SAMPLES", |c| c.trend_change_samples = 1),
            ("TREND_DEADBAND_PERCENTAGE", |c| {
                c.trend_deadband_percentage = -1.0
            }),
            ("MIN_TREND_SLOPE_PERCENT_PER_MINUTE", |c| {
                c.min_trend_slope_percent_per_minute = -1.0
            }),
            ("MIN_TREND_R_SQUARED", |c| c.min_trend_r_squared = 2.0),
            ("MIN_TREND_MOVE_VOLATILITY_RATIO", |c| {
                c.min_trend_move_volatility_ratio = -1.0
            }),
            ("VAT_PERCENTAGE", |c| c.vat_percentage = 101.0),
            ("OTHER_FEES_PERCENTAGE", |c| c.other_fees_percentage = -1.0),
            ("TAX_PERCENTAGE", |c| c.tax_percentage = 101.0),
            ("MIN_PROFIT_MULTIPLIER", |c| c.min_profit_multiplier = 0.9),
            ("STOP_LOSS_PERCENTAGE", |c| c.stop_loss_percentage = 0.0),
            ("READONLY_SLIPPAGE_BPS", |c| c.readonly_slippage_bps = -1.0),
            ("MAX_MARKET_DATA_AGE_SECS", |c| {
                c.max_market_data_age_secs = 0
            }),
            ("MAX_OPTION_SPREAD_PERCENTAGE", |c| {
                c.max_option_spread_percentage = 0.0
            }),
            ("DATA_DISK_MIN_FREE_BYTES", |c| {
                c.data_disk_min_free_bytes = 1_099_511_627_777
            }),
            ("MAX_OPTION_MONEYNESS_DISTANCE_PERCENTAGE", |c| {
                c.max_option_moneyness_distance_percentage = 0.0
            }),
            ("MIN_REWARD_RISK_RATIO", |c| c.min_reward_risk_ratio = 0.9),
            ("LEARNING_SLIPPAGE_BPS", |c| c.learning_slippage_bps = -1.0),
            ("VIX_REFRESH_SECS", |c| c.vix_refresh_secs = 0),
            ("VIX_MAX_AGE_SECS", |c| c.vix_max_age_secs = 59),
            ("VIX_PREVIOUS_CLOSE_MAX_AGE_SECS", |c| {
                c.vix_previous_close_max_age_secs = 59
            }),
            ("VIX_ELEVATED_LEVEL", |c| c.vix_elevated_level = 4.9),
            ("VIX_SPIKE_CHANGE_PERCENTAGE", |c| {
                c.vix_spike_change_percentage = 0.0
            }),
            ("VIX_ELEVATED_POSITION_FACTOR", |c| {
                c.vix_elevated_position_factor = 0.0
            }),
            ("VIX_SPIKE_THRESHOLD_BONUS", |c| {
                c.vix_spike_threshold_bonus = 0.5
            }),
            ("LIVE_LEARNING_MIN_PROFIT_FACTOR", |c| {
                c.live_learning_min_profit_factor = 0.9
            }),
            ("CANARY_MAX_INVESTMENT_AMOUNT", |c| {
                c.canary_max_investment_amount = 0.0
            }),
            ("CANARY_MAX_LOSS_PER_TRADE", |c| {
                c.canary_max_loss_per_trade = 0.0
            }),
            ("CANARY_MAX_DAILY_LOSS", |c| c.canary_max_daily_loss = 0.0),
            ("TIME_REFERENCE_REFRESH_SECS", |c| {
                c.time_reference_refresh_secs = 29
            }),
            ("ORDER_TRACKING_TIMEOUT_SECS", |c| {
                c.order_tracking_timeout_secs = 0
            }),
            ("ORDER_STATUS_POLL_INTERVAL_MILLIS", |c| {
                c.order_status_poll_interval_millis = 99
            }),
            ("ORDER_CANCEL_TIMEOUT_SECS", |c| {
                c.order_cancel_timeout_secs = 0
            }),
            ("DYNAMIC_LIMIT_STEPS", |c| c.dynamic_limit_steps = 0),
            ("DYNAMIC_LIMIT_FRAME_WAIT_SECS", |c| {
                c.dynamic_limit_frame_wait_secs = 0
            }),
            ("DYNAMIC_LIMIT_QUEUE_AHEAD_FACTOR", |c| {
                c.dynamic_limit_queue_ahead_factor = -1.0
            }),
            ("DYNAMIC_LIMIT_ADVERSE_SELECTION_BPS", |c| {
                c.dynamic_limit_adverse_selection_bps = -1.0
            }),
            ("OPTION_BINOMIAL_STEPS", |c| c.option_binomial_steps = 24),
            ("OPTION_RISK_FREE_RATE", |c| c.option_risk_free_rate = -0.6),
            ("OPTION_DIVIDEND_YIELD", |c| c.option_dividend_yield = -0.1),
            ("OPTION_MARKET_INPUTS_MAX_AGE_SECS", |c| {
                c.option_market_inputs_max_age_secs = 0
            }),
            ("OPTION_MIN_IMPLIED_VOLATILITY", |c| {
                c.option_min_implied_volatility = 0.0
            }),
            ("OPTION_MAX_IMPLIED_VOLATILITY", |c| {
                c.option_max_implied_volatility = 5.1
            }),
            ("OPTION_MAX_EXTRINSIC_PERCENTAGE", |c| {
                c.option_max_extrinsic_percentage = 101.0
            }),
            ("IV_RANK_WINDOW_SESSIONS", |c| c.iv_rank_window_sessions = 1),
            ("TREE_META_FILTER_MIN_IMPROVEMENT", |c| {
                c.tree_meta_filter_min_improvement = -1.0
            }),
            ("MAX_FRICTION_STOP_RATIO", |c| {
                c.max_friction_stop_ratio = 0.0
            }),
            ("TARGET_UNDERLYING_VOLATILITY_PERCENTAGE", |c| {
                c.target_underlying_volatility_percentage = 0.0
            }),
            ("MAX_CONCURRENT_REQUESTS", |c| c.max_concurrent_requests = 0),
            ("CACHE_TTL_SECS", |c| c.cache_ttl_secs = 0),
        ];

        for (expected, mutate) in mutations {
            let mut candidate = config();
            mutate(&mut candidate);
            assert_invalid_name(&candidate, expected);
        }
    }

    #[test]
    fn compound_time_and_metadata_guards_cover_each_rejection_cause() {
        let mutations: &[ConfigMutation] = &[
            ("WEEKEND_RISK_TIMES", |c| {
                c.pre_break_last_entry_minute = 10 * 60 + 29
            }),
            ("WEEKEND_RISK_TIMES", |c| {
                c.pre_break_force_exit_minute = 17 * 60
            }),
            ("WEEKEND_RISK_TIMES", |c| {
                c.expiry_day_force_exit_minute = 10 * 60 + 29
            }),
            ("LUNCH_SLOWDOWN_TIMES", |c| {
                c.lunch_slowdown_start_minute = 10 * 60 + 29
            }),
            ("LUNCH_SLOWDOWN_TIMES", |c| {
                c.lunch_slowdown_end_minute = 16 * 60 + 59;
                c.post_lunch_confirmation_mins = 2;
            }),
            ("OPTION_MARKET_INPUTS", |c| {
                c.option_analytics_enabled = true;
                c.option_market_inputs_observed_at_secs = Some(1);
                c.option_dividend_source.clear();
            }),
            ("TICKER", |c| c.ticker.clear()),
            ("TICKER", |c| c.ticker = "ABCDEFGHIJKLM".into()),
        ];

        for (expected, mutate) in mutations {
            let mut candidate = config();
            mutate(&mut candidate);
            assert_invalid_name(&candidate, expected);
        }
    }

    #[test]
    fn legacy_float_parser_has_explicit_precedence_and_errors() {
        const PRIMARY: &str = "OPTIONS_TEST_PRIMARY_FLOAT";
        const LEGACY: &str = "OPTIONS_TEST_LEGACY_FLOAT";
        unsafe {
            env::remove_var(PRIMARY);
            env::set_var(LEGACY, "12.5");
        }
        assert_eq!(parse_f64_with_legacy(PRIMARY, LEGACY, 1.0).unwrap(), 12.5);

        unsafe { env::set_var(PRIMARY, "7.5") };
        assert_eq!(parse_f64_with_legacy(PRIMARY, LEGACY, 1.0).unwrap(), 7.5);
        unsafe { env::set_var(PRIMARY, "invalid") };
        assert!(parse_f64_with_legacy(PRIMARY, LEGACY, 1.0).is_err());

        unsafe {
            env::remove_var(PRIMARY);
            env::remove_var(LEGACY);
        }
        assert_eq!(parse_f64_with_legacy(PRIMARY, LEGACY, 3.5).unwrap(), 3.5);
    }

    #[test]
    fn environment_presence_is_nonblank_and_confirmation_is_exact() {
        let _guard = CONFIG_ENVIRONMENT_LOCK.lock().unwrap();
        const NAMES: &[&str] = &[
            "REPLAY_PATH",
            "MARKET_SESSIONS_PATH",
            "VIX_QUOTE_URL",
            "TIME_REFERENCE_URL",
            "IOL_ORDER_PATH",
            "LIVE_READINESS_PATH",
            "LIVE_AUTHORIZATION_PATH",
            "OPTIONS_MASTER_KEY_PATH",
            "LIVE_TRADING_CONFIRMATION",
            "IOL_USERNAME",
            "IOL_PASSWORD",
        ];
        let previous = NAMES
            .iter()
            .map(|name| (*name, env::var_os(name)))
            .collect::<Vec<_>>();

        unsafe {
            env::set_var("REPLAY_PATH", "/tmp/options-config-replay.jsonl");
            for name in &NAMES[1..8] {
                env::set_var(name, " \t ");
            }
            env::set_var("LIVE_TRADING_CONFIRMATION", LIVE_CONFIRMATION);
            env::remove_var("IOL_USERNAME");
            env::remove_var("IOL_PASSWORD");
        }
        let parsed = Config::from_env().unwrap();
        assert_eq!(
            parsed.replay_path,
            Some(PathBuf::from("/tmp/options-config-replay.jsonl"))
        );
        assert!(parsed.market_sessions_path.is_none());
        assert!(parsed.vix_quote_url.is_none());
        assert!(parsed.time_reference_url.is_none());
        assert!(parsed.iol_order_path.is_none());
        assert!(parsed.live_readiness_path.is_none());
        assert!(parsed.live_authorization_path.is_none());
        assert!(parsed.master_key_path.is_none());
        assert!(parsed.live_confirmed);

        unsafe { env::set_var("LIVE_TRADING_CONFIRMATION", format!("{LIVE_CONFIRMATION} ")) };
        assert!(!Config::from_env().unwrap().live_confirmed);

        unsafe {
            env::set_var("REPLAY_PATH", "  ");
            env::set_var("IOL_USERNAME", "user");
            env::set_var("IOL_PASSWORD", " ");
        }
        assert!(matches!(
            Config::from_env(),
            Err(ConfigError::MissingSecret("IOL_USERNAME/IOL_PASSWORD"))
        ));
        unsafe {
            env::set_var("IOL_USERNAME", " ");
            env::set_var("IOL_PASSWORD", "v3:payload");
        }
        assert!(matches!(
            Config::from_env(),
            Err(ConfigError::MissingSecret("IOL_USERNAME/IOL_PASSWORD"))
        ));
        unsafe {
            env::set_var("IOL_USERNAME", "user");
            env::set_var("IOL_PASSWORD", "v3:payload");
        }
        let parsed = Config::from_env().unwrap();
        assert!(parsed.replay_path.is_none());

        for (name, value) in previous {
            match value {
                Some(value) => unsafe { env::set_var(name, value) },
                None => unsafe { env::remove_var(name) },
            }
        }
    }

    #[test]
    fn from_env_attributes_every_malformed_typed_value_to_its_source() {
        let _guard = CONFIG_ENVIRONMENT_LOCK.lock().unwrap();
        let previous_replay = env::var_os("REPLAY_PATH");
        unsafe { env::set_var("REPLAY_PATH", "/tmp/options-config-parse-test.jsonl") };

        assert_invalid_environment_value("MODE", "unsupported");

        for name in [
            "CHECK_INTERVAL_SECS",
            "PRICE_HISTORY_MINUTES",
            "MIN_SAMPLES_FOR_TREND",
            "TREND_CHANGE_SAMPLES",
            "REVERSAL_COOLDOWN_SECS",
            "OPTION_EXPIRY_DAYS",
            "MAX_POSITION_SIZE",
            "POSITION_TIMEOUT_MINS",
            "MAX_CONCURRENT_REQUESTS",
            "CACHE_TTL_SECS",
            "DATA_DIR_MAX_BYTES",
            "DATA_DISK_MIN_FREE_BYTES",
            "MARKET_CAPTURE_RETENTION_DAYS",
            "ENTRY_DELAY_AFTER_OPEN_MINS",
            "POST_LUNCH_CONFIRMATION_MINS",
            "LUNCH_LIQUIDITY_WINDOW_MINS",
            "LUNCH_MIN_QUOTE_UPDATES",
            "CONNECTION_RETRY_ATTEMPTS",
            "CONNECTION_RETRY_DELAY_SECS",
            "MAX_TRADES_PER_DAY",
            "CONTRACT_MULTIPLIER",
            "MAX_MARKET_DATA_AGE_SECS",
            "MIN_OPTION_VOLUME",
            "MIN_OPTION_CHAIN_CONTRACTS_PER_SIDE",
            "OPTION_TARGET_EXPIRY_DAYS",
            "OPTION_MAX_EXPIRY_DAYS",
            "VIX_REFRESH_SECS",
            "VIX_MAX_AGE_SECS",
            "VIX_PREVIOUS_CLOSE_MAX_AGE_SECS",
            "LIVE_LEARNING_MIN_TRADES",
            "LIVE_LEARNING_MIN_CALL_TRADES",
            "LIVE_LEARNING_MIN_PUT_TRADES",
            "LIVE_LEARNING_MIN_SESSIONS",
            "LIVE_REGRESSION_WINDOW_TRADES",
            "LIVE_MAX_CONSECUTIVE_LOSSES",
            "CANARY_MIN_TRADES",
            "CANARY_MIN_CALL_TRADES",
            "CANARY_MIN_PUT_TRADES",
            "CANARY_MIN_SESSIONS",
            "CANARY_MAX_POSITION_SIZE",
            "CANARY_MAX_TRADES_PER_DAY",
            "TIME_REFERENCE_REFRESH_SECS",
            "TIME_REFERENCE_MAX_SKEW_SECS",
            "ORDER_TRACKING_TIMEOUT_SECS",
            "ORDER_STATUS_POLL_INTERVAL_MILLIS",
            "ORDER_CANCEL_TIMEOUT_SECS",
            "DYNAMIC_LIMIT_STEPS",
            "DYNAMIC_LIMIT_FRAME_WAIT_SECS",
            "OPTION_MARKET_INPUTS_OBSERVED_AT_SECS",
            "OPTION_MARKET_INPUTS_MAX_AGE_SECS",
            "OPTION_BINOMIAL_STEPS",
            "IV_RANK_WINDOW_SESSIONS",
            "IV_RANK_MIN_SESSIONS",
            "META_FILTER_MIN_EXAMPLES",
            "META_FILTER_MIN_TRAIN_EXAMPLES",
            "META_FILTER_MIN_ACCEPTED_HOLDOUT",
        ] {
            assert_invalid_environment_value(name, "not-an-integer");
        }

        for name in [
            "TREND_DEADBAND_PERCENTAGE",
            "MIN_TREND_SLOPE_PERCENT_PER_MINUTE",
            "MIN_TREND_R_SQUARED",
            "MIN_TREND_MOVE_VOLATILITY_RATIO",
            "COMMISSION_PERCENTAGE",
            "VAT_PERCENTAGE",
            "OTHER_FEES_PERCENTAGE",
            "TAX_PERCENTAGE",
            "MIN_PROFIT_MULTIPLIER",
            "MAX_INVESTMENT_AMOUNT",
            "MAX_LOSS_PER_TRADE",
            "MAX_DAILY_LOSS",
            "STOP_LOSS_PERCENTAGE",
            "READONLY_SLIPPAGE_BPS",
            "MAX_OPTION_SPREAD_PERCENTAGE",
            "MIN_OPTION_CHAIN_ACCEPTANCE_PERCENTAGE",
            "MAX_OPTION_MONEYNESS_DISTANCE_PERCENTAGE",
            "MIN_REWARD_RISK_RATIO",
            "LEARNING_SLIPPAGE_BPS",
            "VIX_ELEVATED_LEVEL",
            "VIX_SPIKE_CHANGE_PERCENTAGE",
            "VIX_ELEVATED_POSITION_FACTOR",
            "VIX_SPIKE_THRESHOLD_BONUS",
            "LIVE_LEARNING_MIN_PROFIT_FACTOR",
            "CANARY_MAX_INVESTMENT_AMOUNT",
            "CANARY_MAX_LOSS_PER_TRADE",
            "CANARY_MAX_DAILY_LOSS",
            "DYNAMIC_LIMIT_QUEUE_AHEAD_FACTOR",
            "DYNAMIC_LIMIT_ADVERSE_SELECTION_BPS",
            "OPTION_RISK_FREE_RATE",
            "OPTION_DIVIDEND_YIELD",
            "OPTION_MIN_ABS_DELTA",
            "OPTION_MAX_ABS_DELTA",
            "OPTION_MIN_IMPLIED_VOLATILITY",
            "OPTION_MAX_IMPLIED_VOLATILITY",
            "OPTION_MAX_EXTRINSIC_PERCENTAGE",
            "IV_RANK_MIN",
            "IV_RANK_MAX",
            "MAX_FRICTION_STOP_RATIO",
            "TARGET_UNDERLYING_VOLATILITY_PERCENTAGE",
            "META_FILTER_MIN_COVERAGE",
            "META_FILTER_MAX_BRIER_SCORE",
            "META_FILTER_MIN_POSITIVE_FOLD_RATIO",
            "META_FILTER_MAX_CONCENTRATION",
            "TREE_META_FILTER_MIN_IMPROVEMENT",
        ] {
            assert_invalid_environment_value(name, "not-a-decimal");
        }

        for name in [
            "TUI_ENABLED",
            "RECOVER_STATE",
            "CAPTURE_MARKET_DATA",
            "WEEKEND_RISK_ENABLED",
            "LUNCH_SLOWDOWN_ENABLED",
            "CONTRACT_MULTIPLIER_CONFIRMED",
            "IOL_WEBSOCKET_ENABLED",
            "DYNAMIC_LIMIT_ENABLED",
            "OPTION_ANALYTICS_ENABLED",
            "IV_RANK_FILTER_ENABLED",
            "ADAPTIVE_ENTRY_FILTER_ENABLED",
            "VOLATILITY_NORMALIZED_SIGNALS_ENABLED",
            "NONLINEAR_META_FILTER_ENABLED",
            "TREE_META_FILTER_ENABLED",
            "EXPERIMENT_RUNNER_ENABLED",
            "VERTICAL_SPREAD_RESEARCH_ENABLED",
            "VERTICAL_ATOMIC_EXECUTION_VERIFIED",
        ] {
            assert_invalid_environment_value(name, "not-a-boolean");
        }

        for name in [
            "PRE_BREAK_LAST_ENTRY_TIME",
            "PRE_BREAK_FORCE_EXIT_TIME",
            "EXPIRY_DAY_FORCE_EXIT_TIME",
            "LUNCH_SLOWDOWN_START_TIME",
            "LUNCH_SLOWDOWN_END_TIME",
        ] {
            assert_invalid_environment_value(name, "not-a-time");
        }

        match previous_replay {
            Some(previous) => unsafe { env::set_var("REPLAY_PATH", previous) },
            None => unsafe { env::remove_var("REPLAY_PATH") },
        }
    }
}
