use std::io;

use thiserror::Error;

#[derive(Debug, Error)]
pub enum AppError {
    #[error("dato de mercado invalido: {0}")]
    InvalidMarketData(String),
    #[error("orden no permitida: {0}")]
    OrderRejected(String),
    #[error("operacion de persistencia fallida: {0}")]
    Persistence(#[from] io::Error),
    #[error("serializacion fallida: {0}")]
    Serialization(#[from] serde_json::Error),
    #[error("recuperacion fallida: {0}")]
    Recovery(String),
    #[error("integracion externa fallida: {0}")]
    External(String),
    #[error("conexión con IOL fallida: {0}")]
    Connection(String),
}
