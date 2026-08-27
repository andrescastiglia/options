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
    crate::time_utils::argentina_session_day(timestamp_secs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

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
        let mut manager = risk();
        assert_eq!(manager.allow_entry(manager.limits.max_notional), Ok(()));
        assert_eq!(
            manager.allow_entry(1_001.0),
            Err("nocional excede el limite".into())
        );
    }

    #[test]
    fn daily_loss_engages_kill_switch() {
        let mut profitable = risk();
        profitable.record_close_at(1_000, 50.0);
        assert!(!profitable.state.kill_switch);

        let mut losing = risk();
        losing.record_close_at(1_000, -200.0);
        assert!(losing.state.kill_switch);
        assert!(losing.allow_entry(10.0).is_err());
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

    #[test]
    fn every_entry_limit_and_resume_boundary_fails_closed() {
        for invalid in [f64::NAN, f64::INFINITY, 0.0, -1.0] {
            let mut manager = risk();
            assert_eq!(
                manager.allow_entry(invalid),
                Err("nocional invalido".into())
            );
        }

        let mut daily = risk();
        daily.state.realized_pnl = -daily.limits.max_daily_loss;
        assert_eq!(
            daily.allow_entry(10.0),
            Err("limite de perdida diaria alcanzado".into())
        );
        assert_eq!(
            daily.resume(),
            Err("no se puede reanudar: perdida diaria excedida".into())
        );

        let mut trades = risk();
        trades.state.trades_today = trades.limits.max_trades_per_day;
        assert_eq!(
            trades.allow_entry(10.0),
            Err("limite diario de operaciones alcanzado".into())
        );
        trades.state.trades_today = 0;
        assert_eq!(trades.allow_entry(10.0), Ok(()));
        assert_eq!(trades.state.last_rejection, None);

        let mut non_finite_close = risk();
        non_finite_close.record_close(f64::NAN);
        assert_eq!(non_finite_close.state.realized_pnl, 0.0);
        assert_eq!(non_finite_close.state.trades_today, 1);

        let mut manual = risk();
        manual.engage_kill_switch();
        assert_eq!(manual.resume(), Ok(()));
        assert!(!manual.state.kill_switch);
        manual.engage_kill_switch();
        manual.clear_operational_halt();
        assert!(manual.state.kill_switch);
        assert_eq!(
            manual.state.kill_switch_reason,
            Some(KillSwitchReason::Manual)
        );
    }

    proptest! {
        #[test]
        fn notional_boundary_is_closed_and_deterministic(
            limit in 0.01_f64..1_000_000.0,
            candidate in 0.01_f64..2_000_000.0,
        ) {
            let mut manager = RiskManager::new(RiskLimits {
                max_notional: limit,
                max_loss_per_trade: 1.0,
                max_daily_loss: 1_000_000.0,
                max_trades_per_day: u32::MAX,
            });
            prop_assert_eq!(manager.allow_entry(candidate).is_ok(), candidate <= limit);
        }

        #[test]
        fn finite_closed_trade_pnl_is_accumulated_exactly(values in prop::collection::vec(-10_000.0_f64..10_000.0, 0..100)) {
            let mut manager = RiskManager::new(RiskLimits {
                max_notional: 1.0,
                max_loss_per_trade: 1.0,
                max_daily_loss: 2_000_000.0,
                max_trades_per_day: u32::MAX,
            });
            for value in &values {
                manager.record_close(*value);
            }
            let expected = values.iter().sum::<f64>();
            let tolerance = 1e-9 * expected.abs().max(1.0);
            prop_assert!((manager.state.realized_pnl - expected).abs() <= tolerance);
            prop_assert_eq!(manager.state.trades_today, values.len() as u32);
        }
    }
}
