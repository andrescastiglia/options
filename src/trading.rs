use serde::{Deserialize, Serialize};

use crate::{market::OptionKind, pattern::Direction};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PositionKind {
    Call,
    Put,
}

impl From<OptionKind> for PositionKind {
    fn from(value: OptionKind) -> Self {
        match value {
            OptionKind::Call => Self::Call,
            OptionKind::Put => Self::Put,
        }
    }
}

impl From<PositionKind> for OptionKind {
    fn from(value: PositionKind) -> Self {
        match value {
            PositionKind::Call => Self::Call,
            PositionKind::Put => Self::Put,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Position {
    pub operation_id: String,
    pub option_symbol: String,
    pub kind: PositionKind,
    pub entry_price: f64,
    pub contracts: u32,
    pub contract_multiplier: u32,
    pub opened_at_secs: i64,
    #[serde(default)]
    pub economics: Option<PositionEconomics>,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct PositionEconomics {
    pub operating_cost_percentage: f64,
    pub tax_percentage: f64,
    pub exit_slippage_bps: f64,
    pub stop_price: f64,
    pub target_price: f64,
    pub max_net_loss: f64,
    pub target_net_profit: f64,
}

impl Position {
    pub fn notional(&self) -> f64 {
        self.entry_price * self.contracts as f64 * self.contract_multiplier as f64
    }

    pub fn direction(&self) -> Direction {
        match self.kind {
            PositionKind::Call => Direction::Up,
            PositionKind::Put => Direction::Down,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Pnl {
    pub gross: f64,
    #[serde(default)]
    pub entry_cost: f64,
    #[serde(default)]
    pub exit_cost: f64,
    pub commission: f64,
    pub tax: f64,
    pub net: f64,
    pub threshold: f64,
}

pub fn calculate_pnl(
    entry_price: f64,
    exit_price: f64,
    contracts: u32,
    commission_percentage: f64,
    tax_percentage: f64,
    multiplier: f64,
) -> Pnl {
    calculate_pnl_with_contract_multiplier(
        entry_price,
        exit_price,
        contracts,
        1,
        commission_percentage,
        tax_percentage,
        multiplier,
    )
}

#[allow(clippy::too_many_arguments)]
pub fn calculate_pnl_with_contract_multiplier(
    entry_price: f64,
    exit_price: f64,
    contracts: u32,
    contract_multiplier: u32,
    commission_percentage: f64,
    tax_percentage: f64,
    profit_multiplier: f64,
) -> Pnl {
    let units = contracts as f64 * contract_multiplier as f64;
    let gross = (exit_price - entry_price) * units;
    let commission_rate = commission_percentage / 100.0;
    let tax_rate = tax_percentage / 100.0;
    // `commission_rate` es el costo operativo efectivo: comisión y otros cargos,
    // ambos con IVA. Se cobra independientemente en la compra y en la venta.
    let entry_cost = entry_price * units * commission_rate;
    let exit_cost = exit_price * units * commission_rate;
    let commission = entry_cost + exit_cost;
    let tax = gross.max(0.0) * tax_rate;
    let net = gross - commission - tax;
    Pnl {
        gross,
        entry_cost,
        exit_cost,
        commission,
        tax,
        net,
        // El impuesto depende de la propia ganancia; incluirlo en el objetivo y luego
        // multiplicarlo puede volver la salida matemáticamente inalcanzable.
        // El objetivo neto se expresa como múltiplo del costo operativo de ida y vuelta.
        threshold: commission * profit_multiplier,
    }
}

#[allow(clippy::too_many_arguments)]
pub fn build_position_economics(
    entry_price: f64,
    contracts: u32,
    contract_multiplier: u32,
    operating_cost_percentage: f64,
    tax_percentage: f64,
    exit_slippage_bps: f64,
    stop_loss_percentage: f64,
    max_loss_per_trade: f64,
    min_profit_multiplier: f64,
    min_reward_risk_ratio: f64,
) -> Option<PositionEconomics> {
    if !entry_price.is_finite()
        || entry_price <= 0.0
        || contracts == 0
        || contract_multiplier == 0
        || max_loss_per_trade <= 0.0
    {
        return None;
    }
    let units = contracts as f64 * contract_multiplier as f64;
    let cost_rate = operating_cost_percentage.max(0.0) / 100.0;
    let tax_rate = (tax_percentage / 100.0).clamp(0.0, 1.0);
    let slip = (exit_slippage_bps.max(0.0) / 10_000.0).min(0.99);

    let percentage_trigger = entry_price * (1.0 - stop_loss_percentage / 100.0);
    let absolute_fill = (entry_price * (1.0 + cost_rate) - max_loss_per_trade / units)
        / (1.0 - cost_rate).max(f64::EPSILON);
    let absolute_trigger = absolute_fill.max(0.0) / (1.0 - slip);
    let stop_price = percentage_trigger.max(absolute_trigger).max(f64::EPSILON);
    if stop_price >= entry_price {
        return None;
    }
    let stop_fill = stop_price * (1.0 - slip);
    let stop_pnl = calculate_pnl_with_contract_multiplier(
        entry_price,
        stop_fill,
        contracts,
        contract_multiplier,
        operating_cost_percentage,
        tax_percentage,
        min_profit_multiplier,
    );
    let max_net_loss = (-stop_pnl.net).max(0.0).min(max_loss_per_trade);

    let after_tax_rate = 1.0 - tax_rate;
    let denominator = after_tax_rate - cost_rate;
    if denominator <= 0.0 {
        return None;
    }
    let mut target_fill = entry_price;
    let mut target_net_profit = 0.0;
    for _ in 0..8 {
        let round_trip_cost = cost_rate * (entry_price + target_fill) * units;
        target_net_profit =
            (round_trip_cost * min_profit_multiplier).max(max_net_loss * min_reward_risk_ratio);
        target_fill =
            (target_net_profit / units + entry_price * (after_tax_rate + cost_rate)) / denominator;
    }
    let target_price = target_fill / (1.0 - slip);
    if !target_price.is_finite() || target_price <= entry_price {
        return None;
    }
    Some(PositionEconomics {
        operating_cost_percentage,
        tax_percentage,
        exit_slippage_bps,
        stop_price,
        target_price,
        max_net_loss,
        target_net_profit,
    })
}

pub fn calculate_position_pnl(position: &Position, exit_price: f64) -> Pnl {
    let economics = position.economics.unwrap_or(PositionEconomics {
        operating_cost_percentage: 0.0,
        tax_percentage: 0.0,
        exit_slippage_bps: 0.0,
        stop_price: 0.0,
        target_price: f64::INFINITY,
        max_net_loss: f64::INFINITY,
        target_net_profit: 0.0,
    });
    let mut pnl = calculate_pnl_with_contract_multiplier(
        position.entry_price,
        exit_price,
        position.contracts,
        position.contract_multiplier,
        economics.operating_cost_percentage,
        economics.tax_percentage,
        1.0,
    );
    pnl.threshold = economics.target_net_profit;
    pnl
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TradingState {
    Idle,
    SearchingCall,
    SearchingPut,
    Buying,
    CallActive,
    PutActive,
    Selling,
    Halted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ExitReason {
    ProfitTarget,
    StopLoss,
    TrendReversal,
    Timeout,
    RiskLimit,
    Manual,
    Defensive,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TradingEngine {
    pub state: TradingState,
    pub position: Option<Position>,
    pub last_exit_reason: Option<ExitReason>,
}

impl TradingEngine {
    pub fn new() -> Self {
        Self {
            state: TradingState::Idle,
            position: None,
            last_exit_reason: None,
        }
    }

    pub fn consider_entry(&mut self, direction: Direction) -> bool {
        if self.position.is_some() || self.state == TradingState::Halted {
            return false;
        }
        self.state = match direction {
            Direction::Up => TradingState::SearchingCall,
            Direction::Down => TradingState::SearchingPut,
            Direction::Neutral => TradingState::Idle,
        };
        direction != Direction::Neutral
    }

    pub fn mark_buying(&mut self) {
        if matches!(
            self.state,
            TradingState::SearchingCall | TradingState::SearchingPut
        ) {
            self.state = TradingState::Buying;
        }
    }

    pub fn open_position(&mut self, position: Position) -> bool {
        if self.position.is_some()
            || !position.entry_price.is_finite()
            || position.entry_price <= 0.0
            || position.contracts == 0
            || position.contract_multiplier == 0
            || position.operation_id.trim().is_empty()
            || position.option_symbol.trim().is_empty()
        {
            return false;
        }
        self.state = match position.kind {
            PositionKind::Call => TradingState::CallActive,
            PositionKind::Put => TradingState::PutActive,
        };
        self.position = Some(position);
        true
    }

    #[allow(clippy::too_many_arguments)]
    pub fn should_exit(
        &self,
        current_price: f64,
        pnl: Pnl,
        opposite_trend: bool,
        now_secs: i64,
        timeout_secs: i64,
        max_loss: f64,
        stop_loss_percentage: f64,
    ) -> Option<ExitReason> {
        let position = self.position.as_ref()?;
        if !current_price.is_finite() || current_price <= 0.0 {
            return Some(ExitReason::Defensive);
        }
        let frozen_stop = position.economics.map(|economics| economics.stop_price);
        let frozen_target = position.economics.map(|economics| economics.target_price);
        if pnl.net <= -max_loss
            || current_price
                <= frozen_stop.unwrap_or_else(|| {
                    position.entry_price * (1.0 - stop_loss_percentage.max(0.0) / 100.0)
                })
        {
            return Some(ExitReason::StopLoss);
        }
        if current_price >= frozen_target.unwrap_or(0.0)
            && pnl.net >= pnl.threshold
            && pnl.net > 0.0
        {
            return Some(ExitReason::ProfitTarget);
        }
        if opposite_trend {
            return Some(ExitReason::TrendReversal);
        }
        if now_secs.saturating_sub(position.opened_at_secs) >= timeout_secs {
            return Some(ExitReason::Timeout);
        }
        None
    }

    pub fn mark_selling(&mut self) {
        if self.position.is_some() {
            self.state = TradingState::Selling;
        }
    }

    pub fn close(&mut self, reason: ExitReason) -> Option<Position> {
        let position = self.position.take()?;
        self.last_exit_reason = Some(reason);
        self.state = TradingState::Idle;
        Some(position)
    }

    pub fn halt(&mut self) {
        self.state = TradingState::Halted;
    }

    pub fn resume(&mut self) {
        self.state = self
            .position
            .as_ref()
            .map_or(TradingState::Idle, |position| match position.kind {
                PositionKind::Call => TradingState::CallActive,
                PositionKind::Put => TradingState::PutActive,
            });
    }
}

impl Default for TradingEngine {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn position(opened_at_secs: i64) -> Position {
        Position {
            operation_id: "op-1".into(),
            option_symbol: "GAL-C-100".into(),
            kind: PositionKind::Call,
            entry_price: 2.0,
            contracts: 1,
            contract_multiplier: 1,
            opened_at_secs,
            economics: None,
        }
    }

    #[test]
    fn pnl_uses_entry_and_exit_commission() {
        let pnl = calculate_pnl(2.15, 2.95, 5, 0.19, 35.0, 2.0);
        assert!((pnl.gross - 4.0).abs() < 1e-9);
        assert!(pnl.entry_cost > 0.0);
        assert!(pnl.exit_cost > 0.0);
        assert!((pnl.commission - pnl.entry_cost - pnl.exit_cost).abs() < 1e-12);
        assert!(pnl.tax > 0.0);
        assert!(pnl.net < pnl.gross);
    }

    #[test]
    fn losing_trade_still_pays_both_sides_but_not_profit_tax() {
        let pnl = calculate_pnl(10.0, 8.0, 2, 0.25, 35.0, 2.0);
        assert!(pnl.entry_cost > 0.0);
        assert!(pnl.exit_cost > 0.0);
        assert_eq!(pnl.tax, 0.0);
        assert!(pnl.net < pnl.gross);
    }

    #[test]
    fn contract_multiplier_scales_pnl() {
        let plain = calculate_pnl(2.0, 3.0, 1, 0.0, 0.0, 2.0);
        let scaled = calculate_pnl_with_contract_multiplier(2.0, 3.0, 1, 100, 0.0, 0.0, 2.0);
        assert_eq!(scaled.net, plain.net * 100.0);
    }

    #[test]
    fn profit_target_remains_reachable_when_profit_is_taxed() {
        let pnl = calculate_pnl(1.0, 2.0, 1, 0.19, 35.0, 2.0);
        assert!(pnl.net > pnl.threshold);
    }

    #[test]
    fn frozen_economics_cover_costs_risk_ratio_and_exit_slippage() {
        let economics =
            build_position_economics(10.0, 2, 100, 0.25, 35.0, 25.0, 10.0, 500.0, 2.0, 1.25)
                .unwrap();
        assert!(economics.stop_price < 10.0);
        assert!(economics.target_price > 10.0);
        assert!(economics.target_net_profit >= economics.max_net_loss * 1.25);

        let mut frozen = position(1);
        frozen.entry_price = 10.0;
        frozen.contracts = 2;
        frozen.contract_multiplier = 100;
        frozen.economics = Some(economics);
        let at_target = calculate_position_pnl(&frozen, economics.target_price);
        assert!(at_target.net >= economics.target_net_profit - 0.01);
    }

    #[test]
    fn engine_does_not_duplicate_positions() {
        let mut engine = TradingEngine::new();
        assert!(engine.open_position(position(1)));
        assert!(!engine.open_position(position(2)));
        assert_eq!(engine.state, TradingState::CallActive);
    }

    #[test]
    fn stop_loss_has_precedence() {
        let mut engine = TradingEngine::new();
        engine.open_position(position(1));
        let pnl = calculate_pnl(2.0, 1.5, 1, 0.19, 35.0, 2.0);
        assert_eq!(
            engine.should_exit(1.5, pnl, true, 2, 60, 5_000.0, 15.0),
            Some(ExitReason::StopLoss)
        );
    }

    #[test]
    fn timeout_requests_exit() {
        let mut engine = TradingEngine::new();
        engine.open_position(position(1));
        let pnl = calculate_pnl(2.0, 1.9, 1, 0.19, 35.0, 2.0);
        assert_eq!(
            engine.should_exit(1.9, pnl, false, 121, 60, 5_000.0, 50.0),
            Some(ExitReason::Timeout)
        );
    }
}
