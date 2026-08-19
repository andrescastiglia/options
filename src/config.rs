use std::{env, num::ParseFloatError, num::ParseIntError};

use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Fake,
    Live,
}

impl Mode {
    fn parse(value: &str) -> Result<Self, ConfigError> {
        match value.to_ascii_lowercase().as_str() {
            "fake" | "sim" | "simulation" => Ok(Self::Fake),
            "live" => Ok(Self::Live),
            other => Err(ConfigError::InvalidValue {
                name: "MODE",
                value: other.to_string(),
            }),
        }
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
        let mode = Mode::parse(&env_or("MODE", "fake"))?;
        let config = Self {
            mode,
            ticker: env_or("TICKER", "GAL"),
            check_interval_secs: parse_u64("CHECK_INTERVAL_SECS", 5)?,
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
        };
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<(), ConfigError> {
        bounded("CHECK_INTERVAL_SECS", self.check_interval_secs, 1, 60)?;
        bounded("PRICE_HISTORY_MINUTES", self.price_history_minutes, 1, 120)?;
        bounded("MIN_SAMPLES_FOR_TREND", self.min_samples_for_trend, 2, 100)?;
        bounded("TREND_CHANGE_SAMPLES", self.trend_change_samples, 1, 100)?;
        bounded_float(
            "COMMISSION_PERCENTAGE",
            self.commission_percentage,
            0.01,
            1.0,
        )?;
        bounded_float("TAX_PERCENTAGE", self.tax_percentage, 0.0, 100.0)?;
        bounded_float(
            "MIN_PROFIT_MULTIPLIER",
            self.min_profit_multiplier,
            1.0,
            10.0,
        )?;
        if self.ticker.trim().is_empty() {
            return Err(ConfigError::InvalidValue {
                name: "TICKER",
                value: self.ticker.clone(),
            });
        }
        if self.max_position_size == 0 || self.position_timeout_mins == 0 {
            return Err(ConfigError::InvalidValue {
                name: "POSITION_LIMITS",
                value: "deben ser mayores que cero".to_string(),
            });
        }
        bounded(
            "MAX_CONCURRENT_REQUESTS",
            self.max_concurrent_requests,
            1,
            100,
        )?;
        bounded("CACHE_TTL_SECS", self.cache_ttl_secs, 1, 86_400)?;
        if !matches!(self.log_level.as_str(), "debug" | "info" | "warn" | "error") {
            return Err(ConfigError::InvalidValue {
                name: "LOG_LEVEL",
                value: self.log_level.clone(),
            });
        }
        if self.mode == Mode::Live
            && (env::var("IOL_USERNAME").is_err() || env::var("IOL_PASSWORD").is_err())
        {
            return Err(ConfigError::MissingSecret("IOL_USERNAME/IOL_PASSWORD"));
        }
        Ok(())
    }
}

fn env_or(name: &str, default: &str) -> String {
    env::var(name).unwrap_or_else(|_| default.to_string())
}

fn parse_u64(name: &'static str, default: u64) -> Result<u64, ConfigError> {
    env::var(name)
        .unwrap_or_else(|_| default.to_string())
        .parse()
        .map_err(|source| ConfigError::Number {
            name,
            source: NumericError::Int(source),
        })
}

fn parse_u32(name: &'static str, default: u32) -> Result<u32, ConfigError> {
    env::var(name)
        .unwrap_or_else(|_| default.to_string())
        .parse()
        .map_err(|source| ConfigError::Number {
            name,
            source: NumericError::Int(source),
        })
}

fn parse_usize(name: &'static str, default: usize) -> Result<usize, ConfigError> {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_valid_and_fake() {
        let config = Config {
            mode: Mode::Fake,
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
        };
        assert!(config.validate().is_ok());
    }
}
