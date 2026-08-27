use options_trading::redaction::sanitize_operational_message;

#[test]
fn every_free_text_operational_sink_applies_the_central_redactor() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let app = std::fs::read_to_string(root.join("src/app.rs")).unwrap();
    let tui = std::fs::read_to_string(root.join("src/tui.rs")).unwrap();
    let main = std::fs::read_to_string(root.join("src/main.rs")).unwrap();
    let iol = std::fs::read_to_string(root.join("src/iol_client.rs")).unwrap();

    assert!(app.contains("let message = crate::redaction::sanitize_operational_message(&message);"));
    assert!(tui.contains(
        "let operational_status = crate::redaction::sanitize_operational_message(&app.status);"
    ));
    assert!(main
        .contains("options_trading::redaction::sanitize_operational_message(&error.to_string())"));
    assert!(!iol.contains("Orden IOL {broker_order_id}"));
    assert!(!iol.contains("orden {broker_order_id} después"));
}

#[test]
fn adversarial_broker_text_cannot_reach_a_human_sink_verbatim() {
    let hostile = "error account 2033590 access_token=secret\nsecond-line";
    let rendered = sanitize_operational_message(hostile);
    assert_eq!(rendered, "Detalle sensible ocultado");
    assert!(!rendered.contains("2033590"));
    assert!(!rendered.contains("secret"));
}
