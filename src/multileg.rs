use std::{io::Write, path::Path};

use serde::{Deserialize, Serialize};

use crate::{
    errors::AppError,
    market::{MarketFrame, OptionKind, OptionQuote},
    secure_fs::open_private_append_bounded,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LegSide {
    Long,
    Short,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LegExecutionState {
    Planned,
    Submitted,
    PartiallyFilled,
    Filled,
    CancelRequested,
    Cancelled,
    Rejected,
    MissingSeries,
    Assigned,
    Exercised,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LegExecution {
    pub symbol: String,
    pub state: LegExecutionState,
    pub requested_quantity: u32,
    pub filled_quantity: u32,
    pub average_fill_price: Option<f64>,
    pub total_cost: f64,
    pub updated_at_secs: i64,
    pub broker_order_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OptionLeg {
    pub symbol: String,
    pub side: LegSide,
    pub kind: OptionKind,
    pub strike: f64,
    pub quantity: u32,
    pub entry_price: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MultiLegPosition {
    pub strategy: String,
    pub legs: Vec<OptionLeg>,
    pub expiry_days: u32,
    pub contract_multiplier: u32,
    pub net_debit: f64,
    pub maximum_loss: f64,
    pub maximum_profit: f64,
    pub partial_fill_maximum_loss: f64,
    pub early_assignment_cash_obligation: f64,
    pub assignment_is_covered: bool,
    pub atomic_execution_supported: bool,
    pub shadow_only: bool,
    pub leg_executions: Vec<LegExecution>,
    pub temporary_cash_obligation: f64,
    pub dividend_assignment_exposure: f64,
    pub recovery_action: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct VerticalRiskRules {
    pub available_cash: f64,
    pub margin_requirement_percentage: f64,
    pub expected_dividend_per_share: f64,
    pub short_leg_ex_dividend_risk: bool,
    pub atomic_combo_verified: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VerticalRecoveryAssessment {
    pub maximum_economic_loss: f64,
    pub temporary_cash_obligation: f64,
    pub margin_required: f64,
    pub cash_sufficient: bool,
    pub duplicate_exposure: bool,
    pub real_execution_allowed: bool,
    pub action: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StrategyComparison {
    pub signal_id: String,
    pub session_id: i64,
    pub long_only_net_pnl: f64,
    pub vertical_net_pnl: f64,
    pub long_only_maximum_loss: f64,
    pub vertical_maximum_loss: f64,
    pub vertical_temporary_cash_obligation: f64,
    pub all_costs_included: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StrategyComparisonInput {
    pub signal_id: String,
    pub session_id: i64,
    pub long_entry: f64,
    pub long_exit: f64,
    pub short_entry: f64,
    pub short_exit: f64,
    pub contract_multiplier: u32,
    pub round_trip_cost_percentage: f64,
}

pub fn compare_long_and_vertical(
    input: &StrategyComparisonInput,
    vertical: &MultiLegPosition,
) -> StrategyComparison {
    let multiplier = input.contract_multiplier as f64;
    let cost = input.round_trip_cost_percentage.max(0.0) / 100.0;
    let long_turnover = input.long_entry + input.long_exit;
    let vertical_turnover =
        input.long_entry + input.long_exit + input.short_entry + input.short_exit;
    StrategyComparison {
        signal_id: input.signal_id.clone(),
        session_id: input.session_id,
        long_only_net_pnl: (input.long_exit - input.long_entry - long_turnover * cost) * multiplier,
        vertical_net_pnl: ((input.long_exit - input.long_entry)
            + (input.short_entry - input.short_exit)
            - vertical_turnover * cost)
            * multiplier,
        long_only_maximum_loss: input.long_entry * (1.0 + cost) * multiplier,
        vertical_maximum_loss: vertical.maximum_loss,
        vertical_temporary_cash_obligation: vertical.temporary_cash_obligation,
        all_costs_included: true,
    }
}

pub fn research_vertical_spread(
    frame: &MarketFrame,
    kind: OptionKind,
    selected_long_symbol: &str,
    contract_multiplier: u32,
    operating_cost_percentage: f64,
) -> Option<MultiLegPosition> {
    let long = frame.option(selected_long_symbol)?;
    let short = compatible_short_leg(frame, long)?;
    let long_price = long.executable_buy_price()?;
    let short_price = short.executable_sell_price()?;
    let width = (short.strike - long.strike).abs();
    let gross_debit = long_price - short_price;
    if gross_debit <= 0.0 || width <= gross_debit {
        return None;
    }
    let fees = (long_price + short_price) * operating_cost_percentage / 100.0;
    let net_debit = gross_debit + fees;
    let multiplier = contract_multiplier as f64;
    Some(MultiLegPosition {
        strategy: match kind {
            OptionKind::Call => "bull_call_debit",
            OptionKind::Put => "bear_put_debit",
        }
        .into(),
        legs: vec![
            OptionLeg {
                symbol: long.symbol.clone(),
                side: LegSide::Long,
                kind,
                strike: long.strike,
                quantity: 1,
                entry_price: long_price,
            },
            OptionLeg {
                symbol: short.symbol.clone(),
                side: LegSide::Short,
                kind,
                strike: short.strike,
                quantity: 1,
                entry_price: short_price,
            },
        ],
        expiry_days: long.expiry_days,
        contract_multiplier,
        net_debit: net_debit * multiplier,
        maximum_loss: net_debit * multiplier,
        maximum_profit: ((width - gross_debit).max(0.0) - fees) * multiplier,
        // Se simula ejecución long-first: si falla la pata corta queda una opción
        // comprada, con pérdida limitada a la prima y sus costos.
        partial_fill_maximum_loss: long_price
            * (1.0 + operating_cost_percentage / 100.0)
            * multiplier,
        early_assignment_cash_obligation: short.strike * multiplier,
        assignment_is_covered: true,
        atomic_execution_supported: false,
        shadow_only: true,
        leg_executions: vec![
            LegExecution {
                symbol: long.symbol.clone(),
                state: LegExecutionState::Planned,
                requested_quantity: 1,
                filled_quantity: 0,
                average_fill_price: None,
                total_cost: 0.0,
                updated_at_secs: frame.underlying.timestamp_secs,
                broker_order_id: None,
            },
            LegExecution {
                symbol: short.symbol.clone(),
                state: LegExecutionState::Planned,
                requested_quantity: 1,
                filled_quantity: 0,
                average_fill_price: None,
                total_cost: 0.0,
                updated_at_secs: frame.underlying.timestamp_secs,
                broker_order_id: None,
            },
        ],
        temporary_cash_obligation: short.strike * multiplier,
        dividend_assignment_exposure: 0.0,
        recovery_action: "shadow_only: no enviar patas secuenciales".into(),
    })
}

/// Recalcula el peor escenario observable ante fills parciales, caída del
/// proceso, asignación temprana, dividendo o desaparición de una serie.
pub fn assess_vertical_recovery(
    position: &MultiLegPosition,
    rules: VerticalRiskRules,
) -> VerticalRecoveryAssessment {
    let long = position.leg_executions.iter().find(|leg| {
        position
            .legs
            .iter()
            .any(|item| item.symbol == leg.symbol && item.side == LegSide::Long)
    });
    let short = position.leg_executions.iter().find(|leg| {
        position
            .legs
            .iter()
            .any(|item| item.symbol == leg.symbol && item.side == LegSide::Short)
    });
    let long_filled = long.map_or(0, |leg| leg.filled_quantity);
    let short_filled = short.map_or(0, |leg| leg.filled_quantity);
    let unmatched_short = short_filled.saturating_sub(long_filled);
    let short_kind = short.and_then(|execution| {
        position
            .legs
            .iter()
            .find(|leg| leg.symbol == execution.symbol && leg.side == LegSide::Short)
            .map(|leg| leg.kind)
    });
    let assigned = short.is_some_and(|leg| leg.state == LegExecutionState::Assigned);
    let missing = position
        .leg_executions
        .iter()
        .any(|leg| leg.state == LegExecutionState::MissingSeries);
    let assignment_cash = if assigned || unmatched_short > 0 {
        position.early_assignment_cash_obligation * short_filled.max(1) as f64
    } else {
        0.0
    };
    let dividend_exposure = if rules.short_leg_ex_dividend_risk {
        rules.expected_dividend_per_share
            * position.contract_multiplier as f64
            * short_filled as f64
    } else {
        0.0
    };
    let maximum_economic_loss = if unmatched_short > 0 {
        match short_kind {
            // Una CALL corta desnuda no tiene cota económica.
            Some(OptionKind::Call) | None => f64::INFINITY,
            // Una PUT corta queda acotada por strike × unidades, aunque puede
            // exceder ampliamente efectivo y margen disponibles.
            Some(OptionKind::Put) => {
                position.early_assignment_cash_obligation * unmatched_short as f64
                    + dividend_exposure
            }
        }
    } else {
        position.maximum_loss + dividend_exposure
    };
    let margin_required =
        assignment_cash * (rules.margin_requirement_percentage / 100.0).clamp(0.0, 1.0);
    let cash_sufficient = rules.available_cash >= assignment_cash.max(margin_required);
    let real_execution_allowed = rules.atomic_combo_verified
        && position.atomic_execution_supported
        && !missing
        && maximum_economic_loss.is_finite()
        && cash_sufficient;
    let action = if !rules.atomic_combo_verified || !position.atomic_execution_supported {
        "mantener shadow_only: atomicidad/permiso del broker no verificado"
    } else if unmatched_short > 0 {
        "bloquear nuevas órdenes y cubrir primero la pata corta"
    } else if missing {
        "bloquear y reconciliar la serie faltante con el broker"
    } else if !cash_sufficient {
        "bloquear: efectivo/margen insuficiente para asignación"
    } else {
        "posición recuperable; reconciliar cantidades antes de continuar"
    };
    VerticalRecoveryAssessment {
        maximum_economic_loss,
        temporary_cash_obligation: assignment_cash,
        margin_required,
        cash_sufficient,
        duplicate_exposure: false,
        real_execution_allowed,
        action: action.into(),
    }
}

fn compatible_short_leg<'a>(frame: &'a MarketFrame, long: &OptionQuote) -> Option<&'a OptionQuote> {
    frame
        .options
        .iter()
        .filter(|candidate| {
            candidate.kind == long.kind
                && candidate.expiry_days == long.expiry_days
                && candidate.symbol != long.symbol
                && candidate.executable_sell_price().is_some()
                && match long.kind {
                    OptionKind::Call => candidate.strike > long.strike,
                    OptionKind::Put => candidate.strike < long.strike,
                }
        })
        .min_by(|left, right| {
            (left.strike - long.strike)
                .abs()
                .total_cmp(&(right.strike - long.strike).abs())
        })
}

pub fn append_research(
    path: &Path,
    timestamp_secs: i64,
    position: &MultiLegPosition,
) -> Result<(), AppError> {
    let mut file = open_private_append_bounded(path, 128 * 1024 * 1024)?;
    serde_json::to_writer(
        &mut file,
        &serde_json::json!({
            "schema_version": 2,
            "timestamp_secs": timestamp_secs,
            "position": position,
            "execution_scope": "shadow_only"
        }),
    )?;
    file.write_all(b"\n")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::market::{
        ContractMetadataSource, ExerciseStyle, OptionQuote, QuoteTimestampSource, UnderlyingQuote,
    };

    fn quote(symbol: &str, strike: f64, bid: f64, ask: f64) -> OptionQuote {
        OptionQuote {
            symbol: symbol.into(),
            underlying: "GGAL".into(),
            kind: OptionKind::Call,
            strike,
            expiry_days: 20,
            expiration_timestamp_secs: None,
            catalog_contract_multiplier: None,
            catalog_observed_at_secs: None,
            catalog_schema_version: 0,
            catalog_sha256: None,
            catalog_archived: false,
            contract_metadata_source: ContractMetadataSource::Legacy,
            exercise_style: ExerciseStyle::American,
            last: (bid + ask) / 2.0,
            bid: Some(bid),
            ask: Some(ask),
            volume: 100,
            timestamp_secs: 1,
            exchange_timestamp_secs: Some(1),
            received_at_secs: 1,
            timestamp_source: QuoteTimestampSource::Exchange,
        }
    }

    fn frame() -> MarketFrame {
        MarketFrame {
            underlying: UnderlyingQuote {
                ticker: "GGAL".into(),
                last: 100.0,
                bid: Some(99.9),
                ask: Some(100.1),
                timestamp_secs: 1,
                exchange_timestamp_secs: None,
                received_at_secs: 0,
                timestamp_source: QuoteTimestampSource::Legacy,
            },
            options: vec![
                quote("GGALC100", 100.0, 4.9, 5.0),
                quote("GGALC110", 110.0, 1.9, 2.0),
            ],
            option_chain_quality: None,
            vix: None,
        }
    }

    #[test]
    fn bull_call_has_bounded_loss_and_profit() {
        let frame = frame();
        let spread =
            research_vertical_spread(&frame, OptionKind::Call, "GGALC100", 100, 0.2).unwrap();
        assert_eq!(spread.strategy, "bull_call_debit");
        assert!(spread.maximum_loss > 0.0 && spread.maximum_profit > 0.0);
        assert!(!spread.atomic_execution_supported);
    }

    #[test]
    fn unmatched_short_is_never_treated_as_bounded_or_live_ready() {
        let frame = frame();
        let mut spread =
            research_vertical_spread(&frame, OptionKind::Call, "GGALC100", 100, 0.5).unwrap();
        spread.leg_executions[1].filled_quantity = 1;
        spread.leg_executions[1].state = LegExecutionState::Filled;
        let assessment = assess_vertical_recovery(
            &spread,
            VerticalRiskRules {
                available_cash: 1_000_000.0,
                margin_requirement_percentage: 50.0,
                expected_dividend_per_share: 0.0,
                short_leg_ex_dividend_risk: false,
                atomic_combo_verified: false,
            },
        );
        assert!(assessment.maximum_economic_loss.is_infinite());
        assert!(!assessment.real_execution_allowed);
        assert!(assessment.action.contains("shadow_only"));
    }
}
