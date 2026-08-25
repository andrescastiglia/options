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
    pub max_investment_amount: f64,
    pub max_loss_per_trade: f64,
    pub max_daily_loss: f64,
    pub max_trades_per_day: u32,
    pub stop_loss_percentage: f64,
    pub contract_multiplier: u32,
    pub readonly_slippage_bps: f64,
    pub max_market_data_age_secs: u64,
    pub max_option_spread_percentage: f64,
    pub min_option_volume: u64,
    pub option_target_expiry_days: u32,
    pub option_max_expiry_days: u32,
    pub max_option_moneyness_distance_percentage: f64,
    pub min_reward_risk_ratio: f64,
    pub learning_slippage_bps: f64,
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
    pub iol_websocket_url: String,
    pub iol_order_path: Option<String>,
    pub live_authorization_path: Option<PathBuf>,
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
            replay_path: env::var("REPLAY_PATH").ok().map(PathBuf::from),
            capture_market_data: parse_bool("CAPTURE_MARKET_DATA", true)?,
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
            readonly_slippage_bps: parse_f64("READONLY_SLIPPAGE_BPS", 5.0)?,
            max_market_data_age_secs: parse_u64("MAX_MARKET_DATA_AGE_SECS", 15)?,
            max_option_spread_percentage: parse_f64("MAX_OPTION_SPREAD_PERCENTAGE", 3.0)?,
            min_option_volume: parse_u64("MIN_OPTION_VOLUME", 10)?,
            option_target_expiry_days: parse_u32("OPTION_TARGET_EXPIRY_DAYS", 21)?,
            option_max_expiry_days: parse_u32("OPTION_MAX_EXPIRY_DAYS", 45)?,
            max_option_moneyness_distance_percentage: parse_f64(
                "MAX_OPTION_MONEYNESS_DISTANCE_PERCENTAGE",
                10.0,
            )?,
            min_reward_risk_ratio: parse_f64("MIN_REWARD_RISK_RATIO", 1.25)?,
            learning_slippage_bps: parse_f64("LEARNING_SLIPPAGE_BPS", 25.0)?,
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
            iol_websocket_url: env_or(
                "IOL_WEBSOCKET_URL",
                "wss://websocket-movements.invertironline.com/",
            ),
            iol_order_path: env::var("IOL_ORDER_PATH").ok(),
            live_authorization_path: env::var("LIVE_AUTHORIZATION_PATH").ok().map(PathBuf::from),
            live_confirmed: env::var("LIVE_TRADING_CONFIRMATION")
                .is_ok_and(|value| value == LIVE_CONFIRMATION),
        };
        config.validate()?;
        if config.replay_path.is_none()
            && (env::var("IOL_USERNAME").is_err() || env::var("IOL_PASSWORD").is_err())
        {
            return Err(ConfigError::MissingSecret("IOL_USERNAME/IOL_PASSWORD"));
        }
        Ok(config)
    }

    pub fn validate(&self) -> Result<(), ConfigError> {
        bounded("CHECK_INTERVAL_SECS", self.check_interval_secs, 1, 60)?;
        bounded("PRICE_HISTORY_MINUTES", self.price_history_minutes, 1, 120)?;
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
        if self.min_option_volume == 0
            || self.option_target_expiry_days < self.option_expiry_days
            || self.option_max_expiry_days < self.option_target_expiry_days
            || self.live_learning_min_trades == 0
            || self.live_learning_min_call_trades + self.live_learning_min_put_trades
                > self.live_learning_min_trades
            || self.live_learning_min_sessions == 0
            || self.live_regression_window_trades == 0
            || self.live_max_consecutive_losses == 0
            || self.canary_min_trades == 0
            || self.canary_min_call_trades + self.canary_min_put_trades > self.canary_min_trades
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
        if self.ticker.trim().is_empty() {
            return invalid("TICKER", &self.ticker);
        }
        if self.mode.uses_iol_market_data()
            && !(self.iol_websocket_url.starts_with("wss://")
                || self.iol_websocket_url.starts_with("ws://"))
        {
            return invalid("IOL_WEBSOCKET_URL", "debe usar ws:// o wss://");
        }
        if self.max_position_size == 0
            || self.position_timeout_mins == 0
            || self.max_trades_per_day == 0
            || self.contract_multiplier == 0
        {
            return invalid("POSITION_LIMITS", "deben ser mayores que cero");
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
        self.live_confirmed
            && self
                .iol_order_path
                .as_deref()
                .is_some_and(|path| !path.trim().is_empty())
            && self.live_authorization_path.is_some()
    }
}

fn env_or(name: &str, default: &str) -> String {
    env::var(name).unwrap_or_else(|_| default.to_string())
}

fn parse_u64(name: &'static str, default: u64) -> Result<u64, ConfigError> {
    parse(name, default)
}

fn parse_u32(name: &'static str, default: u32) -> Result<u32, ConfigError> {
    parse(name, default)
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

fn invalid(name: &'static str, value: impl ToString) -> Result<(), ConfigError> {
    Err(ConfigError::InvalidValue {
        name,
        value: value.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> Config {
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
            max_investment_amount: 100_000.0,
            max_loss_per_trade: 5_000.0,
            max_daily_loss: 10_000.0,
            max_trades_per_day: 20,
            stop_loss_percentage: 15.0,
            contract_multiplier: 1,
            readonly_slippage_bps: 5.0,
            max_market_data_age_secs: 15,
            max_option_spread_percentage: 20.0,
            min_option_volume: 10,
            option_target_expiry_days: 21,
            option_max_expiry_days: 45,
            max_option_moneyness_distance_percentage: 10.0,
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

    #[test]
    fn readonly_defaults_are_valid() {
        assert!(config().validate().is_ok());
    }

    #[test]
    fn only_readonly_and_live_are_public_modes() {
        assert_eq!(Mode::parse("readonly").unwrap(), Mode::Readonly);
        assert_eq!(Mode::parse("live").unwrap(), Mode::Live);
        assert!(Mode::parse("paper").is_err());
        assert!(Mode::parse("replay").is_err());
    }

    #[test]
    fn live_ordering_requires_confirmation_and_order_contract() {
        let mut config = config();
        config.mode = Mode::Live;
        assert!(!config.live_ordering_ready());
        config.live_confirmed = true;
        assert!(!config.live_ordering_ready());
        config.iol_order_path = Some("/api/orders".into());
        assert!(!config.live_ordering_ready());
        config.live_authorization_path = Some("data/live/authorization.json".into());
        assert!(config.live_ordering_ready());
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
}
