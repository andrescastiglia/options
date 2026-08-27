use std::{
    collections::{BTreeMap, HashMap, VecDeque},
    path::PathBuf,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};

use futures_util::{SinkExt, StreamExt};
use reqwest::{Client, Response, StatusCode};
use ring::digest::{digest, SHA256};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use thiserror::Error;
use tokio::{
    sync::{mpsc, Semaphore},
    task::JoinHandle,
    time::{sleep, MissedTickBehavior},
};
use tokio_tungstenite::{
    connect_async_with_config,
    tungstenite::{protocol::WebSocketConfig, Message},
};
use zeroize::{Zeroize, Zeroizing};

use crate::{
    broker::{
        validate_order_execution, validate_order_transition, AccountFunds, AccountOrder,
        AccountPosition, AccountSnapshot, OrderExecution, OrderRequest, OrderSide, OrderStatus,
    },
    market::{
        ContractMetadataSource, ExerciseStyle, MarketFrame, OptionChainQuality,
        OptionExpiryQuality, OptionKind, OptionQuote, QuoteTimestampSource, UnderlyingQuote,
    },
    secrets::{decrypt_for_this_machine, SecretError},
    trading::PositionKind,
};

const MAX_IOL_JSON_BYTES: usize = 8_388_608;
const TOKEN_REFRESH_MARGIN: Duration = Duration::from_secs(30);

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
    #[error("resultado de orden ambiguo: {0}")]
    AmbiguousOrder(String),
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

    pub fn masked_account_number(&self) -> String {
        let visible = self
            .account_number
            .chars()
            .rev()
            .take(4)
            .collect::<Vec<_>>();
        let suffix = visible.into_iter().rev().collect::<String>();
        format!("••••{suffix}")
    }

    pub fn redacted_name(&self) -> String {
        let initial = self.last_name.chars().next();
        match (self.first_name.trim(), initial) {
            ("", Some(initial)) => format!("{initial}."),
            (first, Some(initial)) => format!("{first} {initial}."),
            (first, None) => first.to_string(),
        }
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
    #[serde(default)]
    pub observed_contract_multiplier: Option<u32>,
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
    #[serde(
        default,
        rename = "NumeroOperacion",
        alias = "numeroOperacion",
        alias = "orderId",
        deserialize_with = "deserialize_optional_scalar"
    )]
    pub numero_operacion: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WebsocketConnectionState {
    Disabled,
    Connecting,
    Connected,
    Reconnecting,
    Offline,
}

#[derive(Debug, Clone, PartialEq)]
pub enum IolRealtimeEvent {
    Status {
        state: WebsocketConnectionState,
        detail: String,
    },
    Movement(AccountMovement),
    Notice(String),
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct OrderTrackingMetrics {
    pub rest_polls: u32,
    pub websocket_signals: u32,
    pub cancellation_requested: bool,
}

#[derive(Debug, Default)]
pub struct IolStartupContext {
    pub profile: Option<AccountProfile>,
    pub calibration: Option<CostCalibration>,
    pub warnings: Vec<String>,
}

#[derive(Debug)]
struct MovementStream {
    events: mpsc::Receiver<IolRealtimeEvent>,
    commands: mpsc::Sender<MovementCommand>,
    dropped_events: Arc<AtomicU64>,
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
    websocket_enabled: bool,
    movement_stream: Option<MovementStream>,
    deferred_realtime_events: VecDeque<IolRealtimeEvent>,
    order_tracking_metrics: OrderTrackingMetrics,
    request_limit: Arc<Semaphore>,
    catalog_archive_dir: Option<PathBuf>,
}

#[derive(Debug, Clone)]
struct OptionContract {
    symbol: String,
    kind: OptionKind,
    strike: f64,
    expiry_days: u32,
    expiration_timestamp_secs: Option<i64>,
    contract_multiplier: Option<u32>,
    observed_at_secs: i64,
    exercise_style: ExerciseStyle,
    catalog_schema_version: u32,
    catalog_sha256: [u8; 32],
    catalog_archived: bool,
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
            .redirect(reqwest::redirect::Policy::none())
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
            websocket_enabled: false,
            movement_stream: None,
            deferred_realtime_events: VecDeque::new(),
            order_tracking_metrics: OrderTrackingMetrics::default(),
            request_limit: Arc::new(Semaphore::new(10)),
            catalog_archive_dir: None,
        })
    }

    pub fn with_catalog_cache_ttl(mut self, seconds: u64) -> Self {
        self.catalog_cache_ttl = Duration::from_secs(seconds);
        self
    }

    pub fn with_catalog_archive_dir(mut self, path: PathBuf) -> Self {
        self.catalog_archive_dir = Some(path);
        self
    }

    pub fn with_max_concurrent_requests(mut self, maximum: usize) -> Self {
        self.request_limit = Arc::new(Semaphore::new(maximum.max(1)));
        self
    }

    pub fn with_websocket_url(mut self, url: impl Into<String>) -> Self {
        self.websocket_url = url.into();
        self
    }

    pub fn with_websocket_enabled(mut self, enabled: bool) -> Self {
        self.websocket_enabled = enabled;
        self
    }

    pub async fn startup_context(&mut self) -> Result<IolStartupContext, IolClientError> {
        self.ensure_access_token().await?;
        if self.websocket_enabled {
            self.start_movement_stream();
        }
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
        let mut events = self.deferred_realtime_events.drain(..).collect::<Vec<_>>();
        let Some(stream) = &mut self.movement_stream else {
            return events;
        };
        let dropped = stream.dropped_events.swap(0, Ordering::Relaxed);
        if dropped > 0 {
            events.push(IolRealtimeEvent::Notice(format!(
                "WebSocket descartó {dropped} eventos por capacidad; REST mantiene la autoridad"
            )));
        }
        while let Ok(event) = stream.events.try_recv() {
            events.push(event);
        }
        events
    }

    pub async fn shutdown(&mut self) {
        let Some(mut stream) = self.movement_stream.take() else {
            return;
        };
        let _ = stream.commands.try_send(MovementCommand::Shutdown);
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
        const EVENT_CAPACITY: usize = 256;
        let (event_tx, event_rx) = mpsc::channel(EVENT_CAPACITY);
        let (command_tx, command_rx) = mpsc::channel(1);
        let dropped_events = Arc::new(AtomicU64::new(0));
        let url = self.websocket_url.clone();
        let username = Zeroizing::new(self.username.to_string());
        let encrypted_password = self.encrypted_password.clone();
        let task = tokio::spawn(run_movement_stream(
            url,
            username,
            encrypted_password,
            event_tx,
            command_rx,
            Arc::clone(&dropped_events),
        ));
        self.movement_stream = Some(MovementStream {
            events: event_rx,
            commands: command_tx,
            dropped_events,
            task,
        });
    }

    pub async fn authenticate(&mut self) -> Result<(), IolClientError> {
        let _permit = self
            .request_limit
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| IolClientError::InvalidResponse("límite HTTP cerrado".into()))?;
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
        let _permit = self
            .request_limit
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| IolClientError::InvalidResponse("límite HTTP cerrado".into()))?;
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
        let decoded = self
            .authorized_json_get_with_raw(&format!("/api/v2/BCBA/Titulos/{ticker}/Opciones"))
            .await?;
        let archived = self.archive_option_catalog(ticker, &decoded)?;
        let contracts = parse_option_catalog(
            ticker,
            &decoded.value,
            decoded.received_at_secs,
            decoded.sha256,
            archived,
        )?;
        self.option_catalog.insert(
            ticker.to_string(),
            CachedOptionCatalog {
                expires_at: Instant::now() + self.catalog_cache_ttl,
                contracts: contracts.clone(),
            },
        );
        Ok(contracts)
    }

    fn archive_option_catalog(
        &self,
        ticker: &str,
        decoded: &DecodedJson<serde_json::Value>,
    ) -> Result<bool, IolClientError> {
        let Some(root) = &self.catalog_archive_dir else {
            return Ok(false);
        };
        let digest = hex_sha256(&decoded.sha256);
        let directory = root.join(ticker);
        crate::secure_fs::ensure_private_dir(&directory).map_err(|error| {
            IolClientError::InvalidResponse(format!(
                "no se pudo preparar el archivo de catálogo: {error}"
            ))
        })?;
        let path = directory.join(format!("iol-options-v1-{digest}.json"));
        if path.exists() {
            let existing = crate::secure_fs::read_private_limited(&path, 8 * 1024 * 1024).map_err(
                |error| {
                    IolClientError::InvalidResponse(format!(
                        "no se pudo verificar el catálogo archivado: {error}"
                    ))
                },
            )?;
            if digest_bytes(&existing) != decoded.sha256 {
                return Err(IolClientError::InvalidResponse(
                    "el catálogo archivado no coincide con su nombre SHA-256".into(),
                ));
            }
        } else {
            crate::secure_fs::write_new(&path, &decoded.raw).map_err(|error| {
                IolClientError::InvalidResponse(format!(
                    "no se pudo archivar el catálogo IOL: {error}"
                ))
            })?;
        }
        Ok(true)
    }

    pub async fn market_frame_with_retry(
        &mut self,
        ticker: &str,
        attempts: u32,
    ) -> Result<MarketFrame, IolClientError> {
        if let Some(until) = self.circuit_open_until {
            if circuit_is_open(Instant::now(), until) {
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
                Err(error) => {
                    self.failures = self.failures.saturating_add(1);
                    if circuit_breaker_should_open(self.failures) {
                        let until = circuit_breaker_deadline(Instant::now());
                        self.circuit_open_until = Some(until);
                        return Err(IolClientError::CircuitOpen(until));
                    }
                    if error_is_retryable(&error) && retry_attempt_remains(attempt, attempts) {
                        sleep(market_retry_delay(attempt)).await;
                    } else {
                        return Err(error);
                    }
                }
            }
        }
        unreachable!()
    }

    pub async fn account_snapshot(&mut self) -> Result<AccountSnapshot, IolClientError> {
        let account_state = self.authorized_json_get("/api/v2/estadocuenta").await?;
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
            funds: Some(parse_account_funds(&account_state)?),
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
        let response = self.authorized_post(order_path, &body).await?;
        if response.status() == StatusCode::UNAUTHORIZED {
            // Un POST de orden no es seguro de repetir salvo que el broker
            // garantice idempotencia por `identificador`. Se renueva el token
            // para las consultas posteriores, pero la intención queda en estado
            // ambiguo y debe reconciliarse antes de cualquier reenvío.
            self.refresh().await?;
            return Err(IolClientError::AmbiguousOrder(format!(
                "IOL devolvió 401 al enviar {}; no se reenvió la orden",
                request.operation_id
            )));
        }
        let response = response.error_for_status()?;
        parse_order_execution(request, decode_json_limited(response).await?)
    }

    /// Sigue una orden aceptada hasta un estado terminal. El WebSocket sólo
    /// adelanta una consulta cuando el movimiento trae el mismo número de
    /// operación; REST conserva siempre la autoridad sobre el estado final.
    pub async fn track_order_to_terminal(
        &mut self,
        request: &OrderRequest,
        initial: OrderExecution,
        tracking_timeout: Duration,
        poll_interval: Duration,
        cancel_timeout: Duration,
    ) -> Result<OrderExecution, IolClientError> {
        self.order_tracking_metrics = OrderTrackingMetrics::default();
        if order_is_terminal(&initial) {
            validate_order_execution(request, &initial).map_err(IolClientError::InvalidResponse)?;
            return Ok(initial);
        }
        validate_order_execution(request, &initial).map_err(IolClientError::InvalidResponse)?;
        let broker_order_id = initial
            .broker_order_id
            .as_deref()
            .filter(|value| !value.is_empty())
            .ok_or_else(|| {
                IolClientError::InvalidResponse(
                    "IOL aceptó una orden no terminal sin numeroOperacion; no puede seguirse ni cancelarse de forma segura"
                        .into(),
                )
            })?
            .to_string();
        let display_order_id = crate::redaction::masked_identifier(&broker_order_id);
        self.deferred_realtime_events
            .push_back(IolRealtimeEvent::Notice(format!(
                "Orden IOL {display_order_id}: estado inicial {:?}; comienza el seguimiento",
                initial.status
            )));
        let mut last_execution = initial;

        if let Some(execution) = self
            .poll_order_until(
                request,
                &broker_order_id,
                tracking_timeout,
                poll_interval,
                &mut last_execution,
            )
            .await?
        {
            self.deferred_realtime_events
                .push_back(IolRealtimeEvent::Notice(format!(
                    "Orden IOL {display_order_id}: estado final {:?}",
                    execution.status
                )));
            return Ok(execution);
        }

        // Última lectura antes del DELETE: evita cancelar una orden que terminó
        // justo al vencer el plazo local.
        match self.order_status(request, &broker_order_id).await {
            Ok(before_cancel) => {
                validate_order_transition(&last_execution, &before_cancel)
                    .map_err(IolClientError::InvalidResponse)?;
                if order_is_terminal(&before_cancel) {
                    return Ok(before_cancel);
                }
                last_execution = before_cancel;
            }
            Err(error) if error_is_retryable(&error) => {}
            Err(error) => return Err(error),
        }
        self.deferred_realtime_events
            .push_back(IolRealtimeEvent::Notice(format!(
            "Orden IOL {display_order_id}: venció el seguimiento; se solicita cancelar el remanente"
        )));
        self.order_tracking_metrics.cancellation_requested = true;
        if let Err(cancel_error) = self.cancel_order(&broker_order_id).await {
            if let Ok(execution) = self.order_status(request, &broker_order_id).await {
                validate_order_transition(&last_execution, &execution)
                    .map_err(IolClientError::InvalidResponse)?;
                if order_is_terminal(&execution) {
                    return Ok(execution);
                }
            }
            return Err(IolClientError::InvalidResponse(format!(
                "no se pudo cancelar la orden {display_order_id} después del timeout: {cancel_error}"
            )));
        }

        let execution = self
            .poll_order_until(
                request,
                &broker_order_id,
                cancel_timeout,
                poll_interval,
                &mut last_execution,
            )
        .await?
        .ok_or_else(|| {
            IolClientError::InvalidResponse(format!(
                "IOL aceptó cancelar la orden {display_order_id}, pero no confirmó un estado terminal dentro del plazo"
            ))
        })?;
        self.deferred_realtime_events
            .push_back(IolRealtimeEvent::Notice(format!(
                "Orden IOL {display_order_id}: estado final {:?} después de cancelar",
                execution.status
            )));
        Ok(execution)
    }

    pub fn order_tracking_metrics(&self) -> OrderTrackingMetrics {
        self.order_tracking_metrics
    }

    async fn poll_order_until(
        &mut self,
        request: &OrderRequest,
        broker_order_id: &str,
        timeout: Duration,
        poll_interval: Duration,
        last_execution: &mut OrderExecution,
    ) -> Result<Option<OrderExecution>, IolClientError> {
        let deadline = Instant::now() + timeout;
        loop {
            self.order_tracking_metrics.rest_polls =
                self.order_tracking_metrics.rest_polls.saturating_add(1);
            match self.order_status(request, broker_order_id).await {
                Ok(execution) => {
                    validate_order_transition(last_execution, &execution)
                        .map_err(IolClientError::InvalidResponse)?;
                    *last_execution = execution.clone();
                    if order_is_terminal(&execution) {
                        return Ok(Some(execution));
                    }
                }
                Err(error) if error_is_retryable(&error) => {}
                Err(error) => return Err(error),
            }
            let now = Instant::now();
            let Some(remaining) = deadline.checked_duration_since(now) else {
                return Ok(None);
            };
            let wait = poll_interval.min(remaining);
            self.wait_for_order_signal(broker_order_id, wait).await;
        }
    }

    async fn wait_for_order_signal(&mut self, broker_order_id: &str, wait: Duration) {
        let Some(stream) = &mut self.movement_stream else {
            sleep(wait).await;
            return;
        };
        let started = Instant::now();
        match tokio::time::timeout(wait, stream.events.recv()).await {
            Ok(Some(event)) => {
                let correlated = matches!(
                    &event,
                    IolRealtimeEvent::Movement(movement)
                        if movement.numero_operacion.as_deref() == Some(broker_order_id)
                );
                self.deferred_realtime_events.push_back(event);
                if correlated {
                    self.order_tracking_metrics.websocket_signals = self
                        .order_tracking_metrics
                        .websocket_signals
                        .saturating_add(1);
                    self.deferred_realtime_events
                        .push_back(IolRealtimeEvent::Notice(format!(
                            "WebSocket informó actividad para la orden IOL {}; se verifica el estado por REST",
                            crate::redaction::masked_identifier(broker_order_id)
                        )));
                } else {
                    sleep(wait.saturating_sub(started.elapsed())).await;
                }
            }
            Ok(None) => sleep(wait.saturating_sub(started.elapsed())).await,
            Err(_) => {}
        }
    }

    async fn order_status(
        &mut self,
        request: &OrderRequest,
        broker_order_id: &str,
    ) -> Result<OrderExecution, IolClientError> {
        let body = self
            .authorized_json_get(&format!("/api/v2/operaciones/{broker_order_id}"))
            .await?;
        parse_order_execution(request, body)
    }

    async fn cancel_order(&mut self, broker_order_id: &str) -> Result<(), IolClientError> {
        self.ensure_access_token().await?;
        let path = format!("/api/v2/operaciones/{broker_order_id}");
        let mut response = self.authorized_delete(&path).await?;
        if response.status() == StatusCode::UNAUTHORIZED {
            self.refresh().await?;
            response = self.authorized_delete(&path).await?;
        }
        response.error_for_status()?;
        Ok(())
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
        decode_json_limited(response.error_for_status()?).await
    }

    async fn authorized_json_get_with_raw(
        &mut self,
        path: &str,
    ) -> Result<DecodedJson<serde_json::Value>, IolClientError> {
        self.ensure_access_token().await?;
        let mut response = self.authorized_get(path).await?;
        if response.status() == StatusCode::UNAUTHORIZED {
            self.refresh().await?;
            response = self.authorized_get(path).await?;
        }
        decode_json_limited_with_raw(response.error_for_status()?).await
    }

    async fn authorized_get(&self, path: &str) -> Result<Response, IolClientError> {
        let _permit = self
            .request_limit
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| IolClientError::InvalidResponse("límite HTTP cerrado".into()))?;
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
        let _permit = self
            .request_limit
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| IolClientError::InvalidResponse("límite HTTP cerrado".into()))?;
        let token = self.access_token()?;
        Ok(self
            .http
            .post(format!("{}{}", self.base_url, normalize_path(path)))
            .bearer_auth(token)
            .json(body)
            .send()
            .await?)
    }

    async fn authorized_delete(&self, path: &str) -> Result<Response, IolClientError> {
        let _permit = self
            .request_limit
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| IolClientError::InvalidResponse("límite HTTP cerrado".into()))?;
        let token = self.access_token()?;
        Ok(self
            .http
            .delete(format!("{}{}", self.base_url, normalize_path(path)))
            .bearer_auth(token)
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
        } else if token_needs_refresh(Instant::now(), self.access_expires_at) {
            self.refresh().await?;
        }
        Ok(())
    }

    async fn apply_token_response(&mut self, response: Response) -> Result<(), IolClientError> {
        if !response.status().is_success() {
            return Err(IolClientError::Authentication(format!(
                "HTTP {} (cuerpo omitido para no exponer credenciales)",
                response.status()
            )));
        }
        let token: TokenResponse = decode_json_limited(response).await?;
        if token.access_token.trim().is_empty()
            || token.expires_in == 0
            || !token.token_type.eq_ignore_ascii_case("bearer")
        {
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

struct DecodedJson<T> {
    value: T,
    raw: Vec<u8>,
    sha256: [u8; 32],
    received_at_secs: i64,
}

async fn decode_json_limited<T: DeserializeOwned>(response: Response) -> Result<T, IolClientError> {
    Ok(decode_json_limited_with_raw(response).await?.value)
}

async fn decode_json_limited_with_raw<T: DeserializeOwned>(
    response: Response,
) -> Result<DecodedJson<T>, IolClientError> {
    let received_at_secs = unix_now();
    if let Some(date) = response.headers().get(reqwest::header::DATE) {
        let date = date.to_str().map_err(|_| {
            IolClientError::InvalidResponse("encabezado Date de IOL inválido".into())
        })?;
        validate_http_date_value(date, received_at_secs)?;
    }
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_ascii_lowercase();
    if !is_json_content_type(&content_type) {
        return Err(IolClientError::InvalidResponse(
            "Content-Type no es JSON".into(),
        ));
    }
    if response
        .content_length()
        .is_some_and(json_body_exceeds_limit)
    {
        return Err(IolClientError::InvalidResponse(format!(
            "respuesta JSON excede {MAX_IOL_JSON_BYTES} bytes"
        )));
    }
    let mut bytes = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        if json_body_exceeds_limit(bytes.len().saturating_add(chunk.len()) as u64) {
            return Err(IolClientError::InvalidResponse(format!(
                "respuesta JSON excede {MAX_IOL_JSON_BYTES} bytes"
            )));
        }
        bytes.extend_from_slice(&chunk);
    }
    let value = serde_json::from_slice(&bytes)
        .map_err(|error| IolClientError::InvalidResponse(format!("JSON inválido: {error}")))?;
    Ok(DecodedJson {
        value,
        sha256: digest_bytes(&bytes),
        raw: bytes,
        received_at_secs,
    })
}

fn token_needs_refresh(now: Instant, expires_at: Instant) -> bool {
    expires_at.saturating_duration_since(now) <= TOKEN_REFRESH_MARGIN
}

fn circuit_is_open(now: Instant, open_until: Instant) -> bool {
    open_until > now
}

fn retry_attempt_remains(attempt: u32, attempts: u32) -> bool {
    attempt < attempts.saturating_sub(1)
}

fn circuit_breaker_should_open(failures: u32) -> bool {
    failures >= 3
}

fn circuit_breaker_deadline(now: Instant) -> Instant {
    now.checked_add(Duration::from_secs(300))
        .expect("300 seconds must fit in Instant")
}

fn market_retry_delay(attempt: u32) -> Duration {
    const DELAYS_MS: [u64; 7] = [250, 500, 1_000, 2_000, 4_000, 8_000, 16_000];
    Duration::from_millis(DELAYS_MS[attempt.min(6) as usize])
}

fn is_json_content_type(value: &str) -> bool {
    let media_type = value.split(';').next().unwrap_or_default().trim();
    media_type == "application/json"
        || media_type
            .split_once('/')
            .is_some_and(|(_, subtype)| subtype.ends_with("+json"))
}

fn json_body_exceeds_limit(length: u64) -> bool {
    length > MAX_IOL_JSON_BYTES as u64
}

fn digest_bytes(bytes: &[u8]) -> [u8; 32] {
    let mut result = [0_u8; 32];
    result.copy_from_slice(digest(&SHA256, bytes).as_ref());
    result
}

fn hex_sha256(value: &[u8; 32]) -> String {
    value.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn validate_http_date_value(value: &str, received_at_secs: i64) -> Result<(), IolClientError> {
    let source_secs = chrono::DateTime::parse_from_rfc2822(value)
        .map_err(|_| IolClientError::InvalidResponse("encabezado Date de IOL inválido".into()))?
        .timestamp();
    let skew = source_secs.abs_diff(received_at_secs);
    if skew > crate::market::MAX_SOURCE_CLOCK_SKEW_SECS as u64 {
        return Err(IolClientError::InvalidResponse(format!(
            "desvío entre reloj local y Date de IOL: {skew}s"
        )));
    }
    Ok(())
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
    events: mpsc::Sender<IolRealtimeEvent>,
    mut commands: mpsc::Receiver<MovementCommand>,
    dropped_events: Arc<AtomicU64>,
) {
    let mut retry_delay = Duration::from_secs(1);
    loop {
        if commands.try_recv().is_ok() {
            return;
        }
        let mut websocket_config = WebSocketConfig::default();
        websocket_config.max_message_size = Some(65_536);
        websocket_config.max_frame_size = Some(16_384);
        let result = tokio::time::timeout(
            Duration::from_secs(10),
            connect_async_with_config(&url, Some(websocket_config), false),
        )
        .await;
        let (mut socket, _) = match result {
            Ok(Ok(connection)) => connection,
            result => {
                let detail = match result {
                    Err(_) => {
                        "La conexión WebSocket de IOL excedió 10 segundos; se reintentará".into()
                    }
                    Ok(Err(error)) => format!(
                        "Se cortó la conexión con los movimientos de IOL: {error}; volviendo a intentar"
                    ),
                    Ok(Ok(_)) => unreachable!("la conexión exitosa se resolvió antes"),
                };
                if !publish_realtime_event(
                    &events,
                    &dropped_events,
                    IolRealtimeEvent::Status {
                        state: WebsocketConnectionState::Reconnecting,
                        detail,
                    },
                ) {
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
                retry_delay = next_websocket_retry_delay(retry_delay);
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
                let _ = publish_realtime_event(
                    &events,
                    &dropped_events,
                    IolRealtimeEvent::Status {
                        state: WebsocketConnectionState::Offline,
                        detail: format!("No se pudo usar la contraseña guardada: {error}"),
                    },
                );
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
            let _ = publish_realtime_event(&events, &dropped_events, IolRealtimeEvent::Status {
                state: WebsocketConnectionState::Offline,
                detail: "IOL no permitió recibir movimientos de la cuenta; los precios y demás datos siguen disponibles"
                    .into(),
            });
            return;
        }

        retry_delay = Duration::from_secs(1);
        if !publish_realtime_event(
            &events,
            &dropped_events,
            IolRealtimeEvent::Status {
                state: WebsocketConnectionState::Connected,
                detail: "WebSocket de movimientos de IOL conectado".into(),
            },
        ) {
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
                                if !publish_realtime_event(&events, &dropped_events, event) {
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
            let _ = publish_realtime_event(
                &events,
                &dropped_events,
                IolRealtimeEvent::Status {
                    state: WebsocketConnectionState::Reconnecting,
                    detail:
                        "Se perdió la conexión con los movimientos de IOL; volviendo a intentar"
                            .into(),
                },
            );
        }
    }
}

fn next_websocket_retry_delay(current: Duration) -> Duration {
    Duration::from_secs(current.as_secs().saturating_mul(2).min(60))
}

fn publish_realtime_event(
    events: &mpsc::Sender<IolRealtimeEvent>,
    dropped_events: &AtomicU64,
    event: IolRealtimeEvent,
) -> bool {
    match events.try_send(event) {
        Ok(()) => true,
        Err(mpsc::error::TrySendError::Full(_)) => {
            dropped_events.fetch_add(1, Ordering::Relaxed);
            true
        }
        Err(mpsc::error::TrySendError::Closed(_)) => false,
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

/// Decodifica un mensaje textual del WebSocket con el mismo parser usado por
/// el runtime. Los mensajes de autenticación no se convierten en movimientos.
pub fn parse_realtime_message(text: &str) -> Option<IolRealtimeEvent> {
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

fn parse_account_funds(body: &serde_json::Value) -> Result<AccountFunds, IolClientError> {
    let accounts = collection(body, &["cuentas"])?;
    if accounts.iter().any(|account| !account.is_object()) {
        return Err(IolClientError::InvalidResponse(
            "cuentas contiene un elemento que no es objeto".into(),
        ));
    }
    let matching = accounts
        .iter()
        .map(|account| account.as_object().expect("validado como objeto"))
        .filter(|account| {
            text_optional(account, &["tipo"]) == Some("inversion_Argentina_Pesos")
                && text_optional(account, &["moneda"]) == Some("peso_Argentino")
        })
        .collect::<Vec<_>>();
    if matching.len() != 1 {
        return Err(IolClientError::InvalidResponse(format!(
            "se esperaba una única cuenta de inversión operable en pesos; se encontraron {}",
            matching.len()
        )));
    }
    let account = matching[0];
    let status = text_optional(account, &["estado"])
        .ok_or_else(|| IolClientError::InvalidResponse("estado de cuenta ausente".into()))?;
    if status != "operable" {
        return Err(IolClientError::InvalidResponse(format!(
            "la cuenta de inversión en pesos no está operable: {status}"
        )));
    }
    let available = non_negative_finite(number(account, &["disponible"])?, "disponible")?;
    let balances = account
        .get("saldos")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| IolClientError::InvalidResponse("saldos no es una colección".into()))?;
    if balances.iter().any(|balance| !balance.is_object()) {
        return Err(IolClientError::InvalidResponse(
            "saldos contiene un elemento que no es objeto".into(),
        ));
    }
    let immediate = balances
        .iter()
        .map(|balance| balance.as_object().expect("validado como objeto"))
        .filter(|balance| text_optional(balance, &["liquidacion"]) == Some("inmediato"))
        .collect::<Vec<_>>();
    if immediate.len() != 1 {
        return Err(IolClientError::InvalidResponse(format!(
            "se esperaba un único saldo de liquidación inmediata; se encontraron {}",
            immediate.len()
        )));
    }
    let immediate_available_to_trade = non_negative_finite(
        number(immediate[0], &["disponibleOperar"])?,
        "disponibleOperar",
    )?;
    let account_number = scalar_text(account, &["numero"])
        .ok_or_else(|| IolClientError::InvalidResponse("número de cuenta ausente".into()))?;
    Ok(AccountFunds {
        account_number,
        currency: "peso_Argentino".into(),
        status: status.into(),
        available,
        immediate_available_to_trade,
    })
}

fn non_negative_finite(value: f64, field: &str) -> Result<f64, IolClientError> {
    if value.is_finite() && value >= 0.0 {
        Ok(value)
    } else {
        Err(IolClientError::InvalidResponse(format!(
            "{field} debe ser finito y no negativo"
        )))
    }
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
    let operated_quantity =
        optional_number(object, &["cantidadOperada", "cantidad", "operatedQuantity"]).map(f64::abs);
    let operated_price =
        optional_number(object, &["precioOperado", "precio", "operatedPrice"]).map(f64::abs);
    let observed_contract_multiplier = operated_quantity
        .zip(operated_price)
        .and_then(|(quantity, price)| infer_contract_multiplier(operation_amount, price, quantity));
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
        observed_contract_multiplier,
    })
}

fn infer_contract_multiplier(amount: f64, price: f64, quantity: f64) -> Option<u32> {
    let raw = amount / (price * quantity);
    let rounded = raw.round();
    (raw.is_finite()
        && rounded >= 1.0
        && rounded <= u32::MAX as f64
        && (raw - rounded).abs() <= (rounded * 0.01).max(0.01))
    .then_some(rounded as u32)
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
    let received_at_secs = unix_now();
    let exchange_timestamp_secs = market_timestamp(underlying);
    let (timestamp_secs, timestamp_source) = exchange_timestamp_secs
        .map(|timestamp| (timestamp, QuoteTimestampSource::Exchange))
        .unwrap_or((received_at_secs, QuoteTimestampSource::Received));
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
    let mut options = Vec::new();
    let mut missing_quote_contracts = 0_usize;
    let mut invalid_quote_contracts = 0_usize;
    let mut by_expiry = BTreeMap::<u32, OptionExpiryQuality>::new();
    for contract in contracts {
        let expiry_quality = by_expiry
            .entry(contract.expiry_days)
            .or_insert(OptionExpiryQuality {
                expiry_days: contract.expiry_days,
                catalog_contracts: 0,
                accepted_contracts: 0,
                missing_quote_contracts: 0,
                invalid_quote_contracts: 0,
                accepted_call_contracts: 0,
                accepted_put_contracts: 0,
            });
        expiry_quality.catalog_contracts = expiry_quality.catalog_contracts.saturating_add(1);
        let Some(quote) = quotes_by_symbol.get(&contract.symbol.to_ascii_uppercase()) else {
            missing_quote_contracts = missing_quote_contracts.saturating_add(1);
            expiry_quality.missing_quote_contracts =
                expiry_quality.missing_quote_contracts.saturating_add(1);
            continue;
        };
        match parse_option_quote(ticker, contract, quote) {
            Ok(option) if option.validate(ticker).is_ok() => {
                expiry_quality.accepted_contracts =
                    expiry_quality.accepted_contracts.saturating_add(1);
                match option.kind {
                    OptionKind::Call => {
                        expiry_quality.accepted_call_contracts =
                            expiry_quality.accepted_call_contracts.saturating_add(1);
                    }
                    OptionKind::Put => {
                        expiry_quality.accepted_put_contracts =
                            expiry_quality.accepted_put_contracts.saturating_add(1);
                    }
                }
                options.push(option);
            }
            _ => {
                invalid_quote_contracts = invalid_quote_contracts.saturating_add(1);
                expiry_quality.invalid_quote_contracts =
                    expiry_quality.invalid_quote_contracts.saturating_add(1);
            }
        }
    }
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
            exchange_timestamp_secs,
            received_at_secs,
            timestamp_source,
        },
        option_chain_quality: Some(OptionChainQuality {
            catalog_contracts: contracts.len(),
            quote_rows: quote_values.len(),
            accepted_contracts: options.len(),
            missing_quote_contracts,
            invalid_quote_contracts,
            accepted_call_contracts: options
                .iter()
                .filter(|option| option.kind == OptionKind::Call)
                .count(),
            accepted_put_contracts: options
                .iter()
                .filter(|option| option.kind == OptionKind::Put)
                .count(),
            by_expiry: by_expiry.into_values().collect(),
        }),
        options,
        vix: None,
    })
}

fn parse_option_catalog(
    ticker: &str,
    body: &serde_json::Value,
    observed_at_secs: i64,
    catalog_sha256: [u8; 32],
    catalog_archived: bool,
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
            let expiration_day = text_optional(object, &["fechaVencimiento", "expirationDate"])
                .and_then(date_days_since_epoch);
            let expiry_days = integer(object, &["diasVencimiento", "expiryDays"])
                .map(|days| days.max(0) as u32)
                .or_else(|| {
                    expiration_day.map(|expiry| expiry.saturating_sub(today).max(0) as u32)
                })?;
            let exercise_style = match text_optional(object, &["estiloEjercicio", "exerciseStyle"])
                .map(str::to_ascii_lowercase)
                .as_deref()
            {
                Some("europea") | Some("european") => ExerciseStyle::European,
                Some("americana") | Some("american") | None => ExerciseStyle::American,
                Some(_) => ExerciseStyle::Unknown,
            };
            Some(OptionContract {
                symbol: symbol.to_string(),
                kind,
                strike,
                expiry_days,
                // La API de catálogo informa fecha civil. Se representa al cierre
                // de la rueda (17:00 Argentina = 20:00 UTC), no a medianoche.
                expiration_timestamp_secs: expiration_day
                    .map(|day| day.saturating_mul(86_400).saturating_add(20 * 3_600)),
                contract_multiplier: integer(
                    object,
                    &["lote", "tamanoContrato", "contractMultiplier"],
                )
                .and_then(|value| u32::try_from(value).ok())
                .filter(|value| *value > 0),
                observed_at_secs,
                exercise_style,
                catalog_schema_version: 1,
                catalog_sha256,
                catalog_archived,
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
    contract: &OptionContract,
    object: &serde_json::Map<String, serde_json::Value>,
) -> Result<OptionQuote, IolClientError> {
    let last = number(object, &["ultimoPrecio", "ultimo", "last", "price"])?;
    let (bid, ask) = book_prices(object);
    let received_at_secs = unix_now();
    let exchange_timestamp_secs = market_timestamp(object);
    let (timestamp_secs, timestamp_source) = exchange_timestamp_secs
        .map(|timestamp| (timestamp, QuoteTimestampSource::Exchange))
        .unwrap_or((received_at_secs, QuoteTimestampSource::Received));
    Ok(OptionQuote {
        symbol: contract.symbol.clone(),
        underlying: ticker.into(),
        kind: contract.kind,
        strike: optional_number(object, &["precioEjercicio", "strike"]).unwrap_or(contract.strike),
        expiry_days: contract.expiry_days,
        expiration_timestamp_secs: contract.expiration_timestamp_secs,
        catalog_contract_multiplier: contract.contract_multiplier,
        catalog_observed_at_secs: Some(contract.observed_at_secs),
        catalog_schema_version: contract.catalog_schema_version,
        catalog_sha256: Some(contract.catalog_sha256),
        catalog_archived: contract.catalog_archived,
        contract_metadata_source: ContractMetadataSource::IolCatalog,
        exercise_style: contract.exercise_style,
        last,
        bid,
        ask,
        volume: optional_number(object, &["volumen", "volume"])
            .unwrap_or(0.0)
            .max(0.0) as u64,
        timestamp_secs,
        exchange_timestamp_secs,
        received_at_secs,
        timestamp_source,
    })
}

/// Decodifica y valida una respuesta de orden IOL contra la intención original.
/// Los campos ausentes o contradictorios nunca se completan desde la solicitud.
pub fn parse_order_execution(
    request: &OrderRequest,
    body: serde_json::Value,
) -> Result<OrderExecution, IolClientError> {
    let value = body
        .as_array()
        .and_then(|items| items.first())
        .unwrap_or(&body);
    let object = value
        .as_object()
        .ok_or_else(|| IolClientError::InvalidResponse("orden no es objeto".into()))?;
    let raw_status = text_optional(object, &["estado", "status"]);
    let status = match raw_status
        .unwrap_or("pendiente")
        .trim()
        .to_ascii_lowercase()
        .as_str()
    {
        "pendiente" | "pending" => OrderStatus::Pending,
        "ejecutada" | "executed" | "filled" | "terminada" | "operada" => OrderStatus::Executed,
        "parcial" | "partially_filled" | "parcialmente ejecutada" | "parcialmente operada" => {
            OrderStatus::PartiallyExecuted
        }
        "rechazada" | "rejected" => OrderStatus::Rejected,
        "cancelada" | "cancelled" | "canceled" => OrderStatus::Cancelled,
        unknown => {
            return Err(IolClientError::InvalidResponse(format!(
                "estado de orden desconocido: {unknown}"
            )))
        }
    };
    let filled_quantity = strict_optional_integer(
        object,
        &["cantidadEjecutada", "cantidadOperada", "filledQuantity"],
    )?
    .unwrap_or(0);
    if filled_quantity < 0 || filled_quantity > i64::from(u32::MAX) {
        return Err(IolClientError::InvalidResponse(
            "cantidad ejecutada fuera de rango".into(),
        ));
    }
    let execution = OrderExecution {
        operation_id: request.operation_id.clone(),
        status,
        filled_quantity: filled_quantity as u32,
        fill_price: optional_number(object, &["precioEjecutado", "precioOperado", "fillPrice"]),
        broker_order_id: scalar_text(object, &["numeroOperacion", "numero", "orderId"])
            .map(|id| id.trim().to_string())
            .filter(|id| !id.is_empty()),
        message: text_optional(object, &["mensaje", "message"]).map(str::to_string),
    };
    validate_order_execution(request, &execution).map_err(IolClientError::InvalidResponse)?;
    Ok(execution)
}

fn order_is_terminal(execution: &OrderExecution) -> bool {
    matches!(
        execution.status,
        OrderStatus::Executed | OrderStatus::Rejected | OrderStatus::Cancelled
    )
}

fn parse_account_positions(
    body: &serde_json::Value,
) -> Result<Vec<AccountPosition>, IolClientError> {
    let values = collection(body, &["activos", "titulos", "positions"])?;
    let mut positions = Vec::with_capacity(values.len());
    for (index, value) in values.iter().enumerate() {
        let object = value.as_object().ok_or_else(|| {
            IolClientError::InvalidResponse(format!("posición {index} no es un objeto"))
        })?;
        let instrument = nested_instrument(object);
        let symbol = text_optional(instrument, &["simbolo", "symbol", "ticker"])
            .or_else(|| text_optional(object, &["simbolo", "symbol", "ticker"]))
            .map(str::trim)
            .filter(|symbol| !symbol.is_empty())
            .ok_or_else(|| {
                IolClientError::InvalidResponse(format!("posición {index} sin símbolo"))
            })?;
        let quantity = positive_whole_quantity(
            object,
            &["cantidad", "quantity", "tenencia"],
            &format!("posición {index}"),
        )?;
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
        positions.push(AccountPosition {
            symbol: symbol.to_string(),
            quantity,
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
        });
    }
    Ok(positions)
}

fn parse_pending_orders(body: &serde_json::Value) -> Result<Vec<AccountOrder>, IolClientError> {
    let values = collection(body, &["operaciones", "orders", "items"])?;
    let mut orders = Vec::with_capacity(values.len());
    for (index, value) in values.iter().enumerate() {
        let object = value.as_object().ok_or_else(|| {
            IolClientError::InvalidResponse(format!("orden pendiente {index} no es un objeto"))
        })?;
        let instrument = nested_instrument(object);
        let symbol = text_optional(instrument, &["simbolo", "symbol", "ticker"])
            .or_else(|| text_optional(object, &["simbolo", "symbol", "ticker"]))
            .map(str::trim)
            .filter(|symbol| !symbol.is_empty())
            .ok_or_else(|| {
                IolClientError::InvalidResponse(format!("orden pendiente {index} sin símbolo"))
            })?;
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
        let side = match text_optional(object, &["operacion", "tipoOperacion", "side"])
            .map(str::trim)
            .map(str::to_ascii_lowercase)
            .as_deref()
        {
            Some("compra" | "buy") => OrderSide::Buy,
            Some("venta" | "sell") => OrderSide::Sell,
            _ => {
                return Err(IolClientError::InvalidResponse(format!(
                    "orden pendiente {index} sin lado reconocido"
                )))
            }
        };
        let broker_order_id = scalar_text(object, &["numero", "numeroOperacion", "orderId"])
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
            .ok_or_else(|| {
                IolClientError::InvalidResponse(format!(
                    "orden pendiente {index} sin broker_order_id"
                ))
            })?;
        let quantity = positive_whole_quantity(
            object,
            &["cantidad", "quantity"],
            &format!("orden pendiente {index}"),
        )?;
        orders.push(AccountOrder {
            broker_order_id,
            symbol: symbol.to_string(),
            side: Some(side),
            quantity,
            kind,
            is_option,
        });
    }
    Ok(orders)
}

fn positive_whole_quantity(
    object: &serde_json::Map<String, serde_json::Value>,
    names: &[&str],
    context: &str,
) -> Result<u32, IolClientError> {
    let quantity = optional_number(object, names)
        .ok_or_else(|| IolClientError::InvalidResponse(format!("{context} sin cantidad")))?;
    if !quantity.is_finite()
        || quantity <= 0.0
        || quantity > u32::MAX as f64
        || quantity.fract() != 0.0
    {
        return Err(IolClientError::InvalidResponse(format!(
            "{context} tiene cantidad no entera, no positiva o fuera de rango"
        )));
    }
    Ok(quantity as u32)
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
        !character.is_ascii_digit()
            && character != ','
            && character != '.'
            && character != '-'
            && character != '+'
    });
    let comma = raw.rfind(',');
    let dot = raw.rfind('.');
    let normalized = match (comma, dot) {
        (Some(comma), Some(dot)) if comma.cmp(&dot).is_lt() => raw.replace(',', ""),
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
    let date = chrono::NaiveDate::parse_from_str(date, "%Y-%m-%d").ok()?;
    let epoch = chrono::NaiveDate::from_ymd_opt(1970, 1, 1)?;
    Some(date.signed_duration_since(epoch).num_days())
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

fn strict_optional_integer(
    object: &serde_json::Map<String, serde_json::Value>,
    names: &[&str],
) -> Result<Option<i64>, IolClientError> {
    let Some((name, value)) = names
        .iter()
        .find_map(|name| object.get(*name).map(|value| (*name, value)))
    else {
        return Ok(None);
    };
    if value.is_null() {
        return Ok(None);
    }
    value
        .as_i64()
        .or_else(|| value.as_str().and_then(|value| value.parse().ok()))
        .map(Some)
        .ok_or_else(|| IolClientError::InvalidResponse(format!("entero inválido para {name}")))
}

fn text_optional<'a>(
    object: &'a serde_json::Map<String, serde_json::Value>,
    names: &[&str],
) -> Option<&'a str> {
    names
        .iter()
        .find_map(|name| object.get(*name).and_then(serde_json::Value::as_str))
}

fn deserialize_optional_scalar<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Option::<serde_json::Value>::deserialize(deserializer)?;
    Ok(value.and_then(|value| match value {
        serde_json::Value::String(value) => Some(value),
        serde_json::Value::Number(value) => Some(value.to_string()),
        _ => None,
    }))
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
        crate::secrets::encrypt_legacy_for_test("pass")
    }

    #[test]
    fn configured_http_concurrency_limit_is_effective() {
        let client = IolClient::new(
            "https://localhost",
            "user".into(),
            encrypted_test_password(),
            String::new(),
        )
        .unwrap()
        .with_max_concurrent_requests(3);
        assert_eq!(client.request_limit.available_permits(), 3);
    }

    fn official_account_state_fixture() -> serde_json::Value {
        serde_json::json!({
            "cuentas": [{
                "numero": "2033590",
                "tipo": "inversion_Argentina_Pesos",
                "moneda": "peso_Argentino",
                "disponible": 50_000.0,
                "comprometido": 0.0,
                "saldo": 50_000.0,
                "titulosValorizados": 0.0,
                "total": 50_000.0,
                "margenDescubierto": 0.0,
                "saldos": [{
                    "liquidacion": "inmediato",
                    "saldo": 50_000.0,
                    "comprometido": 0.0,
                    "disponible": 50_000.0,
                    "disponibleOperar": 45_000.0
                }],
                "estado": "operable"
            }],
            "estadisticas": [],
            "totalEnPesos": 50_000.0
        })
    }

    #[test]
    fn parses_only_operable_immediate_peso_funds() {
        let funds = parse_account_funds(&official_account_state_fixture()).unwrap();
        assert_eq!(funds.account_number, "2033590");
        assert_eq!(funds.available, 50_000.0);
        assert_eq!(funds.immediate_available_to_trade, 45_000.0);
    }

    #[test]
    fn funds_require_account_type_and_currency_on_the_same_row() {
        let mut fixture = official_account_state_fixture();
        let accounts = fixture["cuentas"].as_array_mut().unwrap();
        let mut wrong_currency = accounts[0].clone();
        wrong_currency["numero"] = serde_json::json!("wrong-currency");
        wrong_currency["moneda"] = serde_json::json!("dolar_Estadounidense");
        accounts.push(wrong_currency);
        let mut wrong_type = accounts[0].clone();
        wrong_type["numero"] = serde_json::json!("wrong-type");
        wrong_type["tipo"] = serde_json::json!("comitente_Argentina_Pesos");
        accounts.push(wrong_type);

        let funds = parse_account_funds(&fixture).unwrap();
        assert_eq!(funds.account_number, "2033590");
    }

    #[test]
    fn rejects_blocked_or_ambiguous_account_funds() {
        let mut blocked = official_account_state_fixture();
        blocked["cuentas"][0]["estado"] = serde_json::json!("bloqueada");
        assert!(parse_account_funds(&blocked).is_err());

        let mut ambiguous = official_account_state_fixture();
        let duplicate = ambiguous["cuentas"][0].clone();
        ambiguous["cuentas"].as_array_mut().unwrap().push(duplicate);
        assert!(parse_account_funds(&ambiguous).is_err());

        let mut malformed = official_account_state_fixture();
        malformed["cuentas"]
            .as_array_mut()
            .unwrap()
            .push(serde_json::json!("cuenta-inválida"));
        assert!(parse_account_funds(&malformed).is_err());
    }

    #[test]
    fn rejects_missing_immediate_or_negative_account_funds() {
        let mut missing_immediate = official_account_state_fixture();
        missing_immediate["cuentas"][0]["saldos"][0]["liquidacion"] = serde_json::json!("hrs24");
        assert!(parse_account_funds(&missing_immediate).is_err());

        let mut negative = official_account_state_fixture();
        negative["cuentas"][0]["disponible"] = serde_json::json!(-1.0);
        assert!(parse_account_funds(&negative).is_err());

        let mut malformed_balance = official_account_state_fixture();
        malformed_balance["cuentas"][0]["saldos"]
            .as_array_mut()
            .unwrap()
            .push(serde_json::json!(false));
        assert!(parse_account_funds(&malformed_balance).is_err());
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
        assert_eq!(profile.masked_account_number(), "••••3456");
        assert_eq!(profile.redacted_name(), "Ada L.");
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
    fn latest_operation_excludes_newer_non_option_instruments() {
        let operation = latest_operation_number(&serde_json::json!([
            {
                "numero": 10,
                "fechaOperada": "2026-08-20T10:00:00-03:00",
                "tipoInstrumento": "Opción CALL"
            },
            {
                "numero": 11,
                "fechaOperada": "2026-08-21T09:00:00-03:00",
                "tipoInstrumento": "Acciones"
            }
        ]));
        assert_eq!(operation.as_deref(), Some("10"));
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
    fn cost_calibration_rejects_zero_operation_amount() {
        let error = parse_cost_calibration(
            "zero",
            &serde_json::json!({
                "montoOperado": 0,
                "aranceles": [{"tipo": "Comisión", "neto": 1, "iva": 0.21}]
            }),
        )
        .unwrap_err();
        assert!(error.to_string().contains("monto operado ausente o cero"));
    }

    #[test]
    fn cost_calibration_normalizes_millisecond_timestamps_above_exact_boundary() {
        let calibration_at = |timestamp| {
            parse_cost_calibration(
                "timestamp",
                &serde_json::json!({
                    "montoOperado": 100,
                    "timestamp": timestamp,
                    "aranceles": [{"tipo": "Comisión", "neto": 1}]
                }),
            )
            .unwrap()
            .observed_at_secs
        };
        assert_eq!(calibration_at(10_000_000_000_i64), 10_000_000_000);
        assert_eq!(calibration_at(10_000_000_001_i64), 10_000_000);
    }

    #[test]
    fn cost_calibration_preserves_net_only_and_vat_only_components() {
        let calibration = parse_cost_calibration(
            "split-fees",
            &serde_json::json!({
                "montoOperado": 100,
                "aranceles": [
                    {"tipo": "Derecho de mercado", "neto": 10, "iva": 0},
                    {"tipo": "IVA aislado", "neto": 0, "iva": 2},
                    {"tipo": "Sin valor", "neto": 0, "iva": 0}
                ]
            }),
        )
        .unwrap();
        assert_eq!(calibration.components.len(), 2);
        assert!((calibration.commission_percentage - 10.0).abs() < 1e-9);
        assert_eq!(calibration.other_fees_percentage, 0.0);
        assert!((calibration.vat_percentage - 20.0).abs() < 1e-9);
        assert!((calibration.total_cost_percentage - 12.0).abs() < 1e-9);

        let vat_only = parse_cost_calibration(
            "vat-only",
            &serde_json::json!({
                "montoOperado": 100,
                "aranceles": [{"tipo": "IVA aislado", "neto": 0, "iva": 2}]
            }),
        )
        .unwrap();
        assert_eq!(vat_only.components.len(), 1);
        assert_eq!(vat_only.commission_percentage, 0.0);
        assert_eq!(vat_only.vat_percentage, 0.0);
        assert_eq!(vat_only.other_fees_percentage, 0.0);
        assert!((vat_only.total_cost_percentage - 2.0).abs() < 1e-9);
    }

    #[test]
    fn commission_classifier_recognizes_each_contractual_alias_independently() {
        for alias in ["Comision IOL", "Comisión IOL", "Honorarios", "Corretaje"] {
            assert!(is_commission(alias), "alias no reconocido: {alias}");
        }
        assert!(!is_commission("Derecho de mercado"));
    }

    #[test]
    fn infers_contract_multiplier_from_settled_amount_price_and_quantity() {
        let calibration = parse_cost_calibration(
            "43",
            &serde_json::json!({
                "montoOperado": 25_000.0,
                "precioOperado": 2.5,
                "cantidadOperada": 100.0,
                "tipoInstrumento": "Opción",
                "aranceles": [{"tipo": "Comisión", "neto": 10.0, "iva": 2.1}]
            }),
        )
        .unwrap();
        assert_eq!(calibration.observed_contract_multiplier, Some(100));
    }

    #[test]
    fn contract_multiplier_enforces_finite_integer_range_and_tolerance() {
        assert_eq!(infer_contract_multiplier(1.0, 1.0, 1.0), Some(1));
        assert_eq!(infer_contract_multiplier(50.0, 1.0, 1.0), Some(50));
        assert_eq!(
            infer_contract_multiplier(f64::from(u32::MAX), 1.0, 1.0),
            Some(u32::MAX)
        );
        assert_eq!(infer_contract_multiplier(-1.0, 1.0, 1.0), None);
        assert_eq!(infer_contract_multiplier(3.0, 1.0, 2.0), None);
        assert_eq!(
            infer_contract_multiplier(f64::from(u32::MAX) + 1.0, 1.0, 1.0),
            None
        );
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

    #[test]
    fn parses_websocket_order_number_when_present() {
        let event = parse_realtime_message(
            r#"{"IdMovimiento":1111,"Estado":"Confirmada","Tipo":"Operacion","Simbolo":"GFGC100","NumeroOperacion":87044496}"#,
        )
        .unwrap();
        let IolRealtimeEvent::Movement(movement) = event else {
            panic!("se esperaba movimiento");
        };
        assert_eq!(movement.numero_operacion.as_deref(), Some("87044496"));
    }
    use std::{
        io::{Read, Write},
        net::TcpListener,
        thread,
    };

    #[test]
    fn exact_catalog_body_is_content_addressed_and_verified_on_reuse() {
        let directory = std::env::temp_dir().join(format!(
            "options-iol-catalog-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let raw = br#"[{"simbolo":"GFGC100","lote":100}]"#.to_vec();
        let sha256 = digest_bytes(&raw);
        let decoded = DecodedJson {
            value: serde_json::from_slice(&raw).unwrap(),
            raw: raw.clone(),
            sha256,
            received_at_secs: 1_000,
        };
        let client = IolClient::new(
            "http://127.0.0.1:1",
            "user".into(),
            encrypted_test_password(),
            String::new(),
        )
        .unwrap()
        .with_catalog_archive_dir(directory.clone());

        assert!(client.archive_option_catalog("GGAL", &decoded).unwrap());
        let path = directory
            .join("GGAL")
            .join(format!("iol-options-v1-{}.json", hex_sha256(&sha256)));
        assert_eq!(std::fs::read(&path).unwrap(), raw);

        std::fs::write(&path, b"alterado").unwrap();
        assert!(client.archive_option_catalog("GGAL", &decoded).is_err());
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn parses_market_contract_with_nested_book() {
        let underlying = serde_json::json!({
            "ultimoPrecio": 100.5,
            "puntas": [{"precioCompra": 100.4, "precioVenta": 100.6}],
            "timestamp_secs": 10
        });
        let contracts = ["GFGC100", "GFGC110", "GFGC120"]
            .into_iter()
            .map(|symbol| OptionContract {
                symbol: symbol.into(),
                kind: OptionKind::Call,
                strike: 100.0,
                expiry_days: 1,
                expiration_timestamp_secs: None,
                contract_multiplier: None,
                observed_at_secs: unix_now(),
                exercise_style: ExerciseStyle::American,
                catalog_schema_version: 1,
                catalog_sha256: [1; 32],
                catalog_archived: true,
            })
            .collect::<Vec<_>>();
        let quotes = serde_json::json!({"titulos": [
            {
                "simbolo": "GFGC100", "ultimoPrecio": 2.1, "volumen": 20,
                "puntas": {"precioCompra": 2.0, "precioVenta": 2.2}
            },
            {
                "simbolo": "GFGC110", "ultimoPrecio": -1.0, "volumen": 20
            }
        ]});
        let frame = parse_market_frame("GGAL", underlying, &contracts, quotes).unwrap();
        assert_eq!(frame.underlying.ask, Some(100.6));
        assert_eq!(frame.options[0].bid, Some(2.0));
        assert_eq!(frame.options[0].kind, OptionKind::Call);
        assert_eq!(
            frame.options[0].contract_metadata_source,
            ContractMetadataSource::IolCatalog
        );
        assert!(frame.options[0].catalog_observed_at_secs.is_some());
        assert_eq!(
            frame.option_chain_quality,
            Some(OptionChainQuality {
                catalog_contracts: 3,
                quote_rows: 2,
                accepted_contracts: 1,
                missing_quote_contracts: 1,
                invalid_quote_contracts: 1,
                accepted_call_contracts: 1,
                accepted_put_contracts: 0,
                by_expiry: vec![OptionExpiryQuality {
                    expiry_days: 1,
                    catalog_contracts: 3,
                    accepted_contracts: 1,
                    missing_quote_contracts: 1,
                    invalid_quote_contracts: 1,
                    accepted_call_contracts: 1,
                    accepted_put_contracts: 0,
                }],
            })
        );
        assert_eq!(
            frame.options[0].timestamp_source,
            QuoteTimestampSource::Received
        );
        assert_ne!(
            frame.options[0].timestamp_secs,
            frame.underlying.timestamp_secs
        );
    }

    #[test]
    fn http_date_detects_host_clock_skew_at_the_closed_boundary() {
        let source = 1_787_326_245;
        let date = chrono::DateTime::from_timestamp(source, 0)
            .unwrap()
            .to_rfc2822();
        assert!(validate_http_date_value(
            &date,
            source + crate::market::MAX_SOURCE_CLOCK_SKEW_SECS
        )
        .is_ok());
        assert!(validate_http_date_value(
            &date,
            source + crate::market::MAX_SOURCE_CLOCK_SKEW_SECS + 1
        )
        .is_err());
        assert!(validate_http_date_value("not-a-date", source).is_err());
    }

    #[test]
    fn parses_live_iol_strike_from_catalog_description() {
        let catalog = serde_json::json!([{
            "cotizacion": {"ultimoPrecio": 2424.629},
            "simboloSubyacente": "GGAL",
            "fechaVencimiento": "2026-08-21T15:30:00",
            "tipoOpcion": "Call",
            "simbolo": "GFGC4200AG",
            "descripcion": "Call GGAL 4,200.00 Vencimiento: 21/08/2026",
            "lote": 100
        }]);

        let contracts = parse_option_catalog("GGAL", &catalog, unix_now(), [7; 32], true).unwrap();

        assert_eq!(contracts.len(), 1);
        assert_eq!(contracts[0].strike, 4200.0);
        assert_eq!(contracts[0].expiration_timestamp_secs, Some(1_787_342_400));
        assert_eq!(contracts[0].contract_multiplier, Some(100));
        assert!(contracts[0].observed_at_secs > 0);
        assert_eq!(contracts[0].catalog_sha256, [7; 32]);
        assert!(contracts[0].catalog_archived);
    }

    #[test]
    fn strike_description_normalizes_supported_locales_and_rejects_non_positive_values() {
        for (description, expected) in [
            ("Call GGAL $4,200.00;", 4_200.0),
            ("Call GGAL 4.200,00", 4_200.0),
            ("Call GGAL 4,200", 4_200.0),
            ("Call GGAL 42,50", 42.5),
            ("Call GGAL +42.50", 42.5),
        ] {
            assert_eq!(strike_from_option_description(description), Some(expected));
        }
        for description in ["Call GGAL 0", "Call GGAL -1"] {
            assert_eq!(strike_from_option_description(description), None);
        }
    }

    #[test]
    fn catalog_multiplier_accepts_one_and_rejects_zero_at_the_exact_boundary() {
        let catalog = serde_json::json!([
            {
                "simboloSubyacente": "GGAL",
                "simbolo": "GFGC100",
                "tipoOpcion": "Call",
                "precioEjercicio": 100,
                "diasVencimiento": 21,
                "lote": 1
            },
            {
                "simboloSubyacente": "GGAL",
                "simbolo": "GFGP90",
                "tipoOpcion": "Put",
                "precioEjercicio": 90,
                "diasVencimiento": 21,
                "lote": 0
            }
        ]);
        let contracts = parse_option_catalog("GGAL", &catalog, 1_000, [9; 32], true).unwrap();
        assert_eq!(contracts.len(), 2);
        assert_eq!(contracts[0].contract_multiplier, Some(1));
        assert_eq!(contracts[1].contract_multiplier, None);
    }

    #[test]
    fn invalid_numeric_strike_falls_back_to_the_valid_description() {
        for numeric in [0.0, -1.0] {
            let value = serde_json::json!({
                "precioEjercicio": numeric,
                "descripcion": "Call GGAL 4,200.00"
            });
            assert_eq!(option_strike(value.as_object().unwrap()), Some(4_200.0));
        }
    }

    #[test]
    fn parses_live_iol_panel_with_null_option_metadata_and_empty_book() {
        let underlying = serde_json::json!({"ultimoPrecio": 6600.0});
        let contracts = vec![OptionContract {
            symbol: "GFGC4200AG".into(),
            kind: OptionKind::Call,
            strike: 4200.0,
            expiry_days: 0,
            expiration_timestamp_secs: None,
            contract_multiplier: None,
            observed_at_secs: unix_now(),
            exercise_style: ExerciseStyle::American,
            catalog_schema_version: 1,
            catalog_sha256: [2; 32],
            catalog_archived: true,
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
    fn parses_iol_terminal_order_states_and_numeric_id() {
        let request = OrderRequest {
            operation_id: "op-1".into(),
            symbol: "GFGC100".into(),
            quantity: 2,
            market_price: 2.0,
            limit_price: 2.1,
            side: OrderSide::Buy,
        };
        let executed = parse_order_execution(
            &request,
            serde_json::json!({
                "numero": 42,
                "estado": "Terminada",
                "cantidadOperada": 2,
                "precioOperado": 2.05
            }),
        )
        .unwrap();
        assert_eq!(executed.status, OrderStatus::Executed);
        assert_eq!(executed.broker_order_id.as_deref(), Some("42"));
        assert_eq!(executed.fill_price, Some(2.05));

        let cancelled = parse_order_execution(
            &request,
            serde_json::json!({"numero": 42, "estado": "Cancelada", "cantidadOperada": 0}),
        )
        .unwrap();
        assert_eq!(cancelled.status, OrderStatus::Cancelled);
    }

    #[test]
    fn rejects_executed_order_without_independent_fill_fields() {
        let request = test_order_request();
        let cases = [
            serde_json::json!({
                "numero": 42,
                "estado": "Terminada",
                "precioOperado": 2.05
            }),
            serde_json::json!({
                "numero": 42,
                "estado": "Terminada",
                "cantidadOperada": 0,
                "precioOperado": 2.05
            }),
            serde_json::json!({
                "numero": 42,
                "estado": "Terminada",
                "cantidadOperada": -1,
                "precioOperado": 2.05
            }),
            serde_json::json!({
                "numero": 42,
                "estado": "Terminada",
                "cantidadOperada": 3,
                "precioOperado": 2.05
            }),
            serde_json::json!({
                "numero": 42,
                "estado": "Terminada",
                "cantidadOperada": 2.5,
                "precioOperado": 2.05
            }),
            serde_json::json!({
                "numero": 42,
                "estado": "Terminada",
                "cantidadOperada": 2
            }),
            serde_json::json!({
                "estado": "Terminada",
                "cantidadOperada": 2,
                "precioOperado": 2.05
            }),
        ];

        for body in cases {
            assert!(parse_order_execution(&request, body).is_err());
        }
    }

    #[test]
    fn executed_quantity_accepts_u32_max_exactly_and_rejects_the_next_integer() {
        let mut request = test_order_request();
        request.quantity = u32::MAX;
        let accepted = parse_order_execution(
            &request,
            serde_json::json!({
                "numero": 42,
                "estado": "Terminada",
                "cantidadOperada": u32::MAX,
                "precioOperado": 2.05
            }),
        )
        .unwrap();
        assert_eq!(accepted.filled_quantity, u32::MAX);

        let above_u32 = i64::from(u32::MAX) + 1;
        let rejected = parse_order_execution(
            &request,
            serde_json::json!({
                "numero": 42,
                "estado": "Cancelada",
                "cantidadOperada": above_u32
            }),
        )
        .unwrap_err();
        assert!(rejected
            .to_string()
            .contains("cantidad ejecutada fuera de rango"));
    }

    #[test]
    fn rejects_unknown_or_contradictory_order_states() {
        let request = test_order_request();
        let unknown = parse_order_execution(
            &request,
            serde_json::json!({"numero": 42, "estado": "EnRevision"}),
        );
        assert!(unknown.is_err());

        let pending_with_fill = parse_order_execution(
            &request,
            serde_json::json!({
                "numero": 42,
                "estado": "Pendiente",
                "cantidadOperada": 1,
                "precioOperado": 2.05
            }),
        );
        assert!(pending_with_fill.is_err());

        let rejected_with_fill = parse_order_execution(
            &request,
            serde_json::json!({
                "numero": 42,
                "estado": "Rechazada",
                "cantidadOperada": 1,
                "precioOperado": 2.05
            }),
        );
        assert!(rejected_with_fill.is_err());
    }

    #[test]
    fn accepts_explicit_partial_fill_and_partial_cancellation() {
        let request = test_order_request();
        let partial = parse_order_execution(
            &request,
            serde_json::json!({
                "numero": 42,
                "estado": "Parcial",
                "cantidadOperada": 1,
                "precioOperado": 2.05
            }),
        )
        .unwrap();
        assert_eq!(partial.status, OrderStatus::PartiallyExecuted);
        assert_eq!(partial.filled_quantity, 1);

        let cancelled = parse_order_execution(
            &request,
            serde_json::json!({
                "numero": 42,
                "estado": "Cancelada",
                "cantidadOperada": 1,
                "precioOperado": 2.05
            }),
        )
        .unwrap();
        assert_eq!(cancelled.status, OrderStatus::Cancelled);
        assert_eq!(cancelled.filled_quantity, 1);
    }

    #[tokio::test]
    async fn tracks_pending_order_by_rest_until_executed() {
        let token = r#"{"access_token":"access-1","refresh_token":"refresh-1","expires_in":3600,"token_type":"Bearer"}"#;
        let Some((base_url, server)) = mock_server(vec![
            ("POST /token", "200 OK", token),
            (
                "GET /api/v2/operaciones/42",
                "200 OK",
                r#"{"numero":42,"estado":"Terminada","cantidadOperada":2,"precioOperado":2.05}"#,
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
        let request = test_order_request();
        let initial = OrderExecution {
            operation_id: request.operation_id.clone(),
            status: OrderStatus::Pending,
            filled_quantity: 0,
            fill_price: None,
            broker_order_id: Some("42".into()),
            message: None,
        };

        let execution = client
            .track_order_to_terminal(
                &request,
                initial,
                Duration::from_secs(1),
                Duration::from_millis(10),
                Duration::from_secs(1),
            )
            .await
            .unwrap();

        assert_eq!(execution.status, OrderStatus::Executed);
        assert_eq!(execution.filled_quantity, 2);
        server.join().unwrap();
    }

    #[tokio::test]
    async fn polling_retries_transport_errors_within_a_positive_deadline() {
        let token = r#"{"access_token":"access-1","refresh_token":"refresh-1","expires_in":3600,"token_type":"Bearer"}"#;
        let executed =
            r#"{"numero":42,"estado":"Terminada","cantidadOperada":2,"precioOperado":2.05}"#;
        let Some((base_url, server)) = mock_server(vec![
            ("POST /token", "200 OK", token),
            (
                "GET /api/v2/operaciones/42",
                "500 Internal Server Error",
                "{}",
            ),
            ("GET /api/v2/operaciones/42", "200 OK", executed),
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
        let request = test_order_request();
        let mut last = pending_execution(&request, Some("42"));
        let execution = tokio::time::timeout(
            Duration::from_secs(1),
            client.poll_order_until(
                &request,
                "42",
                Duration::from_millis(200),
                Duration::ZERO,
                &mut last,
            ),
        )
        .await
        .expect("el poll interno debe respetar su deadline")
        .unwrap()
        .expect("el segundo poll debe observar la ejecución");
        assert_eq!(execution.status, OrderStatus::Executed);
        assert_eq!(client.order_tracking_metrics().rest_polls, 2);
        server.join().unwrap();
    }

    #[tokio::test]
    async fn polling_rejects_non_retryable_payload_errors_immediately() {
        let token = r#"{"access_token":"access-1","refresh_token":"refresh-1","expires_in":3600,"token_type":"Bearer"}"#;
        let Some((base_url, server)) = mock_server(vec![
            ("POST /token", "200 OK", token),
            (
                "GET /api/v2/operaciones/42",
                "200 OK",
                r#"{"numero":42,"estado":"EnRevision"}"#,
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
        let request = test_order_request();
        let mut last = pending_execution(&request, Some("42"));
        let result = tokio::time::timeout(
            Duration::from_secs(1),
            client.poll_order_until(
                &request,
                "42",
                Duration::from_millis(50),
                Duration::ZERO,
                &mut last,
            ),
        )
        .await
        .expect("un payload inválido no debe bloquear el poll")
        .unwrap_err();
        assert!(matches!(result, IolClientError::InvalidResponse(_)));
        assert!(result.to_string().contains("estado de orden desconocido"));
        assert_eq!(client.order_tracking_metrics().rest_polls, 1);
        server.join().unwrap();
    }

    #[tokio::test]
    async fn zero_poll_deadline_returns_after_the_first_pending_read() {
        let token = r#"{"access_token":"access-1","refresh_token":"refresh-1","expires_in":3600,"token_type":"Bearer"}"#;
        let pending = r#"{"numero":42,"estado":"Pendiente","cantidadOperada":0}"#;
        let Some((base_url, server)) = mock_server(vec![
            ("POST /token", "200 OK", token),
            ("GET /api/v2/operaciones/42", "200 OK", pending),
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
        let request = test_order_request();
        let mut last = pending_execution(&request, Some("42"));
        let result = tokio::time::timeout(
            Duration::from_millis(250),
            client.poll_order_until(&request, "42", Duration::ZERO, Duration::ZERO, &mut last),
        )
        .await
        .expect("un deadline cero debe terminar de forma acotada")
        .unwrap();
        assert!(result.is_none());
        assert_eq!(client.order_tracking_metrics().rest_polls, 1);
        server.join().unwrap();
    }

    #[tokio::test]
    async fn cancels_order_after_tracking_timeout_and_confirms_cancellation() {
        let token = r#"{"access_token":"access-1","refresh_token":"refresh-1","expires_in":3600,"token_type":"Bearer"}"#;
        let pending = r#"{"numero":42,"estado":"Pendiente","cantidadOperada":0}"#;
        let Some((base_url, server)) = mock_server(vec![
            ("POST /token", "200 OK", token),
            ("GET /api/v2/operaciones/42", "200 OK", pending),
            ("GET /api/v2/operaciones/42", "200 OK", pending),
            ("DELETE /api/v2/operaciones/42", "200 OK", "{}"),
            (
                "GET /api/v2/operaciones/42",
                "200 OK",
                r#"{"numero":42,"estado":"Cancelada","cantidadOperada":0}"#,
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
        let request = test_order_request();
        let initial = OrderExecution {
            operation_id: request.operation_id.clone(),
            status: OrderStatus::Pending,
            filled_quantity: 0,
            fill_price: None,
            broker_order_id: Some("42".into()),
            message: None,
        };

        let execution = client
            .track_order_to_terminal(
                &request,
                initial,
                Duration::ZERO,
                Duration::from_millis(10),
                Duration::ZERO,
            )
            .await
            .unwrap();

        assert_eq!(execution.status, OrderStatus::Cancelled);
        assert_eq!(execution.filled_quantity, 0);
        server.join().unwrap();
    }

    fn pending_execution(request: &OrderRequest, broker_order_id: Option<&str>) -> OrderExecution {
        OrderExecution {
            operation_id: request.operation_id.clone(),
            status: OrderStatus::Pending,
            filled_quantity: 0,
            fill_price: None,
            broker_order_id: broker_order_id.map(str::to_string),
            message: None,
        }
    }

    #[tokio::test]
    async fn terminal_initial_order_returns_without_network_and_missing_id_fails_closed() {
        let mut client = IolClient::new(
            "http://127.0.0.1:1",
            "user".into(),
            encrypted_test_password(),
            String::new(),
        )
        .unwrap();
        let request = test_order_request();
        let terminal = OrderExecution {
            operation_id: request.operation_id.clone(),
            status: OrderStatus::Rejected,
            filled_quantity: 0,
            fill_price: None,
            broker_order_id: Some("42".into()),
            message: Some("rechazo contractual".into()),
        };
        assert_eq!(
            client
                .track_order_to_terminal(
                    &request,
                    terminal.clone(),
                    Duration::ZERO,
                    Duration::ZERO,
                    Duration::ZERO,
                )
                .await
                .unwrap(),
            terminal
        );
        assert_eq!(
            client.order_tracking_metrics(),
            OrderTrackingMetrics::default()
        );

        let error = client
            .track_order_to_terminal(
                &request,
                pending_execution(&request, None),
                Duration::ZERO,
                Duration::ZERO,
                Duration::ZERO,
            )
            .await
            .unwrap_err();
        assert!(matches!(error, IolClientError::InvalidResponse(_)));
    }

    #[tokio::test]
    async fn final_pre_cancel_read_prevents_deleting_an_already_executed_order() {
        let token = r#"{"access_token":"access-1","refresh_token":"refresh-1","expires_in":3600,"token_type":"Bearer"}"#;
        let pending = r#"{"numero":42,"estado":"Pendiente","cantidadOperada":0}"#;
        let executed =
            r#"{"numero":42,"estado":"Terminada","cantidadOperada":2,"precioOperado":2.05}"#;
        let Some((base_url, server)) = mock_server(vec![
            ("POST /token", "200 OK", token),
            ("GET /api/v2/operaciones/42", "200 OK", pending),
            ("GET /api/v2/operaciones/42", "200 OK", executed),
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
        let request = test_order_request();
        let result = client
            .track_order_to_terminal(
                &request,
                pending_execution(&request, Some("42")),
                Duration::ZERO,
                Duration::ZERO,
                Duration::ZERO,
            )
            .await
            .unwrap();
        assert_eq!(result.status, OrderStatus::Executed);
        assert!(!client.order_tracking_metrics().cancellation_requested);
        server.join().unwrap();
    }

    #[tokio::test]
    async fn final_pre_cancel_read_retries_only_transport_errors() {
        let token = r#"{"access_token":"access-1","refresh_token":"refresh-1","expires_in":3600,"token_type":"Bearer"}"#;
        let pending = r#"{"numero":42,"estado":"Pendiente","cantidadOperada":0}"#;
        let cancelled = r#"{"numero":42,"estado":"Cancelada","cantidadOperada":0}"#;
        let Some((base_url, server)) = mock_server(vec![
            ("POST /token", "200 OK", token),
            ("GET /api/v2/operaciones/42", "200 OK", pending),
            (
                "GET /api/v2/operaciones/42",
                "500 Internal Server Error",
                "{}",
            ),
            ("DELETE /api/v2/operaciones/42", "200 OK", "{}"),
            ("GET /api/v2/operaciones/42", "200 OK", cancelled),
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
        let request = test_order_request();
        let result = client
            .track_order_to_terminal(
                &request,
                pending_execution(&request, Some("42")),
                Duration::ZERO,
                Duration::ZERO,
                Duration::ZERO,
            )
            .await
            .unwrap();
        assert_eq!(result.status, OrderStatus::Cancelled);
        assert!(client.order_tracking_metrics().cancellation_requested);
        server.join().unwrap();
    }

    #[tokio::test]
    async fn final_pre_cancel_read_propagates_non_retryable_payload_errors() {
        let token = r#"{"access_token":"access-1","refresh_token":"refresh-1","expires_in":3600,"token_type":"Bearer"}"#;
        let pending = r#"{"numero":42,"estado":"Pendiente","cantidadOperada":0}"#;
        let Some((base_url, server)) = mock_server(vec![
            ("POST /token", "200 OK", token),
            ("GET /api/v2/operaciones/42", "200 OK", pending),
            (
                "GET /api/v2/operaciones/42",
                "200 OK",
                r#"{"numero":42,"estado":"EnRevision"}"#,
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
        let request = test_order_request();
        let error = client
            .track_order_to_terminal(
                &request,
                pending_execution(&request, Some("42")),
                Duration::ZERO,
                Duration::ZERO,
                Duration::ZERO,
            )
            .await
            .unwrap_err();
        assert!(matches!(error, IolClientError::InvalidResponse(_)));
        assert!(error.to_string().contains("estado de orden desconocido"));
        assert!(!client.order_tracking_metrics().cancellation_requested);
        server.join().unwrap();
    }

    #[tokio::test]
    async fn failed_cancel_reconciles_a_terminal_state_before_reporting_error() {
        let token = r#"{"access_token":"access-1","refresh_token":"refresh-1","expires_in":3600,"token_type":"Bearer"}"#;
        let pending = r#"{"numero":42,"estado":"Pendiente","cantidadOperada":0}"#;
        let executed =
            r#"{"numero":42,"estado":"Terminada","cantidadOperada":2,"precioOperado":2.05}"#;
        let Some((base_url, server)) = mock_server(vec![
            ("POST /token", "200 OK", token),
            ("GET /api/v2/operaciones/42", "200 OK", pending),
            ("GET /api/v2/operaciones/42", "200 OK", pending),
            (
                "DELETE /api/v2/operaciones/42",
                "500 Internal Server Error",
                "{}",
            ),
            ("GET /api/v2/operaciones/42", "200 OK", executed),
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
        let request = test_order_request();
        let result = client
            .track_order_to_terminal(
                &request,
                pending_execution(&request, Some("42")),
                Duration::ZERO,
                Duration::ZERO,
                Duration::ZERO,
            )
            .await
            .unwrap();
        assert_eq!(result.status, OrderStatus::Executed);
        assert!(client.order_tracking_metrics().cancellation_requested);
        server.join().unwrap();
    }

    #[tokio::test]
    async fn failed_cancel_reconciliation_rejects_a_regressive_terminal_fill() {
        let token = r#"{"access_token":"access-1","refresh_token":"refresh-1","expires_in":3600,"token_type":"Bearer"}"#;
        let partial =
            r#"{"numero":42,"estado":"Parcial","cantidadOperada":1,"precioOperado":2.05}"#;
        let regressive_cancelled = r#"{"numero":42,"estado":"Cancelada","cantidadOperada":0}"#;
        let Some((base_url, server)) = mock_server(vec![
            ("POST /token", "200 OK", token),
            ("GET /api/v2/operaciones/42", "200 OK", partial),
            ("GET /api/v2/operaciones/42", "200 OK", partial),
            (
                "DELETE /api/v2/operaciones/42",
                "500 Internal Server Error",
                "{}",
            ),
            ("GET /api/v2/operaciones/42", "200 OK", regressive_cancelled),
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
        let request = test_order_request();
        let error = client
            .track_order_to_terminal(
                &request,
                pending_execution(&request, Some("42")),
                Duration::ZERO,
                Duration::ZERO,
                Duration::ZERO,
            )
            .await
            .unwrap_err();
        assert!(matches!(error, IolClientError::InvalidResponse(_)));
        assert!(
            error.to_string().contains("cantidad ejecutada retrocedió"),
            "error inesperado: {error}"
        );
        assert!(client.order_tracking_metrics().cancellation_requested);
        server.join().unwrap();
    }

    #[tokio::test]
    async fn accepted_cancel_without_terminal_confirmation_remains_an_error() {
        let token = r#"{"access_token":"access-1","refresh_token":"refresh-1","expires_in":3600,"token_type":"Bearer"}"#;
        let pending = r#"{"numero":42,"estado":"Pendiente","cantidadOperada":0}"#;
        let Some((base_url, server)) = mock_server(vec![
            ("POST /token", "200 OK", token),
            ("GET /api/v2/operaciones/42", "200 OK", pending),
            ("GET /api/v2/operaciones/42", "200 OK", pending),
            ("DELETE /api/v2/operaciones/42", "200 OK", "{}"),
            ("GET /api/v2/operaciones/42", "200 OK", pending),
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
        let request = test_order_request();
        let error = client
            .track_order_to_terminal(
                &request,
                pending_execution(&request, Some("42")),
                Duration::ZERO,
                Duration::ZERO,
                Duration::ZERO,
            )
            .await
            .unwrap_err();
        assert!(matches!(error, IolClientError::InvalidResponse(_)));
        assert!(
            error.to_string().contains("no confirmó un estado terminal"),
            "error inesperado: {error}"
        );
        assert!(client.order_tracking_metrics().cancellation_requested);
        server.join().unwrap();
    }

    #[tokio::test]
    async fn unauthorized_cancel_refreshes_once_then_confirms_by_rest() {
        let token = r#"{"access_token":"access-1","refresh_token":"refresh-1","expires_in":3600,"token_type":"Bearer"}"#;
        let refreshed = r#"{"access_token":"access-2","refresh_token":"refresh-2","expires_in":3600,"token_type":"Bearer"}"#;
        let pending = r#"{"numero":42,"estado":"Pendiente","cantidadOperada":0}"#;
        let cancelled = r#"{"numero":42,"estado":"Cancelada","cantidadOperada":0}"#;
        let Some((base_url, server)) = mock_server(vec![
            ("POST /token", "200 OK", token),
            ("GET /api/v2/operaciones/42", "200 OK", pending),
            ("GET /api/v2/operaciones/42", "200 OK", pending),
            ("DELETE /api/v2/operaciones/42", "401 Unauthorized", "{}"),
            ("POST /token", "200 OK", refreshed),
            ("DELETE /api/v2/operaciones/42", "200 OK", "{}"),
            ("GET /api/v2/operaciones/42", "200 OK", cancelled),
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
        let request = test_order_request();
        let result = client
            .track_order_to_terminal(
                &request,
                pending_execution(&request, Some("42")),
                Duration::ZERO,
                Duration::ZERO,
                Duration::ZERO,
            )
            .await
            .unwrap();
        assert_eq!(result.status, OrderStatus::Cancelled);
        server.join().unwrap();
    }

    #[tokio::test]
    async fn websocket_signal_only_accelerates_the_correlated_rest_poll() {
        let mut client = IolClient::new(
            "http://127.0.0.1:1",
            "user".into(),
            encrypted_test_password(),
            String::new(),
        )
        .unwrap();
        let (event_tx, event_rx) = mpsc::channel(4);
        let (command_tx, mut command_rx) = mpsc::channel(1);
        let dropped_events = Arc::new(AtomicU64::new(2));
        let task = tokio::spawn(async move {
            let _ = command_rx.recv().await;
        });
        client.movement_stream = Some(MovementStream {
            events: event_rx,
            commands: command_tx,
            dropped_events,
            task,
        });
        event_tx
            .send(IolRealtimeEvent::Movement(AccountMovement {
                id_movimiento: Some(1),
                cuenta_comitente: String::new(),
                tipo: "Operacion".into(),
                estado: "Confirmada".into(),
                monto: None,
                cantidad: Some(2.0),
                simbolo: "GFGC100".into(),
                numero_operacion: Some("42".into()),
            }))
            .await
            .unwrap();
        client
            .wait_for_order_signal("42", Duration::from_millis(20))
            .await;
        assert_eq!(client.order_tracking_metrics().websocket_signals, 1);

        event_tx
            .send(IolRealtimeEvent::Notice("evento no correlacionado".into()))
            .await
            .unwrap();
        let events = client.drain_realtime_events();
        assert!(events
            .iter()
            .any(|event| matches!(event, IolRealtimeEvent::Movement(_))));
        assert!(events.iter().any(
            |event| matches!(event, IolRealtimeEvent::Notice(text) if text.contains("descartó 2"))
        ));
        client.shutdown().await;
        assert!(client.movement_stream.is_none());
    }

    fn test_order_request() -> OrderRequest {
        OrderRequest {
            operation_id: "op-1".into(),
            symbol: "GFGC100".into(),
            quantity: 2,
            market_price: 2.0,
            limit_price: 2.1,
            side: OrderSide::Buy,
        }
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

    #[test]
    fn reconciliation_rejects_every_ambiguous_position_row() {
        let cases = [
            serde_json::json!({"activos": [false]}),
            serde_json::json!({"activos": [{"cantidad": 1}]}),
            serde_json::json!({"activos": [{"simbolo": " ", "cantidad": 1}]}),
            serde_json::json!({"activos": [{"simbolo": "GFGC100"}]}),
            serde_json::json!({"activos": [{"simbolo": "GFGC100", "cantidad": 0}]}),
            serde_json::json!({"activos": [{"simbolo": "GFGC100", "cantidad": -1}]}),
            serde_json::json!({"activos": [{"simbolo": "GFGC100", "cantidad": 1.5}]}),
            serde_json::json!({"activos": [{"simbolo": "GFGC100", "cantidad": 4_294_967_296_u64}]}),
        ];
        for body in cases {
            assert!(parse_account_positions(&body).is_err());
        }
    }

    #[test]
    fn reconciliation_rejects_every_ambiguous_pending_order_row() {
        let valid = serde_json::json!({
            "numero": 42,
            "simbolo": "GFGC100",
            "tipoOperacion": "compra",
            "cantidad": 1
        });
        let cases = [
            serde_json::json!({"operaciones": [false]}),
            serde_json::json!({"operaciones": [{"numero": 42, "tipoOperacion": "compra", "cantidad": 1}]}),
            serde_json::json!({"operaciones": [{"numero": 42, "simbolo": " ", "tipoOperacion": "compra", "cantidad": 1}]}),
            serde_json::json!({"operaciones": [{"numero": 42, "simbolo": "GFGC100", "cantidad": 1}]}),
            serde_json::json!({"operaciones": [{"numero": 42, "simbolo": "GFGC100", "tipoOperacion": "desconocida", "cantidad": 1}]}),
            serde_json::json!({"operaciones": [{"simbolo": "GFGC100", "tipoOperacion": "compra", "cantidad": 1}]}),
            serde_json::json!({"operaciones": [{"numero": " ", "simbolo": "GFGC100", "tipoOperacion": "compra", "cantidad": 1}]}),
            serde_json::json!({"operaciones": [{"numero": 42, "simbolo": "GFGC100", "tipoOperacion": "compra"}]}),
            serde_json::json!({"operaciones": [{"numero": 42, "simbolo": "GFGC100", "tipoOperacion": "compra", "cantidad": 2.5}]}),
        ];
        for body in cases {
            assert!(parse_pending_orders(&body).is_err());
        }

        let mut sell = valid;
        sell["tipoOperacion"] = serde_json::json!("SELL");
        let parsed = parse_pending_orders(&serde_json::json!([sell])).unwrap();
        assert_eq!(parsed[0].side, Some(OrderSide::Sell));
    }

    #[test]
    fn reconciliation_accepts_aliases_without_silently_rounding_quantities() {
        let positions = parse_account_positions(&serde_json::json!([{
            "instrument": {"symbol": "GFGV100", "kind": "PUT"},
            "quantity": "2",
            "averagePrice": -1
        }]))
        .unwrap();
        assert_eq!(positions[0].quantity, 2);
        assert_eq!(positions[0].kind, Some(PositionKind::Put));
        assert_eq!(positions[0].average_price, None);

        let orders = parse_pending_orders(&serde_json::json!({"items": [{
            "orderId": "order-1",
            "instrument": {"ticker": "GFGC100", "instrumentType": "Opción CALL"},
            "side": "BUY",
            "quantity": "3"
        }]}))
        .unwrap();
        assert_eq!(orders[0].broker_order_id, "order-1");
        assert_eq!(orders[0].side, Some(OrderSide::Buy));
        assert_eq!(orders[0].kind, Some(PositionKind::Call));
    }

    #[test]
    fn reconciliation_accepts_u32_max_without_accepting_larger_quantities() {
        let positions = parse_account_positions(&serde_json::json!([{
            "symbol": "GFGC100",
            "quantity": u32::MAX,
            "kind": "CALL"
        }]))
        .unwrap();
        assert_eq!(positions[0].quantity, u32::MAX);

        let above_u32 = u64::from(u32::MAX) + 1;
        assert!(parse_account_positions(&serde_json::json!([{
            "symbol": "GFGC100",
            "quantity": above_u32,
            "kind": "CALL"
        }]))
        .is_err());
    }

    #[test]
    fn reconciliation_does_not_treat_zero_average_price_as_observed_cost() {
        let positions = parse_account_positions(&serde_json::json!([{
            "symbol": "GFGC100",
            "quantity": 1,
            "averagePrice": 0,
            "kind": "CALL"
        }]))
        .unwrap();
        assert_eq!(positions[0].average_price, None);
    }

    #[test]
    fn pending_generic_option_descriptor_preserves_option_classification() {
        let orders = parse_pending_orders(&serde_json::json!([{
            "numero": 42,
            "simbolo": "GFGC100",
            "tipoInstrumento": "Opciones",
            "tipoOperacion": "compra",
            "cantidad": 1
        }]))
        .unwrap();
        assert!(orders[0].is_option);
        assert_eq!(orders[0].kind, None);
    }

    #[test]
    fn position_kind_recognizes_each_exact_and_descriptive_alias() {
        for value in ["CALL", "Opcion CALL", "Opción CALL"] {
            assert_eq!(parse_position_kind(value), Some(PositionKind::Call));
        }
        for value in ["PUT", "Opcion PUT", "Opción PUT"] {
            assert_eq!(parse_position_kind(value), Some(PositionKind::Put));
        }
        assert_eq!(parse_position_kind("Opciones"), None);
    }

    #[test]
    fn expiration_dates_reject_impossible_calendar_days() {
        let epoch = chrono::NaiveDate::from_ymd_opt(1970, 1, 1).unwrap();
        let valid = chrono::NaiveDate::from_ymd_opt(2028, 2, 29).unwrap();
        assert_eq!(
            date_days_since_epoch("2028-02-29T15:30:00"),
            Some(valid.signed_duration_since(epoch).num_days())
        );
        for invalid in [
            "2026-02-29",
            "2026-04-31",
            "2026-13-01",
            "not-a-date",
            "short",
        ] {
            assert_eq!(date_days_since_epoch(invalid), None);
        }
    }

    #[test]
    fn profile_redaction_and_parser_cover_absent_identity_fields() {
        let surname_only = AccountProfile {
            account_number: "7".into(),
            first_name: " ".into(),
            last_name: "Lovelace".into(),
        };
        assert_eq!(surname_only.masked_account_number(), "••••7");
        assert_eq!(surname_only.redacted_name(), "L.");
        let name_only = AccountProfile {
            last_name: String::new(),
            first_name: "Ada".into(),
            ..surname_only
        };
        assert_eq!(name_only.redacted_name(), "Ada");
        assert!(parse_account_profile(&serde_json::json!([])).is_err());
        assert!(parse_account_profile(&serde_json::json!({"nombre": "Ada"})).is_err());
    }

    #[test]
    fn account_funds_rejects_missing_fields_duplicates_and_nonfinite_strings() {
        let mut missing_status = official_account_state_fixture();
        missing_status["cuentas"][0]
            .as_object_mut()
            .unwrap()
            .remove("estado");
        assert!(parse_account_funds(&missing_status).is_err());

        let mut bad_balances = official_account_state_fixture();
        bad_balances["cuentas"][0]["saldos"] = serde_json::json!({});
        assert!(parse_account_funds(&bad_balances).is_err());

        let mut duplicate_immediate = official_account_state_fixture();
        let duplicate = duplicate_immediate["cuentas"][0]["saldos"][0].clone();
        duplicate_immediate["cuentas"][0]["saldos"]
            .as_array_mut()
            .unwrap()
            .push(duplicate);
        assert!(parse_account_funds(&duplicate_immediate).is_err());

        let mut missing_number = official_account_state_fixture();
        missing_number["cuentas"][0]
            .as_object_mut()
            .unwrap()
            .remove("numero");
        assert!(parse_account_funds(&missing_number).is_err());

        let mut nonfinite = official_account_state_fixture();
        nonfinite["cuentas"][0]["saldos"][0]["disponibleOperar"] = serde_json::json!("NaN");
        assert!(parse_account_funds(&nonfinite).is_err());
    }

    #[test]
    fn websocket_control_messages_never_become_movements() {
        assert!(parse_realtime_message("not-json").is_none());
        assert!(parse_realtime_message(r#"{"code":200,"type":"success"}"#).is_none());
        assert!(parse_realtime_message(r#"{"CuentaComitente":"123"}"#).is_none());
        assert!(websocket_auth_succeeded(&Message::Text(
            r#"{"Code":"200","Type":"SUCCESS"}"#.into()
        )));
        assert!(!websocket_auth_succeeded(&Message::Binary(vec![].into())));
        assert!(!websocket_auth_succeeded(&Message::Text("not-json".into())));
        assert!(!websocket_auth_succeeded(&Message::Text(
            r#"{"code":200,"type":"failure"}"#.into()
        )));
    }

    #[test]
    fn realtime_movement_accepts_each_independent_identity_field() {
        for payload in [
            r#"{"IdMovimiento":42}"#,
            r#"{"Tipo":"Deposito"}"#,
            r#"{"Simbolo":"GGAL"}"#,
        ] {
            assert!(matches!(
                parse_realtime_message(payload),
                Some(IolRealtimeEvent::Movement(_))
            ));
        }
        assert!(parse_realtime_message(r#"{"Tipo":"","Simbolo":""}"#).is_none());
    }

    #[test]
    fn realtime_event_channel_distinguishes_sent_full_and_closed() {
        let (sender, mut receiver) = mpsc::channel(1);
        let dropped = AtomicU64::new(0);
        assert!(publish_realtime_event(
            &sender,
            &dropped,
            IolRealtimeEvent::Notice("primero".into())
        ));
        assert!(publish_realtime_event(
            &sender,
            &dropped,
            IolRealtimeEvent::Notice("descartado".into())
        ));
        assert_eq!(dropped.load(Ordering::Relaxed), 1);
        assert!(receiver.try_recv().is_ok());
        drop(receiver);
        assert!(!publish_realtime_event(
            &sender,
            &dropped,
            IolRealtimeEvent::Notice("cerrado".into())
        ));
    }

    #[test]
    fn websocket_retry_delay_doubles_and_caps_at_sixty_seconds() {
        assert_eq!(next_websocket_retry_delay(Duration::ZERO), Duration::ZERO);
        assert_eq!(
            next_websocket_retry_delay(Duration::from_secs(1)),
            Duration::from_secs(2)
        );
        assert_eq!(
            next_websocket_retry_delay(Duration::from_secs(30)),
            Duration::from_secs(60)
        );
        assert_eq!(
            next_websocket_retry_delay(Duration::from_secs(31)),
            Duration::from_secs(60)
        );
        assert_eq!(
            next_websocket_retry_delay(Duration::from_secs(u64::MAX)),
            Duration::from_secs(60)
        );
    }

    #[tokio::test]
    async fn movement_stream_returns_when_connection_fails_and_event_channel_is_closed() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        drop(listener);
        let (events, receiver) = mpsc::channel(1);
        drop(receiver);
        let (_commands, command_receiver) = mpsc::channel(1);
        tokio::time::timeout(
            Duration::from_secs(2),
            run_movement_stream(
                format!("ws://{address}"),
                Zeroizing::new("user".into()),
                Zeroizing::new(encrypted_test_password()),
                events,
                command_receiver,
                Arc::new(AtomicU64::new(0)),
            ),
        )
        .await
        .expect("un canal cerrado debe detener el retry de conexión");
    }

    #[tokio::test]
    async fn movement_stream_authenticates_publishes_and_shuts_down_against_local_websocket() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
            let auth = tokio::time::timeout(Duration::from_secs(2), socket.next())
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            let Message::Text(auth) = auth else {
                panic!("se esperaba autenticación textual")
            };
            let auth: serde_json::Value = serde_json::from_str(auth.as_ref()).unwrap();
            assert_eq!(auth["action"], "auth");
            assert_eq!(auth["username"], "user");
            assert!(auth["password"]
                .as_str()
                .is_some_and(|value| !value.is_empty()));
            socket
                .send(Message::Text(r#"{"code":200,"type":"success"}"#.into()))
                .await
                .unwrap();
            socket
                .send(Message::Text(r#"{"IdMovimiento":42}"#.into()))
                .await
                .unwrap();

            while let Some(message) = socket.next().await {
                match message.unwrap() {
                    Message::Text(text) if text.contains("disconnect") => break,
                    Message::Close(_) => break,
                    _ => {}
                }
            }
        });

        let (events, mut event_receiver) = mpsc::channel(8);
        let (commands, command_receiver) = mpsc::channel(1);
        let task = tokio::spawn(run_movement_stream(
            format!("ws://{address}"),
            Zeroizing::new("user".into()),
            Zeroizing::new(encrypted_test_password()),
            events,
            command_receiver,
            Arc::new(AtomicU64::new(0)),
        ));
        let connected = tokio::time::timeout(Duration::from_secs(2), event_receiver.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(
            connected,
            IolRealtimeEvent::Status {
                state: WebsocketConnectionState::Connected,
                ..
            }
        ));
        let movement = tokio::time::timeout(Duration::from_secs(2), event_receiver.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(movement, IolRealtimeEvent::Movement(_)));
        commands.send(MovementCommand::Shutdown).await.unwrap();
        tokio::time::timeout(Duration::from_secs(2), task)
            .await
            .expect("el stream debe respetar Shutdown")
            .unwrap();
        tokio::time::timeout(Duration::from_secs(2), server)
            .await
            .expect("el servidor local debe observar la desconexión")
            .unwrap();
    }

    #[test]
    fn scalar_and_collection_helpers_reject_wrong_json_shapes() {
        assert!(collection(&serde_json::json!(1), &["items"]).is_err());
        assert!(collection(&serde_json::json!({}), &["items"]).is_err());
        let object = serde_json::json!({"number": true, "integer": 1.5});
        let object = object.as_object().unwrap();
        assert_eq!(scalar_text(object, &["number"]), None);
        assert_eq!(optional_number(object, &["missing"]), None);
        assert!(strict_optional_integer(object, &["integer"]).is_err());
        assert_eq!(strict_optional_integer(object, &["missing"]).unwrap(), None);
        assert_eq!(
            strict_optional_integer(
                serde_json::json!({"integer": null}).as_object().unwrap(),
                &["integer"]
            )
            .unwrap(),
            None
        );
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
    async fn unauthorized_order_submission_refreshes_but_never_reposts() {
        let token = r#"{"access_token":"access-1","refresh_token":"refresh-1","expires_in":3600,"token_type":"Bearer"}"#;
        let refreshed = r#"{"access_token":"access-2","refresh_token":"refresh-2","expires_in":3600,"token_type":"Bearer"}"#;
        let Some((base_url, server)) = mock_server(vec![
            ("POST /token", "200 OK", token),
            ("POST /api/v2/operar", "401 Unauthorized", "{}"),
            ("POST /token", "200 OK", refreshed),
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

        let error = client
            .submit_order("/api/v2/operar", &test_order_request())
            .await
            .unwrap_err();

        assert!(matches!(error, IolClientError::AmbiguousOrder(_)));
        server.join().unwrap();
    }

    #[tokio::test]
    async fn fetches_portfolio_and_pending_orders_for_startup_reconciliation() {
        let token = r#"{"access_token":"access-1","refresh_token":"refresh-1","expires_in":3600,"token_type":"Bearer"}"#;
        let account_state: &'static str = Box::leak(
            official_account_state_fixture()
                .to_string()
                .into_boxed_str(),
        );
        let portfolio = r#"{"activos":[{"titulo":{"simbolo":"GFGC100","tipo":"Opciones"},"cantidad":1,"ppc":1200.0}]}"#;
        let operations = r#"{"operaciones":[]}"#;
        let Some((base_url, server)) = mock_server(vec![
            ("POST /token", "200 OK", token),
            ("GET /api/v2/estadocuenta", "200 OK", account_state),
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
        assert_eq!(
            account.funds.unwrap().immediate_available_to_trade,
            45_000.0
        );
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

    #[test]
    fn token_refresh_margin_has_an_inclusive_thirty_second_boundary() {
        let now = Instant::now();
        assert!(token_needs_refresh(now, now));
        assert!(token_needs_refresh(
            now,
            now.checked_add(Duration::from_secs(30)).unwrap()
        ));
        assert!(!token_needs_refresh(
            now,
            now.checked_add(Duration::from_secs(31)).unwrap()
        ));
        assert!(token_needs_refresh(
            now,
            now.checked_sub(Duration::from_secs(1)).unwrap()
        ));
    }

    #[test]
    fn circuit_is_open_only_until_its_exclusive_deadline() {
        let now = Instant::now();
        assert!(circuit_is_open(
            now,
            now.checked_add(Duration::from_nanos(1)).unwrap()
        ));
        assert!(!circuit_is_open(now, now));
        assert!(!circuit_is_open(
            now,
            now.checked_sub(Duration::from_nanos(1)).unwrap()
        ));
    }

    #[test]
    fn market_retry_and_circuit_boundaries_are_explicit() {
        assert!(!retry_attempt_remains(0, 0));
        assert!(!retry_attempt_remains(0, 1));
        assert!(retry_attempt_remains(0, 2));
        assert!(!retry_attempt_remains(1, 2));
        assert!(!circuit_breaker_should_open(2));
        assert!(circuit_breaker_should_open(3));
        assert!(circuit_breaker_should_open(u32::MAX));

        let now = Instant::now();
        assert_eq!(
            circuit_breaker_deadline(now).duration_since(now),
            Duration::from_secs(300)
        );
        for (attempt, expected_ms) in [
            (0, 250),
            (1, 500),
            (2, 1_000),
            (5, 8_000),
            (6, 16_000),
            (7, 16_000),
            (u32::MAX, 16_000),
        ] {
            assert_eq!(
                market_retry_delay(attempt),
                Duration::from_millis(expected_ms)
            );
        }
    }

    #[tokio::test]
    async fn near_expiry_access_token_is_refreshed_before_use() {
        let Some((base_url, server)) = mock_server(vec![(
            "POST /token",
            "200 OK",
            r#"{"access_token":"access-2","refresh_token":"refresh-2","expires_in":3600,"token_type":"Bearer"}"#,
        )]) else {
            return;
        };
        let mut client = IolClient::new(
            base_url,
            "user".into(),
            encrypted_test_password(),
            "refresh-1".into(),
        )
        .unwrap();
        client.access_token = Some(Zeroizing::new("access-1".into()));
        client.access_expires_at = Instant::now() + Duration::from_secs(30);

        client.ensure_access_token().await.unwrap();

        assert_eq!(client.access_token().unwrap(), "access-2");
        assert_eq!(client.refresh_token.as_str(), "refresh-2");
        server.join().unwrap();
    }

    #[tokio::test]
    async fn token_response_rejects_each_incomplete_or_unsupported_field() {
        for body in [
            r#"{"access_token":"","expires_in":3600,"token_type":"Bearer"}"#,
            r#"{"access_token":"   ","expires_in":3600,"token_type":"Bearer"}"#,
            r#"{"access_token":"access","expires_in":0,"token_type":"Bearer"}"#,
            r#"{"access_token":"access","expires_in":3600,"token_type":"Basic"}"#,
        ] {
            let Some((base_url, server)) = mock_server(vec![("POST /token", "200 OK", body)])
            else {
                return;
            };
            let mut client = IolClient::new(
                base_url,
                "user".into(),
                encrypted_test_password(),
                String::new(),
            )
            .unwrap();
            let error = client.authenticate().await.unwrap_err();
            assert!(matches!(error, IolClientError::InvalidResponse(_)));
            assert!(client.access_token.is_none());
            server.join().unwrap();
        }
    }

    #[test]
    fn iol_json_contract_enforces_media_type_and_inclusive_size_limit() {
        for accepted in [
            "application/json",
            "APPLICATION/JSON; charset=utf-8",
            "application/problem+json",
            "vendor/problem+json",
        ] {
            assert!(is_json_content_type(&accepted.to_ascii_lowercase()));
        }
        for rejected in [
            "",
            "text/plain",
            "text/application/json",
            "application/json-patch",
            "application/not-json",
        ] {
            assert!(!is_json_content_type(rejected));
        }
        assert!(!json_body_exceeds_limit(8_388_608));
        assert!(json_body_exceeds_limit(8_388_609));
        assert!(json_body_exceeds_limit(u64::MAX));
    }

    #[test]
    fn normalized_iol_paths_have_exactly_one_leading_separator() {
        assert_eq!(normalize_path("api/v2/test"), "/api/v2/test");
        assert_eq!(normalize_path("/api/v2/test"), "/api/v2/test");
        assert_eq!(normalize_path(""), "/");
    }

    #[test]
    fn optional_scalar_deserializer_preserves_strings_and_numbers_only() {
        #[derive(Deserialize)]
        struct Scalar {
            #[serde(default, deserialize_with = "deserialize_optional_scalar")]
            value: Option<String>,
        }

        assert_eq!(
            serde_json::from_str::<Scalar>(r#"{"value":"42"}"#)
                .unwrap()
                .value
                .as_deref(),
            Some("42")
        );
        assert_eq!(
            serde_json::from_str::<Scalar>(r#"{"value":42}"#)
                .unwrap()
                .value
                .as_deref(),
            Some("42")
        );
        assert_eq!(
            serde_json::from_str::<Scalar>(r#"{"value":true}"#)
                .unwrap()
                .value,
            None
        );
        assert_eq!(
            serde_json::from_str::<Scalar>(r#"{"value":null}"#)
                .unwrap()
                .value,
            None
        );
    }

    #[tokio::test]
    async fn market_frame_retry_recovers_only_from_transport_or_http_errors() {
        let token = r#"{"access_token":"access-1","refresh_token":"refresh-1","expires_in":3600,"token_type":"Bearer"}"#;
        let underlying = r#"{"ultimoPrecio":100.5}"#;
        let catalog = r#"[{"simboloSubyacente":"GGAL","simbolo":"GFGC100","tipoOpcion":"CALL","precioEjercicio":100,"diasVencimiento":1}]"#;
        let quotes = r#"{"titulos":[{"simbolo":"GFGC100","ultimoPrecio":2.1,"volumen":20,"bid":2.0,"ask":2.2}]}"#;
        let Some((base_url, server)) = mock_server(vec![
            ("POST /token", "200 OK", token),
            (
                "GET /api/v2/BCBA/Titulos/GGAL/Cotizacion",
                "503 Service Unavailable",
                "{}",
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

        let frame = client.market_frame_with_retry("GGAL", 2).await.unwrap();

        assert_eq!(frame.underlying.last, 100.5);
        assert_eq!(client.failures, 0);
        server.join().unwrap();
    }

    #[tokio::test]
    async fn market_frame_retry_does_not_hide_a_non_retryable_contract_error() {
        let token = r#"{"access_token":"access-1","refresh_token":"refresh-1","expires_in":3600,"token_type":"Bearer"}"#;
        let catalog = r#"[{"simboloSubyacente":"GGAL","simbolo":"GFGC100","tipoOpcion":"CALL","precioEjercicio":100,"diasVencimiento":1}]"#;
        let Some((base_url, server)) = mock_server(vec![
            ("POST /token", "200 OK", token),
            ("GET /api/v2/BCBA/Titulos/GGAL/Cotizacion", "200 OK", "[]"),
            ("GET /api/v2/BCBA/Titulos/GGAL/Opciones", "200 OK", catalog),
            (
                "GET /api/v2/Cotizaciones/Opciones/Todas/Argentina",
                "200 OK",
                r#"{"titulos":[]}"#,
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

        let error = client.market_frame_with_retry("GGAL", 2).await.unwrap_err();

        assert!(matches!(error, IolClientError::InvalidResponse(_)));
        assert_eq!(client.failures, 1);
        server.join().unwrap();
    }

    #[tokio::test]
    async fn third_consecutive_market_failure_opens_circuit_on_the_last_attempt() {
        let token = r#"{"access_token":"access-1","refresh_token":"refresh-1","expires_in":3600,"token_type":"Bearer"}"#;
        let Some((base_url, server)) = mock_server(vec![
            ("POST /token", "200 OK", token),
            (
                "GET /api/v2/BCBA/Titulos/GGAL/Cotizacion",
                "503 Service Unavailable",
                "{}",
            ),
            (
                "GET /api/v2/BCBA/Titulos/GGAL/Cotizacion",
                "503 Service Unavailable",
                "{}",
            ),
            (
                "GET /api/v2/BCBA/Titulos/GGAL/Cotizacion",
                "503 Service Unavailable",
                "{}",
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

        let error = client.market_frame_with_retry("GGAL", 3).await.unwrap_err();

        assert!(matches!(error, IolClientError::CircuitOpen(_)));
        assert_eq!(client.failures, 3);
        assert!(client.circuit_open_until.is_some());
        server.join().unwrap();
    }

    fn mock_server(
        responses: Vec<(&'static str, &'static str, &'static str)>,
    ) -> Option<(String, thread::JoinHandle<()>)> {
        const ACCEPT_TIMEOUT: Duration = Duration::from_secs(15);
        let listener = TcpListener::bind("127.0.0.1:0").ok()?;
        listener.set_nonblocking(true).ok()?;
        let address = listener.local_addr().unwrap();
        let handle = thread::spawn(move || {
            for (expected_request, status, body) in responses {
                let deadline = Instant::now() + ACCEPT_TIMEOUT;
                let mut stream = loop {
                    match listener.accept() {
                        Ok((stream, _)) => break stream,
                        Err(error)
                            if error.kind() == std::io::ErrorKind::WouldBlock
                                && Instant::now() < deadline =>
                        {
                            thread::sleep(Duration::from_millis(5));
                        }
                        Err(error) => panic!(
                            "no llegó {expected_request} antes del límite del servidor local: {error}"
                        ),
                    }
                };
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
