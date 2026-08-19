use std::time::{Duration, Instant};

use reqwest::{Client, StatusCode};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::time::sleep;
use zeroize::Zeroizing;

use crate::market::Quote;

#[derive(Debug, Error)]
pub enum IolClientError {
    #[error("error HTTP de IOL: {0}")]
    Http(#[from] reqwest::Error),
    #[error("IOL rechazo la autenticacion: {0}")]
    Authentication(String),
    #[error("circuit breaker abierto hasta {0:?}")]
    CircuitOpen(Instant),
    #[error("respuesta de IOL invalida: {0}")]
    InvalidResponse(String),
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TokenResponse {
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub expires_in: u64,
    pub token_type: String,
}

#[derive(Debug)]
pub struct IolClient {
    http: Client,
    base_url: String,
    username: Zeroizing<String>,
    password: Zeroizing<String>,
    refresh_token: Zeroizing<String>,
    access_token: Option<Zeroizing<String>>,
    access_expires_at: Instant,
    failures: u32,
    circuit_open_until: Option<Instant>,
}

impl IolClient {
    pub fn new(
        base_url: impl Into<String>,
        username: String,
        password: String,
        refresh_token: String,
    ) -> Self {
        Self {
            http: Client::new(),
            base_url: base_url.into().trim_end_matches('/').to_string(),
            username: Zeroizing::new(username),
            password: Zeroizing::new(password),
            refresh_token: Zeroizing::new(refresh_token),
            access_token: None,
            access_expires_at: Instant::now(),
            failures: 0,
            circuit_open_until: None,
        }
    }

    pub async fn authenticate(&mut self) -> Result<(), IolClientError> {
        let response = self
            .http
            .post(format!("{}/oauth/authorize", self.base_url))
            .form(&[
                ("username", self.username.as_str()),
                ("password", self.password.as_str()),
                ("grant_type", "password"),
            ])
            .send()
            .await?;
        self.apply_token_response(response).await
    }

    pub async fn refresh(&mut self) -> Result<(), IolClientError> {
        let response = self
            .http
            .post(format!("{}/oauth/token", self.base_url))
            .form(&[
                ("refresh_token", self.refresh_token.as_str()),
                ("grant_type", "refresh_token"),
            ])
            .send()
            .await?;
        self.apply_token_response(response).await
    }

    pub async fn quote(&mut self, ticker: &'static str) -> Result<Quote, IolClientError> {
        self.ensure_access_token().await?;
        let response = self.fetch_quote_response(ticker).await?;
        if response.status() == StatusCode::UNAUTHORIZED {
            self.refresh().await?;
            let response = self.fetch_quote_response(ticker).await?;
            return parse_quote(ticker, response.error_for_status()?.json().await?);
        }
        let response = response.error_for_status()?;
        let body: serde_json::Value = response.json().await?;
        parse_quote(ticker, body)
    }

    async fn fetch_quote_response(
        &self,
        ticker: &'static str,
    ) -> Result<reqwest::Response, IolClientError> {
        let token = self
            .access_token
            .as_ref()
            .ok_or_else(|| IolClientError::Authentication("access token ausente".into()))?;
        Ok(self
            .http
            .get(format!("{}/api/v2/opciones/{ticker}", self.base_url))
            .bearer_auth(token.as_str())
            .send()
            .await?)
    }

    async fn ensure_access_token(&mut self) -> Result<(), IolClientError> {
        if self.access_token.is_none() {
            self.authenticate().await?;
        } else if Instant::now() + Duration::from_secs(30) >= self.access_expires_at {
            self.refresh().await?;
        }
        Ok(())
    }

    async fn apply_token_response(
        &mut self,
        response: reqwest::Response,
    ) -> Result<(), IolClientError> {
        if !response.status().is_success() {
            return Err(IolClientError::Authentication(
                response
                    .text()
                    .await
                    .unwrap_or_else(|_| "respuesta no disponible".into()),
            ));
        }
        let token: TokenResponse = response.json().await?;
        if token.access_token.is_empty() || token.expires_in == 0 {
            return Err(IolClientError::InvalidResponse("token incompleto".into()));
        }
        self.access_token = Some(Zeroizing::new(token.access_token));
        if let Some(refresh_token) = token.refresh_token {
            self.refresh_token = Zeroizing::new(refresh_token);
        }
        self.access_expires_at = Instant::now() + Duration::from_secs(token.expires_in);
        self.failures = 0;
        self.circuit_open_until = None;
        Ok(())
    }

    pub async fn quote_with_retry(
        &mut self,
        ticker: &'static str,
        attempts: u32,
    ) -> Result<Quote, IolClientError> {
        if let Some(until) = self.circuit_open_until {
            if Instant::now() < until {
                return Err(IolClientError::CircuitOpen(until));
            }
        }
        let attempts = attempts.max(1);
        for attempt in 0..attempts {
            match self.quote(ticker).await {
                Ok(quote) => {
                    self.failures = 0;
                    return Ok(quote);
                }
                Err(error) if attempt + 1 < attempts => {
                    self.failures += 1;
                    sleep(Duration::from_millis(100 * 2u64.pow(attempt.min(6)))).await;
                    if self.failures >= 3 {
                        let until = Instant::now() + Duration::from_secs(300);
                        self.circuit_open_until = Some(until);
                        return Err(IolClientError::CircuitOpen(until));
                    }
                    if matches!(error, IolClientError::Http(_)) {
                        continue;
                    }
                    return Err(error);
                }
                Err(error) => {
                    self.failures += 1;
                    return Err(error);
                }
            }
        }
        unreachable!()
    }
}

fn parse_quote(ticker: &'static str, body: serde_json::Value) -> Result<Quote, IolClientError> {
    let object = body
        .as_object()
        .ok_or_else(|| IolClientError::InvalidResponse("se esperaba un objeto JSON".into()))?;
    let last = number(object, &["ultimoPrecio", "last", "price"])?;
    let bid = optional_number(object, &["puntas", "bid"]);
    let ask = optional_number(object, &["ask", "precioAsk"]);
    let timestamp_secs = object
        .get("timestamp_secs")
        .and_then(serde_json::Value::as_i64)
        .unwrap_or_else(chrono_like_now);
    Ok(Quote {
        ticker,
        last,
        bid,
        ask,
        timestamp_secs,
    })
}

fn number(
    object: &serde_json::Map<String, serde_json::Value>,
    names: &[&str],
) -> Result<f64, IolClientError> {
    names
        .iter()
        .find_map(|name| object.get(*name).and_then(serde_json::Value::as_f64))
        .ok_or_else(|| IolClientError::InvalidResponse("precio ultimo ausente".into()))
}

fn optional_number(
    object: &serde_json::Map<String, serde_json::Value>,
    names: &[&str],
) -> Option<f64> {
    names
        .iter()
        .find_map(|name| object.get(*name).and_then(serde_json::Value::as_f64))
}

fn chrono_like_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or_default()
}
