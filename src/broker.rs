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
}

pub trait BrokerClient {
    fn submit_limit(&mut self, request: OrderRequest) -> Result<OrderExecution, AppError>;
    fn status(&self, operation_id: &str) -> Option<OrderExecution>;
}

#[derive(Debug)]
pub struct PaperBroker {
    orders: HashMap<String, OrderExecution>,
    slippage_bps: f64,
}

impl PaperBroker {
    pub fn new(slippage_bps: f64) -> Self {
        Self {
            orders: HashMap::new(),
            slippage_bps: slippage_bps.max(0.0),
        }
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
        self.orders.insert(request.operation_id, execution.clone());
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
    }

    #[test]
    fn rejects_fill_outside_limit() {
        let mut broker = PaperBroker::new(100.0);
        let execution = broker.submit_limit(request("op-1")).unwrap();
        assert_eq!(execution.status, OrderStatus::Rejected);
    }
}
