use std::collections::HashMap;

use crate::trading::{Position, PositionKind};

#[derive(Debug, Default)]
pub struct Portfolio {
    positions: HashMap<String, PositionRecord>,
    realized_pnl: f64,
    trades: u64,
}

#[derive(Debug, Clone, Copy)]
pub struct PositionRecord {
    pub kind: PositionKind,
    pub entry_price: f64,
    pub contracts: u32,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PortfolioMetrics {
    pub open_positions: usize,
    pub realized_pnl: f64,
    pub trades: u64,
}

impl Portfolio {
    pub fn open(
        &mut self,
        id: String,
        kind: PositionKind,
        entry_price: f64,
        contracts: u32,
    ) -> bool {
        if self.positions.contains_key(&id) || entry_price <= 0.0 || contracts == 0 {
            return false;
        }
        self.positions.insert(
            id,
            PositionRecord {
                kind,
                entry_price,
                contracts,
            },
        );
        true
    }

    pub fn close(&mut self, id: &str, pnl: f64) -> Option<PositionRecord> {
        let position = self.positions.remove(id)?;
        self.realized_pnl += pnl;
        self.trades += 1;
        Some(position)
    }

    pub fn metrics(&self) -> PortfolioMetrics {
        PortfolioMetrics {
            open_positions: self.positions.len(),
            realized_pnl: self.realized_pnl,
            trades: self.trades,
        }
    }
    pub fn contains(&self, id: &str) -> bool {
        self.positions.contains_key(id)
    }
}

impl From<Position> for PositionRecord {
    fn from(position: Position) -> Self {
        Self {
            kind: position.kind,
            entry_price: position.entry_price,
            contracts: position.contracts,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn portfolio_is_idempotent_and_tracks_realized_pnl() {
        let mut portfolio = Portfolio::default();
        assert!(portfolio.open("op-1".into(), PositionKind::Call, 2.0, 1));
        assert!(!portfolio.open("op-1".into(), PositionKind::Call, 2.0, 1));
        assert_eq!(portfolio.close("op-1", 5.0).unwrap().contracts, 1);
        assert_eq!(
            portfolio.metrics(),
            PortfolioMetrics {
                open_positions: 0,
                realized_pnl: 5.0,
                trades: 1
            }
        );
    }
}
