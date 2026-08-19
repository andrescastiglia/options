use std::time::SystemTime;

use options_trading::persistence::save_snapshot;
use options_trading::{
    calculate_pnl, BrokerClient, Config, Direction, FakeBroker, Journal, MarketDataProvider, Mode,
    OrderRequest, OrderStatus, Portfolio, PositionKind, SimulatedMarket, Snapshot, TradingEngine,
    TrendDetector,
};
use tracing::{info, warn};

fn main() {
    if let Err(error) = run() {
        eprintln!("error: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let _ = dotenvy::dotenv();
    let config = Config::from_env()?;
    tracing_subscriber::fmt()
        .with_env_filter(config_log_filter(&config.log_level))
        .init();
    info!(ticker = %config.ticker, mode = ?config.mode, "configuracion cargada");

    if config.mode == Mode::Live {
        return Err(
            "modo live no implementado: no se habilitan ordenes reales sin cliente IOL probado"
                .into(),
        );
    }

    let mut market = SimulatedMarket::new(vec![100.0, 100.2, 100.4, 100.7, 101.0, 101.3, 101.6]);
    let mut detector = TrendDetector::new(
        config.min_samples_for_trend.max(5),
        config.min_samples_for_trend,
    );
    let mut engine = TradingEngine::new();
    let mut broker = FakeBroker::default();
    let mut portfolio = Portfolio::default();
    let mut journal = Journal::open("data/journal/simulation.jsonl")?;

    for _ in 0..7 {
        let sample = market.next_price()?;
        let trend = detector.push(sample).expect("valid simulated sample");
        info!(price = sample.price, sma = trend.sma, direction = ?trend.direction, confirmed = trend.confirmed, "precio procesado");
        if trend.confirmed && engine.position.is_none() {
            engine.consider_entry(trend.direction);
            let kind = match trend.direction {
                Direction::Up => PositionKind::Call,
                Direction::Down => PositionKind::Put,
                Direction::Neutral => continue,
            };
            if engine.open_fake_position(
                kind,
                sample.price,
                config.max_position_size,
                SystemTime::now(),
            ) {
                let order = OrderRequest {
                    operation_id: "simulation-1".into(),
                    symbol: format!("{}IO", config.ticker),
                    quantity: config.max_position_size,
                    limit_price: sample.price,
                    is_buy: true,
                };
                if broker.submit_limit(order)? != OrderStatus::Executed {
                    return Err("la orden fake no fue ejecutada".into());
                }
                portfolio.open(
                    "simulation-1".into(),
                    kind,
                    sample.price,
                    config.max_position_size,
                );
                journal.append(
                    sample.timestamp_secs,
                    "simulation-1",
                    "BUY",
                    "fake position opened",
                    true,
                )?;
                info!(?kind, price = sample.price, "posicion fake abierta");
            }
        }
    }

    if let Some(position) = engine.position {
        let pnl = calculate_pnl(
            position.entry_price,
            101.6,
            position.contracts,
            config.commission_percentage,
            config.tax_percentage,
            config.min_profit_multiplier,
        );
        warn!(
            net = pnl.net,
            threshold = pnl.threshold,
            "P&L hipotetico de simulacion"
        );
        if pnl.net >= pnl.threshold {
            engine.mark_selling();
            let order = OrderRequest {
                operation_id: "simulation-1-close".into(),
                symbol: format!("{}IO", config.ticker),
                quantity: position.contracts,
                limit_price: 101.6,
                is_buy: false,
            };
            if broker.submit_limit(order)? != OrderStatus::Executed {
                return Err("el cierre fake no fue ejecutado".into());
            }
            portfolio.close("simulation-1", pnl.net);
            journal.append(
                7,
                "simulation-1",
                "SELL",
                "fake position closed at profit target",
                true,
            )?;
            engine.close();
        }
    }
    let metrics = portfolio.metrics();
    let snapshot = Snapshot {
        timestamp_secs: 7,
        state: format!("{:?}", engine.state),
        active_operation_id: engine.position.as_ref().map(|_| "simulation-1".into()),
        last_operation_id: Some("simulation-1".into()),
    };
    save_snapshot("data/snapshots/state.json", &snapshot)?;
    info!(
        open_positions = metrics.open_positions,
        realized_pnl = metrics.realized_pnl,
        trades = metrics.trades,
        "portfolio actualizado"
    );
    info!(state = ?engine.state, "simulacion finalizada");
    Ok(())
}

fn config_log_filter(default_level: &str) -> String {
    std::env::var("RUST_LOG").unwrap_or_else(|_| default_level.to_string())
}
