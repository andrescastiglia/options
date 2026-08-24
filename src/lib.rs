pub mod app;
pub mod broker;
pub mod config;
pub mod errors;
pub mod iol_client;
pub mod learning;
pub mod market;
mod number_format;
pub mod pattern;
pub mod persistence;
pub mod portfolio;
pub mod risk;
pub mod secrets;
pub mod trading;
pub mod tui;

pub use app::TradingApp;
pub use broker::{
    AccountOrder, AccountPosition, AccountSnapshot, BrokerClient, FakeBroker, OrderExecution,
    OrderRequest, OrderSide, OrderStatus, PaperBroker,
};
pub use config::{Config, ConfigError, Mode};
pub use errors::AppError;
pub use iol_client::{
    AccountMovement, AccountProfile, CostCalibration, FeeComponent, IolClient, IolClientError,
    IolRealtimeEvent, IolStartupContext, TokenResponse,
};
pub use learning::{
    trading_regressed, GateRequirements, LearningReport, LearningState, LiveStage, ValidationTrade,
};
pub use market::{
    select_option, select_option_with_criteria, MarketDataProvider, MarketFrame, OptionKind,
    OptionQuote, OptionSelectionCriteria, PriceCache, PriceStream, ReplayMarket, UnderlyingQuote,
};
pub use pattern::{Direction, PriceSample, Trend, TrendCriteria, TrendDetector};
pub use persistence::{Journal, Snapshot};
pub use portfolio::{Portfolio, PortfolioMetrics};
pub use risk::{RiskLimits, RiskManager, RiskState};
pub use trading::{
    build_position_economics, calculate_pnl, calculate_pnl_with_contract_multiplier,
    calculate_position_pnl, ExitReason, Pnl, Position, PositionEconomics, PositionKind,
    TradingEngine, TradingState,
};
