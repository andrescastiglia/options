use std::time::{Duration, SystemTime};

use crate::pattern::Direction;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PositionKind {
    Call,
    Put,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Position {
    pub kind: PositionKind,
    pub entry_price: f64,
    pub contracts: u32,
    pub opened_at: SystemTime,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Pnl {
    pub gross: f64,
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
    let gross = (exit_price - entry_price) * contracts as f64;
    let commission_rate = commission_percentage / 100.0;
    let tax_rate = tax_percentage / 100.0;
    let commission = (entry_price * contracts as f64 * commission_rate)
        + (exit_price * contracts as f64 * commission_rate);
    let tax = gross.max(0.0) * tax_rate;
    let net = gross - commission - tax;
    Pnl {
        gross,
        commission,
        tax,
        net,
        threshold: (commission + tax) * multiplier,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TradingState {
    Idle,
    SearchingCall,
    SearchingPut,
    CallActive,
    PutActive,
    Selling,
    Closed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExitReason {
    ProfitTarget,
    TrendReversal,
    Timeout,
    Defensive,
}

pub struct TradingEngine {
    pub state: TradingState,
    pub position: Option<Position>,
}

impl TradingEngine {
    pub fn new() -> Self {
        Self {
            state: TradingState::Idle,
            position: None,
        }
    }

    pub fn consider_entry(&mut self, direction: Direction) {
        if self.position.is_some() {
            return;
        }
        self.state = match direction {
            Direction::Up => TradingState::SearchingCall,
            Direction::Down => TradingState::SearchingPut,
            Direction::Neutral => TradingState::Idle,
        };
    }

    pub fn open_fake_position(
        &mut self,
        kind: PositionKind,
        entry_price: f64,
        contracts: u32,
        opened_at: SystemTime,
    ) -> bool {
        if self.position.is_some()
            || !entry_price.is_finite()
            || entry_price <= 0.0
            || contracts == 0
        {
            return false;
        }
        self.position = Some(Position {
            kind,
            entry_price,
            contracts,
            opened_at,
        });
        self.state = match kind {
            PositionKind::Call => TradingState::CallActive,
            PositionKind::Put => TradingState::PutActive,
        };
        true
    }

    pub fn should_exit(
        &self,
        current_price: f64,
        pnl: Pnl,
        opposite_trend: bool,
        now: SystemTime,
        timeout: Duration,
    ) -> Option<ExitReason> {
        let position = self.position.as_ref()?;
        if !current_price.is_finite() || current_price <= 0.0 {
            return Some(ExitReason::Defensive);
        }
        if pnl.net >= pnl.threshold {
            return Some(ExitReason::ProfitTarget);
        }
        if opposite_trend {
            return Some(ExitReason::TrendReversal);
        }
        if now.duration_since(position.opened_at).unwrap_or_default() >= timeout {
            return Some(ExitReason::Timeout);
        }
        None
    }

    pub fn mark_selling(&mut self) {
        if self.position.is_some() {
            self.state = TradingState::Selling;
        }
    }
    pub fn close(&mut self) {
        self.position = None;
        self.state = TradingState::Closed;
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

    #[test]
    fn pnl_uses_entry_and_exit_commission() {
        let pnl = calculate_pnl(2.15, 2.95, 5, 0.19, 35.0, 2.0);
        assert!((pnl.gross - 4.0).abs() < 1e-9);
        assert!(pnl.commission > 0.0);
        assert!(pnl.tax > 0.0);
        assert!(pnl.net < pnl.gross);
    }

    #[test]
    fn engine_does_not_duplicate_positions() {
        let mut engine = TradingEngine::new();
        assert!(engine.open_fake_position(PositionKind::Call, 2.0, 1, SystemTime::now()));
        assert!(!engine.open_fake_position(PositionKind::Put, 2.0, 1, SystemTime::now()));
        assert_eq!(engine.state, TradingState::CallActive);
    }

    #[test]
    fn invalid_price_takes_defensive_exit_precedence() {
        let mut engine = TradingEngine::new();
        engine.open_fake_position(PositionKind::Call, 2.0, 1, SystemTime::now());
        let pnl = calculate_pnl(2.0, 2.5, 1, 0.19, 35.0, 2.0);
        assert_eq!(
            engine.should_exit(
                f64::NAN,
                pnl,
                false,
                SystemTime::now(),
                Duration::from_secs(60)
            ),
            Some(ExitReason::Defensive)
        );
    }

    #[test]
    fn timeout_requests_exit() {
        let mut engine = TradingEngine::new();
        let opened_at = SystemTime::now() - Duration::from_secs(120);
        engine.open_fake_position(PositionKind::Put, 2.0, 1, opened_at);
        let pnl = calculate_pnl(2.0, 1.9, 1, 0.19, 35.0, 2.0);
        assert_eq!(
            engine.should_exit(1.9, pnl, false, SystemTime::now(), Duration::from_secs(60)),
            Some(ExitReason::Timeout)
        );
    }
}
