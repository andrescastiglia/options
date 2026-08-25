use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::learning_model::{assess_meta_filter, MetaFilterAssessment};
use crate::trading::PositionKind;

pub const EVIDENCE_SCHEMA_VERSION: u32 = 1;
pub const AUTHORIZATION_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LiveStage {
    #[default]
    Learning,
    Eligible,
    Armed,
    Canary,
    #[serde(alias = "trading")]
    Live,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ValidationTrade {
    pub kind: PositionKind,
    pub net_pnl: f64,
    pub stressed_net_pnl: f64,
    pub closed_at_secs: i64,
    #[serde(default)]
    pub context: ValidationContext,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceSource {
    HistoricalOutOfSample,
    #[default]
    Shadow,
    Canary,
    Live,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ValidationContext {
    pub trade_id: String,
    pub source: EvidenceSource,
    pub option_symbol: String,
    pub opened_at_secs: i64,
    pub entry_price: f64,
    pub exit_price: f64,
    pub contracts: u32,
    pub max_net_loss: f64,
    pub r_multiple: f64,
    pub stressed_r_multiple: f64,
    pub entry_spread_percentage: Option<f64>,
    pub option_volume: Option<u64>,
    pub days_to_expiry: Option<i64>,
    pub moneyness_distance_percentage: Option<f64>,
    pub trend_confidence: Option<f64>,
    pub trend_r_squared: Option<f64>,
    pub trend_slope_percent_per_minute: Option<f64>,
    pub exit_reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LearningState {
    pub epoch: u64,
    pub strategy_fingerprint: String,
    pub trades: Vec<ValidationTrade>,
    pub approved: bool,
}

impl Default for LearningState {
    fn default() -> Self {
        Self::new(String::new())
    }
}

impl LearningState {
    pub fn new(strategy_fingerprint: String) -> Self {
        Self {
            epoch: 1,
            strategy_fingerprint,
            trades: Vec::new(),
            approved: false,
        }
    }

    pub fn reset(&mut self, strategy_fingerprint: String) {
        self.epoch = self.epoch.saturating_add(1);
        self.strategy_fingerprint = strategy_fingerprint;
        self.trades.clear();
        self.approved = false;
    }

    pub fn record(&mut self, trade: ValidationTrade) -> bool {
        if !trade.context.trade_id.is_empty()
            && self
                .trades
                .iter()
                .any(|existing| existing.context.trade_id == trade.context.trade_id)
        {
            return false;
        }
        self.trades.push(trade);
        true
    }

    pub fn report(&self, requirements: GateRequirements) -> LearningReport {
        LearningReport::from_trades(
            self.epoch,
            &self.strategy_fingerprint,
            &self.trades,
            requirements,
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct GateRequirements {
    pub min_trades: u64,
    pub min_call_trades: u64,
    pub min_put_trades: u64,
    pub min_sessions: usize,
    pub min_profit_factor: f64,
    pub max_daily_drawdown: f64,
    pub max_total_drawdown: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StrategyManifest {
    pub schema_version: u32,
    pub fingerprint: String,
    pub build_hash: String,
    pub package_version: String,
    pub parameters: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EvidenceBundle {
    pub schema_version: u32,
    pub manifest: StrategyManifest,
    pub gate_policy: GateRequirements,
    pub learning_state: LearningState,
    pub report: LearningReport,
    #[serde(default)]
    pub dataset_ids: Vec<String>,
    pub updated_at_secs: i64,
}

impl EvidenceBundle {
    pub fn is_compatible(&self, manifest: &StrategyManifest, gate: GateRequirements) -> bool {
        self.schema_version == EVIDENCE_SCHEMA_VERSION
            && self.manifest == *manifest
            && self.gate_policy == gate
            && self.learning_state.strategy_fingerprint == manifest.fingerprint
            && self.report.strategy_fingerprint == manifest.fingerprint
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AuthorizationRequest {
    pub schema_version: u32,
    pub account_number: String,
    pub epoch: u64,
    pub strategy_fingerprint: String,
    pub build_hash: String,
    pub report_sha256: String,
    pub canary_max_position_size: u32,
    pub canary_max_investment_amount: f64,
    pub canary_max_loss_per_trade: f64,
    pub canary_max_daily_loss: f64,
    pub generated_at_secs: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExecutionAuthorization {
    pub schema_version: u32,
    pub request: AuthorizationRequest,
    pub issued_at_secs: i64,
    pub expires_at_secs: i64,
    pub confirmation: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LearningReport {
    pub epoch: u64,
    pub strategy_fingerprint: String,
    pub generated_at_secs: i64,
    pub trades: u64,
    pub call_trades: u64,
    pub put_trades: u64,
    pub sessions: usize,
    pub net_pnl: f64,
    pub call_net_pnl: f64,
    pub put_net_pnl: f64,
    pub profit_factor: f64,
    pub call_profit_factor: f64,
    pub put_profit_factor: f64,
    pub expectancy: f64,
    pub call_expectancy_lower_95: f64,
    pub put_expectancy_lower_95: f64,
    pub max_drawdown: f64,
    pub max_daily_drawdown: f64,
    pub stressed_net_pnl: f64,
    #[serde(default)]
    pub call_stressed_net_pnl: f64,
    #[serde(default)]
    pub put_stressed_net_pnl: f64,
    #[serde(default)]
    pub expectancy_r: f64,
    #[serde(default)]
    pub call_expectancy_r_lower_95: f64,
    #[serde(default)]
    pub put_expectancy_r_lower_95: f64,
    #[serde(default)]
    pub blocking_reasons: Vec<String>,
    #[serde(default)]
    pub meta_filter: MetaFilterAssessment,
    pub eligible: bool,
}

impl LearningReport {
    fn from_trades(
        epoch: u64,
        fingerprint: &str,
        trades: &[ValidationTrade],
        requirements: GateRequirements,
    ) -> Self {
        let calls: Vec<_> = trades
            .iter()
            .filter(|trade| trade.kind == PositionKind::Call)
            .collect();
        let puts: Vec<_> = trades
            .iter()
            .filter(|trade| trade.kind == PositionKind::Put)
            .collect();
        let sessions = trades
            .iter()
            .map(|trade| argentina_day(trade.closed_at_secs))
            .collect::<BTreeSet<_>>()
            .len();
        let net_pnl = trades.iter().map(|trade| trade.net_pnl).sum::<f64>();
        let call_net_pnl = calls.iter().map(|trade| trade.net_pnl).sum::<f64>();
        let put_net_pnl = puts.iter().map(|trade| trade.net_pnl).sum::<f64>();
        let stressed_net_pnl = trades
            .iter()
            .map(|trade| trade.stressed_net_pnl)
            .sum::<f64>();
        let call_stressed = calls
            .iter()
            .map(|trade| trade.stressed_net_pnl)
            .sum::<f64>();
        let put_stressed = puts.iter().map(|trade| trade.stressed_net_pnl).sum::<f64>();
        let aggregate_profit_factor = profit_factor(trades.iter().map(|trade| trade.net_pnl));
        let call_profit_factor = profit_factor(calls.iter().map(|trade| trade.net_pnl));
        let put_profit_factor = profit_factor(puts.iter().map(|trade| trade.net_pnl));
        let expectancy = mean(trades.iter().map(|trade| trade.net_pnl));
        let call_lower = block_bootstrap_lower_95(&calls, |trade| trade.net_pnl);
        let put_lower = block_bootstrap_lower_95(&puts, |trade| trade.net_pnl);
        let expectancy_r = mean(trades.iter().map(r_multiple));
        let call_r_lower = block_bootstrap_lower_95(&calls, |trade| r_multiple(trade));
        let put_r_lower = block_bootstrap_lower_95(&puts, |trade| r_multiple(trade));
        let max_drawdown = max_drawdown(trades.iter().map(|trade| trade.net_pnl));
        let max_daily_drawdown = max_daily_drawdown(trades);
        let mut blocking_reasons = Vec::new();
        require(
            trades.len() as u64 >= requirements.min_trades,
            "min_trades",
            &mut blocking_reasons,
        );
        require(
            calls.len() as u64 >= requirements.min_call_trades,
            "min_call_trades",
            &mut blocking_reasons,
        );
        require(
            puts.len() as u64 >= requirements.min_put_trades,
            "min_put_trades",
            &mut blocking_reasons,
        );
        require(
            sessions >= requirements.min_sessions,
            "min_sessions",
            &mut blocking_reasons,
        );
        require(net_pnl > 0.0, "net_pnl", &mut blocking_reasons);
        require(call_net_pnl > 0.0, "call_net_pnl", &mut blocking_reasons);
        require(put_net_pnl > 0.0, "put_net_pnl", &mut blocking_reasons);
        require(
            aggregate_profit_factor >= requirements.min_profit_factor,
            "profit_factor",
            &mut blocking_reasons,
        );
        require(
            call_profit_factor >= requirements.min_profit_factor,
            "call_profit_factor",
            &mut blocking_reasons,
        );
        require(
            put_profit_factor >= requirements.min_profit_factor,
            "put_profit_factor",
            &mut blocking_reasons,
        );
        require(expectancy > 0.0, "expectancy", &mut blocking_reasons);
        require(
            call_lower > 0.0,
            "call_expectancy_lower_95",
            &mut blocking_reasons,
        );
        require(
            put_lower > 0.0,
            "put_expectancy_lower_95",
            &mut blocking_reasons,
        );
        require(
            call_r_lower > 0.0,
            "call_expectancy_r_lower_95",
            &mut blocking_reasons,
        );
        require(
            put_r_lower > 0.0,
            "put_expectancy_r_lower_95",
            &mut blocking_reasons,
        );
        require(
            max_drawdown <= requirements.max_total_drawdown,
            "max_drawdown",
            &mut blocking_reasons,
        );
        require(
            max_daily_drawdown <= requirements.max_daily_drawdown,
            "max_daily_drawdown",
            &mut blocking_reasons,
        );
        require(
            stressed_net_pnl > 0.0,
            "stressed_net_pnl",
            &mut blocking_reasons,
        );
        require(
            call_stressed > 0.0,
            "call_stressed_net_pnl",
            &mut blocking_reasons,
        );
        require(
            put_stressed > 0.0,
            "put_stressed_net_pnl",
            &mut blocking_reasons,
        );
        let eligible = blocking_reasons.is_empty();
        let meta_filter = assess_meta_filter(trades);
        Self {
            epoch,
            strategy_fingerprint: fingerprint.to_string(),
            generated_at_secs: trades.last().map_or(0, |trade| trade.closed_at_secs),
            trades: trades.len() as u64,
            call_trades: calls.len() as u64,
            put_trades: puts.len() as u64,
            sessions,
            net_pnl,
            call_net_pnl,
            put_net_pnl,
            profit_factor: aggregate_profit_factor,
            call_profit_factor,
            put_profit_factor,
            expectancy,
            call_expectancy_lower_95: call_lower,
            put_expectancy_lower_95: put_lower,
            max_drawdown,
            max_daily_drawdown,
            stressed_net_pnl,
            call_stressed_net_pnl: call_stressed,
            put_stressed_net_pnl: put_stressed,
            expectancy_r,
            call_expectancy_r_lower_95: call_r_lower,
            put_expectancy_r_lower_95: put_r_lower,
            blocking_reasons,
            meta_filter,
            eligible,
        }
    }
}

pub fn trading_regressed(
    trades: &[ValidationTrade],
    window: usize,
    max_consecutive_losses: u32,
    max_drawdown_allowed: f64,
) -> bool {
    let consecutive_losses = trades
        .iter()
        .rev()
        .take_while(|trade| trade.net_pnl <= 0.0)
        .count() as u32;
    if consecutive_losses >= max_consecutive_losses {
        return true;
    }
    if trades.len() < window {
        return false;
    }
    let recent = &trades[trades.len() - window..];
    mean(recent.iter().map(|trade| trade.net_pnl)) <= 0.0
        || profit_factor(recent.iter().map(|trade| trade.net_pnl)) < 1.0
        || max_drawdown(recent.iter().map(|trade| trade.net_pnl)) > max_drawdown_allowed
}

fn argentina_day(timestamp_secs: i64) -> i64 {
    timestamp_secs
        .saturating_sub(3 * 60 * 60)
        .div_euclid(86_400)
}

fn mean(values: impl Iterator<Item = f64>) -> f64 {
    let values: Vec<_> = values.collect();
    if values.is_empty() {
        0.0
    } else {
        values.iter().sum::<f64>() / values.len() as f64
    }
}

fn profit_factor(values: impl Iterator<Item = f64>) -> f64 {
    let (profit, loss) = values.fold((0.0, 0.0), |(profit, loss), value| {
        if value > 0.0 {
            (profit + value, loss)
        } else {
            (profit, loss - value)
        }
    });
    if loss == 0.0 {
        if profit > 0.0 {
            f64::MAX
        } else {
            0.0
        }
    } else {
        profit / loss
    }
}

fn max_drawdown(values: impl Iterator<Item = f64>) -> f64 {
    let mut equity: f64 = 0.0;
    let mut peak: f64 = 0.0;
    let mut drawdown: f64 = 0.0;
    for value in values {
        equity += value;
        peak = peak.max(equity);
        drawdown = drawdown.max(peak - equity);
    }
    drawdown
}

fn max_daily_drawdown(trades: &[ValidationTrade]) -> f64 {
    let mut worst: f64 = 0.0;
    let days = trades
        .iter()
        .map(|trade| argentina_day(trade.closed_at_secs))
        .collect::<BTreeSet<_>>();
    for day in days {
        worst = worst.max(max_drawdown(
            trades
                .iter()
                .filter(|trade| argentina_day(trade.closed_at_secs) == day)
                .map(|trade| trade.net_pnl),
        ));
    }
    worst
}

fn block_bootstrap_lower_95<T>(trades: &[T], value: impl Fn(&T) -> f64) -> f64
where
    T: AsRef<ValidationTrade>,
{
    let mut blocks = BTreeMap::<i64, Vec<f64>>::new();
    for item in trades {
        let trade = item.as_ref();
        blocks
            .entry(argentina_day(trade.closed_at_secs))
            .or_default()
            .push(value(item));
    }
    let blocks = blocks.into_values().collect::<Vec<_>>();
    if blocks.len() < 2 {
        return -f64::MAX;
    }
    let mut state = 0x9e37_79b9_7f4a_7c15_u64 ^ blocks.len() as u64;
    let mut means = Vec::with_capacity(1_000);
    for _ in 0..1_000 {
        let mut total = 0.0;
        let mut observations = 0_u64;
        for _ in &blocks {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1);
            let block = &blocks[(state as usize) % blocks.len()];
            total += block.iter().sum::<f64>();
            observations += block.len() as u64;
        }
        means.push(total / observations.max(1) as f64);
    }
    means.sort_by(f64::total_cmp);
    means[24]
}

impl AsRef<ValidationTrade> for ValidationTrade {
    fn as_ref(&self) -> &ValidationTrade {
        self
    }
}

fn r_multiple(trade: &ValidationTrade) -> f64 {
    if trade.context.max_net_loss.is_finite() && trade.context.max_net_loss > 0.0 {
        trade.net_pnl / trade.context.max_net_loss
    } else {
        trade.net_pnl
    }
}

fn require(condition: bool, reason: &str, reasons: &mut Vec<String>) {
    if !condition {
        reasons.push(reason.to_string());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gate_requires_both_directions() {
        let state = LearningState {
            epoch: 1,
            strategy_fingerprint: "x".into(),
            trades: (0..10)
                .map(|day| ValidationTrade {
                    kind: PositionKind::Call,
                    net_pnl: 10.0,
                    stressed_net_pnl: 5.0,
                    closed_at_secs: day * 86_400,
                    context: ValidationContext::default(),
                })
                .collect(),
            approved: false,
        };
        let report = state.report(GateRequirements {
            min_trades: 10,
            min_call_trades: 5,
            min_put_trades: 5,
            min_sessions: 2,
            min_profit_factor: 1.0,
            max_daily_drawdown: 100.0,
            max_total_drawdown: 100.0,
        });
        assert!(!report.eligible);
    }

    #[test]
    fn regression_detects_consecutive_losses() {
        let trades = (0..3)
            .map(|timestamp| ValidationTrade {
                kind: PositionKind::Put,
                net_pnl: -1.0,
                stressed_net_pnl: -2.0,
                closed_at_secs: timestamp,
                context: ValidationContext::default(),
            })
            .collect::<Vec<_>>();
        assert!(trading_regressed(&trades, 30, 3, 100.0));
    }

    #[test]
    fn gate_approves_profitable_evidence_for_calls_and_puts() {
        let trades = (0..10)
            .map(|day| ValidationTrade {
                kind: if day % 2 == 0 {
                    PositionKind::Call
                } else {
                    PositionKind::Put
                },
                net_pnl: 10.0,
                stressed_net_pnl: 5.0,
                closed_at_secs: day * 86_400,
                context: ValidationContext::default(),
            })
            .collect();
        let report = LearningState {
            epoch: 1,
            strategy_fingerprint: "stable".into(),
            trades,
            approved: false,
        }
        .report(GateRequirements {
            min_trades: 10,
            min_call_trades: 5,
            min_put_trades: 5,
            min_sessions: 5,
            min_profit_factor: 1.25,
            max_daily_drawdown: 100.0,
            max_total_drawdown: 200.0,
        });
        assert!(report.eligible);
        assert!(report.call_expectancy_lower_95 > 0.0);
        assert!(report.put_expectancy_lower_95 > 0.0);
        assert!(serde_json::to_vec(&report).is_ok());
    }
}
