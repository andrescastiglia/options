use std::{
    collections::BTreeMap,
    io::{BufWriter, Write},
    path::Path,
};

use serde::{Deserialize, Serialize};

use crate::{
    errors::AppError,
    learning::ValidationTrade,
    market::{MarketFrame, OptionKind, OptionSelectionCriteria, QuoteTimestampSource},
    pattern::Direction,
    secure_fs::open_private_append_bounded,
};

pub const ANALYTICS_SCHEMA_VERSION: u32 = 2;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExecutionObservation {
    pub schema_version: u32,
    pub submitted_at_secs: i64,
    pub operation_id: String,
    pub broker_order_id: Option<String>,
    pub symbol: String,
    pub side: crate::broker::OrderSide,
    pub requested_quantity: u32,
    pub filled_quantity: u32,
    pub remaining_quantity: u32,
    pub limit_price: f64,
    pub fill_price: Option<f64>,
    pub final_status: crate::broker::OrderStatus,
    pub elapsed_millis: u128,
    pub acceptance_millis: u128,
    pub tracking_millis: u128,
    pub rest_polls: u32,
    pub websocket_signals: u32,
    pub route: String,
    pub websocket_state_at_submit: String,
    pub price_attempts: u32,
    pub cancellation_observed: bool,
    pub cancellation_requested: bool,
}

pub fn append_execution(path: &Path, observation: &ExecutionObservation) -> Result<(), AppError> {
    let file = open_private_append_bounded(path, 128 * 1024 * 1024)?;
    let mut writer = BufWriter::new(file);
    serde_json::to_writer(&mut writer, observation)?;
    writer.write_all(b"\n")?;
    writer.flush()?;
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CandidateObservation {
    pub schema_version: u32,
    pub evaluated_at_secs: i64,
    pub signal: Direction,
    pub symbol: String,
    pub kind: OptionKind,
    pub selected: bool,
    pub rejection_reasons: Vec<String>,
    pub last: f64,
    pub bid: Option<f64>,
    pub ask: Option<f64>,
    pub spread_percentage: Option<f64>,
    pub volume: u64,
    pub days_to_expiry: u32,
    pub moneyness_distance_percentage: f64,
    pub total_friction_percentage: Option<f64>,
    pub quote_timestamp_secs: i64,
    pub exchange_timestamp_secs: Option<i64>,
    pub received_at_secs: i64,
    pub timestamp_source: QuoteTimestampSource,
    pub underlying_timestamp_secs: i64,
    pub vix_level: Option<f64>,
    pub vix_change_percentage: Option<f64>,
    pub vix_value_kind: Option<crate::market::VixValueKind>,
}

pub fn candidate_observations(
    frame: &MarketFrame,
    kind: OptionKind,
    signal: Direction,
    criteria: OptionSelectionCriteria,
    selected_symbol: Option<&str>,
) -> Vec<CandidateObservation> {
    frame
        .options
        .iter()
        .filter(|option| option.kind == kind)
        .map(|option| {
            let spread = option.spread_percentage();
            let moneyness = ((option.strike - frame.underlying.last).abs()
                / frame.underlying.last.max(f64::EPSILON))
                * 100.0;
            let mut reasons = Vec::new();
            if option.expiry_days < criteria.min_expiry_days {
                reasons.push("vencimiento_demasiado_cercano".into());
            }
            if option.expiry_days > criteria.max_expiry_days {
                reasons.push("vencimiento_demasiado_lejano".into());
            }
            if option.volume < criteria.min_volume {
                reasons.push("volumen_insuficiente".into());
            }
            if moneyness > criteria.max_moneyness_distance_percentage {
                reasons.push("distancia_al_dinero_excesiva".into());
            }
            if option.executable_buy_price().is_none() || option.executable_sell_price().is_none() {
                reasons.push("libro_no_ejecutable".into());
            }
            if spread.is_some_and(|value| value > criteria.max_spread_percentage) {
                reasons.push("spread_excesivo".into());
            }
            if option
                .validate_freshness(criteria.now_secs, criteria.max_age_secs)
                .is_err()
            {
                reasons.push("cotizacion_no_vigente".into());
            }
            let total_friction_percentage = spread.map(|value| {
                value
                    + 2.0 * criteria.operating_cost_percentage
                    + 2.0 * criteria.slippage_bps / 100.0
            });
            CandidateObservation {
                schema_version: ANALYTICS_SCHEMA_VERSION,
                evaluated_at_secs: criteria.now_secs,
                signal,
                symbol: option.symbol.clone(),
                kind: option.kind,
                selected: selected_symbol == Some(option.symbol.as_str()),
                rejection_reasons: reasons,
                last: option.last,
                bid: option.bid,
                ask: option.ask,
                spread_percentage: spread,
                volume: option.volume,
                days_to_expiry: option.expiry_days,
                moneyness_distance_percentage: moneyness,
                total_friction_percentage,
                quote_timestamp_secs: option.timestamp_secs,
                exchange_timestamp_secs: option.exchange_timestamp_secs,
                received_at_secs: option.received_at_secs,
                timestamp_source: option.timestamp_source,
                underlying_timestamp_secs: frame.underlying.timestamp_secs,
                vix_level: frame.vix.map(|vix| vix.level),
                vix_change_percentage: frame.vix.and_then(|vix| vix.change_percentage()),
                vix_value_kind: frame.vix.map(|vix| vix.value_kind),
            }
        })
        .collect()
}

pub fn append_candidates(
    path: &Path,
    observations: &[CandidateObservation],
) -> Result<(), AppError> {
    if observations.is_empty() {
        return Ok(());
    }
    let file = open_private_append_bounded(path, 128 * 1024 * 1024)?;
    let mut writer = BufWriter::new(file);
    for observation in observations {
        serde_json::to_writer(&mut writer, observation)?;
        writer.write_all(b"\n")?;
    }
    writer.flush()?;
    Ok(())
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct BaselineSegment {
    pub trades: u64,
    pub wins: u64,
    pub net_pnl: f64,
    pub expectancy: f64,
    pub win_rate: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BaselineReport {
    pub schema_version: u32,
    pub generated_at_secs: i64,
    pub total: BaselineSegment,
    pub by_option_kind: BTreeMap<String, BaselineSegment>,
    pub by_entry_hour_argentina: BTreeMap<String, BaselineSegment>,
    pub by_minutes_after_open: BTreeMap<String, BaselineSegment>,
    pub by_expiry_bucket: BTreeMap<String, BaselineSegment>,
    pub by_spread_bucket: BTreeMap<String, BaselineSegment>,
    pub by_vix_bucket: BTreeMap<String, BaselineSegment>,
}

pub fn baseline_report(trades: &[ValidationTrade], generated_at_secs: i64) -> BaselineReport {
    let mut report = BaselineReport {
        schema_version: ANALYTICS_SCHEMA_VERSION,
        generated_at_secs,
        total: BaselineSegment::default(),
        by_option_kind: BTreeMap::new(),
        by_entry_hour_argentina: BTreeMap::new(),
        by_minutes_after_open: BTreeMap::new(),
        by_expiry_bucket: BTreeMap::new(),
        by_spread_bucket: BTreeMap::new(),
        by_vix_bucket: BTreeMap::new(),
    };
    for trade in trades {
        add(&mut report.total, trade.net_pnl);
        add_map(
            &mut report.by_option_kind,
            format!("{:?}", trade.kind).to_ascii_lowercase(),
            trade.net_pnl,
        );
        let opened = trade.context.opened_at_secs;
        add_map(
            &mut report.by_entry_hour_argentina,
            format!(
                "{:02}",
                opened.saturating_sub(3 * 3_600).rem_euclid(86_400) / 3_600
            ),
            trade.net_pnl,
        );
        let local_minute = opened.saturating_sub(3 * 3_600).rem_euclid(86_400) / 60;
        let minutes_after_open = local_minute - (10 * 60 + 30);
        let opening_bucket = if minutes_after_open < 0 {
            "antes_apertura"
        } else if minutes_after_open <= 30 {
            "0-30"
        } else if minutes_after_open <= 45 {
            "31-45"
        } else if minutes_after_open <= 90 {
            "46-90"
        } else {
            ">90"
        };
        add_map(
            &mut report.by_minutes_after_open,
            opening_bucket.into(),
            trade.net_pnl,
        );
        add_map(
            &mut report.by_expiry_bucket,
            bucket_i64(
                trade.context.days_to_expiry,
                &[(7, "0-7"), (21, "8-21"), (45, "22-45")],
            ),
            trade.net_pnl,
        );
        add_map(
            &mut report.by_spread_bucket,
            bucket_f64(
                trade.context.entry_spread_percentage,
                &[(1.0, "<=1"), (3.0, "1-3"), (5.0, "3-5")],
            ),
            trade.net_pnl,
        );
        add_map(
            &mut report.by_vix_bucket,
            bucket_f64(
                trade.context.vix_level,
                &[(20.0, "<=20"), (25.0, "20-25"), (30.0, "25-30")],
            ),
            trade.net_pnl,
        );
    }
    finish(&mut report.total);
    for map in [
        &mut report.by_option_kind,
        &mut report.by_entry_hour_argentina,
        &mut report.by_minutes_after_open,
        &mut report.by_expiry_bucket,
        &mut report.by_spread_bucket,
        &mut report.by_vix_bucket,
    ] {
        for segment in map.values_mut() {
            finish(segment);
        }
    }
    report
}

fn add_map(map: &mut BTreeMap<String, BaselineSegment>, key: String, pnl: f64) {
    add(map.entry(key).or_default(), pnl);
}

fn add(segment: &mut BaselineSegment, pnl: f64) {
    segment.trades += 1;
    segment.wins += u64::from(pnl > 0.0);
    segment.net_pnl += pnl;
}

fn finish(segment: &mut BaselineSegment) {
    if segment.trades > 0 {
        segment.expectancy = segment.net_pnl / segment.trades as f64;
        segment.win_rate = segment.wins as f64 / segment.trades as f64;
    }
}

fn bucket_i64(value: Option<i64>, limits: &[(i64, &str)]) -> String {
    let Some(value) = value else {
        return "sin_dato".into();
    };
    limits
        .iter()
        .find(|(limit, _)| value <= *limit)
        .map_or_else(
            || format!(">{}", limits.last().map_or(0, |item| item.0)),
            |(_, label)| (*label).into(),
        )
}

fn bucket_f64(value: Option<f64>, limits: &[(f64, &str)]) -> String {
    let Some(value) = value.filter(|value| value.is_finite()) else {
        return "sin_dato".into();
    };
    limits
        .iter()
        .find(|(limit, _)| value <= *limit)
        .map_or_else(
            || format!(">{}", limits.last().map_or(0.0, |item| item.0)),
            |(_, label)| (*label).into(),
        )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{learning::ValidationContext, trading::PositionKind};

    #[test]
    fn baseline_is_segmented_without_dropping_missing_context() {
        let report = baseline_report(
            &[ValidationTrade {
                kind: PositionKind::Call,
                net_pnl: 10.0,
                stressed_net_pnl: 5.0,
                closed_at_secs: 1,
                context: ValidationContext::default(),
            }],
            2,
        );
        assert_eq!(report.total.trades, 1);
        assert_eq!(report.by_vix_bucket["sin_dato"].trades, 1);
    }
}
