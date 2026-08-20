pub mod app;
pub mod broker;
pub mod config;
pub mod errors;
pub mod iol_client;
pub mod market;
pub mod pattern;
pub mod persistence;
pub mod portfolio;
pub mod risk;
pub mod trading;
pub mod tui;

pub use app::TradingApp;
pub use broker::{
    AccountOrder, AccountPosition, AccountSnapshot, BrokerClient, FakeBroker, OrderExecution,
    OrderRequest, OrderSide, OrderStatus, PaperBroker,
};
pub use config::{Config, ConfigError, Mode};
pub use errors::AppError;
pub use iol_client::{IolClient, IolClientError, TokenResponse};
pub use market::{
    select_option, MarketDataProvider, MarketFrame, OptionKind, OptionQuote, PriceCache,
    PriceStream, ReplayMarket, UnderlyingQuote,
};
pub use pattern::{Direction, PriceSample, Trend, TrendDetector};
pub use persistence::{Journal, Snapshot};
pub use portfolio::{Portfolio, PortfolioMetrics};
pub use risk::{RiskLimits, RiskManager, RiskState};
pub use trading::{
    calculate_pnl, calculate_pnl_with_contract_multiplier, ExitReason, Pnl, Position, PositionKind,
    TradingEngine, TradingState,
};
