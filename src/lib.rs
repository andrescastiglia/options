pub mod broker;
pub mod config;
pub mod errors;
pub mod iol_client;
pub mod market;
pub mod pattern;
pub mod persistence;
pub mod portfolio;
pub mod trading;

pub use broker::{BrokerClient, FakeBroker, OrderRequest, OrderStatus};
pub use config::{Config, ConfigError, Mode};
pub use errors::AppError;
pub use iol_client::{IolClient, IolClientError, TokenResponse};
pub use market::{MarketDataProvider, PriceCache, PriceStream, Quote, SimulatedMarket};
pub use pattern::{Direction, PriceSample, Trend, TrendDetector};
pub use persistence::{Journal, Snapshot};
pub use portfolio::{Portfolio, PortfolioMetrics};
pub use trading::{
    calculate_pnl, ExitReason, Pnl, Position, PositionKind, TradingEngine, TradingState,
};
