use serde::{Deserialize, Serialize};

use crate::learning::ValidationTrade;

const FEATURE_COUNT: usize = 7;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SignalFeatures(pub [f64; FEATURE_COUNT]);

impl SignalFeatures {
    pub fn from_trade(trade: &ValidationTrade) -> Option<Self> {
        let context = &trade.context;
        Some(Self([
            context.entry_spread_percentage?,
            context.option_volume? as f64,
            context.days_to_expiry? as f64,
            context.moneyness_distance_percentage?,
            context.trend_confidence?,
            context.trend_r_squared?,
            context.trend_slope_percent_per_minute?.abs(),
        ]))
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LogisticMetaFilter {
    means: [f64; FEATURE_COUNT],
    scales: [f64; FEATURE_COUNT],
    weights: [f64; FEATURE_COUNT],
    bias: f64,
    pub threshold: f64,
}

impl LogisticMetaFilter {
    pub fn fit(examples: &[(SignalFeatures, bool)], l2: f64, threshold: f64) -> Option<Self> {
        if examples.len() < 30
            || examples.iter().all(|item| item.1)
            || examples.iter().all(|item| !item.1)
        {
            return None;
        }
        let mut means = [0.0; FEATURE_COUNT];
        for (features, _) in examples {
            for (index, value) in features.0.iter().enumerate() {
                means[index] += value;
            }
        }
        for mean in &mut means {
            *mean /= examples.len() as f64;
        }
        let mut scales = [0.0; FEATURE_COUNT];
        for (features, _) in examples {
            for index in 0..FEATURE_COUNT {
                scales[index] += (features.0[index] - means[index]).powi(2);
            }
        }
        for scale in &mut scales {
            *scale = (*scale / examples.len() as f64).sqrt().max(1e-9);
        }
        let mut model = Self {
            means,
            scales,
            weights: [0.0; FEATURE_COUNT],
            bias: 0.0,
            threshold: threshold.clamp(0.5, 0.9),
        };
        let learning_rate = 0.05;
        for _ in 0..1_000 {
            let mut weight_gradient = [0.0; FEATURE_COUNT];
            let mut bias_gradient = 0.0;
            for (features, profitable) in examples {
                let normalized = model.normalize(*features);
                let error = model.probability_normalized(normalized) - f64::from(*profitable);
                bias_gradient += error;
                for index in 0..FEATURE_COUNT {
                    weight_gradient[index] += error * normalized[index];
                }
            }
            let count = examples.len() as f64;
            model.bias -= learning_rate * bias_gradient / count;
            for (index, gradient) in weight_gradient.iter().enumerate() {
                model.weights[index] -=
                    learning_rate * (gradient / count + l2.max(0.0) * model.weights[index]);
            }
        }
        Some(model)
    }

    pub fn probability(&self, features: SignalFeatures) -> f64 {
        self.probability_normalized(self.normalize(features))
    }

    pub fn allows(&self, features: SignalFeatures) -> bool {
        self.probability(features) >= self.threshold
    }

    fn normalize(&self, features: SignalFeatures) -> [f64; FEATURE_COUNT] {
        let mut normalized = [0.0; FEATURE_COUNT];
        for (index, value) in normalized.iter_mut().enumerate() {
            *value = (features.0[index] - self.means[index]) / self.scales[index];
        }
        normalized
    }

    fn probability_normalized(&self, normalized: [f64; FEATURE_COUNT]) -> f64 {
        let score = self
            .weights
            .iter()
            .zip(normalized)
            .fold(self.bias, |sum, (weight, value)| sum + weight * value);
        1.0 / (1.0 + (-score.clamp(-30.0, 30.0)).exp())
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct MetaFilterAssessment {
    pub train_examples: usize,
    pub holdout_examples: usize,
    pub accepted_holdout: usize,
    pub baseline_stressed_expectancy: f64,
    pub filtered_stressed_expectancy: f64,
    pub recommended: bool,
    pub model: Option<LogisticMetaFilter>,
}

pub fn assess_meta_filter(trades: &[ValidationTrade]) -> MetaFilterAssessment {
    let usable = trades
        .iter()
        .filter_map(|trade| SignalFeatures::from_trade(trade).map(|features| (features, trade)))
        .collect::<Vec<_>>();
    if usable.len() < 50 {
        return MetaFilterAssessment::default();
    }
    let split = (usable.len() * 4 / 5).max(30).min(usable.len() - 10);
    let training = usable[..split]
        .iter()
        .map(|(features, trade)| (*features, trade.stressed_net_pnl > 0.0))
        .collect::<Vec<_>>();
    let Some(model) = LogisticMetaFilter::fit(&training, 0.01, 0.55) else {
        return MetaFilterAssessment::default();
    };
    let holdout = &usable[split..];
    let baseline = holdout
        .iter()
        .map(|(_, trade)| trade.stressed_net_pnl)
        .sum::<f64>()
        / holdout.len() as f64;
    let accepted = holdout
        .iter()
        .filter(|(features, _)| model.allows(*features))
        .collect::<Vec<_>>();
    let filtered = if accepted.is_empty() {
        0.0
    } else {
        accepted
            .iter()
            .map(|(_, trade)| trade.stressed_net_pnl)
            .sum::<f64>()
            / accepted.len() as f64
    };
    let recommended = accepted.len() >= 10 && filtered > 0.0 && filtered > baseline;
    MetaFilterAssessment {
        train_examples: split,
        holdout_examples: holdout.len(),
        accepted_holdout: accepted.len(),
        baseline_stressed_expectancy: baseline,
        filtered_stressed_expectancy: filtered,
        recommended,
        model: Some(model),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn regularized_model_learns_a_separable_signal() {
        let examples = (0..100)
            .map(|index| {
                let value = index as f64 - 50.0;
                (
                    SignalFeatures([value, 1.0, 2.0, 3.0, 4.0, 5.0, 6.0]),
                    value > 0.0,
                )
            })
            .collect::<Vec<_>>();
        let model = LogisticMetaFilter::fit(&examples, 0.01, 0.55).unwrap();
        assert!(model.probability(SignalFeatures([30.0, 1.0, 2.0, 3.0, 4.0, 5.0, 6.0])) > 0.8);
        assert!(model.probability(SignalFeatures([-30.0, 1.0, 2.0, 3.0, 4.0, 5.0, 6.0])) < 0.2);
    }
}
