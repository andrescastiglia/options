use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::errors::AppError;
use crate::trading::PositionKind;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum OrderSide {
    Buy,
    Sell,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OrderRequest {
    pub operation_id: String,
    pub symbol: String,
    pub quantity: u32,
    pub market_price: f64,
    pub limit_price: f64,
    pub side: OrderSide,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum OrderStatus {
    Pending,
    PartiallyExecuted,
    Executed,
    Rejected,
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OrderExecution {
    pub operation_id: String,
    pub status: OrderStatus,
    pub filled_quantity: u32,
    pub fill_price: Option<f64>,
    pub broker_order_id: Option<String>,
    pub message: Option<String>,
}

impl OrderExecution {
    pub fn remaining_quantity(&self, requested_quantity: u32) -> u32 {
        requested_quantity.saturating_sub(self.filled_quantity)
    }
}

/// Valida que el resultado informado por un broker sea autoconsistente y
/// corresponda exactamente con la intención original. Ningún campo ausente se
/// infiere desde la solicitud: ante ambigüedad, la orden debe reconciliarse.
pub fn validate_order_execution(
    request: &OrderRequest,
    execution: &OrderExecution,
) -> Result<(), String> {
    if execution.operation_id != request.operation_id {
        return Err("el operation_id de la respuesta no coincide con la solicitud".into());
    }
    if execution.filled_quantity > request.quantity {
        return Err(format!(
            "cantidad ejecutada {} supera la solicitada {}",
            execution.filled_quantity, request.quantity
        ));
    }
    if execution
        .fill_price
        .is_some_and(|price| !price.is_finite() || price <= 0.0)
    {
        return Err("precio de ejecución inválido".into());
    }
    if execution
        .broker_order_id
        .as_deref()
        .is_some_and(|id| id.trim().is_empty())
    {
        return Err("broker_order_id vacío".into());
    }

    let has_broker_id = execution.broker_order_id.is_some();
    let has_fill_price = execution.fill_price.is_some();
    match execution.status {
        OrderStatus::Pending => {
            if execution.filled_quantity != 0 || has_fill_price {
                return Err("una orden Pending no puede declarar fills".into());
            }
        }
        OrderStatus::PartiallyExecuted => {
            if execution.filled_quantity == 0
                || execution.filled_quantity >= request.quantity
                || !has_fill_price
                || !has_broker_id
            {
                return Err(
                    "un fill parcial exige cantidad intermedia, precio y broker_order_id".into(),
                );
            }
        }
        OrderStatus::Executed => {
            if execution.filled_quantity != request.quantity || !has_fill_price || !has_broker_id {
                return Err(
                    "una ejecución completa exige cantidad exacta, precio y broker_order_id".into(),
                );
            }
        }
        OrderStatus::Rejected => {
            if execution.filled_quantity != 0 || has_fill_price {
                return Err("una orden Rejected no puede declarar fills".into());
            }
        }
        OrderStatus::Cancelled => {
            if execution.filled_quantity >= request.quantity {
                return Err("una orden Cancelled no puede estar completamente ejecutada".into());
            }
            if !has_broker_id || (execution.filled_quantity > 0) != has_fill_price {
                return Err(
                    "una cancelación exige broker_order_id y precio si conserva un fill parcial"
                        .into(),
                );
            }
        }
    }
    Ok(())
}

/// Comprueba que una actualización no retroceda cantidad, identidad ni estado.
pub fn validate_order_transition(
    previous: &OrderExecution,
    next: &OrderExecution,
) -> Result<(), String> {
    if previous.operation_id != next.operation_id {
        return Err("la actualización cambió operation_id".into());
    }
    if let Some(previous_id) = previous.broker_order_id.as_deref() {
        if next.broker_order_id.as_deref() != Some(previous_id) {
            return Err("la actualización cambió broker_order_id".into());
        }
    }
    if next.filled_quantity < previous.filled_quantity {
        return Err("la cantidad ejecutada retrocedió".into());
    }
    let valid = match previous.status {
        OrderStatus::Pending => matches!(
            next.status,
            OrderStatus::Pending
                | OrderStatus::PartiallyExecuted
                | OrderStatus::Executed
                | OrderStatus::Rejected
                | OrderStatus::Cancelled
        ),
        OrderStatus::PartiallyExecuted => matches!(
            next.status,
            OrderStatus::PartiallyExecuted | OrderStatus::Executed | OrderStatus::Cancelled
        ),
        OrderStatus::Executed | OrderStatus::Rejected | OrderStatus::Cancelled => false,
    };
    if !valid {
        return Err(format!(
            "transición de estado inválida: {:?} -> {:?}",
            previous.status, next.status
        ));
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq)]
pub struct AccountPosition {
    pub symbol: String,
    pub quantity: u32,
    pub average_price: Option<f64>,
    pub kind: Option<PositionKind>,
    pub is_option: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct AccountOrder {
    pub broker_order_id: String,
    pub symbol: String,
    pub side: Option<OrderSide>,
    pub quantity: u32,
    pub kind: Option<PositionKind>,
    pub is_option: bool,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct AccountSnapshot {
    pub positions: Vec<AccountPosition>,
    pub pending_orders: Vec<AccountOrder>,
    pub funds: Option<AccountFunds>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct AccountFunds {
    pub account_number: String,
    pub currency: String,
    pub status: String,
    pub available: f64,
    pub immediate_available_to_trade: f64,
}

pub trait BrokerClient {
    fn submit_limit(&mut self, request: OrderRequest) -> Result<OrderExecution, AppError>;
    fn status(&self, operation_id: &str) -> Option<OrderExecution>;
}

#[derive(Debug)]
pub struct PaperBroker {
    orders: HashMap<String, OrderExecution>,
    requests: HashMap<String, OrderRequest>,
    slippage_bps: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ExecutionFrame {
    pub timestamp_millis: i64,
    pub bid: f64,
    pub ask: f64,
    pub bid_size: u32,
    pub ask_size: u32,
    /// Cantidad conservadora que se supone delante de nuestra orden.
    pub queue_ahead: u32,
    /// Confirma que el broker informó el terminal `Cancelled` antes de reemplazar.
    pub cancellation_acknowledged: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DynamicOrderPhase {
    Pending,
    PartiallyFilled,
    CancelRequested,
    Cancelled,
    ReplacementSubmitted,
    Executed,
    StoppedUncertainCancellation,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DynamicOrderTransition {
    pub timestamp_millis: i64,
    pub attempt: u32,
    pub phase: DynamicOrderPhase,
    pub filled_quantity: u32,
    pub remaining_quantity: u32,
    pub limit_price: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DynamicExecutionSimulation {
    pub execution: OrderExecution,
    pub transitions: Vec<DynamicOrderTransition>,
    pub attempts: u32,
    pub exposure_millis: i64,
    pub slippage_bps: Option<f64>,
    pub adverse_selection_bps: Option<f64>,
    pub duplicate_order_risk: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ExecutionStrategyMetrics {
    pub orders: usize,
    pub fully_filled: usize,
    pub partial_fills: usize,
    pub no_fills: usize,
    pub fill_rate: f64,
    pub average_slippage_bps: f64,
    pub average_adverse_selection_bps: f64,
    pub average_exposure_millis: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExecutionComparisonReport {
    pub fixed_limit: ExecutionStrategyMetrics,
    pub dynamic_limit: ExecutionStrategyMetrics,
    pub no_fill_control: ExecutionStrategyMetrics,
}

pub fn compare_execution_strategies(
    fixed_limit: &[DynamicExecutionSimulation],
    dynamic_limit: &[DynamicExecutionSimulation],
    no_fill_control: &[DynamicExecutionSimulation],
) -> ExecutionComparisonReport {
    ExecutionComparisonReport {
        fixed_limit: summarize_dynamic_executions(fixed_limit),
        dynamic_limit: summarize_dynamic_executions(dynamic_limit),
        no_fill_control: summarize_dynamic_executions(no_fill_control),
    }
}

pub fn summarize_dynamic_executions(
    simulations: &[DynamicExecutionSimulation],
) -> ExecutionStrategyMetrics {
    let fully_filled = simulations
        .iter()
        .filter(|item| item.execution.status == OrderStatus::Executed)
        .count();
    let partial_fills = simulations
        .iter()
        .filter(|item| {
            item.execution.status == OrderStatus::PartiallyExecuted
                || (item.execution.status == OrderStatus::Cancelled
                    && item.execution.filled_quantity > 0)
        })
        .count();
    let no_fills = simulations
        .iter()
        .filter(|item| item.execution.filled_quantity == 0)
        .count();
    ExecutionStrategyMetrics {
        orders: simulations.len(),
        fully_filled,
        partial_fills,
        no_fills,
        fill_rate: mean(simulations.iter().map(|item| {
            let requested = item
                .transitions
                .first()
                .map_or(0, |transition| transition.remaining_quantity);
            if requested == 0 {
                0.0
            } else {
                item.execution.filled_quantity as f64 / requested as f64
            }
        })),
        average_slippage_bps: mean(simulations.iter().filter_map(|item| item.slippage_bps)),
        average_adverse_selection_bps: mean(
            simulations
                .iter()
                .filter_map(|item| item.adverse_selection_bps),
        ),
        average_exposure_millis: mean(simulations.iter().map(|item| item.exposure_millis as f64)),
    }
}

fn mean(values: impl Iterator<Item = f64>) -> f64 {
    let values = values.collect::<Vec<_>>();
    if values.is_empty() {
        0.0
    } else {
        values.iter().sum::<f64>() / values.len() as f64
    }
}

impl PaperBroker {
    pub fn new(slippage_bps: f64) -> Self {
        Self {
            orders: HashMap::new(),
            requests: HashMap::new(),
            slippage_bps: slippage_bps.max(0.0),
        }
    }

    /// Simulación frame-aware: cada reintento consume una foto posterior, espera
    /// confirmación terminal de cancelación y nunca reemplaza cantidad ya llena.
    pub fn simulate_dynamic_limit(
        &mut self,
        request: OrderRequest,
        frames: &[ExecutionFrame],
        maximum_attempts: u32,
    ) -> Result<DynamicExecutionSimulation, AppError> {
        validate_order(&request)?;
        if frames.is_empty() {
            return Err(AppError::InvalidMarketData(
                "la simulación dinámica requiere al menos un frame".into(),
            ));
        }
        if frames.iter().any(|frame| {
            !frame.bid.is_finite()
                || !frame.ask.is_finite()
                || frame.bid <= 0.0
                || frame.ask < frame.bid
        }) || frames
            .windows(2)
            .any(|pair| pair[1].timestamp_millis <= pair[0].timestamp_millis)
        {
            return Err(AppError::InvalidMarketData(
                "frames de ejecución inválidos o fuera de orden".into(),
            ));
        }
        let attempts_cap = maximum_attempts.max(1).min(frames.len() as u32);
        let mut transitions = Vec::new();
        let mut total_filled = 0_u32;
        let mut fill_notional = 0.0;
        let first_timestamp = frames[0].timestamp_millis;
        let mut last_timestamp = first_timestamp;
        let mut uncertain = false;
        let mut attempts = 0_u32;
        for (index, frame) in frames.iter().take(attempts_cap as usize).enumerate() {
            attempts += 1;
            last_timestamp = frame.timestamp_millis;
            let attempt = index as u32 + 1;
            let progress = attempt as f64 / attempts_cap as f64;
            let passive = match request.side {
                OrderSide::Buy => frame.bid,
                OrderSide::Sell => frame.ask,
            };
            let interpolated = passive + (request.limit_price - passive) * progress;
            let limit = match request.side {
                OrderSide::Buy => interpolated.min(request.limit_price),
                OrderSide::Sell => interpolated.max(request.limit_price),
            };
            let remaining = request.quantity.saturating_sub(total_filled);
            transitions.push(DynamicOrderTransition {
                timestamp_millis: frame.timestamp_millis,
                attempt,
                phase: if attempt == 1 {
                    DynamicOrderPhase::Pending
                } else {
                    DynamicOrderPhase::ReplacementSubmitted
                },
                filled_quantity: total_filled,
                remaining_quantity: remaining,
                limit_price: limit,
            });
            let (crosses, available, price) = match request.side {
                OrderSide::Buy => (limit >= frame.ask, frame.ask_size, frame.ask),
                OrderSide::Sell => (limit <= frame.bid, frame.bid_size, frame.bid),
            };
            if crosses {
                let executable = available.saturating_sub(frame.queue_ahead).min(remaining);
                total_filled = total_filled.saturating_add(executable);
                fill_notional += executable as f64 * price;
                transitions.push(DynamicOrderTransition {
                    timestamp_millis: frame.timestamp_millis,
                    attempt,
                    phase: if total_filled == request.quantity {
                        DynamicOrderPhase::Executed
                    } else {
                        DynamicOrderPhase::PartiallyFilled
                    },
                    filled_quantity: total_filled,
                    remaining_quantity: request.quantity.saturating_sub(total_filled),
                    limit_price: limit,
                });
            }
            if total_filled == request.quantity {
                break;
            }
            transitions.push(DynamicOrderTransition {
                timestamp_millis: frame.timestamp_millis,
                attempt,
                phase: DynamicOrderPhase::CancelRequested,
                filled_quantity: total_filled,
                remaining_quantity: request.quantity.saturating_sub(total_filled),
                limit_price: limit,
            });
            if !frame.cancellation_acknowledged {
                uncertain = true;
                transitions.push(DynamicOrderTransition {
                    timestamp_millis: frame.timestamp_millis,
                    attempt,
                    phase: DynamicOrderPhase::StoppedUncertainCancellation,
                    filled_quantity: total_filled,
                    remaining_quantity: request.quantity.saturating_sub(total_filled),
                    limit_price: limit,
                });
                break;
            }
            transitions.push(DynamicOrderTransition {
                timestamp_millis: frame.timestamp_millis,
                attempt,
                phase: DynamicOrderPhase::Cancelled,
                filled_quantity: total_filled,
                remaining_quantity: request.quantity.saturating_sub(total_filled),
                limit_price: limit,
            });
        }
        let average_fill = (total_filled > 0).then(|| fill_notional / total_filled as f64);
        let status = if total_filled == request.quantity {
            OrderStatus::Executed
        } else if uncertain {
            if total_filled > 0 {
                OrderStatus::PartiallyExecuted
            } else {
                OrderStatus::Pending
            }
        } else {
            OrderStatus::Cancelled
        };
        let execution = OrderExecution {
            operation_id: request.operation_id.clone(),
            status,
            filled_quantity: total_filled,
            fill_price: average_fill,
            broker_order_id: Some(format!("paper-{}", request.operation_id)),
            message: uncertain.then(|| "cancelación no confirmada; reemplazo detenido".into()),
        };
        let operation_id = request.operation_id.clone();
        self.requests.insert(operation_id.clone(), request.clone());
        self.orders.insert(operation_id, execution.clone());
        let reference = request.market_price;
        let slippage_bps = average_fill.map(|price| match request.side {
            OrderSide::Buy => (price / reference - 1.0) * 10_000.0,
            OrderSide::Sell => (reference / price - 1.0) * 10_000.0,
        });
        let adverse_selection_bps = average_fill.and_then(|price| {
            let final_mid = frames
                .get(attempts.saturating_sub(1) as usize)
                .map(|frame| (frame.bid + frame.ask) / 2.0)?;
            Some(match request.side {
                OrderSide::Buy => (price / final_mid - 1.0) * 10_000.0,
                OrderSide::Sell => (final_mid / price - 1.0) * 10_000.0,
            })
        });
        Ok(DynamicExecutionSimulation {
            execution,
            transitions,
            attempts,
            exposure_millis: last_timestamp.saturating_sub(first_timestamp),
            slippage_bps,
            adverse_selection_bps,
            duplicate_order_risk: false,
        })
    }
}

impl Default for PaperBroker {
    fn default() -> Self {
        Self::new(0.0)
    }
}

impl BrokerClient for PaperBroker {
    fn submit_limit(&mut self, request: OrderRequest) -> Result<OrderExecution, AppError> {
        validate_order(&request)?;
        if let Some(execution) = self.orders.get(&request.operation_id) {
            if self.requests.get(&request.operation_id) != Some(&request) {
                return Err(AppError::OrderRejected(
                    "operation_id reutilizado con una intención diferente".into(),
                ));
            }
            return Ok(execution.clone());
        }
        let slippage = self.slippage_bps / 10_000.0;
        let fill_price = match request.side {
            OrderSide::Buy => request.market_price * (1.0 + slippage),
            OrderSide::Sell => request.market_price * (1.0 - slippage),
        };
        let crosses_limit = match request.side {
            OrderSide::Buy => fill_price <= request.limit_price,
            OrderSide::Sell => fill_price >= request.limit_price,
        };
        let execution = if crosses_limit {
            OrderExecution {
                operation_id: request.operation_id.clone(),
                status: OrderStatus::Executed,
                filled_quantity: request.quantity,
                fill_price: Some(fill_price),
                broker_order_id: Some(format!("paper-{}", request.operation_id)),
                message: None,
            }
        } else {
            OrderExecution {
                operation_id: request.operation_id.clone(),
                status: OrderStatus::Rejected,
                filled_quantity: 0,
                fill_price: None,
                broker_order_id: None,
                message: Some("precio con slippage fuera del limite".into()),
            }
        };
        let operation_id = request.operation_id.clone();
        self.requests.insert(operation_id.clone(), request);
        self.orders.insert(operation_id, execution.clone());
        Ok(execution)
    }

    fn status(&self, operation_id: &str) -> Option<OrderExecution> {
        self.orders.get(operation_id).cloned()
    }
}

fn validate_order(request: &OrderRequest) -> Result<(), AppError> {
    if request.operation_id.trim().is_empty()
        || request.symbol.trim().is_empty()
        || request.quantity == 0
        || !request.market_price.is_finite()
        || request.market_price <= 0.0
        || !request.limit_price.is_finite()
        || request.limit_price <= 0.0
    {
        return Err(AppError::OrderRejected(
            "parametros de orden invalidos".into(),
        ));
    }
    Ok(())
}

pub type FakeBroker = PaperBroker;

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn request(id: &str) -> OrderRequest {
        OrderRequest {
            operation_id: id.into(),
            symbol: "GAL-C-100".into(),
            quantity: 1,
            market_price: 2.0,
            limit_price: 2.01,
            side: OrderSide::Buy,
        }
    }

    #[test]
    fn paper_broker_is_idempotent() {
        let mut broker = PaperBroker::new(5.0);
        let first = broker.submit_limit(request("op-1")).unwrap();
        let second = broker.submit_limit(request("op-1")).unwrap();
        assert_eq!(first, second);
        assert_eq!(first.status, OrderStatus::Executed);
        assert_eq!(broker.status("op-1"), Some(first));
        assert_eq!(broker.status("unknown"), None);
    }

    #[test]
    fn paper_broker_rejects_conflicting_reuse_of_an_operation_id() {
        let mut broker = PaperBroker::new(0.0);
        let original = request("same-id");
        broker.submit_limit(original.clone()).unwrap();

        let mut changed = original;
        changed.quantity += 1;
        assert!(broker.submit_limit(changed).is_err());
        assert_eq!(broker.status("same-id").unwrap().filled_quantity, 1);
    }

    #[test]
    fn rejects_fill_outside_limit() {
        let mut broker = PaperBroker::new(100.0);
        let execution = broker.submit_limit(request("op-1")).unwrap();
        assert_eq!(execution.status, OrderStatus::Rejected);
    }

    #[test]
    fn fixed_paper_limits_use_exact_buy_and_sell_slippage_boundaries() {
        let mut buy_broker = PaperBroker::new(5.0);
        let mut buy = request("buy-boundary");
        buy.market_price = 100.0;
        buy.limit_price = 100.05;
        let executed_buy = buy_broker.submit_limit(buy.clone()).unwrap();
        assert_eq!(executed_buy.status, OrderStatus::Executed);
        assert_eq!(executed_buy.fill_price, Some(100.05));
        validate_order_execution(&buy, &executed_buy).unwrap();

        let mut rejected_buy = buy;
        rejected_buy.operation_id = "buy-below".into();
        rejected_buy.limit_price = 100.04;
        assert_eq!(
            buy_broker.submit_limit(rejected_buy).unwrap().status,
            OrderStatus::Rejected
        );

        let mut sell_broker = PaperBroker::new(5.0);
        let mut sell = request("sell-boundary");
        sell.side = OrderSide::Sell;
        sell.market_price = 100.0;
        sell.limit_price = 99.95;
        let executed_sell = sell_broker.submit_limit(sell.clone()).unwrap();
        assert_eq!(executed_sell.status, OrderStatus::Executed);
        assert_eq!(executed_sell.fill_price, Some(99.95));
        validate_order_execution(&sell, &executed_sell).unwrap();

        let mut rejected_sell = sell;
        rejected_sell.operation_id = "sell-above".into();
        rejected_sell.limit_price = 99.96;
        assert_eq!(
            sell_broker.submit_limit(rejected_sell).unwrap().status,
            OrderStatus::Rejected
        );
    }

    #[test]
    fn invalid_or_negative_paper_slippage_is_conservatively_zero() {
        for slippage in [f64::NAN, -1.0] {
            let mut broker = PaperBroker::new(slippage);
            let order = request(&format!("slippage-{slippage:?}"));
            let execution = broker.submit_limit(order.clone()).unwrap();
            assert_eq!(execution.fill_price, Some(order.market_price));
        }
    }

    #[test]
    fn order_transition_never_loses_a_confirmed_fill() {
        let previous = OrderExecution {
            operation_id: "op-1".into(),
            status: OrderStatus::PartiallyExecuted,
            filled_quantity: 1,
            fill_price: Some(2.0),
            broker_order_id: Some("42".into()),
            message: None,
        };
        let regressed = OrderExecution {
            operation_id: "op-1".into(),
            status: OrderStatus::Pending,
            filled_quantity: 0,
            fill_price: None,
            broker_order_id: Some("42".into()),
            message: None,
        };

        assert!(validate_order_transition(&previous, &regressed).is_err());
    }

    #[test]
    fn executed_order_requires_exact_quantity_price_and_broker_id() {
        let request = request("op-1");
        let valid = OrderExecution {
            operation_id: request.operation_id.clone(),
            status: OrderStatus::Executed,
            filled_quantity: request.quantity,
            fill_price: Some(2.0),
            broker_order_id: Some("42".into()),
            message: None,
        };
        assert!(validate_order_execution(&request, &valid).is_ok());

        let mut missing_id = valid;
        missing_id.broker_order_id = None;
        assert!(validate_order_execution(&request, &missing_id).is_err());
    }

    #[test]
    fn execution_envelope_rejects_each_invalid_identity_price_and_quantity() {
        let request = request("op-1");
        let valid = OrderExecution {
            operation_id: request.operation_id.clone(),
            status: OrderStatus::Executed,
            filled_quantity: request.quantity,
            fill_price: Some(2.0),
            broker_order_id: Some("42".into()),
            message: None,
        };

        let mut wrong_operation = valid.clone();
        wrong_operation.operation_id = "other".into();
        assert!(validate_order_execution(&request, &wrong_operation).is_err());

        let mut overfilled = valid.clone();
        overfilled.filled_quantity = request.quantity + 1;
        assert!(validate_order_execution(&request, &overfilled).is_err());

        for invalid_price in [f64::NAN, f64::INFINITY, 0.0, -1.0] {
            let mut invalid = valid.clone();
            invalid.fill_price = Some(invalid_price);
            assert!(validate_order_execution(&request, &invalid).is_err());
        }

        let mut blank_broker_id = valid;
        blank_broker_id.broker_order_id = Some(" \t".into());
        assert!(validate_order_execution(&request, &blank_broker_id).is_err());
    }

    #[test]
    fn each_order_status_enforces_its_fields_independently() {
        let mut request = request("matrix");
        request.quantity = 3;
        let execution =
            |status, filled_quantity, fill_price, broker_order_id: Option<&str>| OrderExecution {
                operation_id: request.operation_id.clone(),
                status,
                filled_quantity,
                fill_price,
                broker_order_id: broker_order_id.map(str::to_owned),
                message: None,
            };

        assert!(validate_order_execution(
            &request,
            &execution(OrderStatus::Pending, 0, None, None)
        )
        .is_ok());
        assert!(validate_order_execution(
            &request,
            &execution(OrderStatus::Pending, 1, None, None)
        )
        .is_err());
        assert!(validate_order_execution(
            &request,
            &execution(OrderStatus::Pending, 0, Some(2.0), None)
        )
        .is_err());

        let partial = execution(OrderStatus::PartiallyExecuted, 1, Some(2.0), Some("42"));
        assert!(validate_order_execution(&request, &partial).is_ok());
        for invalid in [
            execution(OrderStatus::PartiallyExecuted, 0, Some(2.0), Some("42")),
            execution(OrderStatus::PartiallyExecuted, 3, Some(2.0), Some("42")),
            execution(OrderStatus::PartiallyExecuted, 1, None, Some("42")),
            execution(OrderStatus::PartiallyExecuted, 1, Some(2.0), None),
        ] {
            assert!(validate_order_execution(&request, &invalid).is_err());
        }

        let executed = execution(OrderStatus::Executed, 3, Some(2.0), Some("42"));
        assert!(validate_order_execution(&request, &executed).is_ok());
        assert!(validate_order_execution(
            &request,
            &execution(OrderStatus::Executed, 2, Some(2.0), Some("42"))
        )
        .is_err());
        assert!(validate_order_execution(
            &request,
            &execution(OrderStatus::Executed, 3, None, Some("42"))
        )
        .is_err());

        assert!(validate_order_execution(
            &request,
            &execution(OrderStatus::Rejected, 0, None, None)
        )
        .is_ok());
        assert!(validate_order_execution(
            &request,
            &execution(OrderStatus::Rejected, 1, None, None)
        )
        .is_err());
        assert!(validate_order_execution(
            &request,
            &execution(OrderStatus::Rejected, 0, Some(2.0), None)
        )
        .is_err());

        assert!(validate_order_execution(
            &request,
            &execution(OrderStatus::Cancelled, 0, None, Some("42"))
        )
        .is_ok());
        assert!(validate_order_execution(
            &request,
            &execution(OrderStatus::Cancelled, 1, Some(2.0), Some("42"))
        )
        .is_ok());
        for invalid in [
            execution(OrderStatus::Cancelled, 3, Some(2.0), Some("42")),
            execution(OrderStatus::Cancelled, 0, None, None),
            execution(OrderStatus::Cancelled, 1, None, Some("42")),
            execution(OrderStatus::Cancelled, 0, Some(2.0), Some("42")),
        ] {
            assert!(validate_order_execution(&request, &invalid).is_err());
        }
    }

    #[test]
    fn order_transitions_preserve_identity_quantity_and_state_monotonicity() {
        let previous = OrderExecution {
            operation_id: "op-1".into(),
            status: OrderStatus::PartiallyExecuted,
            filled_quantity: 2,
            fill_price: Some(2.0),
            broker_order_id: Some("42".into()),
            message: None,
        };
        let valid_next = OrderExecution {
            filled_quantity: 2,
            ..previous.clone()
        };
        assert!(validate_order_transition(&previous, &valid_next).is_ok());

        let mut wrong_operation = valid_next.clone();
        wrong_operation.operation_id = "other".into();
        assert!(validate_order_transition(&previous, &wrong_operation).is_err());

        let mut changed_broker_id = valid_next.clone();
        changed_broker_id.broker_order_id = Some("43".into());
        assert!(validate_order_transition(&previous, &changed_broker_id).is_err());

        let mut lost_broker_id = valid_next.clone();
        lost_broker_id.broker_order_id = None;
        assert!(validate_order_transition(&previous, &lost_broker_id).is_err());

        let mut lower_fill = valid_next.clone();
        lower_fill.filled_quantity = 1;
        assert!(validate_order_transition(&previous, &lower_fill).is_err());

        let mut higher_fill = valid_next.clone();
        higher_fill.filled_quantity = 3;
        assert!(validate_order_transition(&previous, &higher_fill).is_ok());

        let mut invalid_state = valid_next;
        invalid_state.status = OrderStatus::Rejected;
        assert!(validate_order_transition(&previous, &invalid_state).is_err());

        for terminal in [
            OrderStatus::Executed,
            OrderStatus::Rejected,
            OrderStatus::Cancelled,
        ] {
            let terminal_previous = OrderExecution {
                status: terminal,
                ..previous.clone()
            };
            assert!(validate_order_transition(&terminal_previous, &previous).is_err());
        }
    }

    #[test]
    fn order_request_validation_rejects_each_field_independently() {
        let valid = request("valid");
        assert!(validate_order(&valid).is_ok());

        let mut invalid_requests = Vec::new();
        let mut invalid = valid.clone();
        invalid.operation_id = " \t".into();
        invalid_requests.push(invalid);
        let mut invalid = valid.clone();
        invalid.symbol = " \t".into();
        invalid_requests.push(invalid);
        let mut invalid = valid.clone();
        invalid.quantity = 0;
        invalid_requests.push(invalid);
        for market_price in [f64::NAN, 0.0, -1.0] {
            let mut invalid = valid.clone();
            invalid.market_price = market_price;
            invalid_requests.push(invalid);
        }
        for limit_price in [f64::NAN, 0.0, -1.0] {
            let mut invalid = valid.clone();
            invalid.limit_price = limit_price;
            invalid_requests.push(invalid);
        }

        for invalid in invalid_requests {
            assert!(validate_order(&invalid).is_err());
        }
    }

    fn measured_simulation(
        id: &str,
        status: OrderStatus,
        requested: u32,
        filled: u32,
        slippage_bps: Option<f64>,
        adverse_selection_bps: Option<f64>,
        exposure_millis: i64,
    ) -> DynamicExecutionSimulation {
        DynamicExecutionSimulation {
            execution: OrderExecution {
                operation_id: id.into(),
                status,
                filled_quantity: filled,
                fill_price: (filled > 0).then_some(2.0),
                broker_order_id: Some(format!("broker-{id}")),
                message: None,
            },
            transitions: vec![DynamicOrderTransition {
                timestamp_millis: 1_000,
                attempt: 1,
                phase: DynamicOrderPhase::Pending,
                filled_quantity: 0,
                remaining_quantity: requested,
                limit_price: 2.0,
            }],
            attempts: 1,
            exposure_millis,
            slippage_bps,
            adverse_selection_bps,
            duplicate_order_risk: false,
        }
    }

    #[test]
    fn execution_metrics_count_and_average_each_observation_exactly() {
        let simulations = [
            measured_simulation(
                "full",
                OrderStatus::Executed,
                4,
                4,
                Some(2.0),
                Some(3.0),
                100,
            ),
            measured_simulation(
                "partial",
                OrderStatus::PartiallyExecuted,
                2,
                1,
                None,
                Some(5.0),
                300,
            ),
            measured_simulation("none", OrderStatus::Cancelled, 3, 0, None, None, 500),
        ];
        let metrics = summarize_dynamic_executions(&simulations);
        assert_eq!(metrics.orders, 3);
        assert_eq!(metrics.fully_filled, 1);
        assert_eq!(metrics.partial_fills, 1);
        assert_eq!(metrics.no_fills, 1);
        assert!((metrics.fill_rate - 0.5).abs() < f64::EPSILON);
        assert!((metrics.average_slippage_bps - 2.0).abs() < f64::EPSILON);
        assert!((metrics.average_adverse_selection_bps - 4.0).abs() < f64::EPSILON);
        assert!((metrics.average_exposure_millis - 300.0).abs() < f64::EPSILON);

        assert_eq!(
            summarize_dynamic_executions(&[]),
            ExecutionStrategyMetrics::default()
        );
        assert_eq!(mean([2.0, 4.0].into_iter()), 3.0);
        assert_eq!(mean(std::iter::empty()), 0.0);
    }

    #[test]
    fn execution_comparison_keeps_each_strategy_in_its_declared_bucket() {
        let fixed = [measured_simulation(
            "fixed",
            OrderStatus::Executed,
            1,
            1,
            Some(1.0),
            None,
            100,
        )];
        let dynamic = [measured_simulation(
            "dynamic",
            OrderStatus::PartiallyExecuted,
            2,
            1,
            Some(2.0),
            None,
            200,
        )];
        let control = [measured_simulation(
            "control",
            OrderStatus::Cancelled,
            1,
            0,
            None,
            None,
            300,
        )];
        let report = compare_execution_strategies(&fixed, &dynamic, &control);
        assert_eq!(report.fixed_limit, summarize_dynamic_executions(&fixed));
        assert_eq!(report.dynamic_limit, summarize_dynamic_executions(&dynamic));
        assert_eq!(
            report.no_fill_control,
            summarize_dynamic_executions(&control)
        );
        assert_ne!(report.fixed_limit, report.dynamic_limit);
        assert_ne!(report.dynamic_limit, report.no_fill_control);
    }

    #[test]
    fn frame_aware_replacement_waits_for_cancel_and_preserves_partial_fill() {
        let mut broker = PaperBroker::default();
        let mut order = request("frames");
        order.quantity = 5;
        order.limit_price = 2.10;
        let frames = [
            ExecutionFrame {
                timestamp_millis: 1_000,
                bid: 1.98,
                ask: 2.02,
                bid_size: 10,
                ask_size: 3,
                queue_ahead: 1,
                cancellation_acknowledged: true,
            },
            ExecutionFrame {
                timestamp_millis: 2_000,
                bid: 2.00,
                ask: 2.04,
                bid_size: 10,
                ask_size: 10,
                queue_ahead: 0,
                cancellation_acknowledged: true,
            },
        ];
        let result = broker.simulate_dynamic_limit(order, &frames, 2).unwrap();
        assert_eq!(result.execution.status, OrderStatus::Executed);
        assert_eq!(result.execution.filled_quantity, 5);
        assert!(!result.duplicate_order_risk);
        assert!(result.transitions.windows(2).any(|pair| {
            pair[0].phase == DynamicOrderPhase::Cancelled
                && pair[1].phase == DynamicOrderPhase::ReplacementSubmitted
        }));
    }

    #[test]
    fn uncertain_cancellation_stops_replacement() {
        let mut broker = PaperBroker::default();
        let frames = [
            ExecutionFrame {
                timestamp_millis: 1_000,
                bid: 1.90,
                ask: 2.20,
                bid_size: 1,
                ask_size: 1,
                queue_ahead: 1,
                cancellation_acknowledged: false,
            },
            ExecutionFrame {
                timestamp_millis: 2_000,
                bid: 1.90,
                ask: 2.00,
                bid_size: 10,
                ask_size: 10,
                queue_ahead: 0,
                cancellation_acknowledged: true,
            },
        ];
        let result = broker
            .simulate_dynamic_limit(request("uncertain"), &frames, 2)
            .unwrap();
        assert_eq!(result.attempts, 1);
        assert_eq!(result.execution.status, OrderStatus::Pending);
        validate_order_execution(&request("uncertain"), &result.execution).unwrap();
        assert!(result
            .transitions
            .iter()
            .any(|item| item.phase == DynamicOrderPhase::StoppedUncertainCancellation));
    }

    #[test]
    fn acknowledged_partial_fill_finishes_cancelled_without_losing_the_fill() {
        let mut broker = PaperBroker::default();
        let mut order = request("partial-cancel");
        order.quantity = 5;
        order.limit_price = 2.10;
        let frames = [ExecutionFrame {
            timestamp_millis: 1_000,
            bid: 1.98,
            ask: 2.02,
            bid_size: 10,
            ask_size: 3,
            queue_ahead: 1,
            cancellation_acknowledged: true,
        }];
        let result = broker
            .simulate_dynamic_limit(order.clone(), &frames, 1)
            .unwrap();
        assert_eq!(result.execution.status, OrderStatus::Cancelled);
        assert_eq!(result.execution.filled_quantity, 2);
        assert_eq!(result.execution.fill_price, Some(2.02));
        validate_order_execution(&order, &result.execution).unwrap();
        assert_eq!(summarize_dynamic_executions(&[result]).partial_fills, 1);
    }

    #[test]
    fn moving_market_never_pushes_a_dynamic_buy_above_its_cap() {
        let mut broker = PaperBroker::default();
        let mut order = request("cap");
        order.limit_price = 2.01;
        let frames = [
            ExecutionFrame {
                timestamp_millis: 1_000,
                bid: 2.10,
                ask: 2.11,
                bid_size: 10,
                ask_size: 10,
                queue_ahead: 0,
                cancellation_acknowledged: true,
            },
            ExecutionFrame {
                timestamp_millis: 2_000,
                bid: 2.20,
                ask: 2.21,
                bid_size: 10,
                ask_size: 10,
                queue_ahead: 0,
                cancellation_acknowledged: true,
            },
        ];
        let result = broker
            .simulate_dynamic_limit(order.clone(), &frames, 2)
            .unwrap();
        assert!(result
            .transitions
            .iter()
            .all(|transition| transition.limit_price <= order.limit_price));
        assert_eq!(result.execution.filled_quantity, 0);
    }

    #[test]
    fn dynamic_execution_rejects_each_invalid_frame_contract() {
        let valid = ExecutionFrame {
            timestamp_millis: 1_000,
            bid: 1.98,
            ask: 2.02,
            bid_size: 10,
            ask_size: 10,
            queue_ahead: 0,
            cancellation_acknowledged: true,
        };
        let mut broker = PaperBroker::default();
        assert!(broker
            .simulate_dynamic_limit(request("empty-frames"), &[], 1)
            .is_err());

        let mut invalid_frames = Vec::new();
        let mut invalid = valid;
        invalid.bid = f64::NAN;
        invalid_frames.push(invalid);
        let mut invalid = valid;
        invalid.bid = 0.0;
        invalid_frames.push(invalid);
        let mut invalid = valid;
        invalid.ask = f64::NAN;
        invalid_frames.push(invalid);
        let mut invalid = valid;
        invalid.ask = 1.97;
        invalid_frames.push(invalid);
        for (index, invalid) in invalid_frames.into_iter().enumerate() {
            assert!(broker
                .simulate_dynamic_limit(request(&format!("bad-frame-{index}")), &[invalid], 1)
                .is_err());
        }

        let mut same_timestamp = valid;
        same_timestamp.timestamp_millis = valid.timestamp_millis;
        assert!(broker
            .simulate_dynamic_limit(request("same-time"), &[valid, same_timestamp], 2)
            .is_err());
        let mut earlier = valid;
        earlier.timestamp_millis = valid.timestamp_millis - 1;
        assert!(broker
            .simulate_dynamic_limit(request("backward-time"), &[valid, earlier], 2)
            .is_err());
    }

    #[test]
    fn dynamic_buy_interpolates_to_cap_and_reports_exact_execution_metrics() {
        let mut broker = PaperBroker::default();
        let mut order = request("interpolated-buy");
        order.limit_price = 2.10;
        let frames = [
            ExecutionFrame {
                timestamp_millis: 1_000,
                bid: 2.00,
                ask: 2.08,
                bid_size: 10,
                ask_size: 10,
                queue_ahead: 0,
                cancellation_acknowledged: true,
            },
            ExecutionFrame {
                timestamp_millis: 2_000,
                bid: 2.02,
                ask: 2.09,
                bid_size: 10,
                ask_size: 10,
                queue_ahead: 0,
                cancellation_acknowledged: true,
            },
        ];
        let result = broker
            .simulate_dynamic_limit(order.clone(), &frames, 2)
            .unwrap();
        assert_eq!(result.execution.status, OrderStatus::Executed);
        assert_eq!(result.execution.fill_price, Some(2.09));
        assert!(result
            .transitions
            .iter()
            .any(|transition| transition.phase == DynamicOrderPhase::Executed));
        assert_eq!(result.attempts, 2);
        assert_eq!(result.exposure_millis, 1_000);
        let submitted_limits = result
            .transitions
            .iter()
            .filter(|transition| {
                matches!(
                    transition.phase,
                    DynamicOrderPhase::Pending | DynamicOrderPhase::ReplacementSubmitted
                )
            })
            .map(|transition| transition.limit_price)
            .collect::<Vec<_>>();
        assert_eq!(submitted_limits, vec![2.05, 2.10]);
        assert!(submitted_limits
            .iter()
            .all(|limit| *limit <= order.limit_price));
        let expected_slippage = (2.09 / order.market_price - 1.0) * 10_000.0;
        let final_mid = (frames[1].bid + frames[1].ask) / 2.0;
        let expected_adverse = (2.09 / final_mid - 1.0) * 10_000.0;
        assert!((result.slippage_bps.unwrap() - expected_slippage).abs() < 1e-9);
        assert!((result.adverse_selection_bps.unwrap() - expected_adverse).abs() < 1e-9);
    }

    #[test]
    fn dynamic_sell_never_moves_below_its_limit_and_uses_the_executable_bid() {
        let mut broker = PaperBroker::default();
        let mut order = request("dynamic-sell");
        order.side = OrderSide::Sell;
        order.limit_price = 1.90;
        let frames = [ExecutionFrame {
            timestamp_millis: 1_000,
            bid: 1.92,
            ask: 1.96,
            bid_size: 5,
            ask_size: 5,
            queue_ahead: 0,
            cancellation_acknowledged: true,
        }];
        let result = broker
            .simulate_dynamic_limit(order.clone(), &frames, 1)
            .unwrap();
        assert_eq!(result.execution.status, OrderStatus::Executed);
        assert_eq!(result.execution.fill_price, Some(1.92));
        assert!(result
            .transitions
            .iter()
            .all(|transition| transition.limit_price >= order.limit_price));
        let expected_slippage = (order.market_price / 1.92 - 1.0) * 10_000.0;
        let expected_adverse = (((1.92 + 1.96) / 2.0) / 1.92 - 1.0) * 10_000.0;
        assert!((result.slippage_bps.unwrap() - expected_slippage).abs() < 1e-9);
        assert!((result.adverse_selection_bps.unwrap() - expected_adverse).abs() < 1e-9);
    }

    #[test]
    fn dynamic_attempt_cap_is_closed_and_queue_ahead_is_conservative() {
        let frame = ExecutionFrame {
            timestamp_millis: 1_000,
            bid: 1.98,
            ask: 2.02,
            bid_size: 10,
            ask_size: 3,
            queue_ahead: 3,
            cancellation_acknowledged: true,
        };
        let mut broker = PaperBroker::default();
        let mut order = request("attempt-zero");
        order.limit_price = 2.10;
        let result = broker.simulate_dynamic_limit(order, &[frame], 0).unwrap();
        assert_eq!(result.attempts, 1);
        assert_eq!(result.execution.status, OrderStatus::Cancelled);
        assert_eq!(result.execution.filled_quantity, 0);
        assert_eq!(result.execution.fill_price, None);
        assert_eq!(result.exposure_millis, 0);
        assert_eq!(result.slippage_bps, None);
        assert_eq!(result.adverse_selection_bps, None);
    }

    #[test]
    fn dynamic_execution_accepts_a_zero_spread_book_at_the_closed_boundary() {
        let mut broker = PaperBroker::default();
        let mut order = request("zero-spread");
        order.limit_price = 2.0;
        let frame = ExecutionFrame {
            timestamp_millis: 1_000,
            bid: 2.0,
            ask: 2.0,
            bid_size: 1,
            ask_size: 1,
            queue_ahead: 0,
            cancellation_acknowledged: true,
        };
        let result = broker.simulate_dynamic_limit(order, &[frame], 1).unwrap();
        assert_eq!(result.execution.status, OrderStatus::Executed);
        assert_eq!(result.execution.fill_price, Some(2.0));
    }

    #[test]
    fn uncertain_cancellation_with_a_partial_fill_remains_non_terminal() {
        let mut broker = PaperBroker::default();
        let mut order = request("uncertain-partial");
        order.quantity = 5;
        order.limit_price = 2.10;
        let frame = ExecutionFrame {
            timestamp_millis: 1_000,
            bid: 1.98,
            ask: 2.02,
            bid_size: 10,
            ask_size: 3,
            queue_ahead: 1,
            cancellation_acknowledged: false,
        };
        let result = broker
            .simulate_dynamic_limit(order.clone(), &[frame], 1)
            .unwrap();
        assert_eq!(result.execution.status, OrderStatus::PartiallyExecuted);
        assert_eq!(result.execution.filled_quantity, 2);
        assert_eq!(result.execution.fill_price, Some(2.02));
        assert!(result
            .transitions
            .iter()
            .any(|transition| transition.phase == DynamicOrderPhase::StoppedUncertainCancellation));
        validate_order_execution(&order, &result.execution).unwrap();
    }

    proptest! {
        #[test]
        fn remaining_quantity_is_saturating_and_conservative(
            requested in 0_u32..100_000,
            filled in 0_u32..200_000,
        ) {
            let execution = OrderExecution {
                operation_id: "property".into(),
                status: OrderStatus::Pending,
                filled_quantity: filled,
                fill_price: None,
                broker_order_id: None,
                message: None,
            };
            prop_assert_eq!(
                execution.remaining_quantity(requested),
                requested.saturating_sub(filled)
            );
        }

        #[test]
        fn execution_matrix_accepts_only_the_predeclared_quantity_relation(
            requested in 1_u32..1_000,
            filled in 0_u32..1_100,
            state in 0_u8..5,
        ) {
            let status = match state {
                0 => OrderStatus::Pending,
                1 => OrderStatus::PartiallyExecuted,
                2 => OrderStatus::Executed,
                3 => OrderStatus::Rejected,
                _ => OrderStatus::Cancelled,
            };
            let has_fill = filled > 0;
            let execution = OrderExecution {
                operation_id: "property".into(),
                status,
                filled_quantity: filled,
                fill_price: has_fill.then_some(2.0),
                broker_order_id: (!matches!(status, OrderStatus::Pending | OrderStatus::Rejected))
                    .then(|| "broker-1".into()),
                message: None,
            };
            let request = OrderRequest {
                operation_id: "property".into(),
                symbol: "GGALC100".into(),
                quantity: requested,
                market_price: 2.0,
                limit_price: 2.01,
                side: OrderSide::Buy,
            };
            let expected_valid = match status {
                OrderStatus::Pending | OrderStatus::Rejected => filled == 0,
                OrderStatus::PartiallyExecuted => filled > 0 && filled < requested,
                OrderStatus::Executed => filled == requested,
                OrderStatus::Cancelled => filled < requested,
            };
            prop_assert_eq!(validate_order_execution(&request, &execution).is_ok(), expected_valid);
        }
    }
}
