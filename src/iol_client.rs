use std::time::{Duration, Instant};

use reqwest::{Client, Response, StatusCode};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::time::sleep;
use zeroize::Zeroizing;

use crate::{
    broker::{
        AccountOrder, AccountPosition, AccountSnapshot, OrderExecution, OrderRequest, OrderSide,
        OrderStatus,
    },
    market::{MarketFrame, OptionKind, OptionQuote, UnderlyingQuote},
    trading::PositionKind,
};

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
    ) -> Result<Self, IolClientError> {
        let http = Client::builder()
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(15))
            .user_agent("options-trading/0.1")
            .build()?;
        Ok(Self {
            http,
            base_url: base_url.into().trim_end_matches('/').to_string(),
            username: Zeroizing::new(username),
            password: Zeroizing::new(password),
            refresh_token: Zeroizing::new(refresh_token),
            access_token: None,
            access_expires_at: Instant::now(),
            failures: 0,
            circuit_open_until: None,
        })
    }

    pub async fn authenticate(&mut self) -> Result<(), IolClientError> {
        let response = self
            .http
            .post(format!("{}/token", self.base_url))
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
        if self.refresh_token.is_empty() {
            return self.authenticate().await;
        }
        let response = self
            .http
            .post(format!("{}/token", self.base_url))
            .form(&[
                ("refresh_token", self.refresh_token.as_str()),
                ("grant_type", "refresh_token"),
            ])
            .send()
            .await?;
        self.apply_token_response(response).await
    }

    pub async fn market_frame(&mut self, ticker: &str) -> Result<MarketFrame, IolClientError> {
        let body = self
            .authorized_json_get(&format!("/api/v2/opciones/{ticker}"))
            .await?;
        parse_market_frame(ticker, body)
    }

    pub async fn market_frame_with_retry(
        &mut self,
        ticker: &str,
        attempts: u32,
    ) -> Result<MarketFrame, IolClientError> {
        if let Some(until) = self.circuit_open_until {
            if Instant::now() < until {
                return Err(IolClientError::CircuitOpen(until));
            }
        }
        let attempts = attempts.max(1);
        for attempt in 0..attempts {
            match self.market_frame(ticker).await {
                Ok(frame) => {
                    self.failures = 0;
                    self.circuit_open_until = None;
                    return Ok(frame);
                }
                Err(error) if error_is_retryable(&error) && attempt + 1 < attempts => {
                    self.failures = self.failures.saturating_add(1);
                    if self.failures >= 3 {
                        let until = Instant::now() + Duration::from_secs(300);
                        self.circuit_open_until = Some(until);
                        return Err(IolClientError::CircuitOpen(until));
                    }
                    sleep(Duration::from_millis(250 * 2u64.pow(attempt.min(6)))).await;
                }
                Err(error) => {
                    self.failures = self.failures.saturating_add(1);
                    return Err(error);
                }
            }
        }
        unreachable!()
    }

    pub async fn account_snapshot(&mut self) -> Result<AccountSnapshot, IolClientError> {
        let portfolio = self
            .authorized_json_get("/api/v2/portafolio/Argentina")
            .await?;
        let pending = self
            .authorized_json_get(
                "/api/v2/operaciones?filtro.estado=Pendientes&filtro.pais=Argentina",
            )
            .await?;
        Ok(AccountSnapshot {
            positions: parse_account_positions(&portfolio)?,
            pending_orders: parse_pending_orders(&pending)?,
        })
    }

    pub async fn submit_order(
        &mut self,
        order_path: &str,
        request: &OrderRequest,
    ) -> Result<OrderExecution, IolClientError> {
        self.ensure_access_token().await?;
        let body = serde_json::json!({
            "identificador": request.operation_id,
            "simbolo": request.symbol,
            "cantidad": request.quantity,
            "precio": request.limit_price,
            "operacion": match request.side { OrderSide::Buy => "compra", OrderSide::Sell => "venta" },
            "plazo": "inmediata",
            "tipo": "limite"
        });
        let mut response = self.authorized_post(order_path, &body).await?;
        if response.status() == StatusCode::UNAUTHORIZED {
            self.refresh().await?;
            response = self.authorized_post(order_path, &body).await?;
        }
        let response = response.error_for_status()?;
        parse_order_execution(request, response.json().await?)
    }

    async fn authorized_json_get(
        &mut self,
        path: &str,
    ) -> Result<serde_json::Value, IolClientError> {
        self.ensure_access_token().await?;
        let mut response = self.authorized_get(path).await?;
        if response.status() == StatusCode::UNAUTHORIZED {
            self.refresh().await?;
            response = self.authorized_get(path).await?;
        }
        Ok(response.error_for_status()?.json().await?)
    }

    async fn authorized_get(&self, path: &str) -> Result<Response, IolClientError> {
        let token = self.access_token()?;
        Ok(self
            .http
            .get(format!("{}{}", self.base_url, normalize_path(path)))
            .bearer_auth(token)
            .send()
            .await?)
    }

    async fn authorized_post(
        &self,
        path: &str,
        body: &serde_json::Value,
    ) -> Result<Response, IolClientError> {
        let token = self.access_token()?;
        Ok(self
            .http
            .post(format!("{}{}", self.base_url, normalize_path(path)))
            .bearer_auth(token)
            .json(body)
            .send()
            .await?)
    }

    fn access_token(&self) -> Result<&str, IolClientError> {
        self.access_token
            .as_ref()
            .map(|token| token.as_str())
            .ok_or_else(|| IolClientError::Authentication("access token ausente".into()))
    }

    async fn ensure_access_token(&mut self) -> Result<(), IolClientError> {
        if self.access_token.is_none() {
            self.authenticate().await?;
        } else if Instant::now() + Duration::from_secs(30) >= self.access_expires_at {
            self.refresh().await?;
        }
        Ok(())
    }

    async fn apply_token_response(&mut self, response: Response) -> Result<(), IolClientError> {
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
}

fn normalize_path(path: &str) -> String {
    if path.starts_with('/') {
        path.to_string()
    } else {
        format!("/{path}")
    }
}

fn error_is_retryable(error: &IolClientError) -> bool {
    matches!(error, IolClientError::Http(_))
}

fn parse_market_frame(
    ticker: &str,
    body: serde_json::Value,
) -> Result<MarketFrame, IolClientError> {
    let object = body
        .as_object()
        .ok_or_else(|| IolClientError::InvalidResponse("se esperaba un objeto JSON".into()))?;
    let underlying_object = object
        .get("subyacente")
        .and_then(serde_json::Value::as_object)
        .unwrap_or(object);
    let last = number(
        underlying_object,
        &["ultimoPrecio", "ultimo", "last", "price", "underlyingPrice"],
    )?;
    let timestamp_secs = integer(object, &["timestamp_secs", "timestamp"])
        .or_else(|| integer(underlying_object, &["timestamp_secs", "timestamp"]))
        .unwrap_or_else(unix_now);
    let (bid, ask) = book_prices(underlying_object);
    let option_values = ["opciones", "titulos", "options"]
        .iter()
        .find_map(|name| object.get(*name).and_then(serde_json::Value::as_array))
        .ok_or_else(|| IolClientError::InvalidResponse("cadena de opciones ausente".into()))?;
    let options = option_values
        .iter()
        .map(|value| parse_option(ticker, timestamp_secs, value))
        .collect::<Result<Vec<_>, _>>()?;
    if options.is_empty() {
        return Err(IolClientError::InvalidResponse(
            "cadena de opciones vacia".into(),
        ));
    }
    Ok(MarketFrame {
        underlying: UnderlyingQuote {
            ticker: ticker.into(),
            last,
            bid,
            ask,
            timestamp_secs,
        },
        options,
    })
}

fn parse_option(
    ticker: &str,
    default_timestamp: i64,
    value: &serde_json::Value,
) -> Result<OptionQuote, IolClientError> {
    let object = value
        .as_object()
        .ok_or_else(|| IolClientError::InvalidResponse("opcion no es objeto".into()))?;
    let symbol = text(object, &["simbolo", "symbol", "ticker"])?;
    let kind_text = text(object, &["tipoOpcion", "tipo", "kind"])?;
    let kind = match kind_text.to_ascii_lowercase().as_str() {
        "call" | "c" | "compra" => OptionKind::Call,
        "put" | "p" | "venta" => OptionKind::Put,
        other => {
            return Err(IolClientError::InvalidResponse(format!(
                "tipo de opcion desconocido: {other}"
            )))
        }
    };
    let last = number(object, &["ultimoPrecio", "ultimo", "last", "price"])?;
    let (bid, ask) = book_prices(object);
    Ok(OptionQuote {
        symbol,
        underlying: ticker.into(),
        kind,
        strike: number(object, &["strike", "precioEjercicio"])?,
        expiry_days: integer(object, &["diasVencimiento", "expiryDays"])
            .unwrap_or(1)
            .max(0) as u32,
        last,
        bid,
        ask,
        volume: integer(object, &["volumen", "volume"]).unwrap_or(0).max(0) as u64,
        timestamp_secs: integer(object, &["timestamp_secs", "timestamp"])
            .unwrap_or(default_timestamp),
    })
}

fn parse_order_execution(
    request: &OrderRequest,
    body: serde_json::Value,
) -> Result<OrderExecution, IolClientError> {
    let object = body
        .as_object()
        .ok_or_else(|| IolClientError::InvalidResponse("orden no es objeto".into()))?;
    let raw_status = text_optional(object, &["estado", "status"]).unwrap_or("pendiente");
    let status = match raw_status.to_ascii_lowercase().as_str() {
        "ejecutada" | "executed" | "filled" => OrderStatus::Executed,
        "parcial" | "partially_filled" => OrderStatus::PartiallyExecuted,
        "rechazada" | "rejected" | "cancelada" => OrderStatus::Rejected,
        _ => OrderStatus::Pending,
    };
    let filled_quantity = integer(object, &["cantidadEjecutada", "filledQuantity"])
        .unwrap_or(if status == OrderStatus::Executed {
            request.quantity as i64
        } else {
            0
        })
        .max(0) as u32;
    Ok(OrderExecution {
        operation_id: request.operation_id.clone(),
        status,
        filled_quantity,
        fill_price: optional_number(object, &["precioEjecutado", "fillPrice"]),
        broker_order_id: text_optional(object, &["numeroOperacion", "orderId"]).map(str::to_string),
        message: text_optional(object, &["mensaje", "message"]).map(str::to_string),
    })
}

fn parse_account_positions(
    body: &serde_json::Value,
) -> Result<Vec<AccountPosition>, IolClientError> {
    Ok(collection(body, &["activos", "titulos", "positions"])?
        .iter()
        .filter_map(|value| {
            let object = value.as_object()?;
            let instrument = nested_instrument(object);
            let symbol = text_optional(instrument, &["simbolo", "symbol", "ticker"])
                .or_else(|| text_optional(object, &["simbolo", "symbol", "ticker"]))?;
            let quantity = optional_number(object, &["cantidad", "quantity", "tenencia"])?;
            if !quantity.is_finite() || quantity <= 0.0 || quantity > u32::MAX as f64 {
                return None;
            }
            let descriptor = text_optional(
                instrument,
                &[
                    "tipo",
                    "tipoTitulo",
                    "tipoInstrumento",
                    "instrumentType",
                    "kind",
                ],
            )
            .or_else(|| {
                text_optional(
                    object,
                    &[
                        "tipo",
                        "tipoTitulo",
                        "tipoInstrumento",
                        "instrumentType",
                        "kind",
                    ],
                )
            });
            let kind = descriptor.and_then(parse_position_kind);
            let is_option = kind.is_some() || descriptor.is_some_and(is_option_descriptor);
            Some(AccountPosition {
                symbol: symbol.to_string(),
                quantity: quantity.floor() as u32,
                average_price: optional_number(
                    object,
                    &[
                        "ppc",
                        "precioPromedioCompra",
                        "precioPromedio",
                        "averagePrice",
                    ],
                )
                .filter(|price| price.is_finite() && *price > 0.0),
                kind,
                is_option,
            })
        })
        .collect())
}

fn parse_pending_orders(body: &serde_json::Value) -> Result<Vec<AccountOrder>, IolClientError> {
    Ok(collection(body, &["operaciones", "orders", "items"])?
        .iter()
        .filter_map(|value| {
            let object = value.as_object()?;
            let instrument = nested_instrument(object);
            let symbol = text_optional(instrument, &["simbolo", "symbol", "ticker"])
                .or_else(|| text_optional(object, &["simbolo", "symbol", "ticker"]))?;
            let descriptor = text_optional(
                instrument,
                &[
                    "tipo",
                    "tipoTitulo",
                    "tipoInstrumento",
                    "instrumentType",
                    "kind",
                ],
            )
            .or_else(|| {
                text_optional(
                    object,
                    &[
                        "tipo",
                        "tipoTitulo",
                        "tipoInstrumento",
                        "instrumentType",
                        "kind",
                    ],
                )
            });
            let kind = descriptor.and_then(parse_position_kind);
            let is_option = kind.is_some() || descriptor.is_some_and(is_option_descriptor);
            let side =
                text_optional(object, &["operacion", "tipoOperacion", "side"]).and_then(|value| {
                    match value.to_ascii_lowercase().as_str() {
                        "compra" | "buy" => Some(OrderSide::Buy),
                        "venta" | "sell" => Some(OrderSide::Sell),
                        _ => None,
                    }
                });
            let broker_order_id = scalar_text(object, &["numero", "numeroOperacion", "orderId"])
                .unwrap_or_else(|| "desconocida".into());
            let quantity = optional_number(object, &["cantidad", "quantity"])
                .filter(|quantity| quantity.is_finite() && *quantity > 0.0)
                .unwrap_or_default()
                .min(u32::MAX as f64)
                .floor() as u32;
            Some(AccountOrder {
                broker_order_id,
                symbol: symbol.to_string(),
                side,
                quantity,
                kind,
                is_option,
            })
        })
        .collect())
}

fn collection<'a>(
    body: &'a serde_json::Value,
    names: &[&str],
) -> Result<&'a [serde_json::Value], IolClientError> {
    if let Some(values) = body.as_array() {
        return Ok(values);
    }
    let object = body
        .as_object()
        .ok_or_else(|| IolClientError::InvalidResponse("se esperaba una coleccion JSON".into()))?;
    names
        .iter()
        .find_map(|name| object.get(*name).and_then(serde_json::Value::as_array))
        .map(Vec::as_slice)
        .ok_or_else(|| IolClientError::InvalidResponse(format!("coleccion ausente: {}", names[0])))
}

fn nested_instrument(
    object: &serde_json::Map<String, serde_json::Value>,
) -> &serde_json::Map<String, serde_json::Value> {
    ["titulo", "instrumento", "instrument"]
        .iter()
        .find_map(|name| object.get(*name).and_then(serde_json::Value::as_object))
        .unwrap_or(object)
}

fn parse_position_kind(value: &str) -> Option<PositionKind> {
    let value = value.to_ascii_lowercase();
    if value == "call" || value.contains("opcion call") || value.contains("opción call") {
        Some(PositionKind::Call)
    } else if value == "put" || value.contains("opcion put") || value.contains("opción put") {
        Some(PositionKind::Put)
    } else {
        None
    }
}

fn is_option_descriptor(value: &str) -> bool {
    let value = value.to_ascii_lowercase();
    value.contains("opcion") || value.contains("opción") || value == "options"
}

fn scalar_text(
    object: &serde_json::Map<String, serde_json::Value>,
    names: &[&str],
) -> Option<String> {
    names.iter().find_map(|name| {
        object.get(*name).and_then(|value| {
            value
                .as_str()
                .map(str::to_string)
                .or_else(|| value.as_i64().map(|value| value.to_string()))
        })
    })
}

fn book_prices(object: &serde_json::Map<String, serde_json::Value>) -> (Option<f64>, Option<f64>) {
    let direct_bid = optional_number(object, &["bid", "precioCompra"]);
    let direct_ask = optional_number(object, &["ask", "precioVenta", "precioAsk"]);
    let point = object
        .get("puntas")
        .and_then(serde_json::Value::as_array)
        .and_then(|points| points.first())
        .and_then(serde_json::Value::as_object);
    (
        direct_bid
            .or_else(|| point.and_then(|value| optional_number(value, &["precioCompra", "bid"]))),
        direct_ask
            .or_else(|| point.and_then(|value| optional_number(value, &["precioVenta", "ask"]))),
    )
}

fn number(
    object: &serde_json::Map<String, serde_json::Value>,
    names: &[&str],
) -> Result<f64, IolClientError> {
    optional_number(object, names)
        .ok_or_else(|| IolClientError::InvalidResponse(format!("numero ausente: {}", names[0])))
}

fn optional_number(
    object: &serde_json::Map<String, serde_json::Value>,
    names: &[&str],
) -> Option<f64> {
    names.iter().find_map(|name| {
        object.get(*name).and_then(|value| {
            value
                .as_f64()
                .or_else(|| value.as_str().and_then(|value| value.parse().ok()))
        })
    })
}

fn integer(object: &serde_json::Map<String, serde_json::Value>, names: &[&str]) -> Option<i64> {
    names.iter().find_map(|name| {
        object.get(*name).and_then(|value| {
            value
                .as_i64()
                .or_else(|| value.as_str().and_then(|value| value.parse().ok()))
        })
    })
}

fn text(
    object: &serde_json::Map<String, serde_json::Value>,
    names: &[&str],
) -> Result<String, IolClientError> {
    text_optional(object, names)
        .map(str::to_string)
        .ok_or_else(|| IolClientError::InvalidResponse(format!("texto ausente: {}", names[0])))
}

fn text_optional<'a>(
    object: &'a serde_json::Map<String, serde_json::Value>,
    names: &[&str],
) -> Option<&'a str> {
    names
        .iter()
        .find_map(|name| object.get(*name).and_then(serde_json::Value::as_str))
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io::{Read, Write},
        net::TcpListener,
        thread,
    };

    #[test]
    fn parses_market_contract_with_nested_book() {
        let body = serde_json::json!({
            "subyacente": {"ultimoPrecio": 100.5, "puntas": [{"precioCompra": 100.4, "precioVenta": 100.6}]},
            "timestamp_secs": 10,
            "opciones": [{
                "simbolo": "GAL-C-100", "tipoOpcion": "CALL", "precioEjercicio": 100,
                "diasVencimiento": 1, "ultimoPrecio": 2.1, "volumen": 20,
                "puntas": [{"precioCompra": 2.0, "precioVenta": 2.2}]
            }]
        });
        let frame = parse_market_frame("GAL", body).unwrap();
        assert_eq!(frame.underlying.ask, Some(100.6));
        assert_eq!(frame.options[0].bid, Some(2.0));
        assert_eq!(frame.options[0].kind, OptionKind::Call);
    }

    #[test]
    fn parses_live_order_response_without_assuming_execution() {
        let request = OrderRequest {
            operation_id: "op-1".into(),
            symbol: "GAL-C-100".into(),
            quantity: 5,
            market_price: 2.0,
            limit_price: 2.1,
            side: OrderSide::Buy,
        };
        let execution = parse_order_execution(
            &request,
            serde_json::json!({"numeroOperacion": "42", "estado": "pendiente"}),
        )
        .unwrap();
        assert_eq!(execution.status, OrderStatus::Pending);
        assert_eq!(execution.filled_quantity, 0);
    }

    #[test]
    fn parses_option_positions_from_iol_portfolio() {
        let positions = parse_account_positions(&serde_json::json!({
            "activos": [{
                "titulo": {"simbolo": "GFGC100", "tipo": "Opciones"},
                "cantidad": 3,
                "ppc": 1250.5
            }]
        }))
        .unwrap();
        assert_eq!(positions.len(), 1);
        assert_eq!(positions[0].symbol, "GFGC100");
        assert_eq!(positions[0].quantity, 3);
        assert_eq!(positions[0].average_price, Some(1250.5));
        assert!(positions[0].is_option);
    }

    #[test]
    fn parses_pending_option_orders() {
        let orders = parse_pending_orders(&serde_json::json!({
            "operaciones": [{
                "numero": 87044496,
                "simbolo": "GFGV100",
                "tipoInstrumento": "Opcion PUT",
                "tipoOperacion": "venta",
                "cantidad": 2
            }]
        }))
        .unwrap();
        assert_eq!(orders.len(), 1);
        assert_eq!(orders[0].broker_order_id, "87044496");
        assert_eq!(orders[0].kind, Some(PositionKind::Put));
        assert_eq!(orders[0].side, Some(OrderSide::Sell));
        assert!(orders[0].is_option);
    }

    #[tokio::test]
    async fn authenticates_and_fetches_market_frame_against_http_contract() {
        let market = r#"{"subyacente":{"ultimoPrecio":100.5},"timestamp_secs":10,"opciones":[{"simbolo":"GAL-C-100","tipoOpcion":"CALL","precioEjercicio":100,"diasVencimiento":1,"ultimoPrecio":2.1,"volumen":20,"bid":2.0,"ask":2.2}]}"#;
        let Some((base_url, server)) = mock_server(vec![
            (
                "POST /token",
                "200 OK",
                r#"{"access_token":"access-1","refresh_token":"refresh-1","expires_in":3600,"token_type":"Bearer"}"#,
            ),
            ("GET /api/v2/opciones/GAL", "200 OK", market),
        ]) else {
            return;
        };
        let mut client =
            IolClient::new(base_url, "user".into(), "pass".into(), String::new()).unwrap();
        let frame = client.market_frame("GAL").await.unwrap();
        assert_eq!(frame.underlying.last, 100.5);
        assert_eq!(frame.options.len(), 1);
        server.join().unwrap();
    }

    #[tokio::test]
    async fn refreshes_once_after_unauthorized_market_response() {
        let market = r#"{"subyacente":{"ultimoPrecio":100.5},"opciones":[{"simbolo":"GAL-P-100","tipoOpcion":"PUT","precioEjercicio":100,"diasVencimiento":1,"ultimoPrecio":2.1,"volumen":20,"bid":2.0,"ask":2.2}]}"#;
        let token = r#"{"access_token":"access-1","refresh_token":"refresh-1","expires_in":3600,"token_type":"Bearer"}"#;
        let refreshed = r#"{"access_token":"access-2","refresh_token":"refresh-2","expires_in":3600,"token_type":"Bearer"}"#;
        let Some((base_url, server)) = mock_server(vec![
            ("POST /token", "200 OK", token),
            ("GET /api/v2/opciones/GAL", "401 Unauthorized", "{}"),
            ("POST /token", "200 OK", refreshed),
            ("GET /api/v2/opciones/GAL", "200 OK", market),
        ]) else {
            return;
        };
        let mut client =
            IolClient::new(base_url, "user".into(), "pass".into(), String::new()).unwrap();
        let frame = client.market_frame("GAL").await.unwrap();
        assert_eq!(frame.options[0].kind, OptionKind::Put);
        server.join().unwrap();
    }

    #[tokio::test]
    async fn fetches_portfolio_and_pending_orders_for_startup_reconciliation() {
        let token = r#"{"access_token":"access-1","refresh_token":"refresh-1","expires_in":3600,"token_type":"Bearer"}"#;
        let portfolio = r#"{"activos":[{"titulo":{"simbolo":"GFGC100","tipo":"Opciones"},"cantidad":1,"ppc":1200.0}]}"#;
        let operations = r#"{"operaciones":[]}"#;
        let Some((base_url, server)) = mock_server(vec![
            ("POST /token", "200 OK", token),
            ("GET /api/v2/portafolio/Argentina", "200 OK", portfolio),
            (
                "GET /api/v2/operaciones?filtro.estado=Pendientes&filtro.pais=Argentina",
                "200 OK",
                operations,
            ),
        ]) else {
            return;
        };
        let mut client =
            IolClient::new(base_url, "user".into(), "pass".into(), String::new()).unwrap();

        let account = client.account_snapshot().await.unwrap();

        assert_eq!(account.positions.len(), 1);
        assert!(account.pending_orders.is_empty());
        server.join().unwrap();
    }

    fn mock_server(
        responses: Vec<(&'static str, &'static str, &'static str)>,
    ) -> Option<(String, thread::JoinHandle<()>)> {
        let listener = TcpListener::bind("127.0.0.1:0").ok()?;
        let address = listener.local_addr().unwrap();
        let handle = thread::spawn(move || {
            for (expected_request, status, body) in responses {
                let (mut stream, _) = listener.accept().unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                let mut buffer = [0_u8; 8_192];
                let read = stream.read(&mut buffer).unwrap();
                let request = String::from_utf8_lossy(&buffer[..read]);
                assert!(
                    request.contains(expected_request),
                    "request inesperado: {request}"
                );
                let response = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                stream.write_all(response.as_bytes()).unwrap();
            }
        });
        Some((format!("http://{address}"), handle))
    }
}
