use std::{
    fs::{create_dir_all, File, OpenOptions},
    io::{BufRead, BufReader, Write},
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

use crate::{
    broker::{OrderExecution, OrderSide},
    errors::AppError,
    iol_client::CostCalibration,
    learning::{LearningState, LiveStage, ValidationTrade},
    market::MarketFrame,
    pattern::{Direction, Trend, TrendDetector},
    portfolio::Portfolio,
    risk::RiskManager,
    trading::{ExitReason, Pnl, Position, TradingEngine},
};

pub const SNAPSHOT_VERSION: u32 = 2;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum JournalEventKind {
    Started {
        mode: String,
        ticker: String,
    },
    OrderSubmitted {
        symbol: String,
        side: OrderSide,
        quantity: u32,
        limit_price: f64,
    },
    OrderUpdated {
        execution: OrderExecution,
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
    pub sequence: u64,
    pub timestamp_secs: i64,
    pub operation_id: Option<String>,
    pub event: JournalEventKind,
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
    if let Some(parent) = path.parent() {
        create_dir_all(parent)?;
    }
    let temporary = temporary_path(path);
    let encoded = serde_json::to_vec_pretty(snapshot)?;
    let mut file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&temporary)?;
    file.write_all(&encoded)?;
    file.sync_all()?;
    std::fs::rename(&temporary, path)?;
    if let Some(parent) = path.parent() {
        File::open(parent)?.sync_all()?;
    }
    Ok(())
}

pub fn load_snapshot(path: impl AsRef<Path>) -> Result<Snapshot, AppError> {
    let bytes = std::fs::read(path)?;
    let mut snapshot: Snapshot = serde_json::from_slice(&bytes)?;
    if snapshot.version != 1 && snapshot.version != SNAPSHOT_VERSION {
        return Err(AppError::Recovery(format!(
            "version de snapshot {} no soportada",
            snapshot.version
        )));
    }
    snapshot.version = SNAPSHOT_VERSION;
    Ok(snapshot)
}

fn temporary_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(".tmp");
    PathBuf::from(name)
}

#[derive(Debug)]
pub struct Journal {
    file: File,
    path: PathBuf,
    last_sequence: u64,
}

impl Journal {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, AppError> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            create_dir_all(parent)?;
        }
        let last_sequence = if path.exists() {
            read_events(path)?.last().map_or(0, |event| event.sequence)
        } else {
            0
        };
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        Ok(Self {
            file,
            path: path.to_path_buf(),
            last_sequence,
        })
    }

    pub fn append(
        &mut self,
        timestamp_secs: i64,
        operation_id: Option<String>,
        event: JournalEventKind,
    ) -> Result<JournalEvent, AppError> {
        self.last_sequence = self.last_sequence.saturating_add(1);
        let entry = JournalEvent {
            sequence: self.last_sequence,
            timestamp_secs,
            operation_id,
            event,
        };
        serde_json::to_writer(&mut self.file, &entry)?;
        self.file.write_all(b"\n")?;
        self.file.flush()?;
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
        Ok(read_events(&self.path)?
            .into_iter()
            .filter(|event| event.sequence > sequence)
            .collect())
    }
}

pub fn read_events(path: impl AsRef<Path>) -> Result<Vec<JournalEvent>, AppError> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
    };
    BufReader::new(file)
        .lines()
        .enumerate()
        .filter_map(|(index, line)| match line {
            Ok(line) if line.trim().is_empty() => None,
            Ok(line) => Some(serde_json::from_str(&line).map_err(|error| {
                AppError::Recovery(format!("journal corrupto en linea {}: {error}", index + 1))
            })),
            Err(error) => Some(Err(error.into())),
        })
        .collect()
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
        assert_eq!(read_events(&path).unwrap().len(), 2);
        let _ = std::fs::remove_file(path);
    }

    fn now_for_test() -> u128 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    }
}
