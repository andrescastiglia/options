use std::collections::HashMap;

use crate::errors::AppError;

#[derive(Debug, Clone, PartialEq)]
pub struct OrderRequest {
    pub operation_id: String,
    pub symbol: String,
    pub quantity: u32,
    pub limit_price: f64,
    pub is_buy: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrderStatus {
    Pending,
    Executed,
    Rejected,
}

pub trait BrokerClient {
    fn submit_limit(&mut self, request: OrderRequest) -> Result<OrderStatus, AppError>;
    fn status(&self, operation_id: &str) -> Option<OrderStatus>;
}

#[derive(Debug, Default)]
pub struct FakeBroker {
    orders: HashMap<String, OrderStatus>,
}

impl BrokerClient for FakeBroker {
    fn submit_limit(&mut self, request: OrderRequest) -> Result<OrderStatus, AppError> {
        if request.operation_id.trim().is_empty()
            || request.symbol.trim().is_empty()
            || request.quantity == 0
            || !request.limit_price.is_finite()
            || request.limit_price <= 0.0
        {
            return Err(AppError::OrderRejected(
                "parametros de orden invalidos".into(),
            ));
        }
        if let Some(status) = self.orders.get(&request.operation_id) {
            return Ok(*status);
        }
        self.orders
            .insert(request.operation_id, OrderStatus::Executed);
        Ok(OrderStatus::Executed)
    }

    fn status(&self, operation_id: &str) -> Option<OrderStatus> {
        self.orders.get(operation_id).copied()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(id: &str) -> OrderRequest {
        OrderRequest {
            operation_id: id.into(),
            symbol: "GALIO".into(),
            quantity: 1,
            limit_price: 2.0,
            is_buy: true,
        }
    }

    #[test]
    fn fake_broker_is_idempotent() {
        let mut broker = FakeBroker::default();
        assert_eq!(
            broker.submit_limit(request("op-1")).unwrap(),
            OrderStatus::Executed
        );
        assert_eq!(
            broker.submit_limit(request("op-1")).unwrap(),
            OrderStatus::Executed
        );
        assert_eq!(broker.status("op-1"), Some(OrderStatus::Executed));
    }
}
