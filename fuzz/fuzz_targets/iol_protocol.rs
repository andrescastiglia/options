#![no_main]

use libfuzzer_sys::fuzz_target;
use options_trading::{
    broker::{OrderRequest, OrderSide},
    iol_client::{parse_order_execution, parse_realtime_message},
};

fuzz_target!(|data: &[u8]| {
    if let Ok(value) = serde_json::from_slice::<serde_json::Value>(data) {
        let baseline_request = OrderRequest {
            operation_id: "fuzz-operation".into(),
            symbol: "GAL-C-100".into(),
            quantity: 2,
            market_price: 2.0,
            limit_price: 2.1,
            side: OrderSide::Buy,
        };
        let _ = parse_order_execution(&baseline_request, value.clone());
        if let (Some(request), Some(response)) = (value.get("request"), value.get("response")) {
            if let Ok(request) = serde_json::from_value::<OrderRequest>(request.clone()) {
                let _ = parse_order_execution(&request, response.clone());
            }
        }
    }
    if let Ok(text) = std::str::from_utf8(data) {
        let _ = parse_realtime_message(text);
    }
});
