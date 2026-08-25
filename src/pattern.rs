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
    #[serde(default)]
    pub warmed_up: bool,
    pub samples: usize,
    pub sma: f64,
    pub slope: f64,
    #[serde(default)]
    pub slope_percent_per_minute: f64,
    pub volatility: f64,
    pub r_squared: Option<f64>,
    #[serde(default)]
    pub confidence: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TrendCriteria {
    pub warmup_samples: usize,
    pub deadband_percentage: f64,
    pub min_slope_percent_per_minute: f64,
    pub min_r_squared: f64,
    pub min_move_volatility_ratio: f64,
}

impl Default for TrendCriteria {
    fn default() -> Self {
        Self {
            warmup_samples: 1,
            deadband_percentage: 0.10,
            min_slope_percent_per_minute: 0.0,
            min_r_squared: 0.0,
            min_move_volatility_ratio: 0.0,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TrendDetector {
    samples: VecDeque<PriceSample>,
    capacity: usize,
    min_samples: usize,
    #[serde(default)]
    criteria: TrendCriteria,
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
            criteria: TrendCriteria::default(),
            last_direction: Direction::Neutral,
            confirmation_count: 0,
        }
    }

    pub fn new_robust(capacity: usize, min_samples: usize, criteria: TrendCriteria) -> Self {
        assert!(capacity > 0 && min_samples > 0 && criteria.warmup_samples > 0);
        Self {
            samples: VecDeque::with_capacity(capacity),
            capacity,
            min_samples,
            criteria,
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
        let direction = direction_for(&self.samples, &self.criteria);
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
                warmed_up: false,
                samples: 0,
                sma: 0.0,
                slope: 0.0,
                slope_percent_per_minute: 0.0,
                volatility: 0.0,
                r_squared: None,
                confidence: 0.0,
            };
        }
        let sma = values.iter().sum::<f64>() / values.len() as f64;
        let direction = direction_for(&self.samples, &self.criteria);
        let slope = linear_slope(&values);
        let volatility = (values
            .iter()
            .map(|value| (value - sma).powi(2))
            .sum::<f64>()
            / values.len() as f64)
            .sqrt();
        let r_squared = r_squared(&values);
        let slope_percent_per_minute = slope_percent_per_minute(&self.samples, sma);
        let move_ratio = if volatility > 0.0 {
            (values.last().copied().unwrap_or(sma) - sma).abs() / volatility
        } else {
            0.0
        };
        let warmed_up = self.samples.len() >= self.criteria.warmup_samples;
        let confidence = r_squared.unwrap_or_default().min(1.0)
            * (move_ratio / self.criteria.min_move_volatility_ratio.max(1.0)).min(1.0);
        Trend {
            direction,
            confirmed: warmed_up
                && direction != Direction::Neutral
                && self.confirmation_count >= self.min_samples,
            warmed_up,
            samples: self.confirmation_count,
            sma,
            slope,
            slope_percent_per_minute,
            volatility,
            r_squared,
            confidence,
        }
    }

    pub fn samples(&self) -> &VecDeque<PriceSample> {
        &self.samples
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

    pub fn robust_opposite_confirmed(
        &self,
        position_direction: Direction,
        required: usize,
    ) -> bool {
        if required < 2 || self.samples.len() < required {
            return false;
        }
        let recent = self
            .samples
            .iter()
            .rev()
            .take(required)
            .rev()
            .copied()
            .collect::<VecDeque<_>>();
        let mut criteria = self.criteria.clone();
        criteria.warmup_samples = required;
        let direction = direction_for(&recent, &criteria);
        matches!(
            (position_direction, direction),
            (Direction::Up, Direction::Down) | (Direction::Down, Direction::Up)
        )
    }

    pub fn reset_confirmation(&mut self) {
        self.last_direction = Direction::Neutral;
        self.confirmation_count = 0;
    }
}

fn direction_for(samples: &VecDeque<PriceSample>, criteria: &TrendCriteria) -> Direction {
    if samples.is_empty() || samples.len() < criteria.warmup_samples {
        return Direction::Neutral;
    }
    let sma = samples.iter().map(|sample| sample.price).sum::<f64>() / samples.len() as f64;
    let current = samples.back().map_or(0.0, |sample| sample.price);
    let values: Vec<f64> = samples.iter().map(|sample| sample.price).collect();
    let volatility = (values
        .iter()
        .map(|value| (value - sma).powi(2))
        .sum::<f64>()
        / values.len() as f64)
        .sqrt();
    let move_ratio = if volatility > 0.0 {
        (current - sma).abs() / volatility
    } else {
        0.0
    };
    let slope = slope_percent_per_minute(samples, sma);
    let quality = r_squared(&values).unwrap_or_default();
    let deadband = criteria.deadband_percentage / 100.0;
    let quality_ok =
        quality >= criteria.min_r_squared && move_ratio >= criteria.min_move_volatility_ratio;
    if quality_ok
        && current > sma * (1.0 + deadband)
        && slope >= criteria.min_slope_percent_per_minute
    {
        Direction::Up
    } else if quality_ok
        && current < sma * (1.0 - deadband)
        && slope <= -criteria.min_slope_percent_per_minute
    {
        Direction::Down
    } else {
        Direction::Neutral
    }
}

fn slope_percent_per_minute(samples: &VecDeque<PriceSample>, mean_price: f64) -> f64 {
    if samples.len() < 2 || mean_price <= 0.0 {
        return 0.0;
    }
    let first = samples.front().map_or(0, |sample| sample.timestamp_secs);
    let last = samples.back().map_or(first, |sample| sample.timestamp_secs);
    let elapsed = last.saturating_sub(first) as f64;
    if elapsed <= 0.0 {
        return 0.0;
    }
    let values: Vec<f64> = samples.iter().map(|sample| sample.price).collect();
    let total_change = linear_slope(&values) * (values.len().saturating_sub(1) as f64);
    (total_change / mean_price) * (60.0 / elapsed) * 100.0
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

    #[test]
    fn robust_detector_waits_for_full_warmup() {
        let mut detector = TrendDetector::new_robust(
            5,
            2,
            TrendCriteria {
                warmup_samples: 4,
                deadband_percentage: 0.0,
                min_slope_percent_per_minute: 0.0,
                min_r_squared: 0.0,
                min_move_volatility_ratio: 0.0,
            },
        );
        for (timestamp, price) in [100.0, 101.0, 102.0].into_iter().enumerate() {
            let trend = detector.push(sample(price, timestamp as i64)).unwrap();
            assert!(!trend.warmed_up);
            assert!(!trend.confirmed);
            assert_eq!(trend.direction, Direction::Neutral);
        }
        let first_warm = detector.push(sample(103.0, 3)).unwrap();
        assert!(first_warm.warmed_up);
        assert!(!first_warm.confirmed);
        let confirmed = detector.push(sample(104.0, 4)).unwrap();
        assert!(confirmed.confirmed);
        assert_eq!(confirmed.direction, Direction::Up);

        for (timestamp, price) in [(5, 103.0), (6, 102.0), (7, 101.0)] {
            detector.push(sample(price, timestamp));
        }
        assert!(detector.robust_opposite_confirmed(Direction::Up, 3));
        assert!(!detector.robust_opposite_confirmed(Direction::Down, 3));
    }
}
