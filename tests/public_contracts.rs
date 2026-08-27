use options_trading::{
    pattern::Direction,
    persistence::{read_events, Journal, JournalEventKind},
    trading::{TradingEngine, TradingState},
};

fn temporary_path(label: &str) -> std::path::PathBuf {
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("reloj posterior al epoch")
        .as_nanos();
    std::env::temp_dir().join(format!(
        "options-integration-{label}-{}-{unique}.jsonl",
        std::process::id()
    ))
}

#[test]
fn public_direction_contract_accompanies_up_with_call_and_down_with_put() {
    let mut engine = TradingEngine::new();
    assert!(engine.consider_entry(Direction::Up));
    assert_eq!(engine.state, TradingState::SearchingCall);

    let mut engine = TradingEngine::new();
    assert!(engine.consider_entry(Direction::Down));
    assert_eq!(engine.state, TradingState::SearchingPut);

    let mut engine = TradingEngine::new();
    assert!(!engine.consider_entry(Direction::Neutral));
    assert_eq!(engine.state, TradingState::Idle);
}

#[test]
fn public_journal_contract_is_durable_contiguous_and_tamper_evident() {
    let path = temporary_path("journal");
    let mut journal = Journal::open(&path).expect("crear journal privado");
    journal
        .append(
            1,
            None,
            JournalEventKind::Recovery {
                message: "hecho esperado".into(),
            },
        )
        .expect("append 1");
    journal
        .append(2, None, JournalEventKind::Shutdown { clean: true })
        .expect("append 2");
    journal.sync().expect("sync durable");
    drop(journal);

    let events = read_events(&path).expect("cadena válida");
    assert_eq!(
        events
            .iter()
            .map(|event| event.sequence)
            .collect::<Vec<_>>(),
        [1, 2]
    );

    let changed = std::fs::read_to_string(&path)
        .expect("leer fixture")
        .replace("hecho esperado", "hecho alterado");
    std::fs::write(&path, changed).expect("simular corrupción externa");
    assert!(read_events(&path).is_err());
    std::fs::remove_file(path).expect("limpiar fixture");
}
