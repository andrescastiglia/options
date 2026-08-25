use std::{
    io::{IsTerminal, Write},
    path::PathBuf,
    time::Duration,
};

use options_trading::{
    learning::{AuthorizationRequest, ExecutionAuthorization, AUTHORIZATION_SCHEMA_VERSION},
    secrets::encrypt_for_this_machine,
    tui, Config, TradingApp,
};
use tracing::{error, info};
use zeroize::{Zeroize, Zeroizing};

#[tokio::main]
async fn main() {
    if let Err(error) = run().await {
        eprintln!("error: {error}");
        std::process::exit(1);
    }
}

async fn run() -> Result<(), Box<dyn std::error::Error>> {
    match utility_command()? {
        Some(UtilityCommand::Encrypt(mut plaintext)) => {
            let encrypted = encrypt_for_this_machine(&plaintext)?;
            plaintext.zeroize();
            println!("{encrypted}");
            return Ok(());
        }
        Some(UtilityCommand::Authorize { request, output }) => {
            issue_live_authorization(&request, &output)?;
            return Ok(());
        }
        None => {}
    }
    let _ = dotenvy::dotenv();
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

enum UtilityCommand {
    Encrypt(Zeroizing<String>),
    Authorize { request: PathBuf, output: PathBuf },
}

fn utility_command() -> Result<Option<UtilityCommand>, Box<dyn std::error::Error>> {
    let mut arguments = std::env::args().skip(1);
    let Some(option) = arguments.next() else {
        return Ok(None);
    };
    match option.as_str() {
        "-e" => {
            let plaintext = arguments.next().ok_or("falta el texto: use -e \"texto\"")?;
            if arguments.next().is_some() {
                return Err(
                    "-e acepta un único texto; si contiene espacios, escribirlo entre comillas"
                        .into(),
                );
            }
            Ok(Some(UtilityCommand::Encrypt(Zeroizing::new(plaintext))))
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
        _ => Err(format!(
            "parámetro desconocido: {option}; use -e o --authorize-live REQUEST OUTPUT"
        )
        .into()),
    }
}

fn issue_live_authorization(
    request_path: &std::path::Path,
    output_path: &std::path::Path,
) -> Result<(), Box<dyn std::error::Error>> {
    let request: AuthorizationRequest = serde_json::from_slice(&std::fs::read(request_path)?)?;
    if request.schema_version != AUTHORIZATION_SCHEMA_VERSION {
        return Err(format!(
            "versión de autorización {} no soportada",
            request.schema_version
        )
        .into());
    }
    println!(
        "Cuenta: {}\nEpoch: {}\nFingerprint: {}\nCanary: {} contrato(s), inversión {}, pérdida/trade {}, pérdida/día {}",
        request.account_number,
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
    let authorization = ExecutionAuthorization {
        schema_version: AUTHORIZATION_SCHEMA_VERSION,
        request,
        issued_at_secs,
        expires_at_secs: issued_at_secs.saturating_add(15 * 60),
        confirmation: confirmation.to_string(),
    };
    if let Some(parent) = output_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut file = std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(output_path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    file.write_all(&serde_json::to_vec_pretty(&authorization)?)?;
    file.sync_all()?;
    println!("Autorización creada en {}", output_path.display());
    Ok(())
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
