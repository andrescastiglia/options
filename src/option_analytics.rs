use serde::{Deserialize, Serialize};

use crate::market::OptionKind;

pub const AMERICAN_PRICER_VERSION: &str = "crr-american-v2";

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AmericanReferenceCase {
    pub name: &'static str,
    pub source: &'static str,
    pub kind: OptionKind,
    pub spot: f64,
    pub strike: f64,
    pub time_years: f64,
    pub risk_free_rate: f64,
    pub dividend_yield: f64,
    pub volatility: f64,
    pub expected_price: f64,
    pub tolerance: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PricerValidationCaseResult {
    pub name: String,
    pub source: String,
    pub expected_price: f64,
    pub actual_price: f64,
    pub absolute_error: f64,
    pub tolerance: f64,
    pub passed: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PricerValidationReport {
    pub pricer_version: String,
    pub binomial_steps: u32,
    pub maximum_absolute_error: f64,
    pub failures: usize,
    pub cases: Vec<PricerValidationCaseResult>,
}

/// Casos publicados de referencia tomados de los fixtures de QuantLib/Haug.
/// Los calls sin dividendos agregan una referencia analítica Black-Scholes,
/// válida por el teorema de no ejercicio anticipado.
pub fn published_reference_cases() -> Vec<AmericanReferenceCase> {
    vec![
        AmericanReferenceCase {
            name: "haug-bjerksund-call-with-dividend",
            source: "QuantLib americanoption.cpp / Haug 1998 p.27",
            kind: OptionKind::Call,
            spot: 42.0,
            strike: 40.0,
            time_years: 0.75,
            risk_free_rate: 0.04,
            dividend_yield: 0.08,
            volatility: 0.35,
            expected_price: 5.2704,
            tolerance: 0.06,
        },
        AmericanReferenceCase {
            name: "haug-bjerksund-put",
            source: "QuantLib americanoption.cpp / Haug 1998 VBA fixture",
            kind: OptionKind::Put,
            spot: 36.0,
            strike: 40.0,
            time_years: 1.0,
            risk_free_rate: 0.06,
            dividend_yield: 0.0,
            volatility: 0.20,
            expected_price: 4.4531,
            tolerance: 0.06,
        },
        AmericanReferenceCase {
            name: "haug-baw-put-atm-dividend",
            source: "QuantLib americanoption.cpp / Haug 1998 p.24",
            kind: OptionKind::Put,
            spot: 100.0,
            strike: 100.0,
            time_years: 0.50,
            risk_free_rate: 0.10,
            dividend_yield: 0.10,
            volatility: 0.25,
            expected_price: 6.8014,
            tolerance: 0.08,
        },
        AmericanReferenceCase {
            name: "non-dividend-call-atm",
            source: "Merton no-early-exercise theorem; Black-Scholes closed form",
            kind: OptionKind::Call,
            spot: 100.0,
            strike: 100.0,
            time_years: 1.0,
            risk_free_rate: 0.05,
            dividend_yield: 0.0,
            volatility: 0.20,
            expected_price: 10.450_584,
            tolerance: 0.03,
        },
        AmericanReferenceCase {
            name: "non-dividend-call-otm-short",
            source: "Merton no-early-exercise theorem; Black-Scholes closed form",
            kind: OptionKind::Call,
            spot: 90.0,
            strike: 100.0,
            time_years: 0.25,
            risk_free_rate: 0.03,
            dividend_yield: 0.0,
            volatility: 0.35,
            expected_price: 2.973_494,
            tolerance: 0.04,
        },
    ]
}

pub fn validate_pricer(steps: u32) -> PricerValidationReport {
    let cases = published_reference_cases()
        .into_iter()
        .map(|case| {
            let actual = american_binomial_price(
                case.kind,
                case.spot,
                case.strike,
                case.time_years,
                case.risk_free_rate,
                case.dividend_yield,
                case.volatility,
                steps,
            );
            let error = (actual - case.expected_price).abs();
            PricerValidationCaseResult {
                name: case.name.into(),
                source: case.source.into(),
                expected_price: case.expected_price,
                actual_price: actual,
                absolute_error: error,
                tolerance: case.tolerance,
                passed: error <= case.tolerance,
            }
        })
        .collect::<Vec<_>>();
    PricerValidationReport {
        pricer_version: AMERICAN_PRICER_VERSION.into(),
        binomial_steps: steps,
        maximum_absolute_error: cases
            .iter()
            .map(|case| case.absolute_error)
            .fold(0.0, f64::max),
        failures: cases.iter().filter(|case| !case.passed).count(),
        cases,
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct OptionAnalyticsParameters {
    pub risk_free_rate: f64,
    pub dividend_yield: f64,
    pub binomial_steps: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PointInTimeMarketInputs {
    pub observed_at_secs: i64,
    pub valid_for_secs: u64,
    pub risk_free_rate: f64,
    pub risk_free_source: String,
    pub dividend_yield: f64,
    pub dividend_source: String,
}

impl PointInTimeMarketInputs {
    pub fn parameters_at(
        &self,
        valuation_at_secs: i64,
        binomial_steps: u32,
    ) -> Option<OptionAnalyticsParameters> {
        (self.observed_at_secs <= valuation_at_secs
            && valuation_at_secs.saturating_sub(self.observed_at_secs)
                <= self.valid_for_secs as i64
            && self.risk_free_rate.is_finite()
            && self.dividend_yield.is_finite()
            && !self.risk_free_source.trim().is_empty()
            && !self.dividend_source.trim().is_empty())
        .then_some(OptionAnalyticsParameters {
            risk_free_rate: self.risk_free_rate,
            dividend_yield: self.dividend_yield,
            binomial_steps,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct OptionAnalytics {
    pub intrinsic_value: f64,
    pub extrinsic_value: f64,
    pub implied_volatility: Option<f64>,
    pub delta: Option<f64>,
    pub gamma: Option<f64>,
    pub theta_per_day: Option<f64>,
    pub vega_per_point: Option<f64>,
    pub rho_per_point: Option<f64>,
}

pub fn intrinsic_value(kind: OptionKind, spot: f64, strike: f64) -> f64 {
    match kind {
        OptionKind::Call => (spot - strike).max(0.0),
        OptionKind::Put => (strike - spot).max(0.0),
    }
}

pub fn analyze_american_option(
    kind: OptionKind,
    spot: f64,
    strike: f64,
    premium: f64,
    time_years: f64,
    parameters: OptionAnalyticsParameters,
) -> OptionAnalytics {
    let intrinsic = intrinsic_value(kind, spot, strike);
    let extrinsic = (premium - intrinsic).max(0.0);
    let implied_volatility = implied_volatility_american(
        kind,
        spot,
        strike,
        premium,
        time_years,
        parameters.risk_free_rate,
        parameters.dividend_yield,
        parameters.binomial_steps,
    );
    let Some(volatility) = implied_volatility else {
        return OptionAnalytics {
            intrinsic_value: intrinsic,
            extrinsic_value: extrinsic,
            implied_volatility: None,
            delta: None,
            gamma: None,
            theta_per_day: None,
            vega_per_point: None,
            rho_per_point: None,
        };
    };
    let price = |s: f64, t: f64, vol: f64, rate: f64| {
        american_binomial_price(
            kind,
            s,
            strike,
            t,
            rate,
            parameters.dividend_yield,
            vol,
            parameters.binomial_steps,
        )
    };
    // Un bump del 1% amortigua la oscilación par/impar propia del árbol CRR;
    // 0,1% hacía que gamma dependiera más de la malla que de la opción.
    let spot_step = (spot * 0.01).max(0.01);
    let up = price(
        spot + spot_step,
        time_years,
        volatility,
        parameters.risk_free_rate,
    );
    let center = price(spot, time_years, volatility, parameters.risk_free_rate);
    let down = price(
        (spot - spot_step).max(0.01),
        time_years,
        volatility,
        parameters.risk_free_rate,
    );
    let vol_step = 0.01;
    let rate_step = 0.01;
    OptionAnalytics {
        intrinsic_value: intrinsic,
        extrinsic_value: extrinsic,
        implied_volatility: Some(volatility),
        delta: Some((up - down) / (2.0 * spot_step)),
        gamma: Some((up - 2.0 * center + down) / spot_step.powi(2)),
        theta_per_day: Some(
            price(
                spot,
                (time_years - 1.0 / 365.0).max(1.0 / 3650.0),
                volatility,
                parameters.risk_free_rate,
            ) - center,
        ),
        vega_per_point: Some(
            (price(
                spot,
                time_years,
                volatility + vol_step,
                parameters.risk_free_rate,
            ) - price(
                spot,
                time_years,
                (volatility - vol_step).max(0.0001),
                parameters.risk_free_rate,
            )) / 2.0,
        ),
        rho_per_point: Some(
            (price(
                spot,
                time_years,
                volatility,
                parameters.risk_free_rate + rate_step,
            ) - price(
                spot,
                time_years,
                volatility,
                parameters.risk_free_rate - rate_step,
            )) / 2.0,
        ),
    }
}

#[allow(clippy::too_many_arguments)]
pub fn american_binomial_price(
    kind: OptionKind,
    spot: f64,
    strike: f64,
    time_years: f64,
    risk_free_rate: f64,
    dividend_yield: f64,
    volatility: f64,
    steps: u32,
) -> f64 {
    if !spot.is_finite()
        || !strike.is_finite()
        || spot <= 0.0
        || strike <= 0.0
        || time_years <= 0.0
        || volatility <= 0.0
        || !risk_free_rate.is_finite()
        || !dividend_yield.is_finite()
    {
        return f64::NAN;
    }
    let steps = steps.clamp(10, 2_000) as usize;
    let dt = time_years / steps as f64;
    let up = (volatility * dt.sqrt()).exp();
    let down = 1.0 / up;
    let growth = ((risk_free_rate - dividend_yield) * dt).exp();
    let probability = (growth - down) / (up - down);
    if !probability.is_finite() || !(0.0..=1.0).contains(&probability) {
        return f64::NAN;
    }
    let discount = (-risk_free_rate * dt).exp();
    let mut values = (0..=steps)
        .map(|up_moves| {
            let terminal_spot =
                spot * up.powi(up_moves as i32) * down.powi((steps - up_moves) as i32);
            intrinsic_value(kind, terminal_spot, strike)
        })
        .collect::<Vec<_>>();
    for step in (0..steps).rev() {
        for up_moves in 0..=step {
            let continuation = discount
                * (probability * values[up_moves + 1] + (1.0 - probability) * values[up_moves]);
            let node_spot = spot * up.powi(up_moves as i32) * down.powi((step - up_moves) as i32);
            values[up_moves] = continuation.max(intrinsic_value(kind, node_spot, strike));
        }
    }
    values[0]
}

#[allow(clippy::too_many_arguments)]
pub fn implied_volatility_american(
    kind: OptionKind,
    spot: f64,
    strike: f64,
    premium: f64,
    time_years: f64,
    risk_free_rate: f64,
    dividend_yield: f64,
    steps: u32,
) -> Option<f64> {
    if premium + 1e-8 < intrinsic_value(kind, spot, strike) || premium <= 0.0 || time_years <= 0.0 {
        return None;
    }
    let mut low = 0.0001;
    let mut high = 5.0;
    if premium
        > american_binomial_price(
            kind,
            spot,
            strike,
            time_years,
            risk_free_rate,
            dividend_yield,
            high,
            steps,
        ) + 1e-6
    {
        return None;
    }
    for _ in 0..80 {
        let middle = (low + high) / 2.0;
        let value = american_binomial_price(
            kind,
            spot,
            strike,
            time_years,
            risk_free_rate,
            dividend_yield,
            middle,
            steps,
        );
        if value < premium {
            low = middle;
        } else {
            high = middle;
        }
    }
    Some((low + high) / 2.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invalid_or_model_incompatible_inputs_are_explicitly_rejected() {
        assert!(
            american_binomial_price(OptionKind::Call, -1.0, 100.0, 1.0, 0.05, 0.0, 0.2, 100)
                .is_nan()
        );
        assert!(
            american_binomial_price(OptionKind::Call, 100.0, 100.0, 1.0, 5.0, 0.0, 0.01, 10)
                .is_nan()
        );
    }

    #[test]
    fn american_put_is_never_below_intrinsic_value() {
        let price = american_binomial_price(OptionKind::Put, 80.0, 100.0, 0.5, 0.05, 0.0, 0.3, 200);
        assert!(price >= 20.0);
    }

    #[test]
    fn implied_volatility_round_trips_binomial_price() {
        let premium =
            american_binomial_price(OptionKind::Call, 100.0, 100.0, 0.25, 0.05, 0.0, 0.4, 150);
        let iv = implied_volatility_american(
            OptionKind::Call,
            100.0,
            100.0,
            premium,
            0.25,
            0.05,
            0.0,
            150,
        )
        .unwrap();
        assert!((iv - 0.4).abs() < 0.01);
    }

    #[test]
    fn published_reference_cases_pass_at_production_resolution() {
        let report = validate_pricer(500);
        assert_eq!(report.failures, 0, "{report:#?}");
    }

    #[test]
    fn price_has_expected_economic_properties() {
        for kind in [OptionKind::Call, OptionKind::Put] {
            let base = american_binomial_price(kind, 100.0, 100.0, 0.5, 0.04, 0.01, 0.25, 500);
            let high_vol = american_binomial_price(kind, 100.0, 100.0, 0.5, 0.04, 0.01, 0.35, 500);
            assert!(base >= intrinsic_value(kind, 100.0, 100.0));
            assert!(high_vol >= base);
        }
        let low_spot =
            american_binomial_price(OptionKind::Call, 90.0, 100.0, 0.5, 0.04, 0.0, 0.25, 500);
        let high_spot =
            american_binomial_price(OptionKind::Call, 110.0, 100.0, 0.5, 0.04, 0.0, 0.25, 500);
        assert!(high_spot > low_spot);
    }

    #[test]
    fn production_steps_converge_and_expiry_is_stable() {
        let p250 = american_binomial_price(OptionKind::Put, 40.0, 40.0, 1.0, 0.06, 0.0, 0.2, 250);
        let p500 = american_binomial_price(OptionKind::Put, 40.0, 40.0, 1.0, 0.06, 0.0, 0.2, 500);
        let p1000 =
            american_binomial_price(OptionKind::Put, 40.0, 40.0, 1.0, 0.06, 0.0, 0.2, 1_000);
        assert!((p500 - p1000).abs() <= (p250 - p500).abs() + 0.01);
        let near_expiry = american_binomial_price(
            OptionKind::Call,
            101.0,
            100.0,
            1.0 / 365_000.0,
            0.04,
            0.0,
            0.2,
            500,
        );
        assert!(near_expiry.is_finite());
        assert!((near_expiry - 1.0).abs() < 0.05);
    }

    #[test]
    fn point_in_time_inputs_reject_future_and_expired_values() {
        let inputs = PointInTimeMarketInputs {
            observed_at_secs: 1_000,
            valid_for_secs: 100,
            risk_free_rate: 0.05,
            risk_free_source: "fixture".into(),
            dividend_yield: 0.01,
            dividend_source: "fixture".into(),
        };
        assert!(inputs.parameters_at(999, 500).is_none());
        assert!(inputs.parameters_at(1_050, 500).is_some());
        assert!(inputs.parameters_at(1_101, 500).is_none());
    }

    #[test]
    fn implied_volatility_and_greeks_are_stable_across_step_counts() {
        let premium =
            american_binomial_price(OptionKind::Put, 95.0, 100.0, 0.4, 0.05, 0.02, 0.3, 1_000);
        let evaluate = |steps| {
            analyze_american_option(
                OptionKind::Put,
                95.0,
                100.0,
                premium,
                0.4,
                OptionAnalyticsParameters {
                    risk_free_rate: 0.05,
                    dividend_yield: 0.02,
                    binomial_steps: steps,
                },
            )
        };
        let coarse = evaluate(250);
        let fine = evaluate(500);
        assert!(
            (coarse.implied_volatility.unwrap() - fine.implied_volatility.unwrap()).abs() < 0.01
        );
        assert!((coarse.delta.unwrap() - fine.delta.unwrap()).abs() < 0.03);
        assert!((coarse.gamma.unwrap() - fine.gamma.unwrap()).abs() < 0.03);
        assert!((coarse.theta_per_day.unwrap() - fine.theta_per_day.unwrap()).abs() < 0.05);
        assert!((coarse.vega_per_point.unwrap() - fine.vega_per_point.unwrap()).abs() < 0.05);
        assert!((coarse.rho_per_point.unwrap() - fine.rho_per_point.unwrap()).abs() < 0.05);
    }
}
