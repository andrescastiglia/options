use std::{
    fs::{create_dir_all, OpenOptions},
    io::{self, Write},
    path::Path,
};

use serde::{Deserialize, Serialize};

use crate::errors::AppError;

#[derive(Debug)]
pub struct Journal {
    file: std::fs::File,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Snapshot {
    pub timestamp_secs: i64,
    pub state: String,
    pub active_operation_id: Option<String>,
    pub last_operation_id: Option<String>,
}

pub fn save_snapshot(path: impl AsRef<Path>, snapshot: &Snapshot) -> Result<(), AppError> {
    let path = path.as_ref();
    if let Some(parent) = path.parent() {
        create_dir_all(parent)?;
    }
    let temporary = path.with_extension("tmp");
    let encoded = serde_json::to_vec_pretty(snapshot)?;
    let mut file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&temporary)?;
    file.write_all(&encoded)?;
    file.sync_all()?;
    std::fs::rename(temporary, path)?;
    Ok(())
}

pub fn load_snapshot(path: impl AsRef<Path>) -> Result<Snapshot, AppError> {
    let bytes = std::fs::read(path)?;
    Ok(serde_json::from_slice(&bytes)?)
}

impl Journal {
    pub fn open(path: impl AsRef<Path>) -> io::Result<Self> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            create_dir_all(parent)?;
        }
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        Ok(Self { file })
    }

    pub fn append(
        &mut self,
        timestamp_secs: i64,
        operation_id: &str,
        action: &str,
        details: &str,
        confirmation: bool,
    ) -> io::Result<()> {
        let escaped_details = details.replace('\\', "\\\\").replace('"', "\\\"");
        writeln!(self.file, "{{\"timestamp_secs\":{timestamp_secs},\"operation_id\":\"{operation_id}\",\"action\":\"{action}\",\"details\":\"{escaped_details}\",\"confirmation\":{confirmation}}}")?;
        self.file.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_round_trips_atomically() {
        let path =
            std::env::temp_dir().join(format!("options-snapshot-{}.json", std::process::id()));
        let snapshot = Snapshot {
            timestamp_secs: 7,
            state: "CALL_ACTIVE".into(),
            active_operation_id: Some("op-1".into()),
            last_operation_id: Some("op-1".into()),
        };
        save_snapshot(&path, &snapshot).unwrap();
        assert_eq!(load_snapshot(&path).unwrap(), snapshot);
        let _ = std::fs::remove_file(path);
    }
}
