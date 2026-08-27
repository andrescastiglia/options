use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use std::path::Path;

use crate::{
    datasets::{consume_sealed_holdout, DatasetError, DatasetRole, SignedDatasetManifest},
    learning::ValidationTrade,
    trading::PositionKind,
};

pub const EXPERIMENT_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExperimentVariant {
    pub name: String,
    pub entry_delay_minutes: u32,
    pub extra_cost_bps: f64,
    pub max_risk_multiple: f64,
    pub volatility_normalized: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExperimentManifest {
    pub schema_version: u32,
    pub dataset_ids: Vec<String>,
    pub build_hash: String,
    pub seed: u64,
    pub variants: Vec<ExperimentVariant>,
    #[serde(default)]
    pub selection_start_secs: Option<i64>,
    pub selection_end_secs: i64,
    pub final_holdout_start_secs: i64,
    #[serde(default)]
    pub final_holdout_end_secs: Option<i64>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ExperimentMetrics {
    pub observations: usize,
    pub sessions: usize,
    pub call_observations: usize,
    pub put_observations: usize,
    pub coverage: f64,
    pub net_expectancy: f64,
    pub stressed_expectancy: f64,
    pub maximum_drawdown: f64,
    pub by_hour_argentina: BTreeMap<u8, usize>,
    pub cost_sensitivity: BTreeMap<String, f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExperimentResult {
    pub manifest: ExperimentManifest,
    pub selected_variant: Option<String>,
    pub selection_metrics: BTreeMap<String, ExperimentMetrics>,
    pub final_holdout_metrics: Option<ExperimentMetrics>,
    pub used_untouched_final_holdout: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SealedExperimentRun {
    pub evaluator_id: String,
    pub consumed_at_secs: i64,
    pub build_hash: String,
    pub seed: u64,
    pub variants: Vec<ExperimentVariant>,
}

/// Separa selección y evaluación dentro de una corrida. El llamador actual
/// puede volver a ejecutar el experimento al crecer la evidencia, por lo que
/// este resultado es diagnóstico y no afirma un holdout sellado.
pub fn run_temporal_experiment(
    trades: &[ValidationTrade],
    mut manifest: ExperimentManifest,
) -> ExperimentResult {
    manifest.variants.sort_by(|a, b| a.name.cmp(&b.name));
    let mut chronological = trades.iter().collect::<Vec<_>>();
    chronological.sort_by_key(|trade| (trade.context.opened_at_secs, trade.closed_at_secs));
    let selection = chronological
        .iter()
        .copied()
        .filter(|trade| {
            manifest
                .selection_start_secs
                .is_none_or(|start| trade.context.opened_at_secs >= start)
                && trade.context.opened_at_secs <= manifest.selection_end_secs
        })
        .collect::<Vec<_>>();
    let final_holdout = chronological
        .iter()
        .copied()
        .filter(|trade| {
            trade.context.opened_at_secs >= manifest.final_holdout_start_secs
                && manifest
                    .final_holdout_end_secs
                    .is_none_or(|end| trade.context.opened_at_secs <= end)
        })
        .collect::<Vec<_>>();

    let mut selection_metrics = BTreeMap::new();
    for variant in &manifest.variants {
        selection_metrics.insert(variant.name.clone(), evaluate(&selection, variant));
    }
    let selected_variant = manifest
        .variants
        .iter()
        .filter_map(|variant| {
            let metrics = selection_metrics.get(&variant.name)?;
            (metrics.observations > 0
                && metrics.call_observations > 0
                && metrics.put_observations > 0)
                .then_some((variant, metrics))
        })
        .max_by(|(_, left), (_, right)| {
            left.stressed_expectancy
                .total_cmp(&right.stressed_expectancy)
                .then_with(|| left.coverage.total_cmp(&right.coverage))
        })
        .map(|(variant, _)| variant.name.clone());
    let final_holdout_metrics = selected_variant.as_ref().and_then(|name| {
        let variant = manifest.variants.iter().find(|item| &item.name == name)?;
        Some(evaluate(&final_holdout, variant))
    });
    ExperimentResult {
        manifest,
        selected_variant,
        selection_metrics,
        final_holdout_metrics,
        used_untouched_final_holdout: false,
    }
}

/// Consume el holdout antes de calcular sus métricas y usa exclusivamente los
/// intervalos firmados. Un error posterior no habilita un segundo intento: ésa
/// es la propiedad deliberadamente conservadora del sello de un solo uso.
pub fn run_sealed_temporal_experiment(
    trades: &[ValidationTrade],
    dataset_path: &Path,
    signed_dataset: &SignedDatasetManifest,
    registry_dir: &Path,
    run: SealedExperimentRun,
) -> Result<ExperimentResult, DatasetError> {
    let selection = signed_dataset
        .manifest
        .partitions
        .iter()
        .filter(|partition| partition.role == DatasetRole::Selection)
        .cloned()
        .collect::<Vec<_>>();
    if selection.len() != 1 {
        return Err(DatasetError::InvalidManifest(
            "se exige exactamente una partición selection".into(),
        ));
    }
    let holdout = consume_sealed_holdout(
        dataset_path,
        signed_dataset,
        registry_dir,
        &run.evaluator_id,
        run.consumed_at_secs,
    )?;
    let mut result = run_temporal_experiment(
        trades,
        ExperimentManifest {
            schema_version: EXPERIMENT_SCHEMA_VERSION,
            dataset_ids: vec![signed_dataset.manifest.dataset_id.clone()],
            build_hash: run.build_hash,
            seed: run.seed,
            variants: run.variants,
            selection_start_secs: Some(selection[0].start_secs),
            selection_end_secs: selection[0].end_secs,
            final_holdout_start_secs: holdout.partition.start_secs,
            final_holdout_end_secs: Some(holdout.partition.end_secs),
        },
    );
    result.used_untouched_final_holdout = true;
    Ok(result)
}

fn evaluate(trades: &[&ValidationTrade], variant: &ExperimentVariant) -> ExperimentMetrics {
    let accepted = trades
        .iter()
        .copied()
        .filter(|trade| {
            minutes_after_open(trade.context.opened_at_secs) >= variant.entry_delay_minutes as i64
                && trade.context.r_multiple.abs() <= variant.max_risk_multiple
                && (!variant.volatility_normalized
                    || trade
                        .context
                        .trend_slope_percent_per_minute
                        .is_some_and(|value| value.abs() > 0.0))
        })
        .collect::<Vec<_>>();
    let adjusted = |trade: &ValidationTrade, extra_bps: f64| {
        let notional = trade.context.entry_price * trade.context.contracts as f64;
        trade.stressed_net_pnl - notional * extra_bps / 10_000.0
    };
    let values = accepted
        .iter()
        .map(|trade| adjusted(trade, variant.extra_cost_bps))
        .collect::<Vec<_>>();
    let mut by_hour = BTreeMap::new();
    for trade in &accepted {
        *by_hour
            .entry(local_hour(trade.context.opened_at_secs))
            .or_default() += 1;
    }
    let mut sensitivity = BTreeMap::new();
    for extra in [0.0, 10.0, 25.0, 50.0] {
        sensitivity.insert(
            format!("+{extra:.0}bps"),
            mean(
                accepted
                    .iter()
                    .map(|trade| adjusted(trade, variant.extra_cost_bps + extra)),
            ),
        );
    }
    ExperimentMetrics {
        observations: accepted.len(),
        sessions: accepted
            .iter()
            .map(|trade| argentina_session(trade.context.opened_at_secs))
            .collect::<BTreeSet<_>>()
            .len(),
        call_observations: accepted
            .iter()
            .filter(|trade| trade.kind == PositionKind::Call)
            .count(),
        put_observations: accepted
            .iter()
            .filter(|trade| trade.kind == PositionKind::Put)
            .count(),
        coverage: if trades.is_empty() {
            0.0
        } else {
            accepted.len() as f64 / trades.len() as f64
        },
        net_expectancy: mean(accepted.iter().map(|trade| trade.net_pnl)),
        stressed_expectancy: mean(values.iter().copied()),
        maximum_drawdown: max_drawdown(values.iter().copied()),
        by_hour_argentina: by_hour,
        cost_sensitivity: sensitivity,
    }
}

fn mean(values: impl Iterator<Item = f64>) -> f64 {
    let values = values.collect::<Vec<_>>();
    if values.is_empty() {
        0.0
    } else {
        values.iter().sum::<f64>() / values.len() as f64
    }
}

fn max_drawdown(values: impl Iterator<Item = f64>) -> f64 {
    let (mut equity, mut peak, mut worst) = (0.0_f64, 0.0_f64, 0.0_f64);
    for value in values {
        equity += value;
        peak = peak.max(equity);
        worst = worst.max(peak - equity);
    }
    worst
}

fn argentina_session(timestamp: i64) -> i64 {
    timestamp.saturating_sub(3 * 3_600).div_euclid(86_400)
}

fn local_hour(timestamp: i64) -> u8 {
    (timestamp.saturating_sub(3 * 3_600).rem_euclid(86_400) / 3_600) as u8
}

fn minutes_after_open(timestamp: i64) -> i64 {
    timestamp.saturating_sub(3 * 3_600).rem_euclid(86_400) / 60 - (10 * 60 + 30)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::learning::ValidationContext;

    #[test]
    fn selection_never_reads_the_final_holdout() {
        let mut trades = Vec::new();
        for index in 0..30 {
            trades.push(ValidationTrade {
                kind: if index % 2 == 0 {
                    PositionKind::Call
                } else {
                    PositionKind::Put
                },
                net_pnl: if index < 20 { 1.0 } else { -100.0 },
                stressed_net_pnl: if index < 20 { 1.0 } else { -100.0 },
                closed_at_secs: 50_000 + index * 100,
                context: ValidationContext {
                    opened_at_secs: 50_000 + index * 100,
                    entry_price: 10.0,
                    contracts: 1,
                    r_multiple: 1.0,
                    trend_slope_percent_per_minute: Some(0.1),
                    ..ValidationContext::default()
                },
            });
        }
        let result = run_temporal_experiment(
            &trades,
            ExperimentManifest {
                schema_version: EXPERIMENT_SCHEMA_VERSION,
                dataset_ids: vec!["fixture".into()],
                build_hash: "test".into(),
                seed: 7,
                variants: vec![ExperimentVariant {
                    name: "base".into(),
                    entry_delay_minutes: 0,
                    extra_cost_bps: 0.0,
                    max_risk_multiple: 2.0,
                    volatility_normalized: false,
                }],
                selection_start_secs: None,
                selection_end_secs: 51_999,
                final_holdout_start_secs: 52_000,
                final_holdout_end_secs: None,
            },
        );
        assert!(result.selection_metrics["base"].stressed_expectancy > 0.0);
        assert!(result.final_holdout_metrics.unwrap().stressed_expectancy < 0.0);
    }
}
