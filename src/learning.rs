use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use crate::trading::PositionKind;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LiveStage {
    #[default]
    Learning,
    #[serde(alias = "trading")]
    Live,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ValidationTrade {
    pub kind: PositionKind,
    pub net_pnl: f64,
    pub stressed_net_pnl: f64,
    pub closed_at_secs: i64,
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

    pub fn record(&mut self, trade: ValidationTrade) {
        self.trades.push(trade);
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

#[derive(Debug, Clone, Copy)]
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
        let call_lower =
            bootstrap_lower_95(&calls.iter().map(|trade| trade.net_pnl).collect::<Vec<_>>());
        let put_lower =
            bootstrap_lower_95(&puts.iter().map(|trade| trade.net_pnl).collect::<Vec<_>>());
        let max_drawdown = max_drawdown(trades.iter().map(|trade| trade.net_pnl));
        let max_daily_drawdown = max_daily_drawdown(trades);
        let eligible = trades.len() as u64 >= requirements.min_trades
            && calls.len() as u64 >= requirements.min_call_trades
            && puts.len() as u64 >= requirements.min_put_trades
            && sessions >= requirements.min_sessions
            && net_pnl > 0.0
            && call_net_pnl > 0.0
            && put_net_pnl > 0.0
            && aggregate_profit_factor >= requirements.min_profit_factor
            && call_profit_factor >= requirements.min_profit_factor
            && put_profit_factor >= requirements.min_profit_factor
            && expectancy > 0.0
            && call_lower > 0.0
            && put_lower > 0.0
            && max_drawdown <= requirements.max_total_drawdown
            && max_daily_drawdown <= requirements.max_daily_drawdown
            && stressed_net_pnl > 0.0
            && call_stressed > 0.0
            && put_stressed > 0.0;
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

fn bootstrap_lower_95(values: &[f64]) -> f64 {
    if values.len() < 2 {
        return f64::NEG_INFINITY;
    }
    let mut state = 0x9e37_79b9_7f4a_7c15_u64 ^ values.len() as u64;
    let mut means = Vec::with_capacity(1_000);
    for _ in 0..1_000 {
        let mut total = 0.0;
        for _ in values {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1);
            total += values[(state as usize) % values.len()];
        }
        means.push(total / values.len() as f64);
    }
    means.sort_by(f64::total_cmp);
    means[24]
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
