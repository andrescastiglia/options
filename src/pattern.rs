use std::collections::VecDeque;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Direction {
    Up,
    Down,
    Neutral,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct PriceSample {
    pub price: f64,
    pub timestamp_secs: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Trend {
    pub direction: Direction,
    pub confirmed: bool,
    pub samples: usize,
    pub sma: f64,
    pub slope: f64,
    pub volatility: f64,
    pub r_squared: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TrendDetector {
    samples: VecDeque<PriceSample>,
    capacity: usize,
    min_samples: usize,
    last_direction: Direction,
    confirmation_count: usize,
}

impl TrendDetector {
    pub fn new(capacity: usize, min_samples: usize) -> Self {
        assert!(capacity > 0 && min_samples > 0);
        Self {
            samples: VecDeque::with_capacity(capacity),
            capacity,
            min_samples,
            last_direction: Direction::Neutral,
            confirmation_count: 0,
        }
    }

    pub fn push(&mut self, sample: PriceSample) -> Option<Trend> {
        if !sample.price.is_finite()
            || sample.price <= 0.0
            || self
                .samples
                .back()
                .is_some_and(|last| sample.timestamp_secs < last.timestamp_secs)
        {
            return None;
        }
        if self.samples.len() == self.capacity {
            self.samples.pop_front();
        }
        self.samples.push_back(sample);
        let direction = direction_for(&self.samples);
        if direction == Direction::Neutral {
            self.confirmation_count = 0;
            self.last_direction = Direction::Neutral;
        } else if direction == self.last_direction {
            self.confirmation_count += 1;
        } else {
            self.last_direction = direction;
            self.confirmation_count = 1;
        }
        Some(self.detect())
    }

    pub fn detect(&self) -> Trend {
        let values: Vec<f64> = self.samples.iter().map(|sample| sample.price).collect();
        if values.is_empty() {
            return Trend {
                direction: Direction::Neutral,
                confirmed: false,
                samples: 0,
                sma: 0.0,
                slope: 0.0,
                volatility: 0.0,
                r_squared: None,
            };
        }
        let sma = values.iter().sum::<f64>() / values.len() as f64;
        let direction = direction_for(&self.samples);
        let slope = linear_slope(&values);
        let volatility = (values
            .iter()
            .map(|value| (value - sma).powi(2))
            .sum::<f64>()
            / values.len() as f64)
            .sqrt();
        Trend {
            direction,
            confirmed: direction != Direction::Neutral
                && self.confirmation_count >= self.min_samples,
            samples: self.confirmation_count,
            sma,
            slope,
            volatility,
            r_squared: r_squared(&values),
        }
    }

    pub fn opposite_confirmed(&self, position_direction: Direction, required: usize) -> bool {
        if self.samples.len() < required {
            return false;
        }
        let mut recent: Vec<f64> = self
            .samples
            .iter()
            .rev()
            .take(required)
            .map(|sample| sample.price)
            .collect();
        recent.reverse();
        match position_direction {
            Direction::Up => recent.windows(2).all(|pair| pair[1] < pair[0]),
            Direction::Down => recent.windows(2).all(|pair| pair[1] > pair[0]),
            Direction::Neutral => false,
        }
    }
}

fn direction_for(samples: &VecDeque<PriceSample>) -> Direction {
    if samples.is_empty() {
        return Direction::Neutral;
    }
    let sma = samples.iter().map(|sample| sample.price).sum::<f64>() / samples.len() as f64;
    let current = samples.back().map_or(0.0, |sample| sample.price);
    if current > sma * 1.001 {
        Direction::Up
    } else if current < sma * 0.999 {
        Direction::Down
    } else {
        Direction::Neutral
    }
}

fn linear_slope(values: &[f64]) -> f64 {
    if values.len() < 2 {
        return 0.0;
    }
    let n = values.len() as f64;
    let mean_x = (n - 1.0) / 2.0;
    let mean_y = values.iter().sum::<f64>() / n;
    let numerator: f64 = values
        .iter()
        .enumerate()
        .map(|(index, value)| (index as f64 - mean_x) * (value - mean_y))
        .sum();
    let denominator: f64 = values
        .iter()
        .enumerate()
        .map(|(index, _)| (index as f64 - mean_x).powi(2))
        .sum();
    numerator / denominator
}

fn r_squared(values: &[f64]) -> Option<f64> {
    if values.len() < 2 {
        return None;
    }
    let mean = values.iter().sum::<f64>() / values.len() as f64;
    let slope = linear_slope(values);
    let intercept = mean - slope * (values.len() as f64 - 1.0) / 2.0;
    let total: f64 = values.iter().map(|value| (value - mean).powi(2)).sum();
    if total == 0.0 {
        return Some(1.0);
    }
    let residual: f64 = values
        .iter()
        .enumerate()
        .map(|(index, value)| (value - (intercept + slope * index as f64)).powi(2))
        .sum();
    Some((1.0 - residual / total).clamp(0.0, 1.0))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(price: f64, timestamp_secs: i64) -> PriceSample {
        PriceSample {
            price,
            timestamp_secs,
        }
    }

    #[test]
    fn confirms_uptrend_after_consecutive_samples() {
        let mut detector = TrendDetector::new(10, 3);
        let trend = [100.0, 101.0, 102.0, 103.0]
            .into_iter()
            .enumerate()
            .filter_map(|(timestamp, price)| detector.push(sample(price, timestamp as i64)))
            .last()
            .unwrap();
        assert_eq!(trend.direction, Direction::Up);
        assert!(trend.confirmed);
        assert!(trend.slope > 0.0);
    }

    #[test]
    fn rejects_invalid_or_out_of_order_prices() {
        let mut detector = TrendDetector::new(10, 2);
        assert!(detector.push(sample(0.0, 1)).is_none());
        assert!(detector.push(sample(100.0, 2)).is_some());
        assert!(detector.push(sample(101.0, 1)).is_none());
    }

    #[test]
    fn confirms_reversal_from_monotonic_opposite_samples() {
        let mut detector = TrendDetector::new(10, 2);
        for (timestamp, price) in [100.0, 101.0, 102.0, 101.5, 101.0, 100.5]
            .into_iter()
            .enumerate()
        {
            detector.push(sample(price, timestamp as i64));
        }
        assert!(detector.opposite_confirmed(Direction::Up, 3));
        assert!(!detector.opposite_confirmed(Direction::Down, 3));
    }
}
