pub mod analytics;
pub mod app;
pub mod broker;
pub mod build_identity;
pub mod config;
pub mod datasets;
pub mod errors;
pub mod experiments;
pub mod iol_client;
pub mod iv_rank;
pub mod learning;
pub mod learning_model;
pub mod market;
pub mod market_calendar;
pub mod multileg;
mod number_format;
pub mod option_analytics;
pub mod pattern;
pub mod persistence;
pub mod portfolio;
pub mod redaction;
pub mod release_readiness;
pub mod risk;
pub mod secrets;
pub mod secure_fs;
pub mod storage;
pub mod time_reference;
pub mod time_utils;
pub mod trading;
pub mod tui;
pub mod vix;

pub use app::TradingApp;
pub use broker::{
    AccountOrder, AccountPosition, AccountSnapshot, BrokerClient, FakeBroker, OrderExecution,
    OrderRequest, OrderSide, OrderStatus, PaperBroker,
};
pub use config::{Config, ConfigError, Mode};
pub use errors::AppError;
pub use iol_client::{
    AccountMovement, AccountProfile, CostCalibration, FeeComponent, IolClient, IolClientError,
    IolRealtimeEvent, IolStartupContext, OrderTrackingMetrics, TokenResponse,
    WebsocketConnectionState,
};
pub use learning::{
    trading_regressed, GateRequirements, LearningReport, LearningState, LiveStage, ValidationTrade,
};
pub use market::{
    select_option, select_option_with_criteria, ExerciseStyle, MarketDataProvider, MarketFrame,
    OptionKind, OptionQuote, OptionSelectionCriteria, PriceCache, PriceStream, ReplayMarket,
    UnderlyingQuote, VixFreshnessState, VixObservation, VixValueKind,
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
