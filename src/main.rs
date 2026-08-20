use std::{io::IsTerminal, time::Duration};

use options_trading::{tui, Config, Mode, TradingApp};
use tracing::{error, info};

#[tokio::main]
async fn main() {
    if let Err(error) = run().await {
        eprintln!("error: {error}");
        std::process::exit(1);
    }
}

async fn run() -> Result<(), Box<dyn std::error::Error>> {
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
    let shutdown_result = app.shutdown();
    result?;
    shutdown_result?;
    Ok(())
}

async fn run_headless(app: &mut TradingApp) -> Result<(), options_trading::AppError> {
    info!(mode = ?app.config.mode, ticker = %app.config.ticker, "motor iniciado");
    loop {
        let is_replay = app.config.mode == Mode::Replay;
        let step = app.step();
        let running = if is_replay {
            step.await?
        } else {
            tokio::select! {
                result = step => result?,
                signal = tokio::signal::ctrl_c() => {
                    match signal {
                        Ok(()) => info!("shutdown solicitado"),
                        Err(error) => error!(%error, "fallo esperando señal"),
                    }
                    break;
                }
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
        if !is_replay {
            tokio::time::sleep(Duration::from_secs(app.config.check_interval_secs)).await;
        }
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
