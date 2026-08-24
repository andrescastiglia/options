use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::trading::{ExitReason, Position};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ClosedTrade {
    pub position: Position,
    pub exit_price: f64,
    pub net_pnl: f64,
    pub closed_at_secs: i64,
    pub reason: ExitReason,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Portfolio {
    positions: HashMap<String, Position>,
    closed: Vec<ClosedTrade>,
    realized_pnl: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct PortfolioMetrics {
    pub open_positions: usize,
    pub realized_pnl: f64,
    pub trades: u64,
    pub wins: u64,
    pub losses: u64,
}

impl Portfolio {
    pub fn open(&mut self, position: Position) -> bool {
        if self.positions.contains_key(&position.operation_id)
            || position.entry_price <= 0.0
            || position.contracts == 0
        {
            return false;
        }
        self.positions
            .insert(position.operation_id.clone(), position);
        true
    }

    pub fn close(
        &mut self,
        id: &str,
        exit_price: f64,
        net_pnl: f64,
        closed_at_secs: i64,
        reason: ExitReason,
    ) -> Option<ClosedTrade> {
        let position = self.positions.remove(id)?;
        self.realized_pnl += net_pnl;
        let trade = ClosedTrade {
            position,
            exit_price,
            net_pnl,
            closed_at_secs,
            reason,
        };
        self.closed.push(trade.clone());
        Some(trade)
    }

    pub fn metrics(&self) -> PortfolioMetrics {
        PortfolioMetrics {
            open_positions: self.positions.len(),
            realized_pnl: self.realized_pnl,
            trades: self.closed.len() as u64,
            wins: self
                .closed
                .iter()
                .filter(|trade| trade.net_pnl > 0.0)
                .count() as u64,
            losses: self
                .closed
                .iter()
                .filter(|trade| trade.net_pnl <= 0.0)
                .count() as u64,
        }
    }

    pub fn contains(&self, id: &str) -> bool {
        self.positions.contains_key(id)
    }

    pub fn closed_trades(&self) -> &[ClosedTrade] {
        &self.closed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::trading::PositionKind;

    fn position() -> Position {
        Position {
            operation_id: "op-1".into(),
            option_symbol: "GAL-C-100".into(),
            kind: PositionKind::Call,
            entry_price: 2.0,
            contracts: 1,
            contract_multiplier: 1,
            opened_at_secs: 1,
            economics: None,
        }
    }

    #[test]
    fn portfolio_is_idempotent_and_tracks_realized_pnl() {
        let mut portfolio = Portfolio::default();
        assert!(portfolio.open(position()));
        assert!(!portfolio.open(position()));
        portfolio
            .close("op-1", 3.0, 5.0, 2, ExitReason::ProfitTarget)
            .unwrap();
        assert_eq!(portfolio.metrics().realized_pnl, 5.0);
        assert_eq!(portfolio.metrics().wins, 1);
    }
}
