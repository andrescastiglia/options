use std::{
    fs::File,
    io::{BufRead, BufReader, Write},
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

use crate::{
    broker::{OrderExecution, OrderRequest, OrderSide},
    errors::AppError,
    iol_client::CostCalibration,
    learning::{LearningState, LiveStage, ValidationTrade},
    market::MarketFrame,
    pattern::{Direction, Trend, TrendDetector},
    portfolio::Portfolio,
    risk::RiskManager,
    secure_fs::{
        open_private_append, open_private_read, read_private_limited, reject_symlink, write_atomic,
    },
    trading::{ExitReason, Pnl, Position, TradingEngine},
};

pub const SNAPSHOT_VERSION: u32 = 4;
pub const JOURNAL_SCHEMA_VERSION: u32 = 6;
const HASH_CHAIN_SCHEMA_VERSION: u32 = 5;
const AUTHENTICATED_JOURNAL_SCHEMA_VERSION: u32 = 6;
const MAX_SNAPSHOT_BYTES: u64 = 64 * 1024 * 1024;
const MAX_JOURNAL_BYTES: u64 = 128 * 1024 * 1024;
const MAX_JOURNAL_EVENTS: usize = 1_000_000;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum JournalEventKind {
    Started {
        mode: String,
        ticker: String,
    },
    /// Intención creada localmente. En una ruta real este evento debe estar
    /// sincronizado a disco antes de producir el efecto externo.
    OrderIntentCreated {
        request: OrderRequest,
    },
    /// El broker respondió al envío inicial. Conserva el ID tan pronto como se
    /// conoce, antes de iniciar el seguimiento del estado terminal.
    OrderAccepted {
        execution: OrderExecution,
    },
    /// Evento legado (schemas v1-v3). Las nuevas escrituras usan
    /// `OrderIntentCreated`.
    OrderSubmitted {
        symbol: String,
        side: OrderSide,
        quantity: u32,
        limit_price: f64,
    },
    OrderUpdated {
        execution: OrderExecution,
    },
    PartialFillExposure {
        execution: OrderExecution,
        requested_quantity: u32,
        remaining_quantity: u32,
    },
    OrderUnknown {
        request: crate::broker::OrderRequest,
        reason: String,
    },
    PositionOpened {
        position: Position,
    },
    PositionClosed {
        operation_id: String,
        exit_price: f64,
        net_pnl: f64,
        reason: ExitReason,
        #[serde(default)]
        stage: LiveStage,
        #[serde(default)]
        validation_trade: Option<ValidationTrade>,
    },
    RiskRejected {
        reason: String,
    },
    KillSwitch {
        active: bool,
    },
    LiveStageChanged {
        from: LiveStage,
        to: LiveStage,
        reason: String,
        epoch: u64,
    },
    Recovery {
        message: String,
    },
    Shutdown {
        clean: bool,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JournalEvent {
    #[serde(default = "legacy_journal_schema_version")]
    pub schema_version: u32,
    pub sequence: u64,
    pub timestamp_secs: i64,
    pub operation_id: Option<String>,
    pub event: JournalEventKind,
    #[serde(default)]
    pub previous_hash: String,
    #[serde(default)]
    pub event_hash: String,
    #[serde(default)]
    pub event_hmac: String,
}

fn legacy_journal_schema_version() -> u32 {
    1
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Snapshot {
    pub version: u32,
    pub timestamp_secs: i64,
    pub last_sequence: u64,
    pub engine: TradingEngine,
    pub portfolio: Portfolio,
    pub risk: RiskManager,
    pub runtime: RuntimeSnapshot,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RuntimeSnapshot {
    pub detector: TrendDetector,
    pub current_frame: Option<MarketFrame>,
    pub current_trend: Option<Trend>,
    pub current_pnl: Option<Pnl>,
    pub last_market_timestamp: Option<i64>,
    pub operation_counter: u64,
    pub last_traded_signal: Option<Direction>,
    pub ticks: u64,
    pub selected_option: Option<String>,
    #[serde(default)]
    pub live_stage: LiveStage,
    #[serde(default)]
    pub learning_state: LearningState,
    #[serde(default)]
    pub trading_performance: Vec<ValidationTrade>,
    #[serde(default)]
    pub return_to_learning_pending: bool,
    #[serde(default)]
    pub cooldown_until_secs: i64,
    #[serde(default)]
    pub cost_calibration: Option<CostCalibration>,
}

impl Snapshot {
    pub fn new(
        timestamp_secs: i64,
        last_sequence: u64,
        engine: TradingEngine,
        portfolio: Portfolio,
        risk: RiskManager,
        runtime: RuntimeSnapshot,
    ) -> Self {
        Self {
            version: SNAPSHOT_VERSION,
            timestamp_secs,
            last_sequence,
            engine,
            portfolio,
            risk,
            runtime,
        }
    }
}

pub fn save_snapshot(path: impl AsRef<Path>, snapshot: &Snapshot) -> Result<(), AppError> {
    let path = path.as_ref();
    let encoded = serde_json::to_vec_pretty(snapshot)?;
    if !snapshot_size_is_allowed(encoded.len() as u64) {
        return Err(AppError::Recovery(format!(
            "snapshot excede el máximo de {MAX_SNAPSHOT_BYTES} bytes"
        )));
    }
    write_atomic(path, &encoded)?;
    Ok(())
}

fn snapshot_size_is_allowed(encoded_bytes: u64) -> bool {
    encoded_bytes <= MAX_SNAPSHOT_BYTES
}

pub fn load_snapshot(path: impl AsRef<Path>) -> Result<Snapshot, AppError> {
    let path = path.as_ref();
    let bytes = read_private_limited(path, MAX_SNAPSHOT_BYTES)?;
    let mut snapshot: Snapshot = serde_json::from_slice(&bytes)?;
    if !matches!(snapshot.version, 1 | 2 | 3 | SNAPSHOT_VERSION) {
        return Err(AppError::Recovery(format!(
            "version de snapshot {} no soportada",
            snapshot.version
        )));
    }
    snapshot.version = SNAPSHOT_VERSION;
    Ok(snapshot)
}

#[derive(Debug)]
pub struct Journal {
    file: File,
    path: PathBuf,
    last_sequence: u64,
    last_hash: String,
    authentication_key: Option<Zeroizing<[u8; 32]>>,
}

impl Journal {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, AppError> {
        Self::open_with_key(path.as_ref(), None)
    }

    pub fn open_authenticated(path: impl AsRef<Path>) -> Result<Self, AppError> {
        let key = crate::secrets::journal_authentication_key()
            .map_err(|error| AppError::Recovery(error.to_string()))?;
        Self::open_with_key(path.as_ref(), Some(key))
    }

    pub fn open_authenticated_with_master_key(
        path: impl AsRef<Path>,
        master_key_path: &Path,
    ) -> Result<Self, AppError> {
        let key = crate::secrets::journal_authentication_key_from(master_key_path)
            .map_err(|error| AppError::Recovery(error.to_string()))?;
        Self::open_with_key(path.as_ref(), Some(key))
    }

    fn open_with_key(
        path: &Path,
        authentication_key: Option<Zeroizing<[u8; 32]>>,
    ) -> Result<Self, AppError> {
        reject_symlink(path)?;
        let (last_sequence, last_hash) = if path.exists() {
            let events = read_events_with_key(path, authentication_key.as_deref())?;
            events.last().map_or((0, String::new()), |event| {
                (event.sequence, chain_hash_after(event))
            })
        } else {
            (0, String::new())
        };
        let file = open_private_append(path)?;
        Ok(Self {
            file,
            path: path.to_path_buf(),
            last_sequence,
            last_hash,
            authentication_key,
        })
    }

    pub fn append(
        &mut self,
        timestamp_secs: i64,
        operation_id: Option<String>,
        event: JournalEventKind,
    ) -> Result<JournalEvent, AppError> {
        let next_sequence = self.last_sequence.checked_add(1).ok_or_else(|| {
            AppError::Recovery("overflow de secuencia al escribir journal".into())
        })?;
        let mut entry = JournalEvent {
            schema_version: if self.authentication_key.is_some() {
                AUTHENTICATED_JOURNAL_SCHEMA_VERSION
            } else {
                HASH_CHAIN_SCHEMA_VERSION
            },
            sequence: next_sequence,
            timestamp_secs,
            operation_id,
            event,
            previous_hash: self.last_hash.clone(),
            event_hash: String::new(),
            event_hmac: String::new(),
        };
        validate_event_identity(&entry)?;
        entry.event_hash = calculate_event_hash(&entry)?;
        if let Some(key) = self.authentication_key.as_deref() {
            entry.event_hmac = calculate_event_hmac(&entry, key)?;
        }
        let encoded = serde_json::to_vec(&entry)?;
        if !journal_append_size_is_allowed(self.file.metadata()?.len(), encoded.len() as u64) {
            return Err(AppError::Recovery(format!(
                "journal alcanzaría el máximo de {MAX_JOURNAL_BYTES} bytes"
            )));
        }
        self.file.write_all(&encoded)?;
        self.file.write_all(b"\n")?;
        self.file.flush()?;
        self.last_sequence = next_sequence;
        self.last_hash = entry.event_hash.clone();
        Ok(entry)
    }

    pub fn sync(&mut self) -> Result<(), AppError> {
        self.file.sync_all()?;
        Ok(())
    }

    pub fn last_sequence(&self) -> u64 {
        self.last_sequence
    }

    pub fn events_after(&self, sequence: u64) -> Result<Vec<JournalEvent>, AppError> {
        Ok(
            read_events_with_key(&self.path, self.authentication_key.as_deref())?
                .into_iter()
                .filter(|event| event.sequence > sequence)
                .collect(),
        )
    }
}

fn journal_append_size_is_allowed(current_bytes: u64, encoded_bytes: u64) -> bool {
    current_bytes
        .saturating_add(encoded_bytes)
        .saturating_add(1)
        <= MAX_JOURNAL_BYTES
}

pub fn record_order_intent(
    journal: &mut Journal,
    timestamp_secs: i64,
    request: &OrderRequest,
    durable: bool,
) -> Result<JournalEvent, AppError> {
    let event = journal.append(
        timestamp_secs,
        Some(request.operation_id.clone()),
        JournalEventKind::OrderIntentCreated {
            request: request.clone(),
        },
    )?;
    if durable {
        journal.sync()?;
    }
    Ok(event)
}

pub fn record_order_accepted(
    journal: &mut Journal,
    timestamp_secs: i64,
    operation_id: &str,
    execution: &OrderExecution,
) -> Result<JournalEvent, AppError> {
    let event = journal.append(
        timestamp_secs,
        Some(operation_id.to_string()),
        JournalEventKind::OrderAccepted {
            execution: execution.clone(),
        },
    )?;
    journal.sync()?;
    Ok(event)
}

pub fn record_order_terminal(
    journal: &mut Journal,
    timestamp_secs: i64,
    operation_id: &str,
    execution: &OrderExecution,
    durable: bool,
) -> Result<JournalEvent, AppError> {
    let event = journal.append(
        timestamp_secs,
        Some(operation_id.to_string()),
        JournalEventKind::OrderUpdated {
            execution: execution.clone(),
        },
    )?;
    if durable {
        journal.sync()?;
    }
    Ok(event)
}

pub fn read_events(path: impl AsRef<Path>) -> Result<Vec<JournalEvent>, AppError> {
    read_events_with_key(path.as_ref(), None)
}

fn read_events_with_key(
    path: &Path,
    authentication_key: Option<&[u8; 32]>,
) -> Result<Vec<JournalEvent>, AppError> {
    match std::fs::metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
        Ok(_) => {}
    }
    let file = open_private_read(path, MAX_JOURNAL_BYTES)?;
    let mut events = Vec::new();
    let mut expected_sequence = 1_u64;
    let mut expected_previous_hash = String::new();
    for (index, line) in BufReader::new(file).lines().enumerate() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        if events.len() >= MAX_JOURNAL_EVENTS {
            return Err(AppError::Recovery(format!(
                "journal excede el máximo de {MAX_JOURNAL_EVENTS} eventos"
            )));
        }
        let event = serde_json::from_str::<JournalEvent>(&line).map_err(|error| {
            AppError::Recovery(format!("journal corrupto en linea {}: {error}", index + 1))
        })?;
        if !(1..=JOURNAL_SCHEMA_VERSION).contains(&event.schema_version) {
            return Err(AppError::Recovery(format!(
                "schema de journal {} no soportado en linea {}",
                event.schema_version,
                index + 1
            )));
        }
        if event.sequence != expected_sequence {
            return Err(AppError::Recovery(format!(
                "secuencia de journal inválida en línea {}: esperada {}, recibida {}",
                index + 1,
                expected_sequence,
                event.sequence
            )));
        }
        validate_event_identity(&event).map_err(|error| {
            AppError::Recovery(format!(
                "identidad de journal inválida en línea {}: {error}",
                index + 1
            ))
        })?;
        if event.schema_version >= HASH_CHAIN_SCHEMA_VERSION {
            if event.previous_hash != expected_previous_hash {
                return Err(AppError::Recovery(format!(
                    "enlace de integridad inválido en linea {}",
                    index + 1
                )));
            }
            let expected_hash = if event.schema_version == HASH_CHAIN_SCHEMA_VERSION {
                calculate_event_hash_from_line(&line, &event.event_hash)?
            } else {
                calculate_event_hash(&event)?
            };
            if event.event_hash != expected_hash {
                return Err(AppError::Recovery(format!(
                    "hash de integridad inválido en linea {}",
                    index + 1
                )));
            }
            if event.schema_version >= AUTHENTICATED_JOURNAL_SCHEMA_VERSION {
                let key = authentication_key.ok_or_else(|| {
                    AppError::Recovery(format!(
                        "journal autenticado sin clave en linea {}",
                        index + 1
                    ))
                })?;
                if !verify_event_hmac(&event, key)? {
                    return Err(AppError::Recovery(format!(
                        "HMAC de journal inválido en linea {}",
                        index + 1
                    )));
                }
            }
        }
        expected_previous_hash = chain_hash_after(&event);
        expected_sequence = expected_sequence
            .checked_add(1)
            .ok_or_else(|| AppError::Recovery("overflow de secuencia en journal".into()))?;
        events.push(event);
    }
    Ok(events)
}

/// Valida la correlación tipada de un evento sin depender del archivo que lo
/// contiene. También se expone para harnesses adversariales y herramientas de
/// inspección; la lectura y escritura del journal la aplican obligatoriamente.
pub fn validate_event_identity(event: &JournalEvent) -> Result<(), AppError> {
    let payload_operation_id = match &event.event {
        JournalEventKind::OrderIntentCreated { request }
        | JournalEventKind::OrderUnknown { request, .. } => Some(request.operation_id.as_str()),
        JournalEventKind::OrderAccepted { execution }
        | JournalEventKind::OrderUpdated { execution }
        | JournalEventKind::PartialFillExposure { execution, .. } => {
            Some(execution.operation_id.as_str())
        }
        JournalEventKind::PositionOpened { position } => Some(position.operation_id.as_str()),
        JournalEventKind::PositionClosed { operation_id, .. } => Some(operation_id.as_str()),
        JournalEventKind::OrderSubmitted { .. } => {
            let operation_id = event
                .operation_id
                .as_deref()
                .filter(|value| !value.is_empty());
            if operation_id.is_none() {
                return Err(AppError::Recovery(
                    "evento legado order_submitted sin operation_id".into(),
                ));
            }
            None
        }
        _ => None,
    };
    if let Some(payload_operation_id) = payload_operation_id {
        if payload_operation_id.is_empty()
            || event.operation_id.as_deref() != Some(payload_operation_id)
        {
            return Err(AppError::Recovery(
                "operation_id exterior no coincide con el payload tipado".into(),
            ));
        }
    }
    Ok(())
}

fn calculate_event_hash(event: &JournalEvent) -> Result<String, AppError> {
    let mut unsigned = event.clone();
    unsigned.event_hash.clear();
    unsigned.event_hmac.clear();
    let encoded = serde_json::to_vec(&unsigned)?;
    Ok(ring::digest::digest(&ring::digest::SHA256, &encoded)
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

fn calculate_event_hmac(event: &JournalEvent, key: &[u8; 32]) -> Result<String, AppError> {
    let mut unsigned = event.clone();
    unsigned.event_hmac.clear();
    let encoded = serde_json::to_vec(&unsigned)?;
    Ok(base64::Engine::encode(
        &base64::engine::general_purpose::STANDARD,
        ring::hmac::sign(
            &ring::hmac::Key::new(ring::hmac::HMAC_SHA256, key),
            &encoded,
        )
        .as_ref(),
    ))
}

fn verify_event_hmac(event: &JournalEvent, key: &[u8; 32]) -> Result<bool, AppError> {
    let mut unsigned = event.clone();
    unsigned.event_hmac.clear();
    let encoded = serde_json::to_vec(&unsigned)?;
    let Ok(signature) = base64::Engine::decode(
        &base64::engine::general_purpose::STANDARD,
        event.event_hmac.as_bytes(),
    ) else {
        return Ok(false);
    };
    Ok(ring::hmac::verify(
        &ring::hmac::Key::new(ring::hmac::HMAC_SHA256, key),
        &encoded,
        &signature,
    )
    .is_ok())
}

fn calculate_event_hash_from_line(line: &str, stored_hash: &str) -> Result<String, AppError> {
    let needle = format!("\"event_hash\":\"{stored_hash}\"");
    let position = line.rfind(&needle).ok_or_else(|| {
        AppError::Recovery("evento v5 sin campo event_hash canónico al final".into())
    })?;
    let mut unsigned = String::with_capacity(line.len().saturating_sub(stored_hash.len()));
    unsigned.push_str(&line[..position]);
    unsigned.push_str("\"event_hash\":\"\"");
    unsigned.push_str(&line[position + needle.len()..]);
    Ok(
        ring::digest::digest(&ring::digest::SHA256, unsigned.as_bytes())
            .as_ref()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect(),
    )
}

fn chain_hash_after(event: &JournalEvent) -> String {
    if event.schema_version >= HASH_CHAIN_SCHEMA_VERSION && !event.event_hash.is_empty() {
        event.event_hash.clone()
    } else {
        format!("legacy-sequence-{}", event.sequence)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::risk::RiskLimits;

    fn risk() -> RiskManager {
        RiskManager::new(RiskLimits {
            max_notional: 1_000.0,
            max_loss_per_trade: 100.0,
            max_daily_loss: 200.0,
            max_trades_per_day: 5,
        })
    }

    #[test]
    fn snapshot_round_trips_atomically() {
        let path =
            std::env::temp_dir().join(format!("options-snapshot-{}.json", std::process::id()));
        let snapshot = Snapshot::new(
            7,
            3,
            TradingEngine::new(),
            Portfolio::default(),
            risk(),
            RuntimeSnapshot {
                detector: TrendDetector::new(10, 3),
                current_frame: None,
                current_trend: None,
                current_pnl: None,
                last_market_timestamp: None,
                operation_counter: 0,
                last_traded_signal: None,
                ticks: 0,
                selected_option: None,
                live_stage: LiveStage::Learning,
                learning_state: LearningState::default(),
                trading_performance: Vec::new(),
                return_to_learning_pending: false,
                cooldown_until_secs: 0,
                cost_calibration: None,
            },
        );
        save_snapshot(&path, &snapshot).unwrap();
        assert_eq!(load_snapshot(&path).unwrap(), snapshot);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn journal_is_typed_and_sequence_is_resumed() {
        let path = std::env::temp_dir().join(format!(
            "options-journal-{}-{}.jsonl",
            std::process::id(),
            now_for_test()
        ));
        {
            let mut journal = Journal::open(&path).unwrap();
            journal
                .append(1, None, JournalEventKind::Shutdown { clean: true })
                .unwrap();
        }
        let mut journal = Journal::open(&path).unwrap();
        let event = journal
            .append(2, None, JournalEventKind::Shutdown { clean: true })
            .unwrap();
        assert_eq!(event.sequence, 2);
        assert_eq!(journal.last_sequence(), 2);
        assert_eq!(read_events(&path).unwrap().len(), 2);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn snapshot_v3_migrates_to_current_schema() {
        let path = std::env::temp_dir().join(format!(
            "options-snapshot-v3-{}-{}.json",
            std::process::id(),
            now_for_test()
        ));
        let snapshot = Snapshot::new(
            7,
            0,
            TradingEngine::new(),
            Portfolio::default(),
            risk(),
            RuntimeSnapshot {
                detector: TrendDetector::new(10, 3),
                current_frame: None,
                current_trend: None,
                current_pnl: None,
                last_market_timestamp: None,
                operation_counter: 0,
                last_traded_signal: None,
                ticks: 0,
                selected_option: None,
                live_stage: LiveStage::Learning,
                learning_state: LearningState::default(),
                trading_performance: Vec::new(),
                return_to_learning_pending: false,
                cooldown_until_secs: 0,
                cost_calibration: None,
            },
        );
        let mut legacy = serde_json::to_value(snapshot).unwrap();
        legacy["version"] = serde_json::json!(3);
        write_atomic(&path, &serde_json::to_vec(&legacy).unwrap()).unwrap();
        assert_eq!(load_snapshot(&path).unwrap().version, SNAPSHOT_VERSION);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn journal_v2_remains_readable() {
        let path = std::env::temp_dir().join(format!(
            "options-journal-v2-{}-{}.jsonl",
            std::process::id(),
            now_for_test()
        ));
        let event = JournalEvent {
            schema_version: 2,
            sequence: 1,
            timestamp_secs: 1,
            operation_id: None,
            previous_hash: String::new(),
            event_hash: String::new(),
            event_hmac: String::new(),
            event: JournalEventKind::Shutdown { clean: true },
        };
        let mut encoded = serde_json::to_vec(&event).unwrap();
        encoded.push(b'\n');
        write_atomic(&path, &encoded).unwrap();
        assert_eq!(read_events(&path).unwrap(), vec![event]);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn journal_rejects_missing_or_repeated_sequences() {
        for invalid_sequence in [1_u64, 3_u64] {
            let path = std::env::temp_dir().join(format!(
                "options-journal-gap-{}-{}-{invalid_sequence}.jsonl",
                std::process::id(),
                now_for_test()
            ));
            let first = JournalEvent {
                schema_version: JOURNAL_SCHEMA_VERSION,
                sequence: 1,
                timestamp_secs: 1,
                operation_id: None,
                previous_hash: String::new(),
                event_hash: String::new(),
                event_hmac: String::new(),
                event: JournalEventKind::Shutdown { clean: true },
            };
            let second = JournalEvent {
                schema_version: JOURNAL_SCHEMA_VERSION,
                sequence: invalid_sequence,
                timestamp_secs: 2,
                operation_id: None,
                previous_hash: String::new(),
                event_hash: String::new(),
                event_hmac: String::new(),
                event: JournalEventKind::Shutdown { clean: true },
            };
            let bytes = format!(
                "{}\n{}\n",
                serde_json::to_string(&first).unwrap(),
                serde_json::to_string(&second).unwrap()
            );
            write_atomic(&path, bytes.as_bytes()).unwrap();

            assert!(read_events(&path).is_err());
            let _ = std::fs::remove_file(path);
        }
    }

    #[test]
    fn journal_hash_chain_rejects_a_modified_event() {
        let path = std::env::temp_dir().join(format!(
            "options-journal-tamper-{}-{}.jsonl",
            std::process::id(),
            now_for_test()
        ));
        let mut journal = Journal::open(&path).unwrap();
        journal
            .append(
                1,
                None,
                JournalEventKind::Recovery {
                    message: "original".into(),
                },
            )
            .unwrap();
        journal.sync().unwrap();
        drop(journal);
        let modified = std::fs::read_to_string(&path)
            .unwrap()
            .replace("original", "alterado");
        std::fs::write(&path, modified).unwrap();
        assert!(read_events(&path).is_err());
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn authenticated_journal_rejects_rehashed_tampering_and_wrong_keys() {
        let path = std::env::temp_dir().join(format!(
            "options-journal-hmac-{}-{}.jsonl",
            std::process::id(),
            now_for_test()
        ));
        let key = [31_u8; 32];
        let mut journal = Journal::open_with_key(&path, Some(Zeroizing::new(key))).unwrap();
        let event = journal
            .append(
                1,
                None,
                JournalEventKind::Recovery {
                    message: "original".into(),
                },
            )
            .unwrap();
        assert_eq!(event.schema_version, AUTHENTICATED_JOURNAL_SCHEMA_VERSION);
        assert!(!event.event_hmac.is_empty());
        journal.sync().unwrap();
        drop(journal);

        assert!(Journal::open_with_key(&path, Some(Zeroizing::new(key))).is_ok());
        assert!(Journal::open_with_key(&path, Some(Zeroizing::new([32_u8; 32]))).is_err());
        assert!(Journal::open(&path).is_err());

        let mut modified: JournalEvent =
            serde_json::from_str(std::fs::read_to_string(&path).unwrap().trim()).unwrap();
        modified.event = JournalEventKind::Recovery {
            message: "alterado".into(),
        };
        modified.event_hash = calculate_event_hash(&modified).unwrap();
        let mut encoded = serde_json::to_vec(&modified).unwrap();
        encoded.push(b'\n');
        write_atomic(&path, &encoded).unwrap();
        assert!(Journal::open_with_key(&path, Some(Zeroizing::new(key))).is_err());
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn truncated_last_journal_record_is_never_silently_ignored() {
        let path = std::env::temp_dir().join(format!(
            "options-journal-truncated-{}-{}.jsonl",
            std::process::id(),
            now_for_test()
        ));
        let mut journal = Journal::open(&path).unwrap();
        journal
            .append(1, None, JournalEventKind::Shutdown { clean: false })
            .unwrap();
        journal.sync().unwrap();
        drop(journal);
        let file = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
        let length = file.metadata().unwrap().len();
        file.set_len(length - 5).unwrap();
        drop(file);

        assert!(read_events(&path).is_err());
        assert!(Journal::open(&path).is_err());
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn oversized_snapshot_is_rejected_before_deserialization() {
        let path = std::env::temp_dir().join(format!(
            "options-snapshot-oversized-{}-{}.json",
            std::process::id(),
            now_for_test()
        ));
        let file = open_private_append(&path).unwrap();
        file.set_len(MAX_SNAPSHOT_BYTES + 1).unwrap();
        drop(file);

        let error = load_snapshot(&path).unwrap_err();
        assert!(error.to_string().contains("mayor que"));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn persistence_rejects_each_malformed_schema_boundary() {
        let base = std::env::temp_dir().join(format!(
            "options-persistence-boundaries-{}-{}",
            std::process::id(),
            now_for_test()
        ));
        crate::secure_fs::ensure_private_dir(&base).unwrap();

        assert!(read_events(base.join("absent.jsonl")).unwrap().is_empty());

        let legacy_path = base.join("legacy.jsonl");
        let legacy = r#"{"sequence":1,"timestamp_secs":1,"operation_id":null,"event":{"type":"shutdown","clean":true}}"#;
        write_atomic(&legacy_path, format!("\n{legacy}\n").as_bytes()).unwrap();
        let events = read_events(&legacy_path).unwrap();
        assert_eq!(events[0].schema_version, 1);

        let invalid_schema_path = base.join("invalid-schema.jsonl");
        let mut invalid_schema = events[0].clone();
        invalid_schema.schema_version = JOURNAL_SCHEMA_VERSION + 1;
        write_atomic(
            &invalid_schema_path,
            format!("{}\n", serde_json::to_string(&invalid_schema).unwrap()).as_bytes(),
        )
        .unwrap();
        assert!(read_events(&invalid_schema_path).is_err());

        let invalid_hmac_path = base.join("invalid-hmac.jsonl");
        let key = [7_u8; 32];
        let mut invalid_hmac = JournalEvent {
            schema_version: AUTHENTICATED_JOURNAL_SCHEMA_VERSION,
            sequence: 1,
            timestamp_secs: 1,
            operation_id: None,
            event: JournalEventKind::Shutdown { clean: true },
            previous_hash: String::new(),
            event_hash: String::new(),
            event_hmac: "%%%".into(),
        };
        invalid_hmac.event_hash = calculate_event_hash(&invalid_hmac).unwrap();
        write_atomic(
            &invalid_hmac_path,
            format!("{}\n", serde_json::to_string(&invalid_hmac).unwrap()).as_bytes(),
        )
        .unwrap();
        assert!(read_events_with_key(&invalid_hmac_path, Some(&key)).is_err());

        let unsupported_snapshot_path = base.join("unsupported-snapshot.json");
        let snapshot_path = base.join("snapshot.json");
        let snapshot = Snapshot::new(
            1,
            0,
            TradingEngine::new(),
            Portfolio::default(),
            risk(),
            RuntimeSnapshot {
                detector: TrendDetector::new(10, 3),
                current_frame: None,
                current_trend: None,
                current_pnl: None,
                last_market_timestamp: None,
                operation_counter: 0,
                last_traded_signal: None,
                ticks: 0,
                selected_option: None,
                live_stage: LiveStage::Learning,
                learning_state: LearningState::default(),
                trading_performance: Vec::new(),
                return_to_learning_pending: false,
                cooldown_until_secs: 0,
                cost_calibration: None,
            },
        );
        let mut unsupported = serde_json::to_value(snapshot).unwrap();
        unsupported["version"] = serde_json::json!(999);
        write_atomic(
            &unsupported_snapshot_path,
            &serde_json::to_vec(&unsupported).unwrap(),
        )
        .unwrap();
        assert!(load_snapshot(&unsupported_snapshot_path).is_err());
        assert!(!snapshot_path.exists());

        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn journal_rejects_broken_links_noncanonical_hashes_and_path_errors() {
        let base = std::env::temp_dir().join(format!(
            "options-persistence-integrity-{}-{}",
            std::process::id(),
            now_for_test()
        ));
        crate::secure_fs::ensure_private_dir(&base).unwrap();
        let path = base.join("journal.jsonl");
        let mut journal = Journal::open(&path).unwrap();
        journal
            .append(1, None, JournalEventKind::Shutdown { clean: true })
            .unwrap();
        journal
            .append(2, None, JournalEventKind::Shutdown { clean: true })
            .unwrap();
        journal.sync().unwrap();
        drop(journal);

        let mut lines = std::fs::read_to_string(&path)
            .unwrap()
            .lines()
            .map(str::to_owned)
            .collect::<Vec<_>>();
        let mut second: JournalEvent = serde_json::from_str(&lines[1]).unwrap();
        second.previous_hash = "0".repeat(64);
        second.event_hash = calculate_event_hash(&second).unwrap();
        lines[1] = serde_json::to_string(&second).unwrap();
        write_atomic(&path, format!("{}\n", lines.join("\n")).as_bytes()).unwrap();
        assert!(read_events(&path).is_err());

        assert!(calculate_event_hash_from_line(r#"{"event_hash": "abc"}"#, "abc").is_err());
        let mut unchained = second;
        unchained.event_hash.clear();
        assert_eq!(
            chain_hash_after(&unchained),
            format!("legacy-sequence-{}", unchained.sequence)
        );
        assert!(read_events(Path::new("invalid\0journal")).is_err());
        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn durable_order_helpers_record_the_declared_event_types() {
        let path = std::env::temp_dir().join(format!(
            "options-order-helper-{}-{}.jsonl",
            std::process::id(),
            now_for_test()
        ));
        let mut journal = Journal::open(&path).unwrap();
        let request = OrderRequest {
            operation_id: "helper-order".into(),
            symbol: "GGALC100".into(),
            quantity: 1,
            market_price: 10.0,
            limit_price: 10.01,
            side: crate::broker::OrderSide::Buy,
        };
        let execution = OrderExecution {
            operation_id: request.operation_id.clone(),
            broker_order_id: Some("42".into()),
            status: crate::broker::OrderStatus::Cancelled,
            filled_quantity: 0,
            fill_price: None,
            message: None,
        };

        record_order_intent(&mut journal, 1, &request, false).unwrap();
        record_order_intent(&mut journal, 2, &request, true).unwrap();
        record_order_accepted(&mut journal, 3, &request.operation_id, &execution).unwrap();
        record_order_terminal(&mut journal, 4, &request.operation_id, &execution, true).unwrap();
        record_order_terminal(&mut journal, 5, &request.operation_id, &execution, false).unwrap();
        let events = journal.events_after(0).unwrap();
        assert_eq!(
            journal
                .events_after(1)
                .unwrap()
                .iter()
                .map(|event| event.sequence)
                .collect::<Vec<_>>(),
            vec![2, 3, 4, 5]
        );
        assert!(matches!(
            events[0].event,
            JournalEventKind::OrderIntentCreated { .. }
        ));
        assert!(matches!(
            events[1].event,
            JournalEventKind::OrderIntentCreated { .. }
        ));
        assert!(matches!(
            events[2].event,
            JournalEventKind::OrderAccepted { .. }
        ));
        assert!(matches!(
            events[3].event,
            JournalEventKind::OrderUpdated { .. }
        ));
        assert!(matches!(
            events[4].event,
            JournalEventKind::OrderUpdated { .. }
        ));
        drop(journal);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn persistence_byte_limits_have_exact_inclusive_boundaries() {
        assert_eq!(MAX_SNAPSHOT_BYTES, 67_108_864);
        assert_eq!(MAX_JOURNAL_BYTES, 134_217_728);
        assert!(snapshot_size_is_allowed(67_108_864));
        assert!(!snapshot_size_is_allowed(67_108_865));

        assert!(journal_append_size_is_allowed(134_217_717, 10));
        assert!(!journal_append_size_is_allowed(134_217_718, 10));
    }

    #[cfg(unix)]
    #[test]
    fn journal_sync_propagates_a_real_descriptor_failure() {
        use std::os::fd::FromRawFd;

        let path = std::env::temp_dir().join(format!(
            "options-journal-sync-error-{}-{}.jsonl",
            std::process::id(),
            now_for_test()
        ));
        let mut journal = Journal::open(&path).unwrap();
        let mut descriptors = [0_i32; 2];
        assert_eq!(unsafe { libc::pipe(descriptors.as_mut_ptr()) }, 0);
        assert_eq!(unsafe { libc::close(descriptors[0]) }, 0);
        journal.file = unsafe { File::from_raw_fd(descriptors[1]) };

        assert!(journal.sync().is_err());
        drop(journal);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn journal_rejects_mismatched_typed_operation_identity_before_write_and_on_legacy_read() {
        let base = std::env::temp_dir().join(format!(
            "options-journal-identity-{}-{}",
            std::process::id(),
            now_for_test()
        ));
        crate::secure_fs::ensure_private_dir(&base).unwrap();
        let path = base.join("journal.jsonl");
        let request = OrderRequest {
            operation_id: "payload-order".into(),
            symbol: "GGALC100".into(),
            quantity: 2,
            market_price: 10.0,
            limit_price: 10.01,
            side: crate::broker::OrderSide::Buy,
        };
        let execution = OrderExecution {
            operation_id: request.operation_id.clone(),
            broker_order_id: Some("42".into()),
            status: crate::broker::OrderStatus::PartiallyExecuted,
            filled_quantity: 1,
            fill_price: Some(10.0),
            message: None,
        };
        let position = Position {
            operation_id: request.operation_id.clone(),
            option_symbol: request.symbol.clone(),
            kind: crate::trading::PositionKind::Call,
            entry_price: 10.0,
            contracts: 2,
            contract_multiplier: 100,
            opened_at_secs: 1,
            economics: None,
            entry_context: None,
        };
        let invalid_events = [
            JournalEventKind::OrderIntentCreated {
                request: request.clone(),
            },
            JournalEventKind::OrderAccepted {
                execution: execution.clone(),
            },
            JournalEventKind::OrderUpdated {
                execution: execution.clone(),
            },
            JournalEventKind::PartialFillExposure {
                execution: execution.clone(),
                requested_quantity: 2,
                remaining_quantity: 1,
            },
            JournalEventKind::OrderUnknown {
                request: request.clone(),
                reason: "timeout".into(),
            },
            JournalEventKind::PositionOpened {
                position: position.clone(),
            },
            JournalEventKind::PositionClosed {
                operation_id: position.operation_id.clone(),
                exit_price: 11.0,
                net_pnl: 200.0,
                reason: crate::trading::ExitReason::ProfitTarget,
                stage: LiveStage::Learning,
                validation_trade: None,
            },
            JournalEventKind::OrderIntentCreated {
                request: OrderRequest {
                    operation_id: String::new(),
                    ..request.clone()
                },
            },
        ];
        let mut journal = Journal::open(&path).unwrap();
        for event in invalid_events {
            assert!(journal
                .append(1, Some("different-order".into()), event)
                .is_err());
            assert_eq!(journal.last_sequence(), 0);
        }
        assert!(journal
            .append(
                1,
                None,
                JournalEventKind::OrderSubmitted {
                    symbol: "GGALC100".into(),
                    side: crate::broker::OrderSide::Buy,
                    quantity: 2,
                    limit_price: 10.01,
                },
            )
            .is_err());
        assert_eq!(std::fs::metadata(&path).unwrap().len(), 0);
        let legacy_written = journal
            .append(
                1,
                Some("legacy-order".into()),
                JournalEventKind::OrderSubmitted {
                    symbol: "GGALC100".into(),
                    side: crate::broker::OrderSide::Buy,
                    quantity: 2,
                    limit_price: 10.01,
                },
            )
            .unwrap();
        assert_eq!(legacy_written.operation_id.as_deref(), Some("legacy-order"));
        drop(journal);

        let legacy = JournalEvent {
            schema_version: 1,
            sequence: 1,
            timestamp_secs: 1,
            operation_id: Some("different-order".into()),
            event: JournalEventKind::OrderAccepted { execution },
            previous_hash: String::new(),
            event_hash: String::new(),
            event_hmac: String::new(),
        };
        write_atomic(&path, &serde_json::to_vec(&legacy).unwrap()).unwrap();
        assert!(read_events(&path).is_err());
        std::fs::remove_dir_all(base).unwrap();
    }

    fn now_for_test() -> u128 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    }
}
