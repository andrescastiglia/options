use std::{
    collections::HashMap,
    time::{Duration, Instant},
};

use futures_util::{SinkExt, StreamExt};
use reqwest::{Client, Response, StatusCode};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::{
    sync::mpsc,
    task::JoinHandle,
    time::{sleep, MissedTickBehavior},
};
use tokio_tungstenite::{connect_async, tungstenite::Message};
use zeroize::{Zeroize, Zeroizing};

use crate::{
    broker::{
        AccountOrder, AccountPosition, AccountSnapshot, OrderExecution, OrderRequest, OrderSide,
        OrderStatus,
    },
    market::{MarketFrame, OptionKind, OptionQuote, UnderlyingQuote},
    secrets::{decrypt_for_this_machine, SecretError},
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
    #[error("no se pudo usar la contraseña guardada: {0}")]
    Secret(#[from] SecretError),
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TokenResponse {
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub expires_in: u64,
    pub token_type: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AccountProfile {
    pub account_number: String,
    pub first_name: String,
    pub last_name: String,
}

impl AccountProfile {
    pub fn full_name(&self) -> String {
        format!("{} {}", self.first_name, self.last_name)
            .trim()
            .to_string()
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FeeComponent {
    pub kind: String,
    pub net: f64,
    pub vat: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CostCalibration {
    pub operation_number: String,
    pub operation_amount: f64,
    pub commission_percentage: f64,
    pub vat_percentage: f64,
    pub other_fees_percentage: f64,
    pub total_cost_percentage: f64,
    pub components: Vec<FeeComponent>,
    pub observed_at_secs: i64,
    pub instrument_is_option: bool,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(rename_all = "PascalCase")]
pub struct AccountMovement {
    #[serde(default)]
    pub id_movimiento: Option<u64>,
    #[serde(default)]
    pub cuenta_comitente: String,
    #[serde(default)]
    pub tipo: String,
    #[serde(default)]
    pub estado: String,
    #[serde(default)]
    pub monto: Option<f64>,
    #[serde(default)]
    pub cantidad: Option<f64>,
    #[serde(default)]
    pub simbolo: String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum IolRealtimeEvent {
    Status(String),
    Movement(AccountMovement),
}

#[derive(Debug, Default)]
pub struct IolStartupContext {
    pub profile: Option<AccountProfile>,
    pub calibration: Option<CostCalibration>,
    pub warnings: Vec<String>,
}

#[derive(Debug)]
struct MovementStream {
    events: mpsc::UnboundedReceiver<IolRealtimeEvent>,
    commands: mpsc::UnboundedSender<MovementCommand>,
    task: JoinHandle<()>,
}

#[derive(Debug)]
enum MovementCommand {
    Shutdown,
}

#[derive(Debug)]
pub struct IolClient {
    http: Client,
    base_url: String,
    username: Zeroizing<String>,
    encrypted_password: Zeroizing<String>,
    refresh_token: Zeroizing<String>,
    access_token: Option<Zeroizing<String>>,
    access_expires_at: Instant,
    failures: u32,
    circuit_open_until: Option<Instant>,
    catalog_cache_ttl: Duration,
    option_catalog: HashMap<String, CachedOptionCatalog>,
    websocket_url: String,
    movement_stream: Option<MovementStream>,
}

#[derive(Debug, Clone)]
struct OptionContract {
    symbol: String,
    kind: OptionKind,
    strike: f64,
    expiry_days: u32,
}

#[derive(Debug)]
struct CachedOptionCatalog {
    expires_at: Instant,
    contracts: Vec<OptionContract>,
}

impl IolClient {
    pub fn new(
        base_url: impl Into<String>,
        username: String,
        encrypted_password: String,
        refresh_token: String,
    ) -> Result<Self, IolClientError> {
        let http = Client::builder()
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(15))
            .user_agent("options-trading/0.1")
            .build()?;
        let encrypted_password = Zeroizing::new(encrypted_password);
        // Valida al crear el cliente, pero no conserva esta copia en texto plano.
        drop(decrypt_for_this_machine(&encrypted_password)?);
        Ok(Self {
            http,
            base_url: base_url.into().trim_end_matches('/').to_string(),
            username: Zeroizing::new(username),
            encrypted_password,
            refresh_token: Zeroizing::new(refresh_token),
            access_token: None,
            access_expires_at: Instant::now(),
            failures: 0,
            circuit_open_until: None,
            catalog_cache_ttl: Duration::from_secs(60),
            option_catalog: HashMap::new(),
            websocket_url: "wss://websocket-movements.invertironline.com/".into(),
            movement_stream: None,
        })
    }

    pub fn with_catalog_cache_ttl(mut self, seconds: u64) -> Self {
        self.catalog_cache_ttl = Duration::from_secs(seconds);
        self
    }

    pub fn with_websocket_url(mut self, url: impl Into<String>) -> Self {
        self.websocket_url = url.into();
        self
    }

    pub async fn startup_context(&mut self) -> Result<IolStartupContext, IolClientError> {
        self.ensure_access_token().await?;
        self.start_movement_stream();
        let mut context = IolStartupContext::default();
        match self.authorized_json_get("/api/v2/datos-perfil").await {
            Ok(body) => match parse_account_profile(&body) {
                Ok(profile) => context.profile = Some(profile),
                Err(error) => context.warnings.push(error.to_string()),
            },
            Err(error) => context
                .warnings
                .push(format!("perfil IOL no disponible: {error}")),
        }
        match self.latest_cost_calibration().await {
            Ok(calibration) => context.calibration = calibration,
            Err(error) => context
                .warnings
                .push(format!("aranceles IOL no disponibles: {error}")),
        }
        Ok(context)
    }

    pub async fn latest_cost_calibration(
        &mut self,
    ) -> Result<Option<CostCalibration>, IolClientError> {
        let operations = self
            .authorized_json_get(
                "/api/v2/operaciones?filtro.estado=Terminadas&filtro.pais=Argentina",
            )
            .await?;
        let Some(operation_number) = latest_operation_number(&operations) else {
            return Ok(None);
        };
        let detail = self
            .authorized_json_get(&format!("/api/v2/operaciones/{operation_number}"))
            .await?;
        parse_cost_calibration(&operation_number, &detail).map(Some)
    }

    pub fn drain_realtime_events(&mut self) -> Vec<IolRealtimeEvent> {
        let Some(stream) = &mut self.movement_stream else {
            return Vec::new();
        };
        let mut events = Vec::new();
        while let Ok(event) = stream.events.try_recv() {
            events.push(event);
        }
        events
    }

    pub async fn shutdown(&mut self) {
        let Some(mut stream) = self.movement_stream.take() else {
            return;
        };
        let _ = stream.commands.send(MovementCommand::Shutdown);
        if tokio::time::timeout(Duration::from_secs(2), &mut stream.task)
            .await
            .is_err()
        {
            stream.task.abort();
        }
    }

    fn start_movement_stream(&mut self) {
        if self.movement_stream.is_some() {
            return;
        }
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let (command_tx, command_rx) = mpsc::unbounded_channel();
        let url = self.websocket_url.clone();
        let username = Zeroizing::new(self.username.to_string());
        let encrypted_password = self.encrypted_password.clone();
        let task = tokio::spawn(run_movement_stream(
            url,
            username,
            encrypted_password,
            event_tx,
            command_rx,
        ));
        self.movement_stream = Some(MovementStream {
            events: event_rx,
            commands: command_tx,
            task,
        });
    }

    pub async fn authenticate(&mut self) -> Result<(), IolClientError> {
        let mut password = decrypt_for_this_machine(&self.encrypted_password)?;
        let request = self.http.post(format!("{}/token", self.base_url)).form(&[
            ("username", self.username.as_str()),
            ("password", password.as_str()),
            ("grant_type", "password"),
        ]);
        password.zeroize();
        let response = request.send().await?;
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
        let ticker = ticker.to_ascii_uppercase();
        let underlying = self
            .authorized_json_get(&format!("/api/v2/BCBA/Titulos/{ticker}/Cotizacion"))
            .await?;
        let contracts = self.option_contracts(&ticker).await?;
        let option_quotes = self
            .authorized_json_get("/api/v2/Cotizaciones/Opciones/Todas/Argentina")
            .await?;
        parse_market_frame(&ticker, underlying, &contracts, option_quotes)
    }

    async fn option_contracts(
        &mut self,
        ticker: &str,
    ) -> Result<Vec<OptionContract>, IolClientError> {
        if let Some(cached) = self.option_catalog.get(ticker) {
            if Instant::now() < cached.expires_at {
                return Ok(cached.contracts.clone());
            }
        }
        let body = self
            .authorized_json_get(&format!("/api/v2/BCBA/Titulos/{ticker}/Opciones"))
            .await?;
        let contracts = parse_option_catalog(ticker, &body)?;
        self.option_catalog.insert(
            ticker.to_string(),
            CachedOptionCatalog {
                expires_at: Instant::now() + self.catalog_cache_ttl,
                contracts: contracts.clone(),
            },
        );
        Ok(contracts)
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

async fn run_movement_stream(
    url: String,
    username: Zeroizing<String>,
    encrypted_password: Zeroizing<String>,
    events: mpsc::UnboundedSender<IolRealtimeEvent>,
    mut commands: mpsc::UnboundedReceiver<MovementCommand>,
) {
    let mut retry_delay = Duration::from_secs(1);
    loop {
        if commands.try_recv().is_ok() {
            return;
        }
        let result = connect_async(&url).await;
        let (mut socket, _) = match result {
            Ok(connection) => connection,
            Err(error) => {
                if events
                    .send(IolRealtimeEvent::Status(format!(
                        "Se cortó la conexión con los movimientos de IOL: {error}; volviendo a intentar"
                    )))
                    .is_err()
                {
                    return;
                }
                tokio::select! {
                    _ = sleep(retry_delay) => {}
                    command = commands.recv() => {
                        if matches!(command, Some(MovementCommand::Shutdown) | None) {
                            return;
                        }
                    }
                }
                retry_delay = (retry_delay * 2).min(Duration::from_secs(60));
                continue;
            }
        };

        #[derive(Serialize)]
        struct Authentication<'a> {
            action: &'static str,
            username: &'a str,
            password: &'a str,
        }
        let mut password = match decrypt_for_this_machine(&encrypted_password) {
            Ok(password) => password,
            Err(error) => {
                let _ = events.send(IolRealtimeEvent::Status(format!(
                    "No se pudo usar la contraseña guardada: {error}"
                )));
                return;
            }
        };
        let mut auth_text = match serde_json::to_string(&Authentication {
            action: "auth",
            username: username.as_str(),
            password: password.as_str(),
        }) {
            Ok(text) => Zeroizing::new(text),
            Err(_) => return,
        };
        password.zeroize();
        let sent = socket.send(Message::Text(auth_text.as_str().into())).await;
        auth_text.zeroize();
        if sent.is_err() {
            continue;
        }
        let authenticated = match tokio::time::timeout(Duration::from_secs(10), socket.next()).await
        {
            Ok(Some(Ok(message))) => websocket_auth_succeeded(&message),
            _ => false,
        };
        if !authenticated {
            let _ = events.send(IolRealtimeEvent::Status(
                "IOL no permitió recibir movimientos de la cuenta; los precios y demás datos siguen disponibles"
                    .into(),
            ));
            return;
        }

        retry_delay = Duration::from_secs(1);
        if events
            .send(IolRealtimeEvent::Status(
                "Conectado con los movimientos de la cuenta IOL".into(),
            ))
            .is_err()
        {
            return;
        }
        let mut heartbeat = tokio::time::interval(Duration::from_secs(30));
        heartbeat.set_missed_tick_behavior(MissedTickBehavior::Delay);
        heartbeat.tick().await;

        let reconnect = loop {
            tokio::select! {
                command = commands.recv() => {
                    if matches!(command, Some(MovementCommand::Shutdown) | None) {
                        let _ = socket.send(Message::Text(
                            serde_json::json!({"action": "disconnect"}).to_string().into()
                        )).await;
                        let _ = socket.close(None).await;
                        return;
                    }
                }
                _ = heartbeat.tick() => {
                    if socket.send(Message::Text(
                        serde_json::json!({"action": "ping"}).to_string().into()
                    )).await.is_err() {
                        break true;
                    }
                }
                message = socket.next() => {
                    match message {
                        Some(Ok(Message::Text(text))) => {
                            if let Some(event) = parse_realtime_message(text.as_ref()) {
                                if events.send(event).is_err() {
                                    return;
                                }
                            }
                        }
                        Some(Ok(Message::Ping(payload))) => {
                            if socket.send(Message::Pong(payload)).await.is_err() {
                                break true;
                            }
                        }
                        Some(Ok(Message::Close(_))) | Some(Err(_)) | None => break true,
                        _ => {}
                    }
                }
            }
        };
        if reconnect {
            let _ = events.send(IolRealtimeEvent::Status(
                "Se perdió la conexión con los movimientos de IOL; volviendo a intentar".into(),
            ));
        }
    }
}

fn websocket_auth_succeeded(message: &Message) -> bool {
    let Message::Text(text) = message else {
        return false;
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(text.as_ref()) else {
        return false;
    };
    integer_value(&value, &["code", "Code"]) == Some(200)
        && text_value(&value, &["type", "Type"])
            .is_some_and(|kind| kind.eq_ignore_ascii_case("success"))
}

fn parse_realtime_message(text: &str) -> Option<IolRealtimeEvent> {
    let value = serde_json::from_str::<serde_json::Value>(text).ok()?;
    if integer_value(&value, &["code", "Code"]) == Some(200) {
        return None;
    }
    serde_json::from_value::<AccountMovement>(value)
        .ok()
        .filter(|movement| {
            movement.id_movimiento.is_some()
                || !movement.tipo.is_empty()
                || !movement.simbolo.is_empty()
        })
        .map(IolRealtimeEvent::Movement)
}

fn parse_account_profile(body: &serde_json::Value) -> Result<AccountProfile, IolClientError> {
    let object = body
        .as_object()
        .ok_or_else(|| IolClientError::InvalidResponse("perfil no es objeto".into()))?;
    let account_number = scalar_text(object, &["numeroCuenta", "numero", "accountNumber"])
        .ok_or_else(|| IolClientError::InvalidResponse("número de cuenta ausente".into()))?;
    let first_name = text_optional(object, &["nombre", "firstName"])
        .unwrap_or_default()
        .to_string();
    let last_name = text_optional(object, &["apellido", "lastName"])
        .unwrap_or_default()
        .to_string();
    Ok(AccountProfile {
        account_number,
        first_name,
        last_name,
    })
}

fn latest_operation_number(body: &serde_json::Value) -> Option<String> {
    let operations = body.as_array().or_else(|| {
        body.get("operaciones")
            .and_then(serde_json::Value::as_array)
    })?;
    operations
        .iter()
        .filter_map(|operation| {
            let object = operation.as_object()?;
            let descriptor = text_optional(
                object,
                &["tipoInstrumento", "instrumento", "descripcion", "tipo"],
            );
            if descriptor.is_some_and(|value| !is_option_descriptor(value)) {
                return None;
            }
            let number = scalar_text(object, &["numero", "numeroOperacion"])?;
            let timestamp = text_optional(
                object,
                &["fechaOperada", "fechaOrden", "fechaAlta", "fecha"],
            )
            .unwrap_or_default();
            Some((timestamp.to_string(), number))
        })
        .max_by(|left, right| left.0.cmp(&right.0))
        .map(|(_, number)| number)
}

fn parse_cost_calibration(
    operation_number: &str,
    body: &serde_json::Value,
) -> Result<CostCalibration, IolClientError> {
    let object = body.as_object().ok_or_else(|| {
        IolClientError::InvalidResponse("detalle de operación no es objeto".into())
    })?;
    let operation_amount = optional_number(object, &["montoOperado", "monto", "montoOperacion"])
        .map(f64::abs)
        .filter(|amount| *amount > 0.0)
        .ok_or_else(|| IolClientError::InvalidResponse("monto operado ausente o cero".into()))?;
    let instrument_is_option = text_optional(
        object,
        &["tipoInstrumento", "instrumento", "descripcion", "tipo"],
    )
    .is_some_and(is_option_descriptor);
    let observed_at_secs = integer(
        object,
        &["timestamp_secs", "timestamp", "fechaOperadaTimestamp"],
    )
    .map(|timestamp| {
        if timestamp > 10_000_000_000 {
            timestamp / 1_000
        } else {
            timestamp
        }
    })
    .or_else(|| {
        text_optional(
            object,
            &["fechaOperada", "fechaOrden", "fechaAlta", "fecha"],
        )
        .and_then(date_days_since_epoch)
        .map(|days| days.saturating_mul(86_400))
    })
    .unwrap_or_else(|| {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_secs() as i64)
            .unwrap_or_default()
    });
    let fees = object
        .get("aranceles")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| IolClientError::InvalidResponse("aranceles ausentes".into()))?;
    let components = fees
        .iter()
        .filter_map(|fee| {
            let fee = fee.as_object()?;
            let kind = text_optional(fee, &["tipo", "descripcion"])
                .unwrap_or("Arancel")
                .to_string();
            let net = optional_number(fee, &["neto", "importe", "monto"])
                .unwrap_or_default()
                .abs();
            let vat = optional_number(fee, &["iva", "IVA"])
                .unwrap_or_default()
                .abs();
            (net > 0.0 || vat > 0.0).then_some(FeeComponent { kind, net, vat })
        })
        .collect::<Vec<_>>();
    if components.is_empty() {
        return Err(IolClientError::InvalidResponse(
            "la operación no contiene aranceles valorizados".into(),
        ));
    }
    let total_net = components.iter().map(|fee| fee.net).sum::<f64>();
    let total_vat = components.iter().map(|fee| fee.vat).sum::<f64>();
    let commission_net = components
        .iter()
        .filter(|fee| is_commission(&fee.kind))
        .map(|fee| fee.net)
        .sum::<f64>();
    let (commission_net, other_net) = if commission_net > 0.0 {
        (commission_net, (total_net - commission_net).max(0.0))
    } else {
        // Algunos contratos históricos no distinguen la comisión del resto de los
        // aranceles. En ese caso se conserva el costo total asignándolo a comisión.
        (total_net, 0.0)
    };
    Ok(CostCalibration {
        operation_number: operation_number.to_string(),
        operation_amount,
        commission_percentage: commission_net / operation_amount * 100.0,
        vat_percentage: if total_net > 0.0 {
            total_vat / total_net * 100.0
        } else {
            0.0
        },
        other_fees_percentage: other_net / operation_amount * 100.0,
        total_cost_percentage: (total_net + total_vat) / operation_amount * 100.0,
        components,
        observed_at_secs,
        instrument_is_option,
    })
}

fn is_commission(kind: &str) -> bool {
    let normalized = kind.to_lowercase();
    normalized.contains("comision")
        || normalized.contains("comisión")
        || normalized.contains("honorario")
        || normalized.contains("corretaje")
}

fn integer_value(value: &serde_json::Value, names: &[&str]) -> Option<i64> {
    let object = value.as_object()?;
    integer(object, names)
}

fn text_value<'a>(value: &'a serde_json::Value, names: &[&str]) -> Option<&'a str> {
    let object = value.as_object()?;
    text_optional(object, names)
}

fn error_is_retryable(error: &IolClientError) -> bool {
    matches!(error, IolClientError::Http(_))
}

fn parse_market_frame(
    ticker: &str,
    underlying_body: serde_json::Value,
    contracts: &[OptionContract],
    option_quotes_body: serde_json::Value,
) -> Result<MarketFrame, IolClientError> {
    let underlying = underlying_body.as_object().ok_or_else(|| {
        IolClientError::InvalidResponse("cotizacion subyacente no es objeto".into())
    })?;
    let last = number(underlying, &["ultimoPrecio", "ultimo", "last", "price"])?;
    let timestamp_secs = market_timestamp(underlying).unwrap_or_else(unix_now);
    let (bid, ask) = book_prices(underlying);
    let quote_values = collection(&option_quotes_body, &["titulos", "opciones", "options"])?;
    let quotes_by_symbol = quote_values
        .iter()
        .filter_map(|value| {
            let object = value.as_object()?;
            let symbol = text_optional(object, &["simbolo", "symbol", "ticker"])?;
            Some((symbol.to_ascii_uppercase(), object))
        })
        .collect::<HashMap<_, _>>();
    let options = contracts
        .iter()
        .filter_map(|contract| {
            let quote = quotes_by_symbol.get(&contract.symbol.to_ascii_uppercase())?;
            let option = parse_option_quote(ticker, timestamp_secs, contract, quote).ok()?;
            option.validate(ticker).ok()?;
            Some(option)
        })
        .collect::<Vec<_>>();
    if options.is_empty() {
        return Err(IolClientError::InvalidResponse(format!(
            "el panel de cotizaciones no contiene contratos de {ticker} (catalogo: {})",
            contracts.len()
        )));
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

fn parse_option_catalog(
    ticker: &str,
    body: &serde_json::Value,
) -> Result<Vec<OptionContract>, IolClientError> {
    let values = collection(body, &["opciones", "titulos", "options"])?;
    let today = unix_now().div_euclid(86_400);
    let contracts = values
        .iter()
        .filter_map(|value| {
            let object = value.as_object()?;
            let underlying = text_optional(
                object,
                &["simboloSubyacente", "underlyingSymbol", "underlying"],
            )?;
            if !underlying.eq_ignore_ascii_case(ticker) {
                return None;
            }
            let symbol = text_optional(object, &["simbolo", "symbol", "ticker"])?;
            let kind = parse_option_kind(text_optional(object, &["tipoOpcion", "kind"])?)?;
            let strike = option_strike(object)?;
            let expiry_days = integer(object, &["diasVencimiento", "expiryDays"])
                .map(|days| days.max(0) as u32)
                .or_else(|| {
                    text_optional(object, &["fechaVencimiento", "expirationDate"])
                        .and_then(date_days_since_epoch)
                        .map(|expiry| expiry.saturating_sub(today).max(0) as u32)
                })?;
            Some(OptionContract {
                symbol: symbol.to_string(),
                kind,
                strike,
                expiry_days,
            })
        })
        .collect::<Vec<_>>();
    if contracts.is_empty() {
        return Err(IolClientError::InvalidResponse(format!(
            "IOL no devolvio contratos de opciones para {ticker}"
        )));
    }
    Ok(contracts)
}

fn parse_option_quote(
    ticker: &str,
    default_timestamp: i64,
    contract: &OptionContract,
    object: &serde_json::Map<String, serde_json::Value>,
) -> Result<OptionQuote, IolClientError> {
    let last = number(object, &["ultimoPrecio", "ultimo", "last", "price"])?;
    let (bid, ask) = book_prices(object);
    Ok(OptionQuote {
        symbol: contract.symbol.clone(),
        underlying: ticker.into(),
        kind: contract.kind,
        strike: optional_number(object, &["precioEjercicio", "strike"]).unwrap_or(contract.strike),
        expiry_days: contract.expiry_days,
        last,
        bid,
        ask,
        volume: optional_number(object, &["volumen", "volume"])
            .unwrap_or(0.0)
            .max(0.0) as u64,
        timestamp_secs: market_timestamp(object).unwrap_or(default_timestamp),
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
    let executable_price =
        |value: Option<f64>| value.filter(|price| price.is_finite() && *price > 0.0);
    let direct_bid = executable_price(optional_number(object, &["bid", "precioCompra"]));
    let direct_ask = executable_price(optional_number(
        object,
        &["ask", "precioVenta", "precioAsk"],
    ));
    let point = object.get("puntas").and_then(|value| {
        value.as_object().or_else(|| {
            value
                .as_array()
                .and_then(|points| points.first())
                .and_then(serde_json::Value::as_object)
        })
    });
    (
        direct_bid.or_else(|| {
            point.and_then(|value| {
                executable_price(optional_number(value, &["precioCompra", "bid"]))
            })
        }),
        direct_ask.or_else(|| {
            point
                .and_then(|value| executable_price(optional_number(value, &["precioVenta", "ask"])))
        }),
    )
}

fn option_strike(object: &serde_json::Map<String, serde_json::Value>) -> Option<f64> {
    optional_number(object, &["precioEjercicio", "strike"])
        .filter(|strike| strike.is_finite() && *strike > 0.0)
        .or_else(|| {
            text_optional(object, &["descripcion", "description"])
                .and_then(strike_from_option_description)
        })
}

fn strike_from_option_description(description: &str) -> Option<f64> {
    let raw = description.split_whitespace().nth(2)?;
    let raw = raw.trim_matches(|character: char| {
        !character.is_ascii_digit() && character != ',' && character != '.'
    });
    let comma = raw.rfind(',');
    let dot = raw.rfind('.');
    let normalized = match (comma, dot) {
        (Some(comma), Some(dot)) if comma < dot => raw.replace(',', ""),
        (Some(_), Some(_)) => raw.replace('.', "").replace(',', "."),
        (Some(comma), None) if raw.len().saturating_sub(comma + 1) == 3 => raw.replace(',', ""),
        (Some(_), None) => raw.replace(',', "."),
        _ => raw.to_string(),
    };
    normalized
        .parse::<f64>()
        .ok()
        .filter(|strike| strike.is_finite() && *strike > 0.0)
}

fn parse_option_kind(value: &str) -> Option<OptionKind> {
    match value.trim().to_ascii_lowercase().as_str() {
        "call" | "c" | "compra" => Some(OptionKind::Call),
        "put" | "p" | "venta" => Some(OptionKind::Put),
        _ => None,
    }
}

fn market_timestamp(object: &serde_json::Map<String, serde_json::Value>) -> Option<i64> {
    integer(object, &["timestamp_secs", "timestamp"])
}

fn date_days_since_epoch(value: &str) -> Option<i64> {
    let date = value.get(..10)?;
    let mut parts = date.split('-');
    let year = parts.next()?.parse::<i64>().ok()?;
    let month = parts.next()?.parse::<i64>().ok()?;
    let day = parts.next()?.parse::<i64>().ok()?;
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    let adjusted_year = year - i64::from(month <= 2);
    let era = adjusted_year.div_euclid(400);
    let year_of_era = adjusted_year - era * 400;
    let adjusted_month = month + if month > 2 { -3 } else { 9 };
    let day_of_year = (153 * adjusted_month + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    Some(era * 146_097 + day_of_era - 719_468)
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

    fn encrypted_test_password() -> String {
        crate::secrets::encrypt_for_this_machine("pass").unwrap()
    }

    #[test]
    fn parses_profile_for_tui() {
        let profile = parse_account_profile(&serde_json::json!({
            "nombre": "Ada",
            "apellido": "Lovelace",
            "numeroCuenta": 123456
        }))
        .unwrap();
        assert_eq!(profile.account_number, "123456");
        assert_eq!(profile.full_name(), "Ada Lovelace");
    }

    #[test]
    fn selects_latest_finished_operation() {
        let operation = latest_operation_number(&serde_json::json!([
            {"numero": 10, "fechaOperada": "2026-08-20T10:00:00-03:00"},
            {"numero": 11, "fechaOperada": "2026-08-21T09:00:00-03:00"}
        ]));
        assert_eq!(operation.as_deref(), Some("11"));
    }

    #[test]
    fn calibrates_commission_vat_and_other_fees() {
        let calibration = parse_cost_calibration(
            "42",
            &serde_json::json!({
                "monto": 10_000.0,
                "aranceles": [
                    {"tipo": "Comisión IOL", "neto": 20.0, "iva": 4.2},
                    {"tipo": "Derecho de mercado", "neto": 5.0, "iva": 1.05}
                ]
            }),
        )
        .unwrap();
        assert!((calibration.commission_percentage - 0.2).abs() < 1e-9);
        assert!((calibration.vat_percentage - 21.0).abs() < 1e-9);
        assert!((calibration.other_fees_percentage - 0.05).abs() < 1e-9);
        assert!((calibration.total_cost_percentage - 0.3025).abs() < 1e-9);
    }

    #[test]
    fn parses_documented_websocket_movement() {
        let event = parse_realtime_message(
            r#"{"IdMovimiento":1111,"Estado":"Confirmada","Tipo":"Extraccion","Monto":253.5,"CuentaComitente":"314738","Simbolo":"AAPL"}"#,
        )
        .unwrap();
        let IolRealtimeEvent::Movement(movement) = event else {
            panic!("se esperaba movimiento");
        };
        assert_eq!(movement.id_movimiento, Some(1111));
        assert_eq!(movement.cuenta_comitente, "314738");
        assert_eq!(movement.monto, Some(253.5));
    }
    use std::{
        io::{Read, Write},
        net::TcpListener,
        thread,
    };

    #[test]
    fn parses_market_contract_with_nested_book() {
        let underlying = serde_json::json!({
            "ultimoPrecio": 100.5,
            "puntas": [{"precioCompra": 100.4, "precioVenta": 100.6}],
            "timestamp_secs": 10
        });
        let contracts = vec![OptionContract {
            symbol: "GFGC100".into(),
            kind: OptionKind::Call,
            strike: 100.0,
            expiry_days: 1,
        }];
        let quotes = serde_json::json!({"titulos": [{
            "simbolo": "GFGC100", "ultimoPrecio": 2.1, "volumen": 20,
            "puntas": {"precioCompra": 2.0, "precioVenta": 2.2}
        }]});
        let frame = parse_market_frame("GGAL", underlying, &contracts, quotes).unwrap();
        assert_eq!(frame.underlying.ask, Some(100.6));
        assert_eq!(frame.options[0].bid, Some(2.0));
        assert_eq!(frame.options[0].kind, OptionKind::Call);
    }

    #[test]
    fn parses_live_iol_strike_from_catalog_description() {
        let catalog = serde_json::json!([{
            "cotizacion": {"ultimoPrecio": 2424.629},
            "simboloSubyacente": "GGAL",
            "fechaVencimiento": "2026-08-21T15:30:00",
            "tipoOpcion": "Call",
            "simbolo": "GFGC4200AG",
            "descripcion": "Call GGAL 4,200.00 Vencimiento: 21/08/2026"
        }]);

        let contracts = parse_option_catalog("GGAL", &catalog).unwrap();

        assert_eq!(contracts.len(), 1);
        assert_eq!(contracts[0].strike, 4200.0);
    }

    #[test]
    fn parses_live_iol_panel_with_null_option_metadata_and_empty_book() {
        let underlying = serde_json::json!({"ultimoPrecio": 6600.0});
        let contracts = vec![OptionContract {
            symbol: "GFGC4200AG".into(),
            kind: OptionKind::Call,
            strike: 4200.0,
            expiry_days: 0,
        }];
        let quotes = serde_json::json!({"titulos": [{
            "simbolo": "GFGC4200AG",
            "ultimoPrecio": 2424.629,
            "precioEjercicio": null,
            "tipoOpcion": null,
            "fechaVencimiento": null,
            "volumen": 2.0,
            "puntas": {
                "cantidadCompra": 0.0,
                "precioCompra": 0.0,
                "precioVenta": 0.0,
                "cantidadVenta": 0.0
            }
        }]});

        let frame = parse_market_frame("GGAL", underlying, &contracts, quotes).unwrap();

        assert_eq!(frame.options.len(), 1);
        assert_eq!(frame.options[0].strike, 4200.0);
        assert_eq!(frame.options[0].bid, None);
        assert_eq!(frame.options[0].ask, None);
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
        let underlying = r#"{"ultimoPrecio":100.5,"timestamp_secs":10}"#;
        let catalog = r#"[{"simboloSubyacente":"GGAL","simbolo":"GFGC100","tipoOpcion":"CALL","precioEjercicio":100,"diasVencimiento":1}]"#;
        let quotes = r#"{"titulos":[{"simbolo":"GFGC100","ultimoPrecio":2.1,"volumen":20,"puntas":{"precioCompra":2.0,"precioVenta":2.2}}]}"#;
        let Some((base_url, server)) = mock_server(vec![
            (
                "POST /token",
                "200 OK",
                r#"{"access_token":"access-1","refresh_token":"refresh-1","expires_in":3600,"token_type":"Bearer"}"#,
            ),
            (
                "GET /api/v2/BCBA/Titulos/GGAL/Cotizacion",
                "200 OK",
                underlying,
            ),
            ("GET /api/v2/BCBA/Titulos/GGAL/Opciones", "200 OK", catalog),
            (
                "GET /api/v2/Cotizaciones/Opciones/Todas/Argentina",
                "200 OK",
                quotes,
            ),
        ]) else {
            return;
        };
        let mut client = IolClient::new(
            base_url,
            "user".into(),
            encrypted_test_password(),
            String::new(),
        )
        .unwrap();
        let frame = client.market_frame("ggal").await.unwrap();
        assert_eq!(frame.underlying.last, 100.5);
        assert_eq!(frame.options.len(), 1);
        server.join().unwrap();
    }

    #[tokio::test]
    async fn refreshes_once_after_unauthorized_market_response() {
        let underlying = r#"{"ultimoPrecio":100.5}"#;
        let catalog = r#"[{"simboloSubyacente":"GGAL","simbolo":"GFGV100","tipoOpcion":"PUT","precioEjercicio":100,"diasVencimiento":1}]"#;
        let quotes = r#"{"titulos":[{"simbolo":"GFGV100","ultimoPrecio":2.1,"volumen":20,"bid":2.0,"ask":2.2}]}"#;
        let token = r#"{"access_token":"access-1","refresh_token":"refresh-1","expires_in":3600,"token_type":"Bearer"}"#;
        let refreshed = r#"{"access_token":"access-2","refresh_token":"refresh-2","expires_in":3600,"token_type":"Bearer"}"#;
        let Some((base_url, server)) = mock_server(vec![
            ("POST /token", "200 OK", token),
            (
                "GET /api/v2/BCBA/Titulos/GGAL/Cotizacion",
                "401 Unauthorized",
                "{}",
            ),
            ("POST /token", "200 OK", refreshed),
            (
                "GET /api/v2/BCBA/Titulos/GGAL/Cotizacion",
                "200 OK",
                underlying,
            ),
            ("GET /api/v2/BCBA/Titulos/GGAL/Opciones", "200 OK", catalog),
            (
                "GET /api/v2/Cotizaciones/Opciones/Todas/Argentina",
                "200 OK",
                quotes,
            ),
        ]) else {
            return;
        };
        let mut client = IolClient::new(
            base_url,
            "user".into(),
            encrypted_test_password(),
            String::new(),
        )
        .unwrap();
        let frame = client.market_frame("GGAL").await.unwrap();
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
        let mut client = IolClient::new(
            base_url,
            "user".into(),
            encrypted_test_password(),
            String::new(),
        )
        .unwrap();

        let account = client.account_snapshot().await.unwrap();

        assert_eq!(account.positions.len(), 1);
        assert!(account.pending_orders.is_empty());
        server.join().unwrap();
    }

    #[tokio::test]
    async fn fetches_latest_operation_detail_for_cost_calibration() {
        let Some((base_url, server)) = mock_server(vec![
            (
                "POST /token",
                "200 OK",
                r#"{"access_token":"access-1","refresh_token":"refresh-1","expires_in":3600,"token_type":"Bearer"}"#,
            ),
            (
                "GET /api/v2/operaciones?filtro.estado=Terminadas&filtro.pais=Argentina",
                "200 OK",
                r#"[{"numero":42,"fechaOperada":"2026-08-21T10:00:00-03:00"}]"#,
            ),
            (
                "GET /api/v2/operaciones/42",
                "200 OK",
                r#"{"monto":10000,"aranceles":[{"tipo":"Comisión IOL","neto":20,"iva":4.2},{"tipo":"Derecho de mercado","neto":5,"iva":1.05}]}"#,
            ),
        ]) else {
            return;
        };
        let mut client = IolClient::new(
            base_url,
            "user".into(),
            encrypted_test_password(),
            String::new(),
        )
        .unwrap();

        let calibration = client.latest_cost_calibration().await.unwrap().unwrap();

        assert_eq!(calibration.operation_number, "42");
        assert!((calibration.total_cost_percentage - 0.3025).abs() < 1e-9);
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
