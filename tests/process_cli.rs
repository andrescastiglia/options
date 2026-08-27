use std::{
    io::Write,
    net::TcpListener,
    process::{Command, Stdio},
    time::Duration,
};

use base64::{engine::general_purpose::STANDARD, Engine as _};
use fs2::FileExt;
use options_trading::market::{
    ContractMetadataSource, ExerciseStyle, MarketFrame, OptionKind, OptionQuote,
    QuoteTimestampSource, UnderlyingQuote,
};
use options_trading::{
    broker::{OrderExecution, OrderRequest, OrderSide, OrderStatus},
    config::LIVE_CONFIRMATION,
    datasets::{
        dataset_sha256, DatasetManifest, DatasetPartition, DatasetRole, SignedDatasetManifest,
        DATASET_MANIFEST_SCHEMA_VERSION,
    },
    learning::{AuthorizationRequest, ExecutionAuthorization, AUTHORIZATION_SCHEMA_VERSION},
    persistence::{
        record_order_accepted, record_order_intent, record_order_terminal, Journal,
        JournalEventKind,
    },
    release_readiness::{
        digest_hex, CoverageEvidence, CoverageMetrics, MutationEvidence, QualityMetrics,
        ReleaseReadiness, RELEASE_READINESS_SCHEMA_VERSION, REQUIRED_CRITICAL_SCOPES,
    },
    secrets::{
        verify_authorization_payload, verify_release_readiness_payload_from, MASTER_KEY_ENV,
    },
    secure_fs::write_new,
};

fn binary() -> Command {
    Command::new(env!("CARGO_BIN_EXE_options-trading"))
}

#[test]
fn removed_password_argument_fails_without_echoing_the_secret() {
    let secret = "never-print-this-password";
    let output = binary()
        .args(["-e", secret])
        .output()
        .expect("ejecutar binario");

    assert!(!output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("-e fue retirado"));
    assert!(!stdout.contains(secret));
    assert!(!stderr.contains(secret));
}

#[test]
fn unknown_utility_command_fails_before_loading_broker_credentials() {
    let output = binary()
        .arg("--comando-inexistente")
        .env_remove("IOL_USERNAME")
        .env_remove("IOL_PASSWORD")
        .output()
        .expect("ejecutar binario");

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("parámetro desconocido"));
    assert!(!stderr.contains("IOL_USERNAME"));
}

#[test]
fn a_second_process_cannot_share_the_same_runtime_state() {
    let directory = temporary_directory("exclusive-process-lock");
    let replay = directory.join("replay.jsonl");
    let frames = (1..=10)
        .map(replay_frame)
        .map(|frame| serde_json::to_string(&frame).unwrap())
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(&replay, format!("{frames}\n")).unwrap();
    let data_dir = directory.join("data");

    let configure = |command: &mut Command| {
        command
            .current_dir(&directory)
            .env("MODE", "readonly")
            .env("REPLAY_PATH", &replay)
            .env("DATA_DIR", &data_dir)
            .env("TUI_ENABLED", "false")
            .env("CHECK_INTERVAL_SECS", "1")
            .env_remove("IOL_USERNAME")
            .env_remove("IOL_PASSWORD");
    };

    let mut first = binary();
    configure(&mut first);
    let mut first = first
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("iniciar primera instancia");
    let lock_path = data_dir.join("storage.lock");
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        if let Ok(lock_file) = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&lock_path)
        {
            if lock_file.try_lock_exclusive().is_err() {
                break;
            }
            FileExt::unlock(&lock_file).unwrap();
        }
        assert!(
            std::time::Instant::now() < deadline,
            "la primera instancia no tomó el lock raíz dentro del plazo"
        );
        std::thread::sleep(Duration::from_millis(25));
    }
    assert!(first.try_wait().unwrap().is_none());

    let mut second = binary();
    configure(&mut second);
    let output = second.output().expect("iniciar segunda instancia");
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("instancia activa"));

    first.kill().expect("terminar proceso de prueba");
    first.wait().expect("recolectar proceso de prueba");

    std::fs::write(
        &replay,
        format!("{}\n", serde_json::to_string(&replay_frame(1)).unwrap()),
    )
    .unwrap();
    let mut restarted = binary();
    configure(&mut restarted);
    let restarted = restarted.output().expect("reiniciar después del corte");
    assert!(
        restarted.status.success(),
        "el lock sobrevivió indebidamente al proceso: {}",
        String::from_utf8_lossy(&restarted.stderr)
    );
    std::fs::remove_dir_all(&directory).expect("limpiar directorio temporal exacto");
}

#[test]
fn unsafe_runtime_configuration_fails_before_opening_data_or_network() {
    let cases = [
        ("TICKER", "../GGAL", "TICKER"),
        ("IOL_BASE_URL", "http://broker.example", "IOL_BASE_URL"),
        ("IOL_ORDER_PATH", "/api/v2/otra-ruta", "IOL_ORDER_PATH"),
        ("MAX_CONCURRENT_REQUESTS", "0", "MAX_CONCURRENT_REQUESTS"),
        (
            "ENTRY_DELAY_AFTER_OPEN_MINS",
            "391",
            "ENTRY_DELAY_AFTER_OPEN_MINS",
        ),
    ];
    for (name, value, expected_error) in cases {
        let output = binary()
            .env("REPLAY_PATH", "/archivo/que/no-debe-abrirse.jsonl")
            .env(name, value)
            .env_remove("IOL_USERNAME")
            .env_remove("IOL_PASSWORD")
            .output()
            .expect("ejecutar configuración inválida");
        assert!(!output.status.success(), "{name}={value} fue aceptado");
        assert!(
            String::from_utf8_lossy(&output.stderr).contains(expected_error),
            "error inesperado para {name}={value}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

#[test]
fn every_incomplete_live_gate_combination_exits_before_broker_network() {
    let directory = temporary_directory("live-gate-process-matrix");
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let broker_url = format!("https://{}", listener.local_addr().unwrap());
    let gate_names = [
        "LIVE_TRADING_CONFIRMATION",
        "IOL_ORDER_PATH",
        "LIVE_AUTHORIZATION_PATH",
        "LIVE_READINESS_PATH",
        "MARKET_SESSIONS_PATH",
        "TIME_REFERENCE_URL",
        "OPTIONS_MASTER_KEY_PATH",
    ];

    for mask in 0_u8..0b111_1111 {
        let case_dir = directory.join(format!("case-{mask:07b}"));
        let mut command = binary();
        command
            .current_dir(&directory)
            .env("MODE", "live")
            .env("TUI_ENABLED", "false")
            .env("DATA_DIR", &case_dir)
            .env("IOL_BASE_URL", &broker_url)
            .env("IOL_USERNAME", "not-used-before-validation")
            .env("IOL_PASSWORD", "v3:not-used-before-validation");
        for name in gate_names {
            command.env_remove(name);
        }
        if mask & 0b000_0001 != 0 {
            command.env("LIVE_TRADING_CONFIRMATION", LIVE_CONFIRMATION);
        }
        if mask & 0b000_0010 != 0 {
            command.env("IOL_ORDER_PATH", "/api/v2/operar");
        }
        if mask & 0b000_0100 != 0 {
            command.env(
                "LIVE_AUTHORIZATION_PATH",
                directory.join("authorization.json"),
            );
        }
        if mask & 0b000_1000 != 0 {
            command.env("LIVE_READINESS_PATH", directory.join("readiness.json"));
        }
        if mask & 0b001_0000 != 0 {
            command.env("MARKET_SESSIONS_PATH", directory.join("sessions.json"));
        }
        if mask & 0b010_0000 != 0 {
            command.env("TIME_REFERENCE_URL", "https://clock.example.invalid/date");
        }
        if mask & 0b100_0000 != 0 {
            command.env("OPTIONS_MASTER_KEY_PATH", directory.join("master.key"));
        }

        let output = command.output().expect("ejecutar matriz live");
        assert!(
            !output.status.success(),
            "la combinación incompleta {mask:07b} inició el runtime"
        );
    }

    assert!(
        matches!(listener.accept(), Err(error) if error.kind() == std::io::ErrorKind::WouldBlock),
        "una configuración live incompleta abrió una conexión al broker"
    );
    std::fs::remove_dir_all(directory).expect("limpiar directorio temporal exacto");
}

#[cfg(unix)]
#[test]
fn master_key_utility_creates_once_with_private_permissions() {
    use std::os::unix::fs::PermissionsExt;

    let directory = temporary_directory("master-key");
    let key_path = directory.join("operator.key");
    let first = binary()
        .args(["--init-master-key", key_path.to_str().unwrap()])
        .output()
        .expect("crear clave");
    assert!(first.status.success());
    let metadata = std::fs::metadata(&key_path).unwrap();
    assert_eq!(metadata.permissions().mode() & 0o777, 0o600);
    let original = std::fs::read(&key_path).unwrap();
    assert_eq!(
        STANDARD
            .decode(String::from_utf8_lossy(&original).trim())
            .unwrap()
            .len(),
        32
    );

    let second = binary()
        .args(["--init-master-key", key_path.to_str().unwrap()])
        .output()
        .expect("rechazar sobrescritura");
    assert!(!second.status.success());
    assert_eq!(std::fs::read(&key_path).unwrap(), original);

    std::fs::remove_dir_all(&directory).expect("limpiar directorio temporal exacto");
}

#[cfg(unix)]
#[test]
fn live_authorization_utility_signs_an_ephemeral_non_overwriting_grant() {
    use std::os::unix::fs::PermissionsExt;

    let directory = temporary_directory("live-authorization");
    let key_path = directory.join("operator.key");
    assert!(binary()
        .args(["--init-master-key", key_path.to_str().unwrap()])
        .output()
        .unwrap()
        .status
        .success());
    let request_path = directory.join("request.json");
    let authorization_path = directory.join("authorization.json");
    let request = AuthorizationRequest {
        schema_version: AUTHORIZATION_SCHEMA_VERSION,
        account_number: "2033590".into(),
        epoch: 7,
        strategy_fingerprint: "c".repeat(64),
        build_hash: options_trading::build_identity::executable_build_hash(),
        readiness_sha256: "b".repeat(64),
        report_sha256: "a".repeat(64),
        canary_max_position_size: 1,
        canary_max_investment_amount: 10_000.0,
        canary_max_loss_per_trade: 500.0,
        canary_max_daily_loss: 1_000.0,
        generated_at_secs: 1_000,
    };
    write_new(&request_path, &serde_json::to_vec(&request).unwrap()).unwrap();

    let mut command = binary();
    let mut child = command
        .args([
            "--authorize-live",
            request_path.to_str().unwrap(),
            authorization_path.to_str().unwrap(),
        ])
        .env(MASTER_KEY_ENV, &key_path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("iniciar autorización");
    writeln!(child.stdin.take().unwrap(), "{LIVE_CONFIRMATION}").unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(!stdout.contains("2033590"));
    assert!(stdout.contains("••••3590"));

    let authorization: ExecutionAuthorization =
        serde_json::from_slice(&std::fs::read(&authorization_path).unwrap()).unwrap();
    assert_eq!(authorization.request, request);
    assert!(authorization.expires_at_secs > authorization.issued_at_secs);
    assert!(authorization.expires_at_secs - authorization.issued_at_secs <= 15 * 60);
    std::env::set_var(MASTER_KEY_ENV, &key_path);
    assert!(verify_authorization_payload(
        &authorization.signing_payload().unwrap(),
        &authorization.signature
    )
    .unwrap());
    std::env::remove_var(MASTER_KEY_ENV);
    assert_eq!(
        std::fs::metadata(&authorization_path)
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );

    let original = std::fs::read(&authorization_path).unwrap();
    let mut second = binary();
    let mut second = second
        .args([
            "--authorize-live",
            request_path.to_str().unwrap(),
            authorization_path.to_str().unwrap(),
        ])
        .env(MASTER_KEY_ENV, &key_path)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    writeln!(second.stdin.take().unwrap(), "{LIVE_CONFIRMATION}").unwrap();
    assert!(!second.wait().unwrap().success());
    assert_eq!(std::fs::read(&authorization_path).unwrap(), original);

    std::fs::remove_dir_all(&directory).expect("limpiar directorio temporal exacto");
}

#[cfg(unix)]
#[test]
fn live_authorization_rejects_a_foreign_build_before_confirmation_or_output() {
    let directory = temporary_directory("foreign-live-authorization");
    let key_path = directory.join("operator.key");
    assert!(binary()
        .args(["--init-master-key", key_path.to_str().unwrap()])
        .output()
        .unwrap()
        .status
        .success());
    let request_path = directory.join("request.json");
    let authorization_path = directory.join("authorization.json");
    let request = AuthorizationRequest {
        schema_version: AUTHORIZATION_SCHEMA_VERSION,
        account_number: "2033590".into(),
        epoch: 7,
        strategy_fingerprint: "c".repeat(64),
        build_hash: "d".repeat(64),
        readiness_sha256: "b".repeat(64),
        report_sha256: "a".repeat(64),
        canary_max_position_size: 1,
        canary_max_investment_amount: 10_000.0,
        canary_max_loss_per_trade: 500.0,
        canary_max_daily_loss: 1_000.0,
        generated_at_secs: 1_000,
    };
    write_new(&request_path, &serde_json::to_vec(&request).unwrap()).unwrap();

    let output = binary()
        .args([
            "--authorize-live",
            request_path.to_str().unwrap(),
            authorization_path.to_str().unwrap(),
        ])
        .env(MASTER_KEY_ENV, &key_path)
        .output()
        .unwrap();

    assert!(!output.status.success());
    assert!(!authorization_path.exists());
    assert!(String::from_utf8_lossy(&output.stderr).contains("otro build"));
    assert!(!String::from_utf8_lossy(&output.stdout).contains("2033590"));
    std::fs::remove_dir_all(&directory).expect("limpiar directorio temporal exacto");
}

#[cfg(unix)]
#[test]
fn readiness_utility_binds_supplied_report_bytes_and_rejects_substitution() {
    let directory = temporary_directory("release-readiness");
    let key_path = directory.join("operator.key");
    assert!(binary()
        .args(["--init-master-key", key_path.to_str().unwrap()])
        .output()
        .unwrap()
        .status
        .success());
    let coverage_path = directory.join("coverage.json");
    let mutation_path = directory.join("mutation.json");
    let fuzz_corpus_path = directory.join("fuzz-evidence.tar.zst");
    let manifest_path = directory.join("readiness-input.json");
    let output_path = directory.join("release-readiness.json");
    let metrics = QualityMetrics {
        lines_percentage: 95.0,
        regions_percentage: 95.0,
        branches_percentage: 90.0,
        mutation_score_percentage: 90.0,
    };
    let build_hash = options_trading::build_identity::executable_build_hash();
    let coverage = serde_json::to_vec(&CoverageEvidence {
        schema_version: RELEASE_READINESS_SCHEMA_VERSION,
        build_hash: build_hash.clone(),
        global: CoverageMetrics {
            lines_percentage: metrics.lines_percentage,
            regions_percentage: metrics.regions_percentage,
            branches_percentage: metrics.branches_percentage,
        },
        critical_scopes: REQUIRED_CRITICAL_SCOPES
            .iter()
            .map(|scope| {
                (
                    (*scope).into(),
                    CoverageMetrics {
                        lines_percentage: metrics.lines_percentage,
                        regions_percentage: metrics.regions_percentage,
                        branches_percentage: metrics.branches_percentage,
                    },
                )
            })
            .collect(),
    })
    .unwrap();
    let mutation = serde_json::to_vec(&MutationEvidence {
        schema_version: RELEASE_READINESS_SCHEMA_VERSION,
        build_hash: build_hash.clone(),
        global_score_percentage: metrics.mutation_score_percentage,
        critical_scope_scores: REQUIRED_CRITICAL_SCOPES
            .iter()
            .map(|scope| ((*scope).into(), metrics.mutation_score_percentage))
            .collect(),
    })
    .unwrap();
    write_new(&coverage_path, &coverage).unwrap();
    write_new(&mutation_path, &mutation).unwrap();
    write_new(&fuzz_corpus_path, b"reviewed-corpus").unwrap();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let readiness = ReleaseReadiness {
        schema_version: RELEASE_READINESS_SCHEMA_VERSION,
        build_hash,
        commit_hash: "c".repeat(64),
        generated_at_secs: now,
        global: metrics,
        critical_scopes: REQUIRED_CRITICAL_SCOPES
            .iter()
            .map(|scope| ((*scope).into(), metrics))
            .collect(),
        coverage_report_sha256: digest_hex(&coverage),
        mutation_report_sha256: digest_hex(&mutation),
        fuzz_corpus_sha256: digest_hex(b"reviewed-corpus"),
        fuzz_campaign_seconds: 3_600,
        signature: String::new(),
    };
    write_new(&manifest_path, &serde_json::to_vec(&readiness).unwrap()).unwrap();

    let output = binary()
        .args([
            "--sign-release-readiness",
            manifest_path.to_str().unwrap(),
            coverage_path.to_str().unwrap(),
            mutation_path.to_str().unwrap(),
            fuzz_corpus_path.to_str().unwrap(),
            output_path.to_str().unwrap(),
        ])
        .env(MASTER_KEY_ENV, &key_path)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let signed: ReleaseReadiness =
        serde_json::from_slice(&std::fs::read(&output_path).unwrap()).unwrap();
    assert!(verify_release_readiness_payload_from(
        &key_path,
        &signed.signing_payload().unwrap(),
        &signed.signature,
    )
    .unwrap());

    std::fs::write(&fuzz_corpus_path, b"substituted-corpus").unwrap();
    let substituted_corpus_output = directory.join("substituted-corpus.json");
    let rejected_corpus = binary()
        .args([
            "--sign-release-readiness",
            manifest_path.to_str().unwrap(),
            coverage_path.to_str().unwrap(),
            mutation_path.to_str().unwrap(),
            fuzz_corpus_path.to_str().unwrap(),
            substituted_corpus_output.to_str().unwrap(),
        ])
        .env(MASTER_KEY_ENV, &key_path)
        .output()
        .unwrap();
    assert!(!rejected_corpus.status.success());
    assert!(!substituted_corpus_output.exists());
    std::fs::write(&fuzz_corpus_path, b"reviewed-corpus").unwrap();

    std::fs::write(&coverage_path, b"substituted").unwrap();
    let substituted_output = directory.join("substituted.json");
    let rejected = binary()
        .args([
            "--sign-release-readiness",
            manifest_path.to_str().unwrap(),
            coverage_path.to_str().unwrap(),
            mutation_path.to_str().unwrap(),
            fuzz_corpus_path.to_str().unwrap(),
            substituted_output.to_str().unwrap(),
        ])
        .env(MASTER_KEY_ENV, &key_path)
        .output()
        .unwrap();
    assert!(!rejected.status.success());
    assert!(!substituted_output.exists());

    std::fs::remove_dir_all(directory).unwrap();
}

#[cfg(unix)]
#[test]
fn signed_dataset_registry_freezes_split_hash_and_one_time_holdout() {
    let directory = temporary_directory("sealed-dataset");
    let key_path = directory.join("operator.key");
    assert!(binary()
        .args(["--init-master-key", key_path.to_str().unwrap()])
        .output()
        .unwrap()
        .status
        .success());
    let dataset_path = directory.join("dataset.jsonl");
    write_new(&dataset_path, b"{\"session\":100}\n{\"session\":300}\n").unwrap();
    let manifest_path = directory.join("manifest.json");
    let signed_path = directory.join("signed.json");
    let registry = directory.join("registry");
    let manifest = DatasetManifest {
        schema_version: DATASET_MANIFEST_SCHEMA_VERSION,
        dataset_id: dataset_sha256(&dataset_path).unwrap(),
        origin: "captura contractual sanitizada".into(),
        license: "uso interno autorizado".into(),
        interval_start_secs: 100,
        interval_end_secs: 399,
        instruments: vec!["GGAL".into()],
        timezone: "America/Argentina/Buenos_Aires".into(),
        transformations: vec!["anonimización revisada".into()],
        source_schema_version: 1,
        created_at_secs: 50,
        partitions: vec![
            DatasetPartition {
                role: DatasetRole::Selection,
                start_secs: 100,
                end_secs: 249,
            },
            DatasetPartition {
                role: DatasetRole::SealedValidation,
                start_secs: 250,
                end_secs: 399,
            },
        ],
    };
    write_new(
        &manifest_path,
        &serde_json::to_vec_pretty(&manifest).unwrap(),
    )
    .unwrap();

    let signed = binary()
        .args([
            "--sign-dataset-manifest",
            manifest_path.to_str().unwrap(),
            signed_path.to_str().unwrap(),
        ])
        .env(MASTER_KEY_ENV, &key_path)
        .output()
        .unwrap();
    assert!(
        signed.status.success(),
        "{}",
        String::from_utf8_lossy(&signed.stderr)
    );
    let registered = binary()
        .args([
            "--register-dataset",
            dataset_path.to_str().unwrap(),
            signed_path.to_str().unwrap(),
            registry.to_str().unwrap(),
        ])
        .env(MASTER_KEY_ENV, &key_path)
        .output()
        .unwrap();
    assert!(
        registered.status.success(),
        "{}",
        String::from_utf8_lossy(&registered.stderr)
    );

    let mut tampered_signature: SignedDatasetManifest =
        serde_json::from_slice(&std::fs::read(&signed_path).unwrap()).unwrap();
    tampered_signature.signature.push('A');
    let tampered_signature_path = directory.join("signed-tampered.json");
    write_new(
        &tampered_signature_path,
        &serde_json::to_vec_pretty(&tampered_signature).unwrap(),
    )
    .unwrap();
    assert!(!binary()
        .args([
            "--register-dataset",
            dataset_path.to_str().unwrap(),
            tampered_signature_path.to_str().unwrap(),
            registry.to_str().unwrap(),
        ])
        .env(MASTER_KEY_ENV, &key_path)
        .output()
        .unwrap()
        .status
        .success());

    let mut changed_split = manifest.clone();
    changed_split.partitions[0].end_secs = 199;
    changed_split.partitions[1].start_secs = 200;
    let changed_manifest_path = directory.join("manifest-changed.json");
    let changed_signed_path = directory.join("signed-changed.json");
    write_new(
        &changed_manifest_path,
        &serde_json::to_vec_pretty(&changed_split).unwrap(),
    )
    .unwrap();
    assert!(binary()
        .args([
            "--sign-dataset-manifest",
            changed_manifest_path.to_str().unwrap(),
            changed_signed_path.to_str().unwrap(),
        ])
        .env(MASTER_KEY_ENV, &key_path)
        .output()
        .unwrap()
        .status
        .success());
    assert!(!binary()
        .args([
            "--register-dataset",
            dataset_path.to_str().unwrap(),
            changed_signed_path.to_str().unwrap(),
            registry.to_str().unwrap(),
        ])
        .env(MASTER_KEY_ENV, &key_path)
        .output()
        .unwrap()
        .status
        .success());

    let consume = || {
        binary()
            .args([
                "--consume-sealed-holdout",
                dataset_path.to_str().unwrap(),
                signed_path.to_str().unwrap(),
                registry.to_str().unwrap(),
                "evaluation-2026-08-25",
            ])
            .env(MASTER_KEY_ENV, &key_path)
            .output()
            .unwrap()
    };
    assert!(consume().status.success());
    assert!(!consume().status.success());

    std::fs::write(&dataset_path, b"contenido alterado\n").unwrap();
    assert!(!binary()
        .args([
            "--register-dataset",
            dataset_path.to_str().unwrap(),
            signed_path.to_str().unwrap(),
            registry.to_str().unwrap(),
        ])
        .env(MASTER_KEY_ENV, &key_path)
        .output()
        .unwrap()
        .status
        .success());

    std::fs::remove_dir_all(&directory).expect("limpiar directorio temporal exacto");
}

#[cfg(unix)]
#[test]
fn order_protocol_crash_helper() {
    let Ok(stage) = std::env::var("ORDER_PROTOCOL_CRASH_STAGE") else {
        return;
    };
    let directory = std::path::PathBuf::from(std::env::var("ORDER_PROTOCOL_DIRECTORY").unwrap());
    let journal_path = directory.join("journal.jsonl");
    let ledger_path = directory.join("external-broker-effect.json");
    let cancellation_path = directory.join("external-cancellation-effect.json");
    if stage == "verify" {
        let expected_events = std::env::var("ORDER_PROTOCOL_EXPECTED_EVENTS")
            .unwrap()
            .parse::<usize>()
            .unwrap();
        let expected_external = std::env::var("ORDER_PROTOCOL_EXPECTED_EXTERNAL").unwrap() == "1";
        let expected_cancellation =
            std::env::var("ORDER_PROTOCOL_EXPECTED_CANCELLATION").unwrap() == "1";
        let journal = Journal::open_authenticated(&journal_path).unwrap();
        let events = journal.events_after(0).unwrap();
        assert_eq!(events.len(), expected_events);
        assert!(matches!(
            events.first().map(|event| &event.event),
            Some(JournalEventKind::OrderIntentCreated { .. })
        ));
        if expected_events >= 2 {
            assert!(matches!(
                &events[1].event,
                JournalEventKind::OrderAccepted { .. }
            ));
        }
        if expected_events >= 3 {
            assert!(matches!(
                &events[2].event,
                JournalEventKind::OrderUpdated { .. }
            ));
        }
        assert_eq!(ledger_path.exists(), expected_external);
        assert_eq!(cancellation_path.exists(), expected_cancellation);
        return;
    }

    let request = OrderRequest {
        operation_id: "crash-contract-order-1".into(),
        symbol: "GGALC100".into(),
        quantity: 1,
        market_price: 2.0,
        limit_price: 2.01,
        side: OrderSide::Buy,
    };
    let mut journal = Journal::open_authenticated(&journal_path).unwrap();
    record_order_intent(&mut journal, 1_000, &request, true).unwrap();
    crash_at(&stage, "after_intent_sync");

    write_new(
        &ledger_path,
        br#"{"operation_id":"crash-contract-order-1","broker_order_id":"87044496"}"#,
    )
    .unwrap();
    crash_at(&stage, "after_external_effect");

    let accepted = OrderExecution {
        operation_id: request.operation_id.clone(),
        status: OrderStatus::Pending,
        filled_quantity: 0,
        fill_price: None,
        broker_order_id: Some("87044496".into()),
        message: None,
    };
    record_order_accepted(&mut journal, 1_001, &request.operation_id, &accepted).unwrap();
    crash_at(&stage, "after_accepted_sync");

    if stage.starts_with("cancel_") {
        write_new(
            &cancellation_path,
            br#"{"broker_order_id":"87044496","action":"cancel"}"#,
        )
        .unwrap();
        crash_at(&stage, "cancel_after_external_effect");
        let cancelled = OrderExecution {
            status: OrderStatus::Cancelled,
            ..accepted.clone()
        };
        record_order_terminal(&mut journal, 1_002, &request.operation_id, &cancelled, true)
            .unwrap();
        crash_at(&stage, "cancel_after_terminal_sync");
    }

    let terminal = OrderExecution {
        status: OrderStatus::Executed,
        filled_quantity: 1,
        fill_price: Some(2.01),
        ..accepted
    };
    record_order_terminal(&mut journal, 1_002, &request.operation_id, &terminal, true).unwrap();
    crash_at(&stage, "after_terminal_sync");
}

#[cfg(unix)]
fn crash_at(configured: &str, current: &str) {
    if configured == current {
        // `_exit` evita destructores y simula una terminación abrupta después
        // de la frontera durable indicada.
        unsafe { libc::_exit(86) }
    }
}

#[cfg(unix)]
#[test]
fn process_cuts_preserve_each_durable_order_boundary() {
    let cases = [
        ("after_intent_sync", 1, false, false),
        ("after_external_effect", 1, true, false),
        ("after_accepted_sync", 2, true, false),
        ("after_terminal_sync", 3, true, false),
        ("cancel_after_external_effect", 2, true, true),
        ("cancel_after_terminal_sync", 3, true, true),
    ];
    for (stage, expected_events, expected_external, expected_cancellation) in cases {
        let directory = temporary_directory(stage);
        let key_path = directory.join("operator.key");
        assert!(binary()
            .args(["--init-master-key", key_path.to_str().unwrap()])
            .output()
            .unwrap()
            .status
            .success());
        let current_test = std::env::current_exe().unwrap();
        let crashed = Command::new(&current_test)
            .args(["--exact", "order_protocol_crash_helper", "--nocapture"])
            .env("ORDER_PROTOCOL_CRASH_STAGE", stage)
            .env("ORDER_PROTOCOL_DIRECTORY", &directory)
            .env(MASTER_KEY_ENV, &key_path)
            .output()
            .unwrap();
        assert!(
            !crashed.status.success(),
            "{stage} no interrumpió el proceso"
        );

        let verified = Command::new(&current_test)
            .args(["--exact", "order_protocol_crash_helper", "--nocapture"])
            .env("ORDER_PROTOCOL_CRASH_STAGE", "verify")
            .env("ORDER_PROTOCOL_DIRECTORY", &directory)
            .env(
                "ORDER_PROTOCOL_EXPECTED_EVENTS",
                expected_events.to_string(),
            )
            .env(
                "ORDER_PROTOCOL_EXPECTED_EXTERNAL",
                if expected_external { "1" } else { "0" },
            )
            .env(
                "ORDER_PROTOCOL_EXPECTED_CANCELLATION",
                if expected_cancellation { "1" } else { "0" },
            )
            .env(MASTER_KEY_ENV, &key_path)
            .output()
            .unwrap();
        assert!(
            verified.status.success(),
            "verificación falló para {stage}: {}",
            String::from_utf8_lossy(&verified.stderr)
        );
        std::fs::remove_dir_all(directory).unwrap();
    }
}

fn replay_frame(timestamp_secs: i64) -> MarketFrame {
    MarketFrame {
        underlying: UnderlyingQuote {
            ticker: "GGAL".into(),
            last: 100.0 + timestamp_secs as f64 / 100.0,
            bid: Some(99.9),
            ask: Some(100.1),
            timestamp_secs,
            exchange_timestamp_secs: None,
            received_at_secs: 0,
            timestamp_source: QuoteTimestampSource::Legacy,
        },
        options: vec![OptionQuote {
            symbol: "GGALC100".into(),
            underlying: "GGAL".into(),
            kind: OptionKind::Call,
            strike: 100.0,
            expiry_days: 30,
            expiration_timestamp_secs: Some(timestamp_secs + 30 * 86_400),
            catalog_contract_multiplier: Some(100),
            catalog_observed_at_secs: None,
            catalog_schema_version: 0,
            catalog_sha256: None,
            catalog_archived: false,
            contract_metadata_source: ContractMetadataSource::Legacy,
            exercise_style: ExerciseStyle::American,
            last: 2.0,
            bid: Some(1.99),
            ask: Some(2.01),
            volume: 1_000,
            timestamp_secs,
            exchange_timestamp_secs: None,
            received_at_secs: 0,
            timestamp_source: QuoteTimestampSource::Legacy,
        }],
        option_chain_quality: None,
        vix: None,
    }
}

fn temporary_directory(label: &str) -> std::path::PathBuf {
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let path = std::env::temp_dir().join(format!(
        "options-process-{label}-{}-{unique}",
        std::process::id()
    ));
    std::fs::create_dir(&path).unwrap();
    path
}
