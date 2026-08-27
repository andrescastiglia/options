use std::{
    io::{IsTerminal, Write},
    path::PathBuf,
    time::Duration,
};

use options_trading::{
    datasets::{
        consume_sealed_holdout, register_dataset, sign_manifest, DatasetManifest,
        SignedDatasetManifest,
    },
    learning::{AuthorizationRequest, ExecutionAuthorization, AUTHORIZATION_SCHEMA_VERSION},
    release_readiness::ReleaseReadiness,
    secrets::{
        encrypt_for_this_machine, initialize_master_key, random_nonce, sign_authorization_payload,
        sign_release_readiness_payload, MASTER_KEY_ENV,
    },
    secure_fs::{read_limited, read_private_limited, write_new},
    tui, Config, TradingApp,
};
use tracing::{error, info};
use zeroize::{Zeroize, Zeroizing};

#[tokio::main]
async fn main() {
    if let Err(error) = run().await {
        eprintln!(
            "error: {}",
            options_trading::redaction::sanitize_operational_message(&error.to_string())
        );
        std::process::exit(1);
    }
}

async fn run() -> Result<(), Box<dyn std::error::Error>> {
    let _ = dotenvy::dotenv();
    match utility_command()? {
        Some(UtilityCommand::PrintBuildHash) => {
            println!(
                "{}",
                options_trading::build_identity::executable_build_hash()
            );
            return Ok(());
        }
        Some(UtilityCommand::EncryptPassword) => {
            let mut plaintext = Zeroizing::new(rpassword::prompt_password(
                "Contraseña IOL (no se mostrará): ",
            )?);
            let encrypted = encrypt_for_this_machine(&plaintext)?;
            plaintext.zeroize();
            println!("{encrypted}");
            return Ok(());
        }
        Some(UtilityCommand::InitMasterKey { output }) => {
            initialize_master_key(&output)?;
            println!(
                "Clave maestra creada en {}. Configure {MASTER_KEY_ENV} con esa ruta.",
                output.display()
            );
            return Ok(());
        }
        Some(UtilityCommand::Authorize { request, output }) => {
            issue_live_authorization(&request, &output)?;
            return Ok(());
        }
        Some(UtilityCommand::SignReleaseReadiness {
            manifest,
            coverage_report,
            mutation_report,
            fuzz_corpus,
            output,
        }) => {
            let mut readiness: ReleaseReadiness =
                serde_json::from_slice(&read_limited(&manifest, 256 * 1024)?)?;
            let coverage = read_limited(&coverage_report, 64 * 1024 * 1024)?;
            let mutation = read_limited(&mutation_report, 64 * 1024 * 1024)?;
            let corpus = read_limited(&fuzz_corpus, 64 * 1024 * 1024)?;
            readiness.bind_evidence(&coverage, &mutation, &corpus)?;
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_secs() as i64;
            readiness.validate_claims(
                &options_trading::build_identity::executable_build_hash(),
                now,
            )?;
            if !readiness.signature.is_empty() {
                return Err("el manifest de readiness de entrada ya contiene una firma".into());
            }
            readiness.signature = sign_release_readiness_payload(&readiness.signing_payload()?)?;
            write_new(&output, &serde_json::to_vec_pretty(&readiness)?)?;
            println!("Readiness pre-canary firmado en {}", output.display());
            return Ok(());
        }
        Some(UtilityCommand::SignDatasetManifest { manifest, output }) => {
            let manifest: DatasetManifest =
                serde_json::from_slice(&read_private_limited(&manifest, 256 * 1024)?)?;
            let signed = sign_manifest(manifest)?;
            write_new(&output, &serde_json::to_vec_pretty(&signed)?)?;
            println!("Manifiesto firmado creado en {}", output.display());
            return Ok(());
        }
        Some(UtilityCommand::RegisterDataset {
            dataset,
            manifest,
            registry,
        }) => {
            let signed: SignedDatasetManifest =
                serde_json::from_slice(&read_private_limited(&manifest, 256 * 1024)?)?;
            let path = register_dataset(&dataset, &signed, &registry)?;
            println!("Dataset registrado en {}", path.display());
            return Ok(());
        }
        Some(UtilityCommand::ConsumeSealedHoldout {
            dataset,
            manifest,
            registry,
            evaluator_id,
        }) => {
            let signed: SignedDatasetManifest =
                serde_json::from_slice(&read_private_limited(&manifest, 256 * 1024)?)?;
            let consumed_at_secs = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_secs() as i64;
            let consumption = consume_sealed_holdout(
                &dataset,
                &signed,
                &registry,
                &evaluator_id,
                consumed_at_secs,
            )?;
            println!(
                "Holdout {} consumido de forma irreversible por {}",
                consumption.dataset_id, consumption.evaluator_id
            );
            return Ok(());
        }
        None => {}
    }
    let config = Config::from_env()?;
    let use_tui = config.tui_enabled && std::io::stdout().is_terminal();
    if !use_tui {
        tracing_subscriber::fmt()
            .with_env_filter(config_log_filter(&config.log_level))
            .init();
    }
    let mut app = TradingApp::new(config)?;
    let result = if use_tui {
        if let Err(error) = connect_before_tui(&mut app).await {
            let _ = app.shutdown_with_status(false).await;
            return Err(error.into());
        }
        tui::run(&mut app).await
    } else {
        run_headless(&mut app).await
    };
    let shutdown_result = app.shutdown_with_status(result.is_ok()).await;
    let suggestion = app.environment_suggestion();
    if let Some(suggestion) = suggestion {
        println!("\nSugerencia para actualizar las variables de entorno:\n{suggestion}");
    }
    result?;
    shutdown_result?;
    Ok(())
}

async fn connect_before_tui(app: &mut TradingApp) -> Result<(), options_trading::AppError> {
    eprintln!("Conectando con IOL antes de abrir la interfaz...");
    match app.connect().await {
        Ok(()) => {
            eprintln!("Conexión con IOL confirmada. Abriendo la interfaz...");
            Ok(())
        }
        Err(error @ options_trading::AppError::Connection(_)) => {
            let attempts = app.config.connection_retry_attempts;
            let delay = Duration::from_secs(app.config.connection_retry_delay_secs);
            let mut last_error = error;
            for attempt in 1..=attempts {
                eprintln!(
                    "Conexión fallida. Reintento {attempt}/{attempts} en {} segundos: {last_error}",
                    app.config.connection_retry_delay_secs
                );
                tokio::time::sleep(delay).await;
                match app.connect().await {
                    Ok(()) => {
                        eprintln!("Conexión con IOL confirmada. Abriendo la interfaz...");
                        return Ok(());
                    }
                    Err(error @ options_trading::AppError::Connection(_)) => last_error = error,
                    Err(error) => return Err(error),
                }
            }
            app.mark_connection_not_operational(attempts, &last_error)?;
            Err(options_trading::AppError::Connection(format!(
                "NO OPERATIVO: no se pudo establecer conexión con IOL después de {attempts} reintentos. La TUI no se abrirá. Último error: {last_error}"
            )))
        }
        Err(error) => Err(error),
    }
}

#[derive(Debug, PartialEq, Eq)]
enum UtilityCommand {
    PrintBuildHash,
    EncryptPassword,
    InitMasterKey {
        output: PathBuf,
    },
    Authorize {
        request: PathBuf,
        output: PathBuf,
    },
    SignReleaseReadiness {
        manifest: PathBuf,
        coverage_report: PathBuf,
        mutation_report: PathBuf,
        fuzz_corpus: PathBuf,
        output: PathBuf,
    },
    SignDatasetManifest {
        manifest: PathBuf,
        output: PathBuf,
    },
    RegisterDataset {
        dataset: PathBuf,
        manifest: PathBuf,
        registry: PathBuf,
    },
    ConsumeSealedHoldout {
        dataset: PathBuf,
        manifest: PathBuf,
        registry: PathBuf,
        evaluator_id: String,
    },
}

fn utility_command() -> Result<Option<UtilityCommand>, Box<dyn std::error::Error>> {
    utility_command_from(std::env::args().skip(1))
}

fn utility_command_from<I, S>(
    arguments: I,
) -> Result<Option<UtilityCommand>, Box<dyn std::error::Error>>
where
    I: IntoIterator<Item = S>,
    S: Into<String>,
{
    let mut arguments = arguments.into_iter().map(Into::into);
    let Some(option) = arguments.next() else {
        return Ok(None);
    };
    match option.as_str() {
        "--print-build-hash" => {
            if arguments.next().is_some() {
                return Err("--print-build-hash no acepta argumentos".into());
            }
            Ok(Some(UtilityCommand::PrintBuildHash))
        }
        "--encrypt-password" => {
            if arguments.next().is_some() {
                return Err("--encrypt-password no acepta argumentos".into());
            }
            Ok(Some(UtilityCommand::EncryptPassword))
        }
        "--init-master-key" => {
            let output = arguments
                .next()
                .ok_or("falta la ruta: --init-master-key RUTA")?;
            if arguments.next().is_some() {
                return Err("--init-master-key acepta una única ruta".into());
            }
            Ok(Some(UtilityCommand::InitMasterKey {
                output: output.into(),
            }))
        }
        "-e" => {
            Err("-e fue retirado porque expone la contraseña en argv e historial; use --encrypt-password".into())
        }
        "--authorize-live" => {
            let request = arguments
                .next()
                .ok_or("falta la ruta del live-authorization-request.json")?;
            let output = arguments
                .next()
                .ok_or("falta la ruta de salida para live-authorization.json")?;
            if arguments.next().is_some() {
                return Err("--authorize-live acepta exactamente REQUEST OUTPUT".into());
            }
            Ok(Some(UtilityCommand::Authorize {
                request: request.into(),
                output: output.into(),
            }))
        }
        "--sign-release-readiness" => {
            let manifest = arguments.next().ok_or(
                "falta MANIFEST en --sign-release-readiness MANIFEST COVERAGE MUTATION FUZZ_CORPUS OUTPUT",
            )?;
            let coverage_report = arguments.next().ok_or(
                "falta COVERAGE en --sign-release-readiness MANIFEST COVERAGE MUTATION FUZZ_CORPUS OUTPUT",
            )?;
            let mutation_report = arguments.next().ok_or(
                "falta MUTATION en --sign-release-readiness MANIFEST COVERAGE MUTATION FUZZ_CORPUS OUTPUT",
            )?;
            let fuzz_corpus = arguments.next().ok_or(
                "falta FUZZ_CORPUS en --sign-release-readiness MANIFEST COVERAGE MUTATION FUZZ_CORPUS OUTPUT",
            )?;
            let output = arguments.next().ok_or(
                "falta OUTPUT en --sign-release-readiness MANIFEST COVERAGE MUTATION FUZZ_CORPUS OUTPUT",
            )?;
            if arguments.next().is_some() {
                return Err("--sign-release-readiness acepta exactamente MANIFEST COVERAGE MUTATION FUZZ_CORPUS OUTPUT".into());
            }
            Ok(Some(UtilityCommand::SignReleaseReadiness {
                manifest: manifest.into(),
                coverage_report: coverage_report.into(),
                mutation_report: mutation_report.into(),
                fuzz_corpus: fuzz_corpus.into(),
                output: output.into(),
            }))
        }
        "--sign-dataset-manifest" => {
            let manifest = arguments
                .next()
                .ok_or("falta MANIFEST en --sign-dataset-manifest MANIFEST OUTPUT")?;
            let output = arguments
                .next()
                .ok_or("falta OUTPUT en --sign-dataset-manifest MANIFEST OUTPUT")?;
            if arguments.next().is_some() {
                return Err("--sign-dataset-manifest acepta exactamente MANIFEST OUTPUT".into());
            }
            Ok(Some(UtilityCommand::SignDatasetManifest {
                manifest: manifest.into(),
                output: output.into(),
            }))
        }
        "--register-dataset" => {
            let dataset = arguments
                .next()
                .ok_or("falta DATASET en --register-dataset DATASET MANIFEST REGISTRY")?;
            let manifest = arguments
                .next()
                .ok_or("falta MANIFEST en --register-dataset DATASET MANIFEST REGISTRY")?;
            let registry = arguments
                .next()
                .ok_or("falta REGISTRY en --register-dataset DATASET MANIFEST REGISTRY")?;
            if arguments.next().is_some() {
                return Err("--register-dataset acepta exactamente DATASET MANIFEST REGISTRY".into());
            }
            Ok(Some(UtilityCommand::RegisterDataset {
                dataset: dataset.into(),
                manifest: manifest.into(),
                registry: registry.into(),
            }))
        }
        "--consume-sealed-holdout" => {
            let dataset = arguments.next().ok_or(
                "falta DATASET en --consume-sealed-holdout DATASET MANIFEST REGISTRY EVALUATOR_ID",
            )?;
            let manifest = arguments.next().ok_or(
                "falta MANIFEST en --consume-sealed-holdout DATASET MANIFEST REGISTRY EVALUATOR_ID",
            )?;
            let registry = arguments.next().ok_or(
                "falta REGISTRY en --consume-sealed-holdout DATASET MANIFEST REGISTRY EVALUATOR_ID",
            )?;
            let evaluator_id = arguments.next().ok_or(
                "falta EVALUATOR_ID en --consume-sealed-holdout DATASET MANIFEST REGISTRY EVALUATOR_ID",
            )?;
            if arguments.next().is_some() {
                return Err("--consume-sealed-holdout acepta exactamente DATASET MANIFEST REGISTRY EVALUATOR_ID".into());
            }
            Ok(Some(UtilityCommand::ConsumeSealedHoldout {
                dataset: dataset.into(),
                manifest: manifest.into(),
                registry: registry.into(),
                evaluator_id,
            }))
        }
        _ => Err("parámetro desconocido; use --print-build-hash, --init-master-key, --encrypt-password, --sign-release-readiness, --authorize-live o las utilidades de dataset documentadas".into()),
    }
}

fn issue_live_authorization(
    request_path: &std::path::Path,
    output_path: &std::path::Path,
) -> Result<(), Box<dyn std::error::Error>> {
    let request: AuthorizationRequest =
        serde_json::from_slice(&read_private_limited(request_path, 64 * 1024)?)?;
    validate_authorization_request_for_issuance(
        &request,
        &options_trading::build_identity::executable_build_hash(),
    )?;
    println!(
        "Cuenta: {}\nEpoch: {}\nFingerprint: {}\nCanary: {} contrato(s), inversión {}, pérdida/trade {}, pérdida/día {}",
        options_trading::redaction::masked_identifier(&request.account_number),
        request.epoch,
        request.strategy_fingerprint,
        request.canary_max_position_size,
        request.canary_max_investment_amount,
        request.canary_max_loss_per_trade,
        request.canary_max_daily_loss,
    );
    print!("Escriba la frase de confirmación exacta para emitir una autorización por 15 minutos: ");
    std::io::stdout().flush()?;
    let mut confirmation = String::new();
    std::io::stdin().read_line(&mut confirmation)?;
    let confirmation = Zeroizing::new(confirmation.trim().to_string());
    if confirmation.as_str() != options_trading::config::LIVE_CONFIRMATION {
        return Err("frase de confirmación incorrecta; no se emitió autorización".into());
    }
    let issued_at_secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs() as i64;
    let mut authorization = ExecutionAuthorization {
        schema_version: AUTHORIZATION_SCHEMA_VERSION,
        request,
        issued_at_secs,
        expires_at_secs: issued_at_secs.saturating_add(15 * 60),
        confirmation: confirmation.to_string(),
        nonce: random_nonce()?,
        signature: String::new(),
    };
    authorization.signature = sign_authorization_payload(&authorization.signing_payload()?)?;
    write_new(output_path, &serde_json::to_vec_pretty(&authorization)?)?;
    println!("Autorización creada en {}", output_path.display());
    Ok(())
}

fn validate_authorization_request_for_issuance(
    request: &AuthorizationRequest,
    expected_build_hash: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    if request.schema_version != AUTHORIZATION_SCHEMA_VERSION {
        return Err(format!(
            "versión de autorización {} no soportada",
            request.schema_version
        )
        .into());
    }
    if request.account_number.trim().is_empty()
        || request.account_number.len() > 128
        || request.account_number.chars().any(char::is_control)
    {
        return Err("cuenta inválida en la solicitud de autorización".into());
    }
    if request.epoch == 0 || request.generated_at_secs <= 0 {
        return Err("epoch o instante inválido en la solicitud de autorización".into());
    }
    for (name, value) in [
        (
            "strategy_fingerprint",
            request.strategy_fingerprint.as_str(),
        ),
        ("build_hash", request.build_hash.as_str()),
        ("readiness_sha256", request.readiness_sha256.as_str()),
        ("report_sha256", request.report_sha256.as_str()),
    ] {
        if !is_lowercase_sha256(value) {
            return Err(format!("{name} no es un SHA-256 hexadecimal canónico").into());
        }
    }
    if request.build_hash != expected_build_hash {
        return Err("la solicitud pertenece a otro build ejecutable".into());
    }
    if request.canary_max_position_size == 0
        || !request.canary_max_investment_amount.is_finite()
        || request.canary_max_investment_amount <= 0.0
        || !request.canary_max_loss_per_trade.is_finite()
        || request.canary_max_loss_per_trade <= 0.0
        || request.canary_max_loss_per_trade > request.canary_max_investment_amount
        || !request.canary_max_daily_loss.is_finite()
        || request.canary_max_daily_loss < request.canary_max_loss_per_trade
    {
        return Err("límites Canary inválidos en la solicitud de autorización".into());
    }
    Ok(())
}

fn is_lowercase_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

async fn run_headless(app: &mut TradingApp) -> Result<(), options_trading::AppError> {
    info!(mode = ?app.config.mode, ticker = %app.config.ticker, "motor iniciado");
    loop {
        let step = app.step();
        let running = tokio::select! {
            result = step => result?,
            signal = tokio::signal::ctrl_c() => {
                match signal {
                    Ok(()) => info!("shutdown solicitado"),
                    Err(error) => error!(%error, "fallo esperando señal"),
                }
                break;
            }
        };
        if let Some(frame) = &app.current_frame {
            info!(
                price = frame.underlying.last,
                state = ?app.engine.state,
                pnl = app.current_pnl.map(|pnl| pnl.net),
                "tick procesado"
            );
        }
        if !running {
            break;
        }
        tokio::time::sleep(Duration::from_secs(app.config.check_interval_secs)).await;
    }
    let metrics = app.metrics();
    info!(
        trades = metrics.trades,
        wins = metrics.wins,
        losses = metrics.losses,
        realized_pnl = metrics.realized_pnl,
        "motor detenido"
    );
    Ok(())
}

fn config_log_filter(default_level: &str) -> String {
    std::env::var("RUST_LOG").unwrap_or_else(|_| default_level.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid_authorization_request() -> AuthorizationRequest {
        AuthorizationRequest {
            schema_version: AUTHORIZATION_SCHEMA_VERSION,
            account_number: "2033590".into(),
            epoch: 1,
            strategy_fingerprint: "a".repeat(64),
            build_hash: options_trading::build_identity::executable_build_hash(),
            readiness_sha256: "b".repeat(64),
            report_sha256: "c".repeat(64),
            canary_max_position_size: 1,
            canary_max_investment_amount: 10_000.0,
            canary_max_loss_per_trade: 500.0,
            canary_max_daily_loss: 1_000.0,
            generated_at_secs: 1,
        }
    }

    #[test]
    fn utility_cli_has_explicit_safe_grammar() {
        assert_eq!(utility_command_from(Vec::<String>::new()).unwrap(), None);
        assert_eq!(
            utility_command_from(["--print-build-hash"]).unwrap(),
            Some(UtilityCommand::PrintBuildHash)
        );
        assert_eq!(
            utility_command_from(["--encrypt-password"]).unwrap(),
            Some(UtilityCommand::EncryptPassword)
        );
        assert_eq!(
            utility_command_from(["--sign-dataset-manifest", "manifest.json", "signed.json"])
                .unwrap(),
            Some(UtilityCommand::SignDatasetManifest {
                manifest: PathBuf::from("manifest.json"),
                output: PathBuf::from("signed.json")
            })
        );
        assert_eq!(
            utility_command_from([
                "--sign-release-readiness",
                "readiness.json",
                "coverage.json",
                "mutation.json",
                "fuzz-evidence.tar.zst",
                "signed-readiness.json",
            ])
            .unwrap(),
            Some(UtilityCommand::SignReleaseReadiness {
                manifest: PathBuf::from("readiness.json"),
                coverage_report: PathBuf::from("coverage.json"),
                mutation_report: PathBuf::from("mutation.json"),
                fuzz_corpus: PathBuf::from("fuzz-evidence.tar.zst"),
                output: PathBuf::from("signed-readiness.json"),
            })
        );
        assert_eq!(
            utility_command_from([
                "--register-dataset",
                "data.jsonl",
                "signed.json",
                "registry"
            ])
            .unwrap(),
            Some(UtilityCommand::RegisterDataset {
                dataset: PathBuf::from("data.jsonl"),
                manifest: PathBuf::from("signed.json"),
                registry: PathBuf::from("registry")
            })
        );
        assert_eq!(
            utility_command_from(["--init-master-key", "/tmp/key"]).unwrap(),
            Some(UtilityCommand::InitMasterKey {
                output: PathBuf::from("/tmp/key")
            })
        );
        assert_eq!(
            utility_command_from(["--authorize-live", "request.json", "grant.json"]).unwrap(),
            Some(UtilityCommand::Authorize {
                request: PathBuf::from("request.json"),
                output: PathBuf::from("grant.json")
            })
        );
        assert!(utility_command_from(["-e", "secret"]).is_err());
        assert!(utility_command_from(["--encrypt-password", "secret"]).is_err());
        assert!(utility_command_from(["--unknown"]).is_err());
    }

    #[test]
    fn utility_cli_rejects_missing_and_extra_arguments_for_every_command() {
        let invalid = [
            vec!["--print-build-hash", "extra"],
            vec!["--init-master-key"],
            vec!["--init-master-key", "key", "extra"],
            vec!["--authorize-live"],
            vec!["--authorize-live", "request"],
            vec!["--authorize-live", "request", "output", "extra"],
            vec!["--sign-release-readiness"],
            vec!["--sign-release-readiness", "manifest"],
            vec![
                "--sign-release-readiness",
                "manifest",
                "coverage",
                "mutation",
                "corpus",
            ],
            vec![
                "--sign-release-readiness",
                "manifest",
                "coverage",
                "mutation",
                "corpus",
                "output",
                "extra",
            ],
            vec!["--sign-dataset-manifest"],
            vec!["--sign-dataset-manifest", "manifest"],
            vec!["--sign-dataset-manifest", "manifest", "output", "extra"],
            vec!["--register-dataset"],
            vec!["--register-dataset", "dataset", "manifest"],
            vec![
                "--register-dataset",
                "dataset",
                "manifest",
                "registry",
                "extra",
            ],
            vec!["--consume-sealed-holdout"],
            vec![
                "--consume-sealed-holdout",
                "dataset",
                "manifest",
                "registry",
            ],
            vec![
                "--consume-sealed-holdout",
                "dataset",
                "manifest",
                "registry",
                "evaluator",
                "extra",
            ],
        ];
        for arguments in invalid {
            assert!(utility_command_from(arguments).is_err());
        }
    }

    #[test]
    fn authorization_issuance_rejects_each_ambiguous_or_unsafe_field() {
        type RequestMutation = Box<dyn Fn(&mut AuthorizationRequest)>;

        let expected_build = options_trading::build_identity::executable_build_hash();
        validate_authorization_request_for_issuance(
            &valid_authorization_request(),
            &expected_build,
        )
        .unwrap();

        let mutations: Vec<RequestMutation> = vec![
            Box::new(|request| request.schema_version += 1),
            Box::new(|request| request.account_number.clear()),
            Box::new(|request| request.account_number = "123\n456".into()),
            Box::new(|request| request.account_number = "x".repeat(129)),
            Box::new(|request| request.epoch = 0),
            Box::new(|request| request.generated_at_secs = 0),
            Box::new(|request| request.strategy_fingerprint = "A".repeat(64)),
            Box::new(|request| request.build_hash = "d".repeat(64)),
            Box::new(|request| request.readiness_sha256 = "short".into()),
            Box::new(|request| request.report_sha256 = "g".repeat(64)),
            Box::new(|request| request.canary_max_position_size = 0),
            Box::new(|request| request.canary_max_investment_amount = f64::NAN),
            Box::new(|request| request.canary_max_investment_amount = 0.0),
            Box::new(|request| request.canary_max_loss_per_trade = f64::INFINITY),
            Box::new(|request| request.canary_max_loss_per_trade = 0.0),
            Box::new(|request| request.canary_max_loss_per_trade = 20_000.0),
            Box::new(|request| request.canary_max_daily_loss = f64::NAN),
            Box::new(|request| request.canary_max_daily_loss = 100.0),
        ];
        for mutate in mutations {
            let mut request = valid_authorization_request();
            mutate(&mut request);
            assert!(
                validate_authorization_request_for_issuance(&request, &expected_build).is_err()
            );
        }
    }
}
