use std::{
    collections::HashMap,
    time::{Duration, Instant},
};

use crate::{errors::AppError, pattern::PriceSample};

pub trait MarketDataProvider {
    type Error;

    fn next_price(&mut self) -> Result<PriceSample, Self::Error>;
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Quote {
    pub ticker: &'static str,
    pub last: f64,
    pub bid: Option<f64>,
    pub ask: Option<f64>,
    pub timestamp_secs: i64,
}

impl Quote {
    pub fn validate(&self, previous_timestamp: Option<i64>) -> Result<(), AppError> {
        if !self.last.is_finite() || self.last <= 0.0 {
            return Err(AppError::InvalidMarketData("last debe ser positivo".into()));
        }
        if let (Some(bid), Some(ask)) = (self.bid, self.ask) {
            if !bid.is_finite() || !ask.is_finite() || bid <= 0.0 || ask <= 0.0 || bid > ask {
                return Err(AppError::InvalidMarketData("bid/ask inconsistentes".into()));
            }
        }
        if previous_timestamp.is_some_and(|previous| self.timestamp_secs < previous) {
            return Err(AppError::InvalidMarketData(
                "timestamp fuera de orden".into(),
            ));
        }
        Ok(())
    }
}

#[derive(Debug)]
pub struct PriceCache {
    ttl: Duration,
    entries: HashMap<String, (Quote, Instant)>,
}

impl PriceCache {
    pub fn new(ttl: Duration) -> Self {
        Self {
            ttl,
            entries: HashMap::new(),
        }
    }

    pub fn insert(&mut self, quote: Quote) {
        self.entries
            .insert(quote.ticker.to_string(), (quote, Instant::now()));
    }

    pub fn get(&self, ticker: &str) -> Option<Quote> {
        self.entries
            .get(ticker)
            .and_then(|(quote, stored)| (stored.elapsed() <= self.ttl).then_some(*quote))
    }
}

#[derive(Debug)]
pub struct PriceStream {
    capacity: usize,
    samples: Vec<PriceSample>,
    last_timestamp: Option<i64>,
}

impl PriceStream {
    pub fn new(capacity: usize) -> Self {
        assert!(capacity > 0);
        Self {
            capacity,
            samples: Vec::with_capacity(capacity),
            last_timestamp: None,
        }
    }

    pub fn push_quote(&mut self, quote: Quote) -> Result<PriceSample, AppError> {
        quote.validate(self.last_timestamp)?;
        let sample = PriceSample {
            price: quote.last,
            timestamp_secs: quote.timestamp_secs,
        };
        if self.samples.len() == self.capacity {
            self.samples.remove(0);
        }
        self.samples.push(sample);
        self.last_timestamp = Some(quote.timestamp_secs);
        Ok(sample)
    }

    pub fn samples(&self) -> &[PriceSample] {
        &self.samples
    }
}

#[derive(Debug, Clone)]
pub struct SimulatedMarket {
    prices: Vec<f64>,
    next_index: usize,
    timestamp_secs: i64,
}

impl SimulatedMarket {
    pub fn new(prices: Vec<f64>) -> Self {
        assert!(!prices.is_empty());
        Self {
            prices,
            next_index: 0,
            timestamp_secs: 0,
        }
    }
}

impl MarketDataProvider for SimulatedMarket {
    type Error = &'static str;

    fn next_price(&mut self) -> Result<PriceSample, Self::Error> {
        let price = self
            .prices
            .get(self.next_index)
            .copied()
            .ok_or("simulacion agotada")?;
        self.next_index += 1;
        self.timestamp_secs += 1;
        Ok(PriceSample {
            price,
            timestamp_secs: self.timestamp_secs,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn simulated_market_returns_ordered_samples() {
        let mut market = SimulatedMarket::new(vec![100.0, 101.0]);
        assert_eq!(market.next_price().unwrap().timestamp_secs, 1);
        assert_eq!(market.next_price().unwrap().price, 101.0);
        assert!(market.next_price().is_err());
    }

    #[test]
    fn quote_rejects_crossed_book() {
        let quote = Quote {
            ticker: "GAL",
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
                .push_quote(Quote {
                    ticker: "GAL",
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
}
