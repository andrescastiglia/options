use std::{
    collections::{HashMap, VecDeque},
    fs::File,
    io::{BufRead, BufReader},
    path::Path,
    time::{Duration, Instant},
};

use serde::{Deserialize, Serialize};

use crate::{errors::AppError, pattern::PriceSample};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum OptionKind {
    Call,
    Put,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UnderlyingQuote {
    pub ticker: String,
    pub last: f64,
    pub bid: Option<f64>,
    pub ask: Option<f64>,
    pub timestamp_secs: i64,
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
    pub last: f64,
    pub bid: Option<f64>,
    pub ask: Option<f64>,
    pub volume: u64,
    pub timestamp_secs: i64,
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
        (midpoint > 0.0).then_some(((ask - bid) / midpoint) * 100.0)
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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MarketFrame {
    pub underlying: UnderlyingQuote,
    pub options: Vec<OptionQuote>,
}

impl MarketFrame {
    pub fn validate(&self, previous_timestamp: Option<i64>) -> Result<(), AppError> {
        self.underlying.validate(previous_timestamp)?;
        for option in &self.options {
            option.validate(&self.underlying.ticker)?;
            if option.timestamp_secs < self.underlying.timestamp_secs - 60 {
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

fn validate_book(last: f64, bid: Option<f64>, ask: Option<f64>) -> Result<(), AppError> {
    if !last.is_finite() || last <= 0.0 {
        return Err(AppError::InvalidMarketData("last debe ser positivo".into()));
    }
    if let (Some(bid), Some(ask)) = (bid, ask) {
        if !bid.is_finite() || !ask.is_finite() || bid <= 0.0 || ask <= 0.0 || bid > ask {
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
                (left.strike - frame.underlying.last).abs(),
            );
            let right_key = (
                right.expiry_days - preferred_expiry_days,
                (right.strike - frame.underlying.last).abs(),
            );
            left_key
                .0
                .cmp(&right_key.0)
                .then_with(|| left_key.1.total_cmp(&right_key.1))
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
        let file = File::open(path)?;
        let mut frames = Vec::new();
        for (index, line) in BufReader::new(file).lines().enumerate() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            let frame = serde_json::from_str(&line).map_err(|error| {
                AppError::InvalidMarketData(format!("linea {} del replay: {error}", index + 1))
            })?;
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
        },
        options,
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
        last,
        bid: Some((last - 0.04).max(0.01)),
        ask: Some(last + 0.04),
        volume: 1_000,
        timestamp_secs,
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
    fn quote_rejects_crossed_book() {
        let quote = UnderlyingQuote {
            ticker: "GAL".into(),
            last: 100.0,
            bid: Some(101.0),
            ask: Some(100.0),
            timestamp_secs: 1,
        };
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
}
