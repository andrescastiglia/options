use std::{
    collections::{BTreeMap, BTreeSet},
    io::{BufRead, BufReader, BufWriter, Write},
    path::Path,
};

use serde::{Deserialize, Serialize};

use crate::{
    errors::AppError,
    learning::ValidationTrade,
    market::OptionKind,
    secure_fs::{open_private_append_bounded, open_private_read},
};

pub const IV_HISTORY_SCHEMA_VERSION: u32 = 1;
const MAX_IV_HISTORY_BYTES: u64 = 128 * 1024 * 1024;
const MAX_IV_OBSERVATIONS: usize = 1_000_000;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IvObservation {
    pub schema_version: u32,
    pub underlying: String,
    pub kind: OptionKind,
    /// Bucket de tenor comparable, en días (por ejemplo 21 o 45).
    pub tenor_days: u32,
    pub observed_at_secs: i64,
    pub implied_volatility: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct IvRankPolicy {
    pub window_sessions: usize,
    pub min_sessions: usize,
    pub min_rank: f64,
    pub max_rank: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IvRankResult {
    pub rank: Option<f64>,
    pub window_sessions: usize,
    pub observations: usize,
    pub first_observed_at_secs: Option<i64>,
    pub last_observed_at_secs: Option<i64>,
    pub missing_reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IvRankTelemetry {
    pub schema_version: u32,
    pub evaluated_at_secs: i64,
    pub underlying: String,
    pub kind: OptionKind,
    pub tenor_days: u32,
    pub current_iv: f64,
    pub filter_enabled: bool,
    pub configured_min: f64,
    pub configured_max: f64,
    pub result: IvRankResult,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct FilterComparisonMetrics {
    pub holdout_observations: usize,
    pub accepted: usize,
    pub call_accepted: usize,
    pub put_accepted: usize,
    pub coverage: f64,
    pub stressed_expectancy_after_costs: f64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct IvFilterComparisonReport {
    pub walk_forward_folds: usize,
    pub no_filter: FilterComparisonMetrics,
    pub spot_iv_filter: FilterComparisonMetrics,
    pub iv_rank_filter: FilterComparisonMetrics,
    pub recommended: bool,
}

/// Compara los tres tratamientos en folds expansivos. Los rangos son política
/// ex ante y cada operación ya contiene IV/IV Rank congelados al entrar.
pub fn compare_iv_filters_walk_forward(
    trades: &[ValidationTrade],
    spot_iv_bounds: (f64, f64),
    rank_bounds: (f64, f64),
    minimum_train: usize,
) -> IvFilterComparisonReport {
    let mut chronological = trades.iter().collect::<Vec<_>>();
    chronological.sort_by_key(|trade| (trade.context.opened_at_secs, trade.closed_at_secs));
    if chronological.len() <= minimum_train || minimum_train == 0 {
        return IvFilterComparisonReport::default();
    }
    let fold_size = (chronological.len() - minimum_train).div_ceil(3);
    let mut report = IvFilterComparisonReport::default();
    let mut start = minimum_train;
    while start < chronological.len() {
        let end = (start + fold_size).min(chronological.len());
        report.walk_forward_folds += 1;
        for trade in &chronological[start..end] {
            add_filter_trade(&mut report.no_filter, trade, true);
            add_filter_trade(
                &mut report.spot_iv_filter,
                trade,
                trade
                    .context
                    .implied_volatility
                    .is_some_and(|value| value >= spot_iv_bounds.0 && value <= spot_iv_bounds.1),
            );
            add_filter_trade(
                &mut report.iv_rank_filter,
                trade,
                trade
                    .context
                    .iv_rank
                    .is_some_and(|value| value >= rank_bounds.0 && value <= rank_bounds.1),
            );
        }
        start = end;
    }
    finalize_filter_metrics(&mut report.no_filter);
    finalize_filter_metrics(&mut report.spot_iv_filter);
    finalize_filter_metrics(&mut report.iv_rank_filter);
    report.recommended = report.iv_rank_filter.accepted > 0
        && report.iv_rank_filter.call_accepted > 0
        && report.iv_rank_filter.put_accepted > 0
        && report.iv_rank_filter.stressed_expectancy_after_costs > 0.0
        && report.iv_rank_filter.stressed_expectancy_after_costs
            > report
                .no_filter
                .stressed_expectancy_after_costs
                .max(report.spot_iv_filter.stressed_expectancy_after_costs);
    report
}

fn add_filter_trade(
    metrics: &mut FilterComparisonMetrics,
    trade: &ValidationTrade,
    accepted: bool,
) {
    metrics.holdout_observations += 1;
    if accepted {
        metrics.accepted += 1;
        match trade.kind {
            crate::trading::PositionKind::Call => metrics.call_accepted += 1,
            crate::trading::PositionKind::Put => metrics.put_accepted += 1,
        }
        metrics.stressed_expectancy_after_costs += trade.stressed_net_pnl;
    }
}

fn finalize_filter_metrics(metrics: &mut FilterComparisonMetrics) {
    metrics.coverage = metrics.accepted as f64 / metrics.holdout_observations.max(1) as f64;
    metrics.stressed_expectancy_after_costs /= metrics.accepted.max(1) as f64;
}

impl IvRankResult {
    pub fn allows(&self, policy: IvRankPolicy) -> bool {
        self.rank
            .is_some_and(|rank| rank >= policy.min_rank && rank <= policy.max_rank)
    }
}

/// Calcula el percentil empírico usando exclusivamente observaciones anteriores
/// al instante evaluado y del mismo subyacente, tipo y bucket de tenor.
pub fn point_in_time_iv_rank(
    history: &[IvObservation],
    current: &IvObservation,
    policy: IvRankPolicy,
) -> IvRankResult {
    let mut prior = history
        .iter()
        .filter(|item| {
            item.observed_at_secs < current.observed_at_secs
                && item.underlying == current.underlying
                && item.kind == current.kind
                && item.tenor_days == current.tenor_days
                && item.implied_volatility.is_finite()
                && item.implied_volatility > 0.0
        })
        .collect::<Vec<_>>();
    prior.sort_by_key(|item| item.observed_at_secs);
    let mut sessions = BTreeMap::<i64, Vec<&IvObservation>>::new();
    for item in prior {
        sessions
            .entry(argentina_session(item.observed_at_secs))
            .or_default()
            .push(item);
    }
    while sessions.len() > policy.window_sessions {
        let Some(first) = sessions.keys().next().copied() else {
            break;
        };
        sessions.remove(&first);
    }
    let flattened = sessions.values().flatten().copied().collect::<Vec<_>>();
    let first = flattened.first().map(|item| item.observed_at_secs);
    let last = flattened.last().map(|item| item.observed_at_secs);
    if sessions.len() < policy.min_sessions {
        return IvRankResult {
            rank: None,
            window_sessions: sessions.len(),
            observations: flattened.len(),
            first_observed_at_secs: first,
            last_observed_at_secs: last,
            missing_reason: Some(format!(
                "historial_insuficiente: {} de {} sesiones",
                sessions.len(),
                policy.min_sessions
            )),
        };
    }
    let below_or_equal = flattened
        .iter()
        .filter(|item| item.implied_volatility <= current.implied_volatility)
        .count();
    IvRankResult {
        rank: Some(100.0 * below_or_equal as f64 / flattened.len() as f64),
        window_sessions: sessions.len(),
        observations: flattened.len(),
        first_observed_at_secs: first,
        last_observed_at_secs: last,
        missing_reason: None,
    }
}

pub fn coverage_by_kind(history: &[IvObservation]) -> BTreeMap<String, usize> {
    let mut result = BTreeMap::new();
    for kind in [OptionKind::Call, OptionKind::Put] {
        let sessions = history
            .iter()
            .filter(|item| item.kind == kind)
            .map(|item| argentina_session(item.observed_at_secs))
            .collect::<BTreeSet<_>>()
            .len();
        result.insert(format!("{kind:?}").to_ascii_lowercase(), sessions);
    }
    result
}

pub fn append_observation(path: &Path, observation: &IvObservation) -> Result<(), AppError> {
    let file = open_private_append_bounded(path, MAX_IV_HISTORY_BYTES)?;
    let mut writer = BufWriter::new(file);
    serde_json::to_writer(&mut writer, observation)?;
    writer.write_all(b"\n")?;
    writer.flush()?;
    Ok(())
}

pub fn append_telemetry(path: &Path, telemetry: &IvRankTelemetry) -> Result<(), AppError> {
    let file = open_private_append_bounded(path, MAX_IV_HISTORY_BYTES)?;
    let mut writer = BufWriter::new(file);
    serde_json::to_writer(&mut writer, telemetry)?;
    writer.write_all(b"\n")?;
    writer.flush()?;
    Ok(())
}

pub fn load_history(path: &Path) -> Result<Vec<IvObservation>, AppError> {
    let file = match open_private_read(path, MAX_IV_HISTORY_BYTES) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
    };
    let mut observations = Vec::new();
    for line in BufReader::new(file).lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        if observations.len() >= MAX_IV_OBSERVATIONS {
            return Err(AppError::InvalidMarketData(
                "historial IV excede el límite de observaciones".into(),
            ));
        }
        observations.push(serde_json::from_str::<IvObservation>(&line)?);
    }
    Ok(observations)
}

fn argentina_session(timestamp_secs: i64) -> i64 {
    crate::time_utils::argentina_session_day(timestamp_secs)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn observation(day: i64, kind: OptionKind, iv: f64) -> IvObservation {
        IvObservation {
            schema_version: IV_HISTORY_SCHEMA_VERSION,
            underlying: "GGAL".into(),
            kind,
            tenor_days: 21,
            observed_at_secs: day * 86_400 + 15 * 3_600,
            implied_volatility: iv,
        }
    }

    #[test]
    fn never_uses_the_current_or_future_observation() {
        let history = vec![
            observation(1, OptionKind::Call, 0.20),
            observation(2, OptionKind::Call, 0.30),
            observation(4, OptionKind::Call, 9.99),
        ];
        let current = observation(3, OptionKind::Call, 0.25);
        let result = point_in_time_iv_rank(
            &history,
            &current,
            IvRankPolicy {
                window_sessions: 10,
                min_sessions: 2,
                min_rank: 0.0,
                max_rank: 100.0,
            },
        );
        assert_eq!(result.rank, Some(50.0));
        assert_eq!(result.observations, 2);
        assert!(result.last_observed_at_secs.unwrap() < current.observed_at_secs);
    }

    #[test]
    fn keeps_call_put_and_tenor_histories_separate() {
        let history = vec![
            observation(1, OptionKind::Call, 0.20),
            observation(2, OptionKind::Put, 0.10),
        ];
        let current = observation(3, OptionKind::Call, 0.25);
        let result = point_in_time_iv_rank(
            &history,
            &current,
            IvRankPolicy {
                window_sessions: 20,
                min_sessions: 2,
                min_rank: 0.0,
                max_rank: 100.0,
            },
        );
        assert!(result.rank.is_none());
        assert_eq!(result.window_sessions, 1);
    }

    #[test]
    fn comparison_uses_future_folds_and_after_cost_pnl() {
        let trades = (0..30)
            .map(|index| ValidationTrade {
                kind: if index % 2 == 0 {
                    crate::trading::PositionKind::Call
                } else {
                    crate::trading::PositionKind::Put
                },
                net_pnl: 100.0,
                stressed_net_pnl: if (20.0..=80.0).contains(&(index as f64 * 4.0)) {
                    5.0
                } else {
                    -5.0
                },
                closed_at_secs: index,
                context: crate::learning::ValidationContext {
                    opened_at_secs: index,
                    implied_volatility: Some(0.5),
                    iv_rank: Some(index as f64 * 4.0),
                    ..crate::learning::ValidationContext::default()
                },
            })
            .collect::<Vec<_>>();
        let report = compare_iv_filters_walk_forward(&trades, (0.1, 1.0), (20.0, 80.0), 12);
        assert_eq!(report.walk_forward_folds, 3);
        assert!(report.iv_rank_filter.coverage < report.no_filter.coverage);
        assert!(report.iv_rank_filter.call_accepted > 0);
        assert!(report.iv_rank_filter.put_accepted > 0);
    }
}
