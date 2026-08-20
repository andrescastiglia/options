use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RiskLimits {
    pub max_notional: f64,
    pub max_loss_per_trade: f64,
    pub max_daily_loss: f64,
    pub max_trades_per_day: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RiskState {
    pub realized_pnl: f64,
    pub trades_today: u32,
    pub kill_switch: bool,
    pub last_rejection: Option<String>,
}

impl Default for RiskState {
    fn default() -> Self {
        Self {
            realized_pnl: 0.0,
            trades_today: 0,
            kill_switch: false,
            last_rejection: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RiskManager {
    pub limits: RiskLimits,
    pub state: RiskState,
}

impl RiskManager {
    pub fn new(limits: RiskLimits) -> Self {
        Self {
            limits,
            state: RiskState::default(),
        }
    }

    pub fn allow_entry(&mut self, notional: f64) -> Result<(), String> {
        let reason = if self.state.kill_switch {
            Some("kill switch activo")
        } else if !notional.is_finite() || notional <= 0.0 {
            Some("nocional invalido")
        } else if notional > self.limits.max_notional {
            Some("nocional excede el limite")
        } else if self.state.realized_pnl <= -self.limits.max_daily_loss {
            Some("limite de perdida diaria alcanzado")
        } else if self.state.trades_today >= self.limits.max_trades_per_day {
            Some("limite diario de operaciones alcanzado")
        } else {
            None
        };
        if let Some(reason) = reason {
            self.state.last_rejection = Some(reason.into());
            return Err(reason.into());
        }
        self.state.last_rejection = None;
        Ok(())
    }

    pub fn record_close(&mut self, pnl: f64) {
        if pnl.is_finite() {
            self.state.realized_pnl += pnl;
        }
        self.state.trades_today = self.state.trades_today.saturating_add(1);
        if self.state.realized_pnl <= -self.limits.max_daily_loss {
            self.state.kill_switch = true;
            self.state.last_rejection = Some("limite de perdida diaria alcanzado".into());
        }
    }

    pub fn engage_kill_switch(&mut self) {
        self.state.kill_switch = true;
        self.state.last_rejection = Some("kill switch manual".into());
    }

    pub fn resume(&mut self) -> Result<(), String> {
        if self.state.realized_pnl <= -self.limits.max_daily_loss {
            return Err("no se puede reanudar: perdida diaria excedida".into());
        }
        self.state.kill_switch = false;
        self.state.last_rejection = None;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn risk() -> RiskManager {
        RiskManager::new(RiskLimits {
            max_notional: 1_000.0,
            max_loss_per_trade: 100.0,
            max_daily_loss: 200.0,
            max_trades_per_day: 2,
        })
    }

    #[test]
    fn blocks_notional_above_limit() {
        assert!(risk().allow_entry(1_001.0).is_err());
    }

    #[test]
    fn daily_loss_engages_kill_switch() {
        let mut risk = risk();
        risk.record_close(-200.0);
        assert!(risk.state.kill_switch);
        assert!(risk.allow_entry(10.0).is_err());
    }
}
