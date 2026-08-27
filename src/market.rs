use std::{
    collections::{HashMap, VecDeque},
    io::{BufRead, BufReader, Cursor},
    path::Path,
    time::{Duration, Instant},
};

use serde::{Deserialize, Serialize};

use crate::{errors::AppError, pattern::PriceSample, secure_fs::open_limited_read};

const MAX_REPLAY_BYTES: u64 = 1024 * 1024 * 1024;
const MAX_REPLAY_FRAMES: usize = 1_000_000;
pub const MAX_SOURCE_CLOCK_SKEW_SECS: i64 = 300;
pub const MARKET_CAPTURE_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum OptionKind {
    Call,
    Put,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExerciseStyle {
    American,
    European,
    #[default]
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UnderlyingQuote {
    pub ticker: String,
    pub last: f64,
    pub bid: Option<f64>,
    pub ask: Option<f64>,
    pub timestamp_secs: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exchange_timestamp_secs: Option<i64>,
    #[serde(default, skip_serializing_if = "is_zero_i64")]
    pub received_at_secs: i64,
    #[serde(default, skip_serializing_if = "is_legacy_timestamp_source")]
    pub timestamp_source: QuoteTimestampSource,
}

impl UnderlyingQuote {
    pub fn validate(&self, previous_timestamp: Option<i64>) -> Result<(), AppError> {
        validate_book(self.last, self.bid, self.ask)?;
        if previous_timestamp.is_some_and(|previous| self.timestamp_secs < previous) {
            return Err(AppError::InvalidMarketData(
                "timestamp fuera de orden".into(),
            ));
        }
        if self.ticker.trim().is_empty() {
            return Err(AppError::InvalidMarketData("ticker vacio".into()));
        }
        validate_quote_timestamp_provenance(
            &self.ticker,
            self.timestamp_secs,
            self.exchange_timestamp_secs,
            self.received_at_secs,
            self.timestamp_source,
        )?;
        Ok(())
    }

    pub fn validate_freshness(&self, now_secs: i64, max_age_secs: u64) -> Result<(), AppError> {
        validate_timestamp_freshness(&self.ticker, self.timestamp_secs, now_secs, max_age_secs)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OptionQuote {
    pub symbol: String,
    pub underlying: String,
    pub kind: OptionKind,
    pub strike: f64,
    pub expiry_days: u32,
    #[serde(default)]
    pub expiration_timestamp_secs: Option<i64>,
    #[serde(default)]
    pub catalog_contract_multiplier: Option<u32>,
    #[serde(default)]
    pub catalog_observed_at_secs: Option<i64>,
    #[serde(default, skip_serializing_if = "is_zero_u32")]
    pub catalog_schema_version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub catalog_sha256: Option<[u8; 32]>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub catalog_archived: bool,
    #[serde(default)]
    pub contract_metadata_source: ContractMetadataSource,
    #[serde(default)]
    pub exercise_style: ExerciseStyle,
    pub last: f64,
    pub bid: Option<f64>,
    pub ask: Option<f64>,
    pub volume: u64,
    pub timestamp_secs: i64,
    #[serde(default)]
    pub exchange_timestamp_secs: Option<i64>,
    #[serde(default)]
    pub received_at_secs: i64,
    #[serde(default)]
    pub timestamp_source: QuoteTimestampSource,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContractMetadataSource {
    IolCatalog,
    #[default]
    Legacy,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum QuoteTimestampSource {
    Exchange,
    Received,
    #[default]
    Legacy,
}

fn is_zero_i64(value: &i64) -> bool {
    *value == 0
}

fn is_zero_u32(value: &u32) -> bool {
    *value == 0
}

fn is_false(value: &bool) -> bool {
    !*value
}

fn is_legacy_timestamp_source(value: &QuoteTimestampSource) -> bool {
    *value == QuoteTimestampSource::Legacy
}

impl OptionQuote {
    pub fn validate(&self, expected_underlying: &str) -> Result<(), AppError> {
        if self.underlying != expected_underlying || self.symbol.trim().is_empty() {
            return Err(AppError::InvalidMarketData(
                "contrato de opcion inconsistente".into(),
            ));
        }
        if !self.strike.is_finite() || self.strike <= 0.0 {
            return Err(AppError::InvalidMarketData("strike invalido".into()));
        }
        validate_quote_timestamp_provenance(
            &self.symbol,
            self.timestamp_secs,
            self.exchange_timestamp_secs,
            self.received_at_secs,
            self.timestamp_source,
        )?;
        validate_book(self.last, self.bid, self.ask)
    }

    pub fn executable_buy_price(&self) -> Option<f64> {
        self.ask.filter(|price| price.is_finite() && *price > 0.0)
    }

    pub fn executable_sell_price(&self) -> Option<f64> {
        self.bid.filter(|price| price.is_finite() && *price > 0.0)
    }

    pub fn spread_percentage(&self) -> Option<f64> {
        let (bid, ask) = (self.executable_sell_price()?, self.executable_buy_price()?);
        let midpoint = (bid + ask) / 2.0;
        Some(((ask - bid) / midpoint) * 100.0)
    }

    pub fn validate_freshness(&self, now_secs: i64, max_age_secs: u64) -> Result<(), AppError> {
        validate_timestamp_freshness(&self.symbol, self.timestamp_secs, now_secs, max_age_secs)
    }

    pub fn validate_entry_quality(
        &self,
        now_secs: i64,
        max_age_secs: u64,
        max_spread_percentage: f64,
    ) -> Result<(), AppError> {
        self.validate_freshness(now_secs, max_age_secs)?;
        let spread = self.spread_percentage().ok_or_else(|| {
            AppError::InvalidMarketData(format!("{} no tiene bid/ask ejecutables", self.symbol))
        })?;
        if spread > max_spread_percentage {
            return Err(AppError::InvalidMarketData(format!(
                "spread de {} {:.2}% excede el maximo {:.2}%",
                self.symbol, spread, max_spread_percentage
            )));
        }
        Ok(())
    }
}

pub fn option_friction_percentage(
    option: &OptionQuote,
    operating_cost_percentage: f64,
    slippage_bps: f64,
) -> Option<f64> {
    if !operating_cost_percentage.is_finite()
        || operating_cost_percentage < 0.0
        || !slippage_bps.is_finite()
        || slippage_bps < 0.0
    {
        return None;
    }
    Some(option.spread_percentage()? + 2.0 * operating_cost_percentage + 2.0 * slippage_bps / 100.0)
}

pub fn moneyness_distance_percentage(strike: f64, underlying: f64) -> Option<f64> {
    if !strike.is_finite() || strike <= 0.0 || !underlying.is_finite() || underlying <= 0.0 {
        return None;
    }
    Some((strike - underlying).abs() / underlying * 100.0)
}

fn validate_quote_timestamp_provenance(
    label: &str,
    timestamp_secs: i64,
    exchange_timestamp_secs: Option<i64>,
    received_at_secs: i64,
    timestamp_source: QuoteTimestampSource,
) -> Result<(), AppError> {
    if timestamp_source == QuoteTimestampSource::Exchange {
        let exchange = exchange_timestamp_secs.ok_or_else(|| {
            AppError::InvalidMarketData(format!(
                "{label} declara hora de mercado sin timestamp de mercado"
            ))
        })?;
        if timestamp_secs != exchange || received_at_secs <= 0 {
            return Err(AppError::InvalidMarketData(format!(
                "timestamps inconsistentes para {label}"
            )));
        }
        let skew = exchange.abs_diff(received_at_secs);
        if skew > MAX_SOURCE_CLOCK_SKEW_SECS as u64 {
            return Err(AppError::InvalidMarketData(format!(
                "desvío de reloj para {label}: {skew}s"
            )));
        }
    } else if timestamp_source == QuoteTimestampSource::Received
        && (exchange_timestamp_secs.is_some()
            || received_at_secs <= 0
            || timestamp_secs != received_at_secs)
    {
        return Err(AppError::InvalidMarketData(format!(
            "timestamps de recepción inconsistentes para {label}"
        )));
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MarketFrame {
    pub underlying: UnderlyingQuote,
    pub options: Vec<OptionQuote>,
    #[serde(default)]
    pub option_chain_quality: Option<OptionChainQuality>,
    #[serde(default)]
    pub vix: Option<VixObservation>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OptionChainQuality {
    pub catalog_contracts: usize,
    pub quote_rows: usize,
    pub accepted_contracts: usize,
    pub missing_quote_contracts: usize,
    pub invalid_quote_contracts: usize,
    pub accepted_call_contracts: usize,
    pub accepted_put_contracts: usize,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub by_expiry: Vec<OptionExpiryQuality>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct OptionExpiryQuality {
    pub expiry_days: u32,
    pub catalog_contracts: usize,
    pub accepted_contracts: usize,
    pub missing_quote_contracts: usize,
    pub invalid_quote_contracts: usize,
    pub accepted_call_contracts: usize,
    pub accepted_put_contracts: usize,
}

impl OptionChainQuality {
    pub fn acceptance_percentage(&self) -> f64 {
        if self.catalog_contracts == 0 {
            return 0.0;
        }
        self.accepted_contracts as f64 / self.catalog_contracts as f64 * 100.0
    }

    pub fn allows_entry(
        &self,
        minimum_acceptance_percentage: f64,
        minimum_contracts_per_side: usize,
    ) -> bool {
        self.acceptance_percentage() >= minimum_acceptance_percentage
            && self.accepted_call_contracts >= minimum_contracts_per_side
            && self.accepted_put_contracts >= minimum_contracts_per_side
    }

    pub fn allows_entry_for_tenor(
        &self,
        minimum_expiry_days: u32,
        maximum_expiry_days: u32,
        minimum_acceptance_percentage: f64,
        minimum_contracts_per_side: usize,
    ) -> bool {
        let Some((catalog, accepted, calls, puts)) =
            self.tenor_totals(minimum_expiry_days, maximum_expiry_days)
        else {
            return false;
        };
        accepted as f64 / catalog as f64 * 100.0 >= minimum_acceptance_percentage
            && calls >= minimum_contracts_per_side
            && puts >= minimum_contracts_per_side
    }

    pub fn tenor_totals(
        &self,
        minimum_expiry_days: u32,
        maximum_expiry_days: u32,
    ) -> Option<(usize, usize, usize, usize)> {
        if self.by_expiry.is_empty() {
            // Compatibilidad exclusiva con captures v1/replays previos al desglose.
            return (self.catalog_contracts > 0).then_some((
                self.catalog_contracts,
                self.accepted_contracts,
                self.accepted_call_contracts,
                self.accepted_put_contracts,
            ));
        }
        let mut totals = (0_usize, 0_usize, 0_usize, 0_usize);
        for quality in self.by_expiry.iter().filter(|quality| {
            quality.expiry_days >= minimum_expiry_days && quality.expiry_days <= maximum_expiry_days
        }) {
            totals.0 = totals.0.checked_add(quality.catalog_contracts)?;
            totals.1 = totals.1.checked_add(quality.accepted_contracts)?;
            totals.2 = totals.2.checked_add(quality.accepted_call_contracts)?;
            totals.3 = totals.3.checked_add(quality.accepted_put_contracts)?;
        }
        (totals.0 > 0).then_some(totals)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CapturedMarketFrame {
    pub schema_version: u32,
    pub source: String,
    pub captured_at_secs: i64,
    pub frame_sha256: String,
    pub frame: MarketFrame,
}

impl CapturedMarketFrame {
    pub fn new(source: &str, captured_at_secs: i64, frame: MarketFrame) -> Result<Self, AppError> {
        if source.trim().is_empty() || captured_at_secs <= 0 {
            return Err(AppError::InvalidMarketData(
                "procedencia de capture inválida".into(),
            ));
        }
        // Normalizar una vez mediante JSON hace que la representación de floats
        // sea un punto fijo antes de calcular el hash que luego verificará replay.
        let frame: MarketFrame = serde_json::from_slice(&serde_json::to_vec(&frame)?)?;
        let frame_sha256 = market_frame_sha256(&frame)?;
        Ok(Self {
            schema_version: MARKET_CAPTURE_SCHEMA_VERSION,
            source: source.into(),
            captured_at_secs,
            frame_sha256,
            frame,
        })
    }

    pub fn validate(&self) -> Result<(), AppError> {
        if self.schema_version != MARKET_CAPTURE_SCHEMA_VERSION
            || self.source.trim().is_empty()
            || self.captured_at_secs <= 0
            || self.frame_sha256 != market_frame_sha256(&self.frame)?
        {
            return Err(AppError::InvalidMarketData(
                "capture de mercado incompatible o alterado".into(),
            ));
        }
        Ok(())
    }
}

fn market_frame_sha256(frame: &MarketFrame) -> Result<String, AppError> {
    let encoded = serde_json::to_vec(frame)?;
    let digest = ring::digest::digest(&ring::digest::SHA256, &encoded);
    Ok(digest
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct VixObservation {
    pub level: f64,
    pub previous_close: Option<f64>,
    pub timestamp_secs: i64,
    #[serde(default)]
    pub previous_close_timestamp_secs: Option<i64>,
    #[serde(default)]
    pub value_kind: VixValueKind,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VixValueKind {
    #[default]
    Current,
    PreviousClose,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VixFreshnessState {
    Current,
    PreviousClose,
    Stale,
}

impl VixObservation {
    pub fn change_percentage(self) -> Option<f64> {
        let previous = self
            .previous_close
            .filter(|value| value.is_finite() && *value > 0.0)?;
        let previous_timestamp = self.previous_close_timestamp_secs?;
        if previous_timestamp >= self.timestamp_secs {
            return None;
        }
        Some((self.level - previous) / previous * 100.0)
    }

    pub fn validated_change_percentage(
        self,
        now_secs: i64,
        previous_max_age_secs: u64,
    ) -> Option<f64> {
        self.validate_previous_close(now_secs, previous_max_age_secs)
            .ok()?;
        self.change_percentage()
    }

    pub fn validate_previous_close(self, now_secs: i64, max_age_secs: u64) -> Result<(), AppError> {
        match (self.previous_close, self.previous_close_timestamp_secs) {
            (None, None) => return Ok(()),
            (Some(value), Some(timestamp)) if value.is_finite() && value > 0.0 => {
                if timestamp >= self.timestamp_secs || timestamp > now_secs.saturating_add(300) {
                    return Err(AppError::InvalidMarketData(
                        "timestamp de cierre previo VIX inválido".into(),
                    ));
                }
                let max_age_secs = i64::try_from(max_age_secs).unwrap_or(i64::MAX);
                if now_secs.saturating_sub(timestamp) > max_age_secs {
                    return Err(AppError::InvalidMarketData(
                        "cierre previo VIX desactualizado".into(),
                    ));
                }
                return Ok(());
            }
            _ => {}
        }
        Err(AppError::InvalidMarketData(
            "cierre previo VIX incompleto o inválido".into(),
        ))
    }

    pub fn validate(self, now_secs: i64, max_age_secs: u64) -> Result<(), AppError> {
        if !self.level.is_finite() || self.level <= 0.0 {
            return Err(AppError::InvalidMarketData("nivel VIX invalido".into()));
        }
        if self
            .previous_close
            .is_some_and(|value| !value.is_finite() || value <= 0.0)
        {
            return Err(AppError::InvalidMarketData(
                "cierre previo VIX invalido".into(),
            ));
        }
        if self.timestamp_secs > now_secs.saturating_add(300) {
            return Err(AppError::InvalidMarketData(
                "timestamp futuro para VIX".into(),
            ));
        }
        let max_age_secs = i64::try_from(max_age_secs).unwrap_or(i64::MAX);
        let age = now_secs.saturating_sub(self.timestamp_secs);
        if age > max_age_secs {
            return Err(AppError::InvalidMarketData(format!(
                "cotizacion de VIX obsoleta por {age}s (maximo {max_age_secs}s)"
            )));
        }
        Ok(())
    }

    pub fn freshness_state(
        self,
        now_secs: i64,
        current_max_age_secs: u64,
        previous_max_age_secs: u64,
    ) -> VixFreshnessState {
        match self.value_kind {
            VixValueKind::Current if self.validate(now_secs, current_max_age_secs).is_ok() => {
                VixFreshnessState::Current
            }
            VixValueKind::PreviousClose
                if self.validate(now_secs, previous_max_age_secs).is_ok() =>
            {
                VixFreshnessState::PreviousClose
            }
            _ => VixFreshnessState::Stale,
        }
    }
}

impl MarketFrame {
    pub fn validate(&self, previous_timestamp: Option<i64>) -> Result<(), AppError> {
        self.underlying.validate(previous_timestamp)?;
        if self
            .option_chain_quality
            .as_ref()
            .is_some_and(|quality| !option_chain_quality_reconciles(quality, self.options.len()))
        {
            return Err(AppError::InvalidMarketData(
                "conteos de calidad de la cadena de opciones inconsistentes".into(),
            ));
        }
        for option in &self.options {
            option.validate(&self.underlying.ticker)?;
            if option.timestamp_secs < self.underlying.timestamp_secs.saturating_sub(60) {
                return Err(AppError::InvalidMarketData(format!(
                    "cotizacion de {} desactualizada",
                    option.symbol
                )));
            }
        }
        Ok(())
    }

    pub fn option(&self, symbol: &str) -> Option<&OptionQuote> {
        self.options.iter().find(|option| option.symbol == symbol)
    }
}

fn option_chain_quality_reconciles(quality: &OptionChainQuality, options_len: usize) -> bool {
    quality.accepted_contracts == options_len
        && quality
            .accepted_contracts
            .checked_add(quality.missing_quote_contracts)
            .and_then(|count| count.checked_add(quality.invalid_quote_contracts))
            == Some(quality.catalog_contracts)
        && quality
            .accepted_call_contracts
            .checked_add(quality.accepted_put_contracts)
            == Some(quality.accepted_contracts)
        && (quality.by_expiry.is_empty() || expiry_quality_reconciles(quality))
}

fn expiry_quality_reconciles(quality: &OptionChainQuality) -> bool {
    let mut previous_expiry = None;
    let mut catalog = 0_usize;
    let mut accepted = 0_usize;
    let mut missing = 0_usize;
    let mut invalid = 0_usize;
    let mut calls = 0_usize;
    let mut puts = 0_usize;
    for expiry in &quality.by_expiry {
        if previous_expiry.is_some_and(|previous| expiry.expiry_days <= previous)
            || expiry
                .accepted_contracts
                .checked_add(expiry.missing_quote_contracts)
                .and_then(|count| count.checked_add(expiry.invalid_quote_contracts))
                != Some(expiry.catalog_contracts)
            || expiry
                .accepted_call_contracts
                .checked_add(expiry.accepted_put_contracts)
                != Some(expiry.accepted_contracts)
        {
            return false;
        }
        previous_expiry = Some(expiry.expiry_days);
        let Some(next_catalog) = catalog.checked_add(expiry.catalog_contracts) else {
            return false;
        };
        let Some(next_accepted) = accepted.checked_add(expiry.accepted_contracts) else {
            return false;
        };
        let Some(next_missing) = missing.checked_add(expiry.missing_quote_contracts) else {
            return false;
        };
        let Some(next_invalid) = invalid.checked_add(expiry.invalid_quote_contracts) else {
            return false;
        };
        let Some(next_calls) = calls.checked_add(expiry.accepted_call_contracts) else {
            return false;
        };
        let Some(next_puts) = puts.checked_add(expiry.accepted_put_contracts) else {
            return false;
        };
        catalog = next_catalog;
        accepted = next_accepted;
        missing = next_missing;
        invalid = next_invalid;
        calls = next_calls;
        puts = next_puts;
    }
    catalog == quality.catalog_contracts
        && accepted == quality.accepted_contracts
        && missing == quality.missing_quote_contracts
        && invalid == quality.invalid_quote_contracts
        && calls == quality.accepted_call_contracts
        && puts == quality.accepted_put_contracts
}

fn validate_book(last: f64, bid: Option<f64>, ask: Option<f64>) -> Result<(), AppError> {
    if !last.is_finite() || last <= 0.0 {
        return Err(AppError::InvalidMarketData("last debe ser positivo".into()));
    }
    if bid.is_some_and(|price| !price.is_finite() || price <= 0.0)
        || ask.is_some_and(|price| !price.is_finite() || price <= 0.0)
    {
        return Err(AppError::InvalidMarketData("bid/ask inconsistentes".into()));
    }
    if let (Some(bid), Some(ask)) = (bid, ask) {
        if bid > ask {
            return Err(AppError::InvalidMarketData("bid/ask inconsistentes".into()));
        }
    }
    Ok(())
}

fn validate_timestamp_freshness(
    symbol: &str,
    timestamp_secs: i64,
    now_secs: i64,
    max_age_secs: u64,
) -> Result<(), AppError> {
    let max_age_secs = i64::try_from(max_age_secs).unwrap_or(i64::MAX);
    if timestamp_secs > now_secs.saturating_add(max_age_secs) {
        return Err(AppError::InvalidMarketData(format!(
            "timestamp futuro para {symbol}"
        )));
    }
    let age = now_secs.saturating_sub(timestamp_secs);
    if age > max_age_secs {
        return Err(AppError::InvalidMarketData(format!(
            "cotizacion de {symbol} obsoleta por {age}s (maximo {max_age_secs}s)"
        )));
    }
    Ok(())
}

pub fn select_option(
    frame: &MarketFrame,
    kind: OptionKind,
    preferred_expiry_days: u32,
) -> Option<&OptionQuote> {
    frame
        .options
        .iter()
        .filter(|option| {
            option.kind == kind
                && option.expiry_days >= preferred_expiry_days
                && option.volume > 0
                && option.executable_buy_price().is_some()
                && option.executable_sell_price().is_some()
        })
        .min_by(|left, right| {
            let left_key = (
                left.expiry_days - preferred_expiry_days,
                moneyness_distance_percentage(left.strike, frame.underlying.last)
                    .unwrap_or(f64::INFINITY),
            );
            let right_key = (
                right.expiry_days - preferred_expiry_days,
                moneyness_distance_percentage(right.strike, frame.underlying.last)
                    .unwrap_or(f64::INFINITY),
            );
            left_key
                .0
                .cmp(&right_key.0)
                .then_with(|| left_key.1.total_cmp(&right_key.1))
        })
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct OptionSelectionCriteria {
    pub min_expiry_days: u32,
    pub target_expiry_days: u32,
    pub max_expiry_days: u32,
    pub min_volume: u64,
    pub max_spread_percentage: f64,
    pub max_moneyness_distance_percentage: f64,
    pub now_secs: i64,
    pub max_age_secs: u64,
    pub operating_cost_percentage: f64,
    pub slippage_bps: f64,
}

pub fn select_option_with_criteria(
    frame: &MarketFrame,
    kind: OptionKind,
    criteria: OptionSelectionCriteria,
) -> Option<&OptionQuote> {
    frame
        .options
        .iter()
        .filter(|option| {
            let moneyness = moneyness_distance_percentage(option.strike, frame.underlying.last)
                .unwrap_or(f64::INFINITY);
            option.kind == kind
                && option.expiry_days >= criteria.min_expiry_days
                && option.expiry_days <= criteria.max_expiry_days
                && option.volume >= criteria.min_volume
                && moneyness <= criteria.max_moneyness_distance_percentage
                && option
                    .validate_entry_quality(
                        criteria.now_secs,
                        criteria.max_age_secs,
                        criteria.max_spread_percentage,
                    )
                    .is_ok()
        })
        .min_by(|left, right| {
            let friction = |option: &OptionQuote| {
                option_friction_percentage(
                    option,
                    criteria.operating_cost_percentage,
                    criteria.slippage_bps,
                )
                .unwrap_or(f64::INFINITY)
            };
            let moneyness = |option: &OptionQuote| {
                moneyness_distance_percentage(option.strike, frame.underlying.last)
                    .unwrap_or(f64::INFINITY)
            };
            friction(left)
                .total_cmp(&friction(right))
                .then_with(|| right.volume.cmp(&left.volume))
                .then_with(|| moneyness(left).total_cmp(&moneyness(right)))
                .then_with(|| {
                    left.expiry_days
                        .abs_diff(criteria.target_expiry_days)
                        .cmp(&right.expiry_days.abs_diff(criteria.target_expiry_days))
                })
        })
}

pub trait MarketDataProvider {
    type Error;

    fn next_frame(&mut self) -> Result<Option<MarketFrame>, Self::Error>;
}

#[derive(Debug)]
pub struct PriceCache {
    ttl: Duration,
    entries: HashMap<String, (UnderlyingQuote, Instant)>,
}

impl PriceCache {
    pub fn new(ttl: Duration) -> Self {
        Self {
            ttl,
            entries: HashMap::new(),
        }
    }

    pub fn insert(&mut self, quote: UnderlyingQuote) {
        self.entries
            .insert(quote.ticker.clone(), (quote, Instant::now()));
    }

    pub fn get(&self, ticker: &str) -> Option<UnderlyingQuote> {
        self.entries
            .get(ticker)
            .and_then(|(quote, stored)| (stored.elapsed() <= self.ttl).then(|| quote.clone()))
    }
}

#[derive(Debug)]
pub struct PriceStream {
    capacity: usize,
    samples: VecDeque<PriceSample>,
    last_timestamp: Option<i64>,
}

impl PriceStream {
    pub fn new(capacity: usize) -> Self {
        assert!(capacity > 0);
        Self {
            capacity,
            samples: VecDeque::with_capacity(capacity),
            last_timestamp: None,
        }
    }

    pub fn push_quote(&mut self, quote: &UnderlyingQuote) -> Result<PriceSample, AppError> {
        quote.validate(self.last_timestamp)?;
        let sample = PriceSample {
            price: quote.last,
            timestamp_secs: quote.timestamp_secs,
        };
        if self.samples.len() == self.capacity {
            self.samples.pop_front();
        }
        self.samples.push_back(sample);
        self.last_timestamp = Some(quote.timestamp_secs);
        Ok(sample)
    }

    pub fn samples(&self) -> &VecDeque<PriceSample> {
        &self.samples
    }
}

#[derive(Debug, Clone)]
pub struct ReplayMarket {
    frames: VecDeque<MarketFrame>,
}

impl ReplayMarket {
    pub fn new(frames: Vec<MarketFrame>) -> Result<Self, AppError> {
        if frames.is_empty() {
            return Err(AppError::InvalidMarketData("replay vacio".into()));
        }
        let mut previous = None;
        for frame in &frames {
            frame.validate(previous)?;
            previous = Some(frame.underlying.timestamp_secs);
        }
        Ok(Self {
            frames: frames.into(),
        })
    }

    pub fn from_jsonl(path: impl AsRef<Path>) -> Result<Self, AppError> {
        let path = path.as_ref();
        let file = open_limited_read(path, MAX_REPLAY_BYTES)?;
        Self::from_jsonl_reader(BufReader::new(file))
    }

    /// Decodifica exactamente el mismo contrato JSONL que `from_jsonl`, pero
    /// desde bytes ya acotados en memoria por el llamador. Se usa también para someter el
    /// parser productivo a fuzzing sin crear archivos especiales de prueba.
    pub fn from_jsonl_bytes(bytes: &[u8]) -> Result<Self, AppError> {
        Self::from_jsonl_reader(BufReader::new(Cursor::new(bytes)))
    }

    fn from_jsonl_reader(reader: impl BufRead) -> Result<Self, AppError> {
        let mut frames = Vec::new();
        for (index, line) in reader.lines().enumerate() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            if frames.len() >= MAX_REPLAY_FRAMES {
                return Err(AppError::InvalidMarketData(format!(
                    "replay excede el máximo de {MAX_REPLAY_FRAMES} frames"
                )));
            }
            let value: serde_json::Value = serde_json::from_str(&line).map_err(|error| {
                AppError::InvalidMarketData(format!("linea {} del replay: {error}", index + 1))
            })?;
            let frame = if value.get("frame").is_some() || value.get("frame_sha256").is_some() {
                let capture: CapturedMarketFrame =
                    serde_json::from_value(value).map_err(|error| {
                        AppError::InvalidMarketData(format!(
                            "capture {} del replay: {error}",
                            index + 1
                        ))
                    })?;
                capture.validate()?;
                capture.frame
            } else {
                serde_json::from_value(value).map_err(|error| {
                    AppError::InvalidMarketData(format!(
                        "frame legado {} del replay: {error}",
                        index + 1
                    ))
                })?
            };
            frames.push(frame);
        }
        Self::new(frames)
    }

    pub fn synthetic(ticker: &str) -> Self {
        let prices = [
            100.0, 100.1, 100.25, 100.45, 100.7, 101.0, 101.4, 101.8, 102.2, 102.5, 102.7, 102.6,
            102.3, 101.9, 101.4, 100.9, 100.4, 99.9, 99.4, 99.0, 98.7, 98.9, 99.2, 99.6, 100.0,
            100.5, 101.0, 101.5,
        ];
        let frames = prices
            .into_iter()
            .enumerate()
            .map(|(index, price)| synthetic_frame(ticker, price, index as i64 + 1))
            .collect();
        Self::new(frames).expect("synthetic replay must be valid")
    }

    pub fn resume_after(&mut self, timestamp_secs: i64) {
        while self
            .frames
            .front()
            .is_some_and(|frame| frame.underlying.timestamp_secs <= timestamp_secs)
        {
            self.frames.pop_front();
        }
    }

    pub fn next_session_days_after(&self, timestamp_secs: i64) -> u32 {
        let current_day = argentina_day(timestamp_secs);
        self.frames
            .iter()
            .map(|frame| argentina_day(frame.underlying.timestamp_secs))
            .find(|day| *day > current_day)
            .map(|day| {
                day.saturating_sub(current_day)
                    .clamp(1, i64::from(u32::MAX)) as u32
            })
            .unwrap_or_else(|| {
                let weekday_from_monday = (current_day + 3).rem_euclid(7);
                if weekday_from_monday == 4 {
                    3
                } else {
                    1
                }
            })
    }
}

fn argentina_day(timestamp_secs: i64) -> i64 {
    crate::time_utils::argentina_session_day(timestamp_secs)
}

impl MarketDataProvider for ReplayMarket {
    type Error = AppError;

    fn next_frame(&mut self) -> Result<Option<MarketFrame>, Self::Error> {
        Ok(self.frames.pop_front())
    }
}

fn synthetic_frame(ticker: &str, underlying: f64, timestamp_secs: i64) -> MarketFrame {
    let options = [98.0, 100.0, 102.0]
        .into_iter()
        .flat_map(|strike| {
            [OptionKind::Call, OptionKind::Put]
                .into_iter()
                .map(move |kind| synthetic_option(ticker, underlying, strike, kind, timestamp_secs))
        })
        .collect();
    MarketFrame {
        underlying: UnderlyingQuote {
            ticker: ticker.into(),
            last: underlying,
            bid: Some(underlying - 0.05),
            ask: Some(underlying + 0.05),
            timestamp_secs,
            exchange_timestamp_secs: Some(timestamp_secs),
            received_at_secs: timestamp_secs,
            timestamp_source: QuoteTimestampSource::Exchange,
        },
        options,
        option_chain_quality: None,
        vix: None,
    }
}

fn synthetic_option(
    ticker: &str,
    underlying: f64,
    strike: f64,
    kind: OptionKind,
    timestamp_secs: i64,
) -> OptionQuote {
    let intrinsic = match kind {
        OptionKind::Call => (underlying - strike).max(0.0),
        OptionKind::Put => (strike - underlying).max(0.0),
    };
    let distance = (underlying - strike).abs();
    let time_value = (2.0 - distance * 0.12).max(0.35);
    let last = intrinsic + time_value;
    let suffix = match kind {
        OptionKind::Call => "C",
        OptionKind::Put => "P",
    };
    OptionQuote {
        symbol: format!("{ticker}-{suffix}-{strike:.0}"),
        underlying: ticker.into(),
        kind,
        strike,
        expiry_days: 1,
        expiration_timestamp_secs: Some(timestamp_secs.saturating_add(86_400)),
        catalog_contract_multiplier: None,
        catalog_observed_at_secs: None,
        catalog_schema_version: 0,
        catalog_sha256: None,
        catalog_archived: false,
        contract_metadata_source: ContractMetadataSource::Legacy,
        exercise_style: ExerciseStyle::American,
        last,
        bid: Some((last - 0.04).max(0.01)),
        ask: Some(last + 0.04),
        volume: 1_000,
        timestamp_secs,
        exchange_timestamp_secs: Some(timestamp_secs),
        received_at_secs: timestamp_secs,
        timestamp_source: QuoteTimestampSource::Exchange,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replay_returns_distinct_underlying_and_option_prices() {
        let mut market = ReplayMarket::synthetic("GAL");
        let frame = market.next_frame().unwrap().unwrap();
        assert_eq!(frame.underlying.last, 100.0);
        assert!(frame.options.iter().all(|option| option.last < 10.0));
    }

    #[test]
    fn replay_bytes_use_the_same_jsonl_contract_as_replay_files() {
        let first = synthetic_frame("GGAL", 100.0, 1_000);
        let second = synthetic_frame("GGAL", 101.0, 1_001);
        let bytes = format!(
            "{}\n\n{}\n",
            serde_json::to_string(&first).unwrap(),
            serde_json::to_string(&second).unwrap()
        );
        let mut replay = ReplayMarket::from_jsonl_bytes(bytes.as_bytes()).unwrap();
        let decoded_first = replay.next_frame().unwrap().unwrap();
        let decoded_second = replay.next_frame().unwrap().unwrap();
        assert_eq!(decoded_first.underlying.ticker, "GGAL");
        assert_eq!(decoded_first.underlying.timestamp_secs, 1_000);
        assert_eq!(decoded_first.options.len(), first.options.len());
        assert_eq!(decoded_second.underlying.last, 101.0);
        assert_eq!(decoded_second.underlying.timestamp_secs, 1_001);
        assert_eq!(decoded_second.options.len(), second.options.len());
        assert_eq!(replay.next_frame().unwrap(), None);
        assert!(ReplayMarket::from_jsonl_bytes(b"{json truncado\n").is_err());
        assert!(ReplayMarket::from_jsonl_bytes(b"\n").is_err());
    }

    #[test]
    fn replay_infers_a_long_break_from_the_next_recorded_session() {
        let thursday = 20_000 * 86_400 + 4 * 60 * 60;
        let monday = thursday + 4 * 86_400;
        let replay = ReplayMarket::new(vec![
            synthetic_frame("GGAL", 100.0, thursday),
            synthetic_frame("GGAL", 101.0, monday),
        ])
        .unwrap();

        assert_eq!(replay.next_session_days_after(thursday), 4);
    }

    #[test]
    fn vix_change_and_point_in_time_validation_are_deterministic() {
        let vix = VixObservation {
            level: 22.0,
            previous_close: Some(20.0),
            timestamp_secs: 1_000,
            previous_close_timestamp_secs: Some(900),
            value_kind: VixValueKind::Current,
        };
        assert_eq!(vix.change_percentage(), Some(10.0));
        assert_eq!(vix.validated_change_percentage(1_100, 300), Some(10.0));
        assert_eq!(vix.validated_change_percentage(1_301, 300), None);
        assert!(vix.validate(1_100, 200).is_ok());
        assert!(vix.validate(1_301, 200).is_err());

        let future = VixObservation {
            timestamp_secs: 1_401,
            ..vix
        };
        assert!(future.validate(1_000, 10_000).is_err());
    }

    #[test]
    fn vix_previous_close_is_never_classified_as_current() {
        let vix = VixObservation {
            level: 20.0,
            previous_close: Some(20.0),
            timestamp_secs: 1_000,
            previous_close_timestamp_secs: Some(1_000),
            value_kind: VixValueKind::PreviousClose,
        };
        assert_eq!(
            vix.freshness_state(1_100, 60, 500),
            VixFreshnessState::PreviousClose
        );
        assert_eq!(
            vix.freshness_state(2_000, 60, 500),
            VixFreshnessState::Stale
        );
    }

    #[test]
    fn captured_market_frame_round_trips_its_vix_for_replay() {
        let mut market = ReplayMarket::synthetic("GAL");
        let mut frame = market.next_frame().unwrap().unwrap();
        frame.vix = Some(VixObservation {
            level: 18.5,
            previous_close: Some(18.0),
            timestamp_secs: frame.underlying.timestamp_secs,
            previous_close_timestamp_secs: None,
            value_kind: VixValueKind::Current,
        });

        let capture = CapturedMarketFrame::new("iol_v2_normalized", 2_000, frame.clone()).unwrap();
        let encoded = serde_json::to_string(&capture).unwrap();
        let decoded: CapturedMarketFrame = serde_json::from_str(&encoded).unwrap();

        decoded.validate().unwrap();
        assert_eq!(decoded.frame.vix, frame.vix);

        let path = std::env::temp_dir().join(format!(
            "options-capture-replay-{}-{}.jsonl",
            std::process::id(),
            frame.underlying.timestamp_secs
        ));
        crate::secure_fs::write_atomic(&path, format!("{encoded}\n").as_bytes()).unwrap();
        let mut replay = ReplayMarket::from_jsonl(&path).unwrap();
        assert_eq!(replay.next_frame().unwrap().unwrap().vix, frame.vix);
        std::fs::remove_file(path).unwrap();

        let mut changed = decoded;
        changed.frame.underlying.last += 1.0;
        assert!(changed.validate().is_err());
    }

    #[test]
    fn captured_market_frame_rejects_each_invalid_envelope_field() {
        let frame = synthetic_frame("GGAL", 100.0, 1_000);
        let capture = CapturedMarketFrame::new("iol_v2_normalized", 2_000, frame).unwrap();
        assert!(capture.validate().is_ok());

        let mut wrong_schema = capture.clone();
        wrong_schema.schema_version += 1;
        assert!(wrong_schema.validate().is_err());

        let mut blank_source = capture.clone();
        blank_source.source = " \t".into();
        assert!(blank_source.validate().is_err());

        let mut zero_timestamp = capture.clone();
        zero_timestamp.captured_at_secs = 0;
        assert!(zero_timestamp.validate().is_err());

        let mut wrong_hash = capture;
        wrong_hash.frame_sha256 = "00".repeat(32);
        assert!(wrong_hash.validate().is_err());
    }

    #[test]
    fn tenor_totals_enforces_legacy_and_expiry_boundaries_without_overflow() {
        let legacy_empty = OptionChainQuality {
            catalog_contracts: 0,
            quote_rows: 0,
            accepted_contracts: 0,
            missing_quote_contracts: 0,
            invalid_quote_contracts: 0,
            accepted_call_contracts: 0,
            accepted_put_contracts: 0,
            by_expiry: Vec::new(),
        };
        assert_eq!(legacy_empty.tenor_totals(0, u32::MAX), None);

        let legacy = OptionChainQuality {
            catalog_contracts: 2,
            accepted_contracts: 2,
            accepted_call_contracts: 1,
            accepted_put_contracts: 1,
            ..legacy_empty.clone()
        };
        assert_eq!(legacy.tenor_totals(30, 7), Some((2, 2, 1, 1)));

        let quality = OptionChainQuality {
            catalog_contracts: 4,
            quote_rows: 4,
            accepted_contracts: 4,
            missing_quote_contracts: 0,
            invalid_quote_contracts: 0,
            accepted_call_contracts: 2,
            accepted_put_contracts: 2,
            by_expiry: vec![
                OptionExpiryQuality {
                    expiry_days: 7,
                    catalog_contracts: 2,
                    accepted_contracts: 2,
                    missing_quote_contracts: 0,
                    invalid_quote_contracts: 0,
                    accepted_call_contracts: 1,
                    accepted_put_contracts: 1,
                },
                OptionExpiryQuality {
                    expiry_days: 30,
                    catalog_contracts: 2,
                    accepted_contracts: 2,
                    missing_quote_contracts: 0,
                    invalid_quote_contracts: 0,
                    accepted_call_contracts: 1,
                    accepted_put_contracts: 1,
                },
            ],
        };
        assert_eq!(quality.tenor_totals(7, 7), Some((2, 2, 1, 1)));
        assert_eq!(quality.tenor_totals(30, 30), Some((2, 2, 1, 1)));
        assert_eq!(quality.tenor_totals(8, 29), None);

        let overflow = OptionChainQuality {
            by_expiry: vec![
                OptionExpiryQuality {
                    expiry_days: 7,
                    catalog_contracts: usize::MAX,
                    accepted_contracts: usize::MAX,
                    missing_quote_contracts: 0,
                    invalid_quote_contracts: 0,
                    accepted_call_contracts: usize::MAX,
                    accepted_put_contracts: 0,
                },
                OptionExpiryQuality {
                    expiry_days: 30,
                    catalog_contracts: 1,
                    accepted_contracts: 1,
                    missing_quote_contracts: 0,
                    invalid_quote_contracts: 0,
                    accepted_call_contracts: 1,
                    accepted_put_contracts: 0,
                },
            ],
            ..legacy_empty
        };
        assert_eq!(overflow.tenor_totals(7, 30), None);
    }

    #[test]
    fn option_chain_quality_counts_must_reconcile_with_preserved_contracts() {
        let mut frame = synthetic_frame("GGAL", 100.0, 1_000);
        frame.option_chain_quality = Some(OptionChainQuality {
            catalog_contracts: frame.options.len() + 1,
            quote_rows: frame.options.len(),
            accepted_contracts: frame.options.len(),
            missing_quote_contracts: 0,
            invalid_quote_contracts: 0,
            accepted_call_contracts: frame
                .options
                .iter()
                .filter(|option| option.kind == OptionKind::Call)
                .count(),
            accepted_put_contracts: frame
                .options
                .iter()
                .filter(|option| option.kind == OptionKind::Put)
                .count(),
            by_expiry: Vec::new(),
        });

        assert!(frame.validate(None).is_err());
    }

    #[test]
    fn option_chain_quality_rejects_each_mismatch_and_integer_overflow() {
        let valid = OptionChainQuality {
            catalog_contracts: 2,
            quote_rows: 2,
            accepted_contracts: 2,
            missing_quote_contracts: 0,
            invalid_quote_contracts: 0,
            accepted_call_contracts: 1,
            accepted_put_contracts: 1,
            by_expiry: Vec::new(),
        };
        assert!(option_chain_quality_reconciles(&valid, 2));

        let mut wrong_options = valid.clone();
        wrong_options.accepted_contracts = 1;
        wrong_options.catalog_contracts = 1;
        wrong_options.accepted_put_contracts = 0;
        assert!(!option_chain_quality_reconciles(&wrong_options, 2));

        let mut wrong_catalog = valid.clone();
        wrong_catalog.catalog_contracts = 3;
        assert!(!option_chain_quality_reconciles(&wrong_catalog, 2));

        let mut wrong_sides = valid.clone();
        wrong_sides.accepted_put_contracts = 0;
        assert!(!option_chain_quality_reconciles(&wrong_sides, 2));

        let count_overflow = OptionChainQuality {
            catalog_contracts: usize::MAX,
            quote_rows: 0,
            accepted_contracts: usize::MAX,
            missing_quote_contracts: 1,
            invalid_quote_contracts: 0,
            accepted_call_contracts: usize::MAX,
            accepted_put_contracts: 0,
            by_expiry: Vec::new(),
        };
        assert!(!option_chain_quality_reconciles(
            &count_overflow,
            usize::MAX
        ));

        let side_overflow = OptionChainQuality {
            missing_quote_contracts: 0,
            accepted_put_contracts: 1,
            ..count_overflow
        };
        assert!(!option_chain_quality_reconciles(&side_overflow, usize::MAX));
    }

    #[test]
    fn expiry_quality_rejects_row_and_aggregate_overflow() {
        let row_overflow = OptionChainQuality {
            catalog_contracts: usize::MAX,
            quote_rows: 0,
            accepted_contracts: usize::MAX,
            missing_quote_contracts: 0,
            invalid_quote_contracts: 0,
            accepted_call_contracts: usize::MAX,
            accepted_put_contracts: 0,
            by_expiry: vec![OptionExpiryQuality {
                expiry_days: 1,
                catalog_contracts: usize::MAX,
                accepted_contracts: usize::MAX,
                missing_quote_contracts: 1,
                invalid_quote_contracts: 0,
                accepted_call_contracts: usize::MAX,
                accepted_put_contracts: 0,
            }],
        };
        assert!(!expiry_quality_reconciles(&row_overflow));

        let aggregate_overflow = OptionChainQuality {
            by_expiry: vec![
                OptionExpiryQuality {
                    expiry_days: 1,
                    catalog_contracts: usize::MAX,
                    accepted_contracts: usize::MAX,
                    missing_quote_contracts: 0,
                    invalid_quote_contracts: 0,
                    accepted_call_contracts: usize::MAX,
                    accepted_put_contracts: 0,
                },
                OptionExpiryQuality {
                    expiry_days: 2,
                    catalog_contracts: 1,
                    accepted_contracts: 1,
                    missing_quote_contracts: 0,
                    invalid_quote_contracts: 0,
                    accepted_call_contracts: 1,
                    accepted_put_contracts: 0,
                },
            ],
            ..row_overflow
        };
        assert!(!expiry_quality_reconciles(&aggregate_overflow));
    }

    #[test]
    fn expiry_quality_reconciles_order_rows_and_every_aggregate() {
        let valid = OptionChainQuality {
            catalog_contracts: 7,
            quote_rows: 7,
            accepted_contracts: 4,
            missing_quote_contracts: 2,
            invalid_quote_contracts: 1,
            accepted_call_contracts: 2,
            accepted_put_contracts: 2,
            by_expiry: vec![
                OptionExpiryQuality {
                    expiry_days: 7,
                    catalog_contracts: 4,
                    accepted_contracts: 2,
                    missing_quote_contracts: 1,
                    invalid_quote_contracts: 1,
                    accepted_call_contracts: 1,
                    accepted_put_contracts: 1,
                },
                OptionExpiryQuality {
                    expiry_days: 30,
                    catalog_contracts: 3,
                    accepted_contracts: 2,
                    missing_quote_contracts: 1,
                    invalid_quote_contracts: 0,
                    accepted_call_contracts: 1,
                    accepted_put_contracts: 1,
                },
            ],
        };
        assert!(expiry_quality_reconciles(&valid));

        let mut duplicate_expiry = valid.clone();
        duplicate_expiry.by_expiry[1].expiry_days = 7;
        assert!(!expiry_quality_reconciles(&duplicate_expiry));

        let mut wrong_row_catalog = valid.clone();
        wrong_row_catalog.by_expiry[0].catalog_contracts += 1;
        assert!(!expiry_quality_reconciles(&wrong_row_catalog));

        let mut wrong_row_catalog_with_matching_aggregate = valid.clone();
        wrong_row_catalog_with_matching_aggregate.by_expiry[0].catalog_contracts += 1;
        wrong_row_catalog_with_matching_aggregate.catalog_contracts += 1;
        assert!(!expiry_quality_reconciles(
            &wrong_row_catalog_with_matching_aggregate
        ));

        let mut wrong_row_sides = valid.clone();
        wrong_row_sides.by_expiry[0].accepted_put_contracts = 0;
        assert!(!expiry_quality_reconciles(&wrong_row_sides));

        let mut aggregate_mismatches = Vec::new();
        let mut wrong = valid.clone();
        wrong.catalog_contracts += 1;
        aggregate_mismatches.push(wrong);
        let mut wrong = valid.clone();
        wrong.accepted_contracts += 1;
        aggregate_mismatches.push(wrong);
        let mut wrong = valid.clone();
        wrong.missing_quote_contracts += 1;
        aggregate_mismatches.push(wrong);
        let mut wrong = valid.clone();
        wrong.invalid_quote_contracts += 1;
        aggregate_mismatches.push(wrong);
        let mut wrong = valid.clone();
        wrong.accepted_call_contracts += 1;
        aggregate_mismatches.push(wrong);
        let mut wrong = valid;
        wrong.accepted_put_contracts += 1;
        aggregate_mismatches.push(wrong);

        for mismatch in aggregate_mismatches {
            assert!(!expiry_quality_reconciles(&mismatch));
        }
    }

    #[test]
    fn option_timestamp_lag_boundary_is_saturating_and_inclusive() {
        let mut frame = synthetic_frame("GGAL", 100.0, 1_000);
        for option in &mut frame.options {
            option.timestamp_secs = 940;
            option.exchange_timestamp_secs = Some(940);
            option.received_at_secs = 940;
        }
        assert!(frame.validate(None).is_ok());
        frame.options[0].timestamp_secs = 939;
        frame.options[0].exchange_timestamp_secs = Some(939);
        frame.options[0].received_at_secs = 939;
        assert!(frame.validate(None).is_err());

        let mut extreme = synthetic_frame("GGAL", 100.0, i64::MIN);
        extreme.underlying.exchange_timestamp_secs = None;
        extreme.underlying.received_at_secs = 0;
        extreme.underlying.timestamp_source = QuoteTimestampSource::Legacy;
        for option in &mut extreme.options {
            option.exchange_timestamp_secs = None;
            option.received_at_secs = 0;
            option.timestamp_source = QuoteTimestampSource::Legacy;
        }
        assert!(extreme.validate(None).is_ok());
    }

    #[test]
    fn option_chain_gate_requires_percentage_and_both_sides_at_the_boundary() {
        let quality = OptionChainQuality {
            catalog_contracts: 10,
            quote_rows: 8,
            accepted_contracts: 8,
            missing_quote_contracts: 1,
            invalid_quote_contracts: 1,
            accepted_call_contracts: 4,
            accepted_put_contracts: 4,
            by_expiry: Vec::new(),
        };
        assert!(quality.allows_entry(80.0, 4));
        assert!(!quality.allows_entry(80.01, 4));
        assert!(!quality.allows_entry(80.0, 5));

        let one_sided = OptionChainQuality {
            accepted_call_contracts: 8,
            accepted_put_contracts: 0,
            ..quality
        };
        assert!(!one_sided.allows_entry(80.0, 1));
    }

    #[test]
    fn liquid_contracts_outside_the_operable_tenor_cannot_mask_degradation() {
        let quality = OptionChainQuality {
            catalog_contracts: 100,
            quote_rows: 92,
            accepted_contracts: 92,
            missing_quote_contracts: 8,
            invalid_quote_contracts: 0,
            accepted_call_contracts: 46,
            accepted_put_contracts: 46,
            by_expiry: vec![
                OptionExpiryQuality {
                    expiry_days: 7,
                    catalog_contracts: 90,
                    accepted_contracts: 90,
                    missing_quote_contracts: 0,
                    invalid_quote_contracts: 0,
                    accepted_call_contracts: 45,
                    accepted_put_contracts: 45,
                },
                OptionExpiryQuality {
                    expiry_days: 21,
                    catalog_contracts: 10,
                    accepted_contracts: 2,
                    missing_quote_contracts: 8,
                    invalid_quote_contracts: 0,
                    accepted_call_contracts: 1,
                    accepted_put_contracts: 1,
                },
            ],
        };
        assert!(quality.allows_entry(80.0, 1));
        assert!(!quality.allows_entry_for_tenor(14, 45, 80.0, 1));
        assert!(quality.allows_entry_for_tenor(1, 7, 80.0, 1));
    }

    #[test]
    fn quote_rejects_crossed_book() {
        let quote = UnderlyingQuote {
            ticker: "GAL".into(),
            last: 100.0,
            bid: Some(101.0),
            ask: Some(100.0),
            timestamp_secs: 1,
            exchange_timestamp_secs: None,
            received_at_secs: 0,
            timestamp_source: QuoteTimestampSource::Legacy,
        };
        assert!(quote.validate(None).is_err());
    }

    #[test]
    fn book_validation_rejects_each_invalid_side_even_when_the_other_is_absent() {
        for last in [0.0, -1.0, f64::NAN, f64::INFINITY] {
            assert!(validate_book(last, None, None).is_err());
        }
        for invalid in [0.0, -1.0, f64::NAN, f64::INFINITY] {
            assert!(validate_book(100.0, Some(invalid), None).is_err());
            assert!(validate_book(100.0, None, Some(invalid)).is_err());
        }
        assert!(validate_book(100.0, None, None).is_ok());
        assert!(validate_book(100.0, Some(99.0), None).is_ok());
        assert!(validate_book(100.0, None, Some(101.0)).is_ok());
        assert!(validate_book(100.0, Some(100.0), Some(100.0)).is_ok());
        assert!(validate_book(100.0, Some(100.01), Some(100.0)).is_err());
    }

    #[test]
    fn executable_prices_and_round_trip_friction_have_closed_boundaries() {
        let mut option = synthetic_option("GGAL", 100.0, 100.0, OptionKind::Call, 100);
        for invalid in [0.0, -1.0, f64::NAN, f64::INFINITY] {
            option.ask = Some(invalid);
            assert_eq!(option.executable_buy_price(), None);
            option.bid = Some(invalid);
            assert_eq!(option.executable_sell_price(), None);
        }
        option.bid = Some(90.0);
        option.ask = Some(110.0);
        assert_eq!(option.executable_sell_price(), Some(90.0));
        assert_eq!(option.executable_buy_price(), Some(110.0));
        assert_eq!(option.spread_percentage(), Some(20.0));
        assert_eq!(option_friction_percentage(&option, 0.0, 0.0), Some(20.0));
        assert_eq!(option_friction_percentage(&option, 0.3, 25.0), Some(21.1));

        for (cost, slippage) in [
            (-0.01, 0.0),
            (0.0, -0.01),
            (f64::NAN, 0.0),
            (0.0, f64::INFINITY),
        ] {
            assert_eq!(option_friction_percentage(&option, cost, slippage), None);
        }
        option.bid = None;
        assert_eq!(option.spread_percentage(), None);
        assert_eq!(option_friction_percentage(&option, 0.3, 25.0), None);
    }

    #[test]
    fn moneyness_distance_is_symmetric_and_rejects_invalid_prices() {
        assert_eq!(moneyness_distance_percentage(110.0, 100.0), Some(10.0));
        assert_eq!(moneyness_distance_percentage(90.0, 100.0), Some(10.0));
        assert_eq!(moneyness_distance_percentage(100.0, 100.0), Some(0.0));
        for (strike, underlying) in [
            (0.0, 100.0),
            (-1.0, 100.0),
            (f64::NAN, 100.0),
            (100.0, 0.0),
            (100.0, -1.0),
            (100.0, f64::INFINITY),
        ] {
            assert_eq!(moneyness_distance_percentage(strike, underlying), None);
        }
    }

    #[test]
    fn option_contract_validation_rejects_each_identity_and_strike_error_independently() {
        let valid = synthetic_option("GGAL", 100.0, 100.0, OptionKind::Call, 100);
        assert!(valid.validate("GGAL").is_ok());

        let mut wrong_underlying = valid.clone();
        wrong_underlying.underlying = "YPFD".into();
        assert!(wrong_underlying.validate("GGAL").is_err());

        let mut empty_symbol = valid.clone();
        empty_symbol.symbol = "   ".into();
        assert!(empty_symbol.validate("GGAL").is_err());

        for strike in [0.0, -1.0, f64::NAN, f64::INFINITY] {
            let mut invalid_strike = valid.clone();
            invalid_strike.strike = strike;
            assert!(invalid_strike.validate("GGAL").is_err());
        }
    }

    #[test]
    fn freshness_and_spread_limits_accept_equality_and_reject_the_next_value() {
        assert!(validate_timestamp_freshness("GGAL", 90, 100, 10).is_ok());
        assert!(validate_timestamp_freshness("GGAL", 110, 100, 10).is_ok());
        assert!(validate_timestamp_freshness("GGAL", 89, 100, 10).is_err());
        assert!(validate_timestamp_freshness("GGAL", 111, 100, 10).is_err());

        let mut option = synthetic_option("GGAL", 100.0, 100.0, OptionKind::Call, 100);
        option.bid = Some(90.0);
        option.ask = Some(110.0);
        assert!(option.validate_entry_quality(110, 10, 20.0).is_ok());
        assert!(option.validate_entry_quality(110, 10, 19.999).is_err());
        assert!(option.validate_entry_quality(111, 10, 20.0).is_err());
        option.ask = None;
        assert!(option.validate_entry_quality(100, 10, 20.0).is_err());
    }

    #[test]
    fn option_rejects_source_clock_skew_and_timestamp_provenance_mismatch() {
        let mut option = synthetic_frame("GGAL", 100.0, 1_000).options.remove(0);
        option.received_at_secs = 1_000 + MAX_SOURCE_CLOCK_SKEW_SECS + 1;
        assert!(option.validate("GGAL").is_err());

        option.received_at_secs = 1_000;
        option.timestamp_source = QuoteTimestampSource::Received;
        assert!(option.validate("GGAL").is_err());
    }

    #[test]
    fn underlying_timestamp_provenance_has_closed_clock_skew_boundaries() {
        let mut quote = synthetic_frame("GGAL", 100.0, 1_000).underlying;
        quote.received_at_secs = 1_000 + MAX_SOURCE_CLOCK_SKEW_SECS;
        assert!(quote.validate(None).is_ok());

        quote.received_at_secs += 1;
        assert!(quote.validate(None).is_err());

        quote.timestamp_secs = 2_000;
        quote.exchange_timestamp_secs = None;
        quote.received_at_secs = 2_000;
        quote.timestamp_source = QuoteTimestampSource::Received;
        assert!(quote.validate(None).is_ok());

        quote.exchange_timestamp_secs = Some(2_000);
        assert!(quote.validate(None).is_err());
    }

    #[test]
    fn price_stream_is_bounded() {
        let mut stream = PriceStream::new(2);
        for timestamp in 1..=3 {
            stream
                .push_quote(&UnderlyingQuote {
                    ticker: "GAL".into(),
                    last: 100.0 + timestamp as f64,
                    bid: None,
                    ask: None,
                    timestamp_secs: timestamp,
                    exchange_timestamp_secs: None,
                    received_at_secs: 0,
                    timestamp_source: QuoteTimestampSource::Legacy,
                })
                .unwrap();
        }
        assert_eq!(stream.samples().len(), 2);
        assert_eq!(stream.samples()[0].timestamp_secs, 2);
    }

    #[test]
    fn selects_liquid_nearest_strike() {
        let mut market = ReplayMarket::synthetic("GAL");
        let frame = market.next_frame().unwrap().unwrap();
        let option = select_option(&frame, OptionKind::Call, 1).unwrap();
        assert_eq!(option.strike, 100.0);
    }

    #[test]
    fn basic_selection_enforces_every_filter_and_preserves_requested_direction() {
        let mut valid_call = synthetic_option("GGAL", 100.0, 101.0, OptionKind::Call, 100);
        valid_call.symbol = "VALID-CALL".into();
        valid_call.expiry_days = 2;
        let mut valid_put = synthetic_option("GGAL", 100.0, 99.0, OptionKind::Put, 100);
        valid_put.symbol = "VALID-PUT".into();
        valid_put.expiry_days = 2;
        let mut too_early = valid_call.clone();
        too_early.symbol = "TOO-EARLY".into();
        too_early.expiry_days = 0;
        let mut no_volume = valid_call.clone();
        no_volume.symbol = "NO-VOLUME".into();
        no_volume.volume = 0;
        let mut no_bid = valid_call.clone();
        no_bid.symbol = "NO-BID".into();
        no_bid.bid = None;
        let mut no_ask = valid_call.clone();
        no_ask.symbol = "NO-ASK".into();
        no_ask.ask = None;
        let frame = MarketFrame {
            underlying: synthetic_frame("GGAL", 100.0, 100).underlying,
            options: vec![
                valid_put.clone(),
                too_early,
                no_volume,
                no_bid,
                no_ask,
                valid_call.clone(),
            ],
            option_chain_quality: None,
            vix: None,
        };

        assert_eq!(
            select_option(&frame, OptionKind::Call, 1).map(|option| option.symbol.as_str()),
            Some("VALID-CALL")
        );
        assert_eq!(
            select_option(&frame, OptionKind::Put, 1).map(|option| option.symbol.as_str()),
            Some("VALID-PUT")
        );
        assert_eq!(select_option(&frame, OptionKind::Call, 3), None);
    }

    #[test]
    fn basic_selection_uses_nearest_strike_independently_of_input_order() {
        let make_frame = |strikes: [f64; 2]| MarketFrame {
            underlying: synthetic_frame("GGAL", 100.0, 100).underlying,
            options: strikes
                .into_iter()
                .map(|strike| {
                    let mut option = synthetic_option("GGAL", 100.0, strike, OptionKind::Call, 100);
                    option.expiry_days = 10;
                    option
                })
                .collect(),
            option_chain_quality: None,
            vix: None,
        };
        for strikes in [[80.0, 101.0], [101.0, 80.0]] {
            assert_eq!(
                select_option(&make_frame(strikes), OptionKind::Call, 10)
                    .map(|option| option.strike),
                Some(101.0)
            );
        }
    }

    #[test]
    fn criteria_selection_enforces_each_closed_eligibility_boundary() {
        let base = synthetic_option("GGAL", 100.0, 110.0, OptionKind::Call, 100);
        let criteria = OptionSelectionCriteria {
            min_expiry_days: 10,
            target_expiry_days: 20,
            max_expiry_days: 30,
            min_volume: 5,
            max_spread_percentage: 20.0,
            max_moneyness_distance_percentage: 10.0,
            now_secs: 110,
            max_age_secs: 10,
            operating_cost_percentage: 0.3,
            slippage_bps: 25.0,
        };
        let eligible = |mut option: OptionQuote| {
            option.expiry_days = 10;
            option.volume = 5;
            option.bid = Some(90.0);
            option.ask = Some(110.0);
            MarketFrame {
                underlying: synthetic_frame("GGAL", 100.0, 100).underlying,
                options: vec![option],
                option_chain_quality: None,
                vix: None,
            }
        };
        let frame = eligible(base.clone());
        assert!(select_option_with_criteria(&frame, OptionKind::Call, criteria).is_some());

        let mut cases = Vec::new();
        let mut wrong_kind = base.clone();
        wrong_kind.kind = OptionKind::Put;
        cases.push(eligible(wrong_kind));
        let mut too_early = base.clone();
        too_early.expiry_days = 9;
        let mut frame = eligible(too_early);
        frame.options[0].expiry_days = 9;
        cases.push(frame);
        let mut too_late = eligible(base.clone());
        too_late.options[0].expiry_days = 31;
        cases.push(too_late);
        let mut no_volume = eligible(base.clone());
        no_volume.options[0].volume = 4;
        cases.push(no_volume);
        let mut too_far = eligible(base.clone());
        too_far.options[0].strike = 110.01;
        cases.push(too_far);
        let mut stale = eligible(base.clone());
        stale.options[0].timestamp_secs = 99;
        stale.options[0].exchange_timestamp_secs = Some(99);
        stale.options[0].received_at_secs = 99;
        cases.push(stale);
        let mut wide = eligible(base);
        wide.options[0].bid = Some(89.0);
        wide.options[0].ask = Some(111.0);
        cases.push(wide);

        for rejected in cases {
            assert!(select_option_with_criteria(&rejected, OptionKind::Call, criteria).is_none());
        }
    }

    #[test]
    fn criteria_selection_orders_by_friction_then_volume_then_moneyness() {
        let criteria = OptionSelectionCriteria {
            min_expiry_days: 1,
            target_expiry_days: 20,
            max_expiry_days: 30,
            min_volume: 1,
            max_spread_percentage: 50.0,
            max_moneyness_distance_percentage: 50.0,
            now_secs: 100,
            max_age_secs: 10,
            operating_cost_percentage: 0.3,
            slippage_bps: 25.0,
        };
        let option = |symbol: &str, strike: f64, volume: u64, bid: f64, ask: f64| {
            let mut option = synthetic_option("GGAL", 100.0, strike, OptionKind::Call, 100);
            option.symbol = symbol.into();
            option.expiry_days = 20;
            option.volume = volume;
            option.bid = Some(bid);
            option.ask = Some(ask);
            option
        };
        let frame = |options| MarketFrame {
            underlying: synthetic_frame("GGAL", 100.0, 100).underlying,
            options,
            option_chain_quality: None,
            vix: None,
        };

        let higher_friction = option("HIGH-FRICTION", 100.0, 100, 80.0, 120.0);
        let lower_friction = option("LOW-FRICTION", 120.0, 1, 90.0, 110.0);
        assert_eq!(
            select_option_with_criteria(
                &frame(vec![higher_friction, lower_friction]),
                OptionKind::Call,
                criteria,
            )
            .map(|selected| selected.symbol.as_str()),
            Some("LOW-FRICTION")
        );

        let low_volume = option("LOW-VOLUME", 100.0, 5, 90.0, 110.0);
        let high_volume = option("HIGH-VOLUME", 120.0, 6, 90.0, 110.0);
        assert_eq!(
            select_option_with_criteria(
                &frame(vec![low_volume, high_volume]),
                OptionKind::Call,
                criteria,
            )
            .map(|selected| selected.symbol.as_str()),
            Some("HIGH-VOLUME")
        );

        for strikes in [[130.0, 101.0], [101.0, 130.0]] {
            let options = strikes
                .into_iter()
                .enumerate()
                .map(|(index, strike)| option(&format!("MONEY-{index}"), strike, 5, 90.0, 110.0))
                .collect::<Vec<_>>();
            assert_eq!(
                select_option_with_criteria(&frame(options), OptionKind::Call, criteria)
                    .map(|selected| selected.strike),
                Some(101.0)
            );
        }
    }

    #[test]
    fn rejects_stale_quotes_for_execution() {
        let mut market = ReplayMarket::synthetic("GAL");
        let frame = market.next_frame().unwrap().unwrap();
        let option = &frame.options[0];
        assert!(option.validate_freshness(20, 10).is_err());
        assert!(option.validate_freshness(11, 10).is_ok());
    }

    #[test]
    fn rejects_entry_when_spread_is_too_wide() {
        let mut market = ReplayMarket::synthetic("GAL");
        let mut frame = market.next_frame().unwrap().unwrap();
        frame.options[0].bid = Some(1.0);
        frame.options[0].ask = Some(2.0);
        assert!(frame.options[0]
            .validate_entry_quality(1, 10, 20.0)
            .is_err());
        assert!((frame.options[0].spread_percentage().unwrap() - 66.666_666).abs() < 1e-5);
    }

    #[test]
    fn criteria_skip_a_bad_series_and_select_a_valid_alternative() {
        let mut market = ReplayMarket::synthetic("GAL");
        let mut frame = market.next_frame().unwrap().unwrap();
        let nearest = frame
            .options
            .iter_mut()
            .find(|option| option.kind == OptionKind::Call && option.strike == 100.0)
            .unwrap();
        nearest.bid = Some(1.0);
        nearest.ask = Some(2.0);
        let selected = select_option_with_criteria(
            &frame,
            OptionKind::Call,
            OptionSelectionCriteria {
                min_expiry_days: 1,
                target_expiry_days: 1,
                max_expiry_days: 45,
                min_volume: 10,
                max_spread_percentage: 10.0,
                max_moneyness_distance_percentage: 10.0,
                now_secs: 1,
                max_age_secs: 10,
                operating_cost_percentage: 0.2,
                slippage_bps: 25.0,
            },
        )
        .unwrap();
        assert_ne!(selected.strike, 100.0);
        assert!(selected.validate_entry_quality(1, 10, 10.0).is_ok());
    }
}
