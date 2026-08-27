use options_trading::{
    broker::{OrderRequest, OrderSide, OrderStatus},
    iol_client::{parse_order_execution, parse_realtime_message, IolRealtimeEvent},
    market::{MarketFrame, OptionKind, ReplayMarket},
    persistence::{validate_event_identity, JournalEvent},
};

const MARKET_FRAME_SEED: &str = include_str!("../fuzz/corpus/market_frame/valid-call-frame.json");
const VALID_JOURNAL_SEED: &str =
    include_str!("../fuzz/corpus/journal_event/valid-order-intent.json");
const INVALID_JOURNAL_SEED: &str =
    include_str!("../fuzz/corpus/journal_event/mismatched-order-intent.json");
const EXECUTED_ORDER_SEED: &str = include_str!("../fuzz/corpus/iol_protocol/executed-order.json");
const ORDER_ENVELOPE_SEED: &str = include_str!("../fuzz/corpus/iol_protocol/order-envelope.json");
const WEBSOCKET_ORDER_SEED: &str = include_str!("../fuzz/corpus/iol_protocol/websocket-order.json");

#[test]
fn market_seed_reaches_a_valid_directional_call_frame() {
    let frame: MarketFrame = serde_json::from_str(MARKET_FRAME_SEED).unwrap();
    assert_eq!(frame.options[0].kind, OptionKind::Call);
    assert_eq!(frame.underlying.ticker, "GGAL");
    ReplayMarket::new(vec![frame]).unwrap();
}

#[test]
fn journal_seeds_exercise_both_identity_outcomes() {
    let valid: JournalEvent = serde_json::from_str(VALID_JOURNAL_SEED).unwrap();
    validate_event_identity(&valid).unwrap();

    let invalid: JournalEvent = serde_json::from_str(INVALID_JOURNAL_SEED).unwrap();
    assert!(validate_event_identity(&invalid).is_err());
}

#[test]
fn iol_seeds_reach_terminal_envelope_and_correlated_websocket_contracts() {
    let baseline_request = OrderRequest {
        operation_id: "fuzz-operation".into(),
        symbol: "GGALC100".into(),
        quantity: 2,
        market_price: 2.0,
        limit_price: 2.1,
        side: OrderSide::Buy,
    };
    let direct: serde_json::Value = serde_json::from_str(EXECUTED_ORDER_SEED).unwrap();
    let execution = parse_order_execution(&baseline_request, direct).unwrap();
    assert_eq!(execution.status, OrderStatus::Executed);
    assert_eq!(execution.filled_quantity, 2);
    assert_eq!(execution.broker_order_id.as_deref(), Some("42"));

    let envelope: serde_json::Value = serde_json::from_str(ORDER_ENVELOPE_SEED).unwrap();
    let request: OrderRequest = serde_json::from_value(envelope["request"].clone()).unwrap();
    let execution = parse_order_execution(&request, envelope["response"].clone()).unwrap();
    assert_eq!(execution.operation_id, request.operation_id);
    assert_eq!(execution.status, OrderStatus::Executed);

    let event = parse_realtime_message(WEBSOCKET_ORDER_SEED).unwrap();
    let IolRealtimeEvent::Movement(movement) = event else {
        panic!("la semilla debe decodificar un movimiento correlacionable")
    };
    assert_eq!(movement.numero_operacion.as_deref(), Some("42"));
}
