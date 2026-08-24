use std::{io::IsTerminal, time::Duration};

use options_trading::{secrets::encrypt_for_this_machine, tui, Config, TradingApp};
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
    if let Some(mut plaintext) = encryption_argument()? {
        let encrypted = encrypt_for_this_machine(&plaintext)?;
        plaintext.zeroize();
        println!("{encrypted}");
        return Ok(());
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
        tui::run(&mut app).await
    } else {
        run_headless(&mut app).await
    };
    let shutdown_result = app.shutdown().await;
    let suggestion = app.environment_suggestion();
    if let Some(suggestion) = suggestion {
        println!("\nSugerencia para actualizar las variables de entorno:\n{suggestion}");
    }
    result?;
    shutdown_result?;
    Ok(())
}

fn encryption_argument() -> Result<Option<Zeroizing<String>>, Box<dyn std::error::Error>> {
    let mut arguments = std::env::args().skip(1);
    let Some(option) = arguments.next() else {
        return Ok(None);
    };
    if option != "-e" {
        return Err(
            format!("parámetro desconocido: {option}; para cifrar use -e \"texto\"").into(),
        );
    }
    let plaintext = arguments.next().ok_or("falta el texto: use -e \"texto\"")?;
    if arguments.next().is_some() {
        return Err(
            "-e acepta un único texto; si contiene espacios, escribirlo entre comillas".into(),
        );
    }
    Ok(Some(Zeroizing::new(plaintext)))
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
