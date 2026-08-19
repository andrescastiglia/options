use std::collections::VecDeque;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Up,
    Down,
    Neutral,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PriceSample {
    pub price: f64,
    pub timestamp_secs: i64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Trend {
    pub direction: Direction,
    pub confirmed: bool,
    pub samples: usize,
    pub sma: f64,
    pub slope: f64,
    pub volatility: f64,
    pub r_squared: Option<f64>,
}

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
        Some(self.detect())
    }

    pub fn detect(&mut self) -> Trend {
        let values: Vec<f64> = self.samples.iter().map(|sample| sample.price).collect();
        let sma = values.iter().sum::<f64>() / values.len() as f64;
        let current = *values.last().unwrap_or(&0.0);
        let direction = if current > sma * 1.001 {
            Direction::Up
        } else if current < sma * 0.999 {
            Direction::Down
        } else {
            Direction::Neutral
        };
        if direction == Direction::Neutral {
            self.confirmation_count = 0;
        } else if direction == self.last_direction {
            self.confirmation_count += 1;
        } else {
            self.last_direction = direction;
            self.confirmation_count = 1;
        }
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
        let recent: Vec<f64> = self
            .samples
            .iter()
            .rev()
            .take(required)
            .map(|sample| sample.price)
            .collect();
        let sma = recent.iter().sum::<f64>() / recent.len() as f64;
        match position_direction {
            Direction::Up => recent.iter().all(|price| *price < sma),
            Direction::Down => recent.iter().all(|price| *price > sma),
            Direction::Neutral => false,
        }
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
        for (timestamp, price) in [100.0, 101.0, 102.0, 103.0].into_iter().enumerate() {
            detector.push(sample(price, timestamp as i64));
        }
        let trend = detector.detect();
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
}
