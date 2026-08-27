use serde::{Deserialize, Serialize};

use crate::learning::ValidationTrade;

const BASE_FEATURE_COUNT: usize = 7;
const VIX_FEATURE_COUNT: usize = 9;
const MIN_EXAMPLES: usize = 50;
const MIN_TRAIN_EXAMPLES: usize = 30;
const WALK_FORWARD_FOLDS: usize = 3;

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct MetaFilterPolicy {
    pub min_examples: usize,
    pub min_train_examples: usize,
    pub min_accepted_holdout: usize,
    pub min_coverage: f64,
    pub max_brier_score: f64,
    pub min_positive_fold_ratio: f64,
    pub max_concentration: f64,
    pub nonlinear_enabled: bool,
    #[serde(default)]
    pub tree_enabled: bool,
    #[serde(default = "default_tree_min_improvement")]
    pub tree_min_stressed_expectancy_improvement: f64,
}

impl Default for MetaFilterPolicy {
    fn default() -> Self {
        Self {
            min_examples: MIN_EXAMPLES,
            min_train_examples: MIN_TRAIN_EXAMPLES,
            min_accepted_holdout: 10,
            min_coverage: 0.10,
            max_brier_score: 0.30,
            min_positive_fold_ratio: 2.0 / 3.0,
            max_concentration: 1.0,
            nonlinear_enabled: false,
            tree_enabled: false,
            tree_min_stressed_expectancy_improvement: default_tree_min_improvement(),
        }
    }
}

fn default_tree_min_improvement() -> f64 {
    0.05
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DecisionStump {
    pub feature: usize,
    pub threshold: f64,
    pub left_score: f64,
    pub right_score: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TreeMetaFilter {
    pub base_score: f64,
    pub learning_rate: f64,
    pub stumps: Vec<DecisionStump>,
    pub threshold: f64,
    pub feature_count: usize,
}

impl TreeMetaFilter {
    /// Gradient boosting determinista de stumps. La grilla queda deliberadamente
    /// acotada para que la complejidad efectiva sea auditable.
    pub fn fit(
        examples: &[(SignalFeatures, bool)],
        rounds: usize,
        learning_rate: f64,
        threshold: f64,
        minimum_examples: usize,
    ) -> Option<Self> {
        let feature_count = examples.first()?.0 .0.len();
        if examples.len() < minimum_examples
            || feature_count == 0
            || examples.iter().any(|(features, _)| {
                features.0.len() != feature_count
                    || features.0.iter().any(|value| !value.is_finite())
            })
            || examples.iter().all(|item| item.1)
            || examples.iter().all(|item| !item.1)
        {
            return None;
        }
        let positive = examples.iter().filter(|item| item.1).count() as f64;
        let rate = (positive / examples.len() as f64).clamp(1e-6, 1.0 - 1e-6);
        let base_score = (rate / (1.0 - rate)).ln();
        let learning_rate = learning_rate.clamp(0.01, 0.3);
        let mut scores = vec![base_score; examples.len()];
        let mut stumps = Vec::new();
        for _ in 0..rounds.clamp(1, 32) {
            let residuals = examples
                .iter()
                .enumerate()
                .map(|(index, item)| f64::from(item.1) - sigmoid(scores[index]))
                .collect::<Vec<_>>();
            let mut best: Option<(f64, DecisionStump)> = None;
            for feature in 0..feature_count {
                let mut values = examples
                    .iter()
                    .map(|item| item.0 .0[feature])
                    .collect::<Vec<_>>();
                values.sort_by(f64::total_cmp);
                values.dedup_by(|a, b| (*a - *b).abs() < 1e-12);
                for quantile in [1_usize, 2, 3] {
                    let threshold_index = quantile * (values.len() - 1) / 4;
                    let threshold_value = values[threshold_index];
                    let (mut left_sum, mut left_n, mut right_sum, mut right_n) =
                        (0.0, 0_usize, 0.0, 0_usize);
                    for (index, example) in examples.iter().enumerate() {
                        if example.0 .0[feature] <= threshold_value {
                            left_sum += residuals[index];
                            left_n += 1;
                        } else {
                            right_sum += residuals[index];
                            right_n += 1;
                        }
                    }
                    if left_n == 0 || right_n == 0 {
                        continue;
                    }
                    let left_score = left_sum / left_n as f64;
                    let right_score = right_sum / right_n as f64;
                    let error = examples
                        .iter()
                        .enumerate()
                        .map(|(index, example)| {
                            let prediction = if example.0 .0[feature] <= threshold_value {
                                left_score
                            } else {
                                right_score
                            };
                            (residuals[index] - prediction).powi(2)
                        })
                        .sum::<f64>();
                    if best.as_ref().is_none_or(|candidate| error < candidate.0) {
                        best = Some((
                            error,
                            DecisionStump {
                                feature,
                                threshold: threshold_value,
                                left_score,
                                right_score,
                            },
                        ));
                    }
                }
            }
            let (_, stump) = best?;
            for (index, example) in examples.iter().enumerate() {
                let update = if example.0 .0[stump.feature] <= stump.threshold {
                    stump.left_score
                } else {
                    stump.right_score
                };
                scores[index] += learning_rate * update;
            }
            stumps.push(stump);
        }
        Some(Self {
            base_score,
            learning_rate,
            stumps,
            threshold: threshold.clamp(0.5, 0.9),
            feature_count,
        })
    }

    pub fn probability(&self, features: &SignalFeatures) -> Option<f64> {
        if features.0.len() != self.feature_count {
            return None;
        }
        let score = self.stumps.iter().fold(self.base_score, |score, stump| {
            let update = if features.0[stump.feature] <= stump.threshold {
                stump.left_score
            } else {
                stump.right_score
            };
            score + self.learning_rate * update
        });
        Some(sigmoid(score))
    }
}

fn sigmoid(score: f64) -> f64 {
    1.0 / (1.0 + (-score.clamp(-30.0, 30.0)).exp())
}

#[derive(Debug, Clone, PartialEq)]
pub struct SignalFeatures(pub Vec<f64>);

impl<const N: usize> From<[f64; N]> for SignalFeatures {
    fn from(value: [f64; N]) -> Self {
        Self(value.to_vec())
    }
}

impl SignalFeatures {
    pub fn from_trade(trade: &ValidationTrade) -> Option<Self> {
        Self::base_from_trade(trade)
    }

    pub fn base_from_trade(trade: &ValidationTrade) -> Option<Self> {
        let context = &trade.context;
        Some(
            [
                context.entry_spread_percentage?,
                context.option_volume? as f64,
                context.days_to_expiry? as f64,
                context.moneyness_distance_percentage?,
                context.trend_confidence?,
                context.trend_r_squared?,
                context.trend_slope_percent_per_minute?.abs(),
            ]
            .into(),
        )
    }

    pub fn vix_from_trade(trade: &ValidationTrade) -> Option<Self> {
        let mut values = Self::base_from_trade(trade)?.0;
        values.push(trade.context.vix_level?);
        values.push(trade.context.vix_change_percentage?);
        Some(Self(values))
    }

    pub fn with_vix(mut self, level: f64, change_percentage: f64) -> Self {
        self.0.push(level);
        self.0.push(change_percentage);
        self
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LogisticMetaFilter {
    means: Vec<f64>,
    scales: Vec<f64>,
    weights: Vec<f64>,
    bias: f64,
    pub threshold: f64,
    #[serde(default)]
    nonlinear: bool,
    #[serde(default)]
    raw_feature_count: usize,
}

impl LogisticMetaFilter {
    pub fn fit(examples: &[(SignalFeatures, bool)], l2: f64, threshold: f64) -> Option<Self> {
        Self::fit_variant(examples, l2, threshold, false, MIN_TRAIN_EXAMPLES)
    }

    fn fit_variant(
        examples: &[(SignalFeatures, bool)],
        l2: f64,
        threshold: f64,
        nonlinear: bool,
        minimum_examples: usize,
    ) -> Option<Self> {
        let first_features = &examples.first()?.0;
        let raw_feature_count = first_features.0.len();
        let feature_count = if nonlinear {
            raw_feature_count * 2
        } else {
            raw_feature_count
        };
        if feature_count == 0
            || examples.len() < minimum_examples
            || examples.iter().all(|item| item.1)
            || examples.iter().all(|item| !item.1)
            || examples.iter().any(|(features, _)| {
                features.0.len() != feature_count
                    || features.0.iter().any(|value| !value.is_finite())
            })
        {
            return None;
        }
        let mut means = vec![0.0; feature_count];
        for (features, _) in examples {
            for (index, value) in transformed(features, nonlinear).iter().enumerate() {
                means[index] += value;
            }
        }
        for mean in &mut means {
            *mean /= examples.len() as f64;
        }
        let mut scales = vec![0.0; feature_count];
        for (features, _) in examples {
            let values = transformed(features, nonlinear);
            for index in 0..feature_count {
                scales[index] += (values[index] - means[index]).powi(2);
            }
        }
        for scale in &mut scales {
            *scale = (*scale / examples.len() as f64).sqrt().max(1e-9);
        }
        let mut model = Self {
            means,
            scales,
            weights: vec![0.0; feature_count],
            bias: 0.0,
            threshold: threshold.clamp(0.5, 0.9),
            nonlinear,
            raw_feature_count,
        };
        let learning_rate = 0.05;
        for _ in 0..1_000 {
            let mut weight_gradient = vec![0.0; feature_count];
            let mut bias_gradient = 0.0;
            for (features, profitable) in examples {
                let normalized = model.normalize(features)?;
                let error = model.probability_normalized(&normalized) - f64::from(*profitable);
                bias_gradient += error;
                for index in 0..feature_count {
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

    pub fn probability(&self, features: &SignalFeatures) -> Option<f64> {
        Some(self.probability_normalized(&self.normalize(features)?))
    }

    pub fn allows(&self, features: &SignalFeatures) -> bool {
        self.probability(features)
            .is_some_and(|probability| probability >= self.threshold)
    }

    pub fn feature_count(&self) -> usize {
        if self.raw_feature_count == 0 {
            self.weights.len()
        } else {
            self.raw_feature_count
        }
    }

    pub fn is_nonlinear(&self) -> bool {
        self.nonlinear
    }

    fn normalize(&self, features: &SignalFeatures) -> Option<Vec<f64>> {
        let raw_count = if self.raw_feature_count == 0 {
            self.weights.len()
        } else {
            self.raw_feature_count
        };
        (features.0.len() == raw_count).then(|| {
            transformed(features, self.nonlinear)
                .iter()
                .enumerate()
                .map(|(index, value)| (value - self.means[index]) / self.scales[index])
                .collect()
        })
    }

    fn probability_normalized(&self, normalized: &[f64]) -> f64 {
        let score = self
            .weights
            .iter()
            .zip(normalized)
            .fold(self.bias, |sum, (weight, value)| sum + weight * value);
        1.0 / (1.0 + (-score.clamp(-30.0, 30.0)).exp())
    }
}

fn transformed(features: &SignalFeatures, nonlinear: bool) -> Vec<f64> {
    if !nonlinear {
        return features.0.clone();
    }
    let mut values = features.0.clone();
    values.extend(features.0.iter().map(|value| value * value));
    values
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
    #[serde(default)]
    pub walk_forward_folds: usize,
    #[serde(default)]
    pub vix_examples: usize,
    #[serde(default)]
    pub vix_holdout_examples: usize,
    #[serde(default)]
    pub vix_accepted_holdout: usize,
    #[serde(default)]
    pub without_vix_stressed_expectancy: f64,
    #[serde(default)]
    pub with_vix_stressed_expectancy: f64,
    #[serde(default)]
    pub vix_recommended: bool,
    #[serde(default)]
    pub uses_vix: bool,
    #[serde(default)]
    pub holdout_coverage: f64,
    #[serde(default)]
    pub brier_score: f64,
    #[serde(default)]
    pub positive_fold_ratio: f64,
    #[serde(default)]
    pub maximum_concentration: f64,
    #[serde(default)]
    pub nonlinear_recommended: bool,
    #[serde(default)]
    pub tree_model: Option<TreeMetaFilter>,
    #[serde(default)]
    pub tree_stressed_expectancy: f64,
    #[serde(default)]
    pub tree_brier_score: f64,
    #[serde(default)]
    pub tree_holdout_coverage: f64,
    #[serde(default)]
    pub tree_recommended: bool,
    #[serde(default)]
    pub tree_positive_fold_ratio: f64,
    #[serde(default)]
    pub tree_maximum_concentration: f64,
    #[serde(default)]
    pub tree_calibration_bins: Vec<CalibrationBin>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CalibrationBin {
    pub lower_probability: f64,
    pub upper_probability: f64,
    pub observations: usize,
    pub predicted_mean: f64,
    pub realized_rate: f64,
}

#[derive(Debug, Default)]
struct WalkForwardResult {
    train_examples: usize,
    holdout_examples: usize,
    accepted: usize,
    baseline_expectancy: f64,
    filtered_expectancy: f64,
    folds: usize,
    positive_folds: usize,
    brier_score: f64,
    maximum_concentration: f64,
}

pub fn assess_meta_filter(trades: &[ValidationTrade]) -> MetaFilterAssessment {
    assess_meta_filter_with_policy(trades, MetaFilterPolicy::default())
}

pub fn assess_meta_filter_with_policy(
    trades: &[ValidationTrade],
    policy: MetaFilterPolicy,
) -> MetaFilterAssessment {
    let mut chronological = trades.iter().collect::<Vec<_>>();
    chronological.sort_by_key(|trade| (trade.context.opened_at_secs, trade.closed_at_secs));

    let base = chronological
        .iter()
        .filter_map(|trade| {
            SignalFeatures::base_from_trade(trade).map(|features| (features, *trade))
        })
        .collect::<Vec<_>>();
    if base.len() < policy.min_examples {
        return MetaFilterAssessment::default();
    }
    debug_assert!(base
        .iter()
        .all(|(features, _)| features.0.len() == BASE_FEATURE_COUNT));
    let base_walk = walk_forward(&base, policy);
    let base_recommended = walk_is_recommended(&base_walk, policy)
        && base_walk.filtered_expectancy > 0.0
        && base_walk.filtered_expectancy > base_walk.baseline_expectancy;

    let vix = chronological
        .iter()
        .filter_map(|trade| {
            SignalFeatures::vix_from_trade(trade).map(|features| (features, *trade))
        })
        .collect::<Vec<_>>();
    debug_assert!(vix
        .iter()
        .all(|(features, _)| features.0.len() == VIX_FEATURE_COUNT));
    let (without_vix_walk, with_vix_walk) = if vix.len() >= policy.min_examples {
        let companion = vix
            .iter()
            .filter_map(|(_, trade)| {
                SignalFeatures::base_from_trade(trade).map(|features| (features, *trade))
            })
            .collect::<Vec<_>>();
        (walk_forward(&companion, policy), walk_forward(&vix, policy))
    } else {
        (WalkForwardResult::default(), WalkForwardResult::default())
    };
    let vix_improves = walk_is_recommended(&with_vix_walk, policy)
        && with_vix_walk.filtered_expectancy > 0.0
        && with_vix_walk.filtered_expectancy > without_vix_walk.filtered_expectancy;

    let deploy_examples = if vix_improves { &vix } else { &base };
    let model = (base_recommended || vix_improves)
        .then(|| fit_all(deploy_examples, policy))
        .flatten();
    let vix_recommended = vix_improves && model.is_some();
    let nonlinear_recommended = model.as_ref().is_some_and(LogisticMetaFilter::is_nonlinear);
    let tree = if policy.tree_enabled {
        assess_tree_candidate(&base, policy)
    } else {
        TreeCandidateAssessment::default()
    };
    let best_simple = base_walk
        .filtered_expectancy
        .max(with_vix_walk.filtered_expectancy);
    let tree_recommended = tree.model.is_some()
        && tree.stressed_expectancy > 0.0
        && tree.stressed_expectancy
            >= best_simple + policy.tree_min_stressed_expectancy_improvement
        && tree.brier_score <= policy.max_brier_score
        && tree.coverage >= policy.min_coverage
        && tree.positive_fold_ratio >= policy.min_positive_fold_ratio
        && tree.maximum_concentration <= policy.max_concentration;
    MetaFilterAssessment {
        train_examples: base_walk.train_examples,
        holdout_examples: base_walk.holdout_examples,
        accepted_holdout: base_walk.accepted,
        baseline_stressed_expectancy: base_walk.baseline_expectancy,
        filtered_stressed_expectancy: base_walk.filtered_expectancy,
        recommended: model.is_some(),
        model,
        walk_forward_folds: base_walk.folds,
        vix_examples: vix.len(),
        vix_holdout_examples: with_vix_walk.holdout_examples,
        vix_accepted_holdout: with_vix_walk.accepted,
        without_vix_stressed_expectancy: without_vix_walk.filtered_expectancy,
        with_vix_stressed_expectancy: with_vix_walk.filtered_expectancy,
        vix_recommended,
        uses_vix: vix_recommended,
        holdout_coverage: if base_walk.holdout_examples == 0 {
            0.0
        } else {
            base_walk.accepted as f64 / base_walk.holdout_examples as f64
        },
        brier_score: base_walk.brier_score,
        positive_fold_ratio: if base_walk.folds == 0 {
            0.0
        } else {
            base_walk.positive_folds as f64 / base_walk.folds as f64
        },
        maximum_concentration: base_walk.maximum_concentration,
        nonlinear_recommended,
        tree_model: tree.model,
        tree_stressed_expectancy: tree.stressed_expectancy,
        tree_brier_score: tree.brier_score,
        tree_holdout_coverage: tree.coverage,
        tree_recommended,
        tree_positive_fold_ratio: tree.positive_fold_ratio,
        tree_maximum_concentration: tree.maximum_concentration,
        tree_calibration_bins: tree.calibration_bins,
    }
}

#[derive(Debug, Default)]
struct TreeCandidateAssessment {
    model: Option<TreeMetaFilter>,
    stressed_expectancy: f64,
    brier_score: f64,
    coverage: f64,
    positive_fold_ratio: f64,
    maximum_concentration: f64,
    calibration_bins: Vec<CalibrationBin>,
}

fn assess_tree_candidate(
    examples: &[(SignalFeatures, &ValidationTrade)],
    policy: MetaFilterPolicy,
) -> TreeCandidateAssessment {
    if examples.len() < policy.min_examples {
        return TreeCandidateAssessment::default();
    }
    let train_end = (examples.len() / 2).max(policy.min_train_examples);
    let selection_end = (examples.len() * 3 / 4).max(train_end + 1);
    if selection_end >= examples.len() {
        return TreeCandidateAssessment::default();
    }
    let labeled = examples[..train_end]
        .iter()
        .map(|(features, trade)| (features.clone(), trade.stressed_net_pnl > 0.0))
        .collect::<Vec<_>>();
    // Grilla pequeña; la selección sólo observa el tramo previo al holdout.
    let mut best: Option<(f64, TreeMetaFilter)> = None;
    for rounds in [4_usize, 8, 16] {
        for rate in [0.05, 0.1, 0.2] {
            let Some(candidate) =
                TreeMetaFilter::fit(&labeled, rounds, rate, 0.55, policy.min_train_examples)
            else {
                continue;
            };
            let score = examples[train_end..selection_end]
                .iter()
                .filter(|(features, _)| {
                    candidate
                        .probability(features)
                        .is_some_and(|probability| probability >= candidate.threshold)
                })
                .map(|(_, trade)| trade.stressed_net_pnl)
                .sum::<f64>();
            if best.as_ref().is_none_or(|item| score > item.0) {
                best = Some((score, candidate));
            }
        }
    }
    let Some((_, selected)) = best else {
        return TreeCandidateAssessment::default();
    };
    let mut accepted = 0;
    let mut pnl = 0.0;
    let mut brier = 0.0;
    let mut accepted_by_kind = [0_usize; 2];
    let mut accepted_by_session = std::collections::BTreeMap::<i64, usize>::new();
    let mut fold_pnl = [0.0_f64; 3];
    let mut calibration = vec![(0.0_f64, 0_usize, 0_usize); 5];
    let final_holdout = &examples[selection_end..];
    for (index, (features, trade)) in final_holdout.iter().enumerate() {
        let probability = selected.probability(features).unwrap_or_default();
        brier += (probability - f64::from(trade.stressed_net_pnl > 0.0)).powi(2);
        let bin = ((probability * 5.0).floor() as usize).min(4);
        calibration[bin].0 += probability;
        calibration[bin].1 += 1;
        calibration[bin].2 += usize::from(trade.stressed_net_pnl > 0.0);
        if probability >= selected.threshold {
            accepted += 1;
            pnl += trade.stressed_net_pnl;
            fold_pnl[(index * 3 / final_holdout.len().max(1)).min(2)] += trade.stressed_net_pnl;
            accepted_by_kind
                [usize::from(matches!(trade.kind, crate::trading::PositionKind::Put))] += 1;
            *accepted_by_session
                .entry(
                    trade
                        .context
                        .opened_at_secs
                        .saturating_sub(3 * 3_600)
                        .div_euclid(86_400),
                )
                .or_default() += 1;
        }
    }
    let holdout = final_holdout.len();
    let deployed = TreeMetaFilter::fit(
        &examples
            .iter()
            .map(|(features, trade)| (features.clone(), trade.stressed_net_pnl > 0.0))
            .collect::<Vec<_>>(),
        selected.stumps.len(),
        selected.learning_rate,
        selected.threshold,
        policy.min_train_examples,
    );
    let maximum_concentration = if accepted == 0 {
        1.0
    } else {
        let kind = *accepted_by_kind.iter().max().unwrap_or(&0) as f64 / accepted as f64;
        let session =
            accepted_by_session.values().max().copied().unwrap_or(0) as f64 / accepted as f64;
        kind.max(session)
    };
    TreeCandidateAssessment {
        model: deployed,
        stressed_expectancy: if accepted == 0 {
            0.0
        } else {
            pnl / accepted as f64
        },
        brier_score: brier / holdout.max(1) as f64,
        coverage: accepted as f64 / holdout.max(1) as f64,
        positive_fold_ratio: fold_pnl.iter().filter(|value| **value > 0.0).count() as f64 / 3.0,
        maximum_concentration,
        calibration_bins: calibration
            .into_iter()
            .enumerate()
            .map(
                |(index, (probability, observations, positives))| CalibrationBin {
                    lower_probability: index as f64 / 5.0,
                    upper_probability: (index + 1) as f64 / 5.0,
                    observations,
                    predicted_mean: probability / observations.max(1) as f64,
                    realized_rate: positives as f64 / observations.max(1) as f64,
                },
            )
            .collect(),
    }
}

fn walk_is_recommended(result: &WalkForwardResult, policy: MetaFilterPolicy) -> bool {
    result.accepted >= policy.min_accepted_holdout
        && result.holdout_examples > 0
        && result.accepted as f64 / result.holdout_examples as f64 >= policy.min_coverage
        && result.brier_score <= policy.max_brier_score
        && result.folds > 0
        && result.positive_folds as f64 / result.folds as f64 >= policy.min_positive_fold_ratio
        && result.maximum_concentration <= policy.max_concentration
}

fn fit_all(
    examples: &[(SignalFeatures, &ValidationTrade)],
    policy: MetaFilterPolicy,
) -> Option<LogisticMetaFilter> {
    let labeled = examples
        .iter()
        .map(|(features, trade)| (features.clone(), trade.stressed_net_pnl > 0.0))
        .collect::<Vec<_>>();
    let (l2, nonlinear) = select_hyperparameters(examples, policy);
    LogisticMetaFilter::fit_variant(&labeled, l2, 0.55, nonlinear, policy.min_train_examples)
}

fn walk_forward(
    examples: &[(SignalFeatures, &ValidationTrade)],
    policy: MetaFilterPolicy,
) -> WalkForwardResult {
    if examples.len() < policy.min_examples {
        return WalkForwardResult::default();
    }
    let initial_train = (examples.len() / 2).max(policy.min_train_examples);
    if initial_train >= examples.len() {
        return WalkForwardResult::default();
    }
    let fold_size = (examples.len() - initial_train).div_ceil(WALK_FORWARD_FOLDS);
    let mut baseline_total = 0.0;
    let mut filtered_total = 0.0;
    let mut holdout = 0;
    let mut accepted = 0;
    let mut folds = 0;
    let mut positive_folds = 0;
    let mut brier_total = 0.0;
    let mut accepted_by_kind = [0usize; 2];
    let mut accepted_by_session = std::collections::BTreeMap::<i64, usize>::new();
    let mut start = initial_train;
    while start < examples.len() {
        let end = (start + fold_size).min(examples.len());
        let training = examples[..start]
            .iter()
            .map(|(features, trade)| (features.clone(), trade.stressed_net_pnl > 0.0))
            .collect::<Vec<_>>();
        let (l2, nonlinear) = select_hyperparameters(&examples[..start], policy);
        let Some(model) = LogisticMetaFilter::fit_variant(
            &training,
            l2,
            0.55,
            nonlinear,
            policy.min_train_examples,
        ) else {
            start = end;
            continue;
        };
        folds += 1;
        let mut fold_total = 0.0;
        for (features, trade) in &examples[start..end] {
            holdout += 1;
            baseline_total += trade.stressed_net_pnl;
            let probability = model.probability(features).unwrap_or_default();
            brier_total += (probability - f64::from(trade.stressed_net_pnl > 0.0)).powi(2);
            if probability >= model.threshold {
                accepted += 1;
                filtered_total += trade.stressed_net_pnl;
                fold_total += trade.stressed_net_pnl;
                accepted_by_kind
                    [usize::from(matches!(trade.kind, crate::trading::PositionKind::Put))] += 1;
                *accepted_by_session
                    .entry(
                        trade
                            .context
                            .opened_at_secs
                            .saturating_sub(3 * 3_600)
                            .div_euclid(86_400),
                    )
                    .or_default() += 1;
            }
        }
        positive_folds += usize::from(fold_total > 0.0);
        start = end;
    }
    WalkForwardResult {
        train_examples: initial_train,
        holdout_examples: holdout,
        accepted,
        baseline_expectancy: if holdout == 0 {
            0.0
        } else {
            baseline_total / holdout as f64
        },
        filtered_expectancy: if accepted == 0 {
            0.0
        } else {
            filtered_total / accepted as f64
        },
        folds,
        positive_folds,
        brier_score: if holdout == 0 {
            f64::INFINITY
        } else {
            brier_total / holdout as f64
        },
        maximum_concentration: if accepted == 0 {
            1.0
        } else {
            let by_kind = *accepted_by_kind.iter().max().unwrap_or(&0) as f64 / accepted as f64;
            let by_session =
                accepted_by_session.values().max().copied().unwrap_or(0) as f64 / accepted as f64;
            by_kind.max(by_session)
        },
    }
}

fn select_hyperparameters(
    examples: &[(SignalFeatures, &ValidationTrade)],
    policy: MetaFilterPolicy,
) -> (f64, bool) {
    if examples.len() < policy.min_train_examples + 10 {
        return (0.01, false);
    }
    let split = (examples.len() * 3 / 4)
        .max(policy.min_train_examples)
        .min(examples.len() - 1);
    let labeled = examples[..split]
        .iter()
        .map(|(features, trade)| (features.clone(), trade.stressed_net_pnl > 0.0))
        .collect::<Vec<_>>();
    let mut best = (f64::NEG_INFINITY, 0.01, false);
    for nonlinear in [false, true] {
        if nonlinear && !policy.nonlinear_enabled {
            continue;
        }
        for l2 in [0.001, 0.01, 0.1] {
            let Some(model) = LogisticMetaFilter::fit_variant(
                &labeled,
                l2,
                0.55,
                nonlinear,
                policy.min_train_examples,
            ) else {
                continue;
            };
            let mut total = 0.0;
            let mut accepted = 0;
            for (features, trade) in &examples[split..] {
                if model.allows(features) {
                    total += trade.stressed_net_pnl;
                    accepted += 1;
                }
            }
            let score = if accepted == 0 {
                f64::NEG_INFINITY
            } else {
                total / accepted as f64
            };
            if score > best.0 {
                best = (score, l2, nonlinear);
            }
        }
    }
    (best.1, best.2)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{learning::ValidationContext, trading::PositionKind};

    #[test]
    fn regularized_model_learns_a_separable_signal() {
        let examples = (0..100)
            .map(|index| {
                let value = index as f64 - 50.0;
                (
                    SignalFeatures::from([value, 1.0, 2.0, 3.0, 4.0, 5.0, 6.0]),
                    value > 0.0,
                )
            })
            .collect::<Vec<_>>();
        let model = LogisticMetaFilter::fit(&examples, 0.01, 0.55).unwrap();
        assert!(
            model
                .probability(&SignalFeatures::from([30.0, 1.0, 2.0, 3.0, 4.0, 5.0, 6.0]))
                .unwrap()
                > 0.8
        );
        assert!(
            model
                .probability(&SignalFeatures::from([-30.0, 1.0, 2.0, 3.0, 4.0, 5.0, 6.0]))
                .unwrap()
                < 0.2
        );
    }

    #[test]
    fn bounded_tree_learns_a_nonlinear_interval() {
        let examples = (0..120)
            .map(|index| {
                let value = index as f64 / 10.0 - 6.0;
                (
                    SignalFeatures::from([value, 1.0, 2.0, 3.0, 4.0, 5.0, 6.0]),
                    value.abs() < 2.0,
                )
            })
            .collect::<Vec<_>>();
        let model = TreeMetaFilter::fit(&examples, 16, 0.2, 0.55, 60).unwrap();
        assert!(model.stumps.len() <= 32);
        assert!(
            model
                .probability(&SignalFeatures::from([0.0, 1.0, 2.0, 3.0, 4.0, 5.0, 6.0]))
                .unwrap()
                > model
                    .probability(&SignalFeatures::from([5.0, 1.0, 2.0, 3.0, 4.0, 5.0, 6.0]))
                    .unwrap()
        );
    }

    #[test]
    fn walk_forward_activates_vix_only_when_it_improves_future_expectancy() {
        let trades = (0..120)
            .map(|index| {
                let quiet = index % 2 == 0;
                sample_trade(
                    index,
                    if quiet { 100.0 } else { -100.0 },
                    if quiet { 15.0 } else { 35.0 },
                )
            })
            .collect::<Vec<_>>();

        let assessment = assess_meta_filter(&trades);

        assert_eq!(assessment.walk_forward_folds, WALK_FORWARD_FOLDS);
        assert!(assessment.vix_recommended);
        assert!(assessment.uses_vix);
        assert!(
            assessment.with_vix_stressed_expectancy > assessment.without_vix_stressed_expectancy
        );
        assert_eq!(
            assessment.model.as_ref().unwrap().feature_count(),
            VIX_FEATURE_COUNT
        );
    }

    #[test]
    fn missing_vix_trades_still_feed_the_base_assessment() {
        let mut trades = (0..60)
            .map(|index| sample_trade(index, if index % 2 == 0 { 10.0 } else { -10.0 }, 20.0))
            .collect::<Vec<_>>();
        for trade in &mut trades {
            trade.context.vix_level = None;
            trade.context.vix_change_percentage = None;
        }

        let assessment = assess_meta_filter(&trades);

        assert!(assessment.holdout_examples > 0);
        assert_eq!(assessment.vix_examples, 0);
        assert!(!assessment.vix_recommended);
    }

    #[test]
    fn complete_vix_is_not_activated_without_strict_holdout_improvement() {
        let mut trades = (0..120)
            .map(|index| {
                sample_trade(
                    index,
                    if index % 2 == 0 { 100.0 } else { -100.0 },
                    15.0 + (index % 7) as f64,
                )
            })
            .collect::<Vec<_>>();
        for (index, trade) in trades.iter_mut().enumerate() {
            trade.context.entry_spread_percentage = Some(if index % 2 == 0 { 1.0 } else { 4.0 });
        }

        let assessment = assess_meta_filter(&trades);

        assert!(!assessment.vix_recommended);
        assert!(!assessment.uses_vix);
        if let Some(model) = assessment.model {
            assert_eq!(model.feature_count(), BASE_FEATURE_COUNT);
        }
    }

    fn sample_trade(index: usize, pnl: f64, vix_level: f64) -> ValidationTrade {
        ValidationTrade {
            kind: PositionKind::Call,
            net_pnl: pnl,
            stressed_net_pnl: pnl,
            closed_at_secs: index as i64 + 1,
            context: ValidationContext {
                trade_id: index.to_string(),
                opened_at_secs: index as i64,
                entry_spread_percentage: Some(1.0),
                option_volume: Some(100),
                days_to_expiry: Some(20),
                moneyness_distance_percentage: Some(1.0),
                trend_confidence: Some(0.8),
                trend_r_squared: Some(0.8),
                trend_slope_percent_per_minute: Some(0.1),
                vix_level: Some(vix_level),
                vix_change_percentage: Some(vix_level - 20.0),
                ..ValidationContext::default()
            },
        }
    }
}
