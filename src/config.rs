use std::{env, num::ParseFloatError, num::ParseIntError, path::PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;

pub const LIVE_CONFIRMATION: &str = "I_UNDERSTAND_THIS_SENDS_REAL_ORDERS";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Mode {
    Replay,
    Paper,
    Live,
}

impl Mode {
    fn parse(value: &str) -> Result<Self, ConfigError> {
        match value.to_ascii_lowercase().as_str() {
            "fake" | "sim" | "simulation" | "replay" => Ok(Self::Replay),
            "paper" => Ok(Self::Paper),
            "live" => Ok(Self::Live),
            other => Err(ConfigError::InvalidValue {
                name: "MODE",
                value: other.to_string(),
            }),
        }
    }

    pub fn uses_iol_market_data(self) -> bool {
        matches!(self, Self::Paper | Self::Live)
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
    pub commission_percentage: f64,
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
    pub max_investment_amount: f64,
    pub max_loss_per_trade: f64,
    pub max_daily_loss: f64,
    pub max_trades_per_day: u32,
    pub stop_loss_percentage: f64,
    pub contract_multiplier: u32,
    pub paper_slippage_bps: f64,
    pub max_market_data_age_secs: u64,
    pub max_option_spread_percentage: f64,
    pub iol_base_url: String,
    pub iol_order_path: Option<String>,
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
        let mode = Mode::parse(&env_or("MODE", "replay"))?;
        let config = Self {
            mode,
            ticker: env_or("TICKER", "GAL"),
            check_interval_secs: parse_u64("CHECK_INTERVAL_SECS", 1)?,
            price_history_minutes: parse_u64("PRICE_HISTORY_MINUTES", 30)?,
            min_samples_for_trend: parse_usize("MIN_SAMPLES_FOR_TREND", 5)?,
            trend_change_samples: parse_usize("TREND_CHANGE_SAMPLES", 3)?,
            commission_percentage: parse_f64("COMMISSION_PERCENTAGE", 0.19)?,
            tax_percentage: parse_f64("TAX_PERCENTAGE", 35.0)?,
            min_profit_multiplier: parse_f64("MIN_PROFIT_MULTIPLIER", 2.0)?,
            option_expiry_days: parse_u32("OPTION_EXPIRY_DAYS", 1)?,
            max_position_size: parse_u32("MAX_POSITION_SIZE", 5)?,
            position_timeout_mins: parse_u64("POSITION_TIMEOUT_MINS", 60)?,
            max_concurrent_requests: parse_usize("MAX_CONCURRENT_REQUESTS", 10)?,
            cache_ttl_secs: parse_u64("CACHE_TTL_SECS", 60)?,
            log_level: env_or("LOG_LEVEL", "info"),
            tui_enabled: parse_bool("TUI_ENABLED", true)?,
            recover_state: parse_bool("RECOVER_STATE", mode != Mode::Replay)?,
            data_dir: PathBuf::from(env_or("DATA_DIR", "data")),
            replay_path: env::var("REPLAY_PATH").ok().map(PathBuf::from),
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
            paper_slippage_bps: parse_f64("PAPER_SLIPPAGE_BPS", 5.0)?,
            max_market_data_age_secs: parse_u64("MAX_MARKET_DATA_AGE_SECS", 15)?,
            max_option_spread_percentage: parse_f64("MAX_OPTION_SPREAD_PERCENTAGE", 20.0)?,
            iol_base_url: env_or("IOL_BASE_URL", "https://api.invertironline.com"),
            iol_order_path: env::var("IOL_ORDER_PATH").ok(),
            live_confirmed: env::var("LIVE_TRADING_CONFIRMATION")
                .is_ok_and(|value| value == LIVE_CONFIRMATION),
        };
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<(), ConfigError> {
        bounded("CHECK_INTERVAL_SECS", self.check_interval_secs, 1, 60)?;
        bounded("PRICE_HISTORY_MINUTES", self.price_history_minutes, 1, 120)?;
        bounded("MIN_SAMPLES_FOR_TREND", self.min_samples_for_trend, 2, 100)?;
        bounded("TREND_CHANGE_SAMPLES", self.trend_change_samples, 2, 100)?;
        bounded_float(
            "COMMISSION_PERCENTAGE",
            self.commission_percentage,
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
        bounded_float("PAPER_SLIPPAGE_BPS", self.paper_slippage_bps, 0.0, 1_000.0)?;
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
        if self.ticker.trim().is_empty() {
            return invalid("TICKER", &self.ticker);
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
        if self.mode.uses_iol_market_data()
            && (env::var("IOL_USERNAME").is_err() || env::var("IOL_PASSWORD").is_err())
        {
            return Err(ConfigError::MissingSecret("IOL_USERNAME/IOL_PASSWORD"));
        }
        if self.mode == Mode::Live {
            if !self.live_confirmed {
                return invalid(
                    "LIVE_TRADING_CONFIRMATION",
                    "confirmacion explicita ausente",
                );
            }
            if self.iol_order_path.as_deref().is_none_or(str::is_empty) {
                return invalid("IOL_ORDER_PATH", "contrato de orden no configurado");
            }
        }
        Ok(())
    }

    pub fn history_capacity(&self) -> usize {
        ((self.price_history_minutes * 60) / self.check_interval_secs)
            .max(self.min_samples_for_trend as u64) as usize
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
            mode: Mode::Replay,
            ticker: "GAL".into(),
            check_interval_secs: 5,
            price_history_minutes: 30,
            min_samples_for_trend: 5,
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
            tui_enabled: true,
            recover_state: false,
            data_dir: PathBuf::from("data"),
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

    #[test]
    fn replay_defaults_are_valid() {
        assert!(config().validate().is_ok());
    }

    #[test]
    fn live_requires_confirmation_and_order_contract() {
        let mut config = config();
        config.mode = Mode::Live;
        assert!(config.validate().is_err());
    }

    #[test]
    fn history_capacity_uses_time_window() {
        assert_eq!(config().history_capacity(), 360);
    }
}
