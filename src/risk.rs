use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KillSwitchReason {
    Manual,
    DailyLoss,
    Operational,
}

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
    #[serde(default)]
    pub trading_day: Option<i64>,
    #[serde(default)]
    pub kill_switch_reason: Option<KillSwitchReason>,
}

impl Default for RiskState {
    fn default() -> Self {
        Self {
            realized_pnl: 0.0,
            trades_today: 0,
            kill_switch: false,
            last_rejection: None,
            trading_day: None,
            kill_switch_reason: None,
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

    pub fn allow_entry_at(&mut self, timestamp_secs: i64, notional: f64) -> Result<(), String> {
        self.rollover(timestamp_secs);
        self.allow_entry(notional)
    }

    pub fn record_close(&mut self, pnl: f64) {
        if pnl.is_finite() {
            self.state.realized_pnl += pnl;
        }
        self.state.trades_today = self.state.trades_today.saturating_add(1);
        if self.state.realized_pnl <= -self.limits.max_daily_loss {
            self.state.kill_switch = true;
            self.state.kill_switch_reason = Some(KillSwitchReason::DailyLoss);
            self.state.last_rejection = Some("limite de perdida diaria alcanzado".into());
        }
    }

    pub fn record_close_at(&mut self, timestamp_secs: i64, pnl: f64) {
        self.rollover(timestamp_secs);
        self.record_close(pnl);
    }

    /// Starts a new Buenos Aires trading day without clearing a manual or
    /// operational halt. A daily-loss halt is safe to release only because its
    /// counters are reset at the same boundary.
    pub fn rollover(&mut self, timestamp_secs: i64) -> bool {
        let day = argentina_day(timestamp_secs);
        if self.state.trading_day == Some(day) {
            return false;
        }
        let first_observation = self.state.trading_day.is_none();
        self.state.trading_day = Some(day);
        if first_observation {
            return false;
        }
        self.state.realized_pnl = 0.0;
        self.state.trades_today = 0;
        if self.state.kill_switch_reason == Some(KillSwitchReason::DailyLoss) {
            self.state.kill_switch = false;
            self.state.kill_switch_reason = None;
            self.state.last_rejection = None;
        }
        true
    }

    pub fn engage_kill_switch(&mut self) {
        self.state.kill_switch = true;
        self.state.kill_switch_reason = Some(KillSwitchReason::Manual);
        self.state.last_rejection = Some("kill switch manual".into());
    }

    pub fn engage_operational_halt(&mut self, reason: impl Into<String>) {
        self.state.kill_switch = true;
        self.state.kill_switch_reason = Some(KillSwitchReason::Operational);
        self.state.last_rejection = Some(reason.into());
    }

    pub fn resume(&mut self) -> Result<(), String> {
        if self.state.realized_pnl <= -self.limits.max_daily_loss {
            return Err("no se puede reanudar: perdida diaria excedida".into());
        }
        if self.state.kill_switch_reason == Some(KillSwitchReason::Operational) {
            return Err("no se puede reanudar: bloqueo operativo sin reconciliar".into());
        }
        self.state.kill_switch = false;
        self.state.kill_switch_reason = None;
        self.state.last_rejection = None;
        Ok(())
    }

    pub fn clear_operational_halt(&mut self) {
        if self.state.kill_switch_reason == Some(KillSwitchReason::Operational) {
            self.state.kill_switch = false;
            self.state.kill_switch_reason = None;
            self.state.last_rejection = None;
        }
    }
}

fn argentina_day(timestamp_secs: i64) -> i64 {
    timestamp_secs
        .saturating_sub(3 * 60 * 60)
        .div_euclid(86_400)
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
        risk.record_close_at(1_000, -200.0);
        assert!(risk.state.kill_switch);
        assert!(risk.allow_entry(10.0).is_err());
    }

    #[test]
    fn a_new_trading_day_resets_daily_loss_but_not_manual_halts() {
        let mut daily = risk();
        daily.record_close_at(4 * 60 * 60, -200.0);
        assert!(daily.state.kill_switch);
        assert!(daily.rollover(86_400 + 4 * 60 * 60));
        assert_eq!(daily.state.realized_pnl, 0.0);
        assert_eq!(daily.state.trades_today, 0);
        assert!(!daily.state.kill_switch);

        let mut manual = risk();
        manual.allow_entry_at(4 * 60 * 60, 10.0).unwrap();
        manual.engage_kill_switch();
        assert!(manual.rollover(86_400 + 4 * 60 * 60));
        assert!(manual.state.kill_switch);
        assert_eq!(
            manual.state.kill_switch_reason,
            Some(KillSwitchReason::Manual)
        );
    }

    #[test]
    fn operational_halt_requires_explicit_reconciliation() {
        let mut risk = risk();
        risk.engage_operational_halt("orden desconocida");
        assert!(risk.resume().is_err());
        risk.clear_operational_halt();
        assert!(!risk.state.kill_switch);
    }
}
