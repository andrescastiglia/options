use std::path::{Path, PathBuf};

use crate::time_utils::argentina_session_day;

const MAX_DIRECTORY_DEPTH: usize = 16;
const MAX_DIRECTORY_ENTRIES: usize = 100_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StorageLimits {
    pub max_total_bytes: u64,
    pub min_free_bytes: u64,
    pub capture_retention_days: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StorageCapacity {
    pub used_bytes: u64,
    pub available_bytes: u64,
}

impl StorageCapacity {
    pub fn allows(self, limits: StorageLimits, incoming_bytes: u64) -> bool {
        self.used_bytes
            .checked_add(incoming_bytes)
            .is_some_and(|projected| projected <= limits.max_total_bytes)
            && self.available_bytes >= limits.min_free_bytes.saturating_add(incoming_bytes)
    }

    pub fn quota_usage_ratio(self, max_total_bytes: u64) -> f64 {
        if max_total_bytes == 0 {
            1.0
        } else {
            self.used_bytes as f64 / max_total_bytes as f64
        }
    }
}

pub fn inspect_capacity(root: &Path) -> std::io::Result<StorageCapacity> {
    crate::secure_fs::ensure_private_dir(root)?;
    Ok(StorageCapacity {
        used_bytes: directory_size(root)?,
        available_bytes: fs2::available_space(root)?,
    })
}

pub fn daily_jsonl_path(
    data_dir: &Path,
    category: &str,
    stream: &str,
    timestamp_secs: i64,
) -> PathBuf {
    data_dir
        .join(category)
        .join(stream)
        .join(format!("{}.jsonl", argentina_session_day(timestamp_secs)))
}

pub fn require_capacity(
    root: &Path,
    limits: StorageLimits,
    incoming_bytes: u64,
) -> std::io::Result<StorageCapacity> {
    let capacity = inspect_capacity(root)?;
    if !capacity.allows(limits, incoming_bytes) {
        return Err(std::io::Error::other(format!(
            "almacenamiento sin margen seguro: usados={} límite={} libres={} reserva_mínima={} escritura_prevista={}",
            capacity.used_bytes,
            limits.max_total_bytes,
            capacity.available_bytes,
            limits.min_free_bytes,
            incoming_bytes
        )));
    }
    Ok(capacity)
}

pub fn prune_expired_market_captures(
    data_dir: &Path,
    now_secs: i64,
    retention_days: u64,
) -> std::io::Result<Vec<PathBuf>> {
    let market_dir = data_dir.join("market");
    if !market_dir.exists() {
        return Ok(Vec::new());
    }
    reject_directory_symlink(&market_dir)?;
    let cutoff = argentina_session_day(now_secs).saturating_sub(retention_days as i64);
    let mut removed = Vec::new();
    let mut entries = 0_usize;
    for ticker_entry in std::fs::read_dir(&market_dir)? {
        entries = checked_entry_count(entries)?;
        let ticker_entry = ticker_entry?;
        let ticker_path = ticker_entry.path();
        let ticker_metadata = std::fs::symlink_metadata(&ticker_path)?;
        if ticker_metadata.file_type().is_symlink() {
            return Err(std::io::Error::other(format!(
                "se rechazó un enlace simbólico en captures: {}",
                ticker_path.display()
            )));
        }
        if !ticker_metadata.is_dir() {
            continue;
        }
        for capture_entry in std::fs::read_dir(&ticker_path)? {
            entries = checked_entry_count(entries)?;
            let capture_entry = capture_entry?;
            let capture_path = capture_entry.path();
            let metadata = std::fs::symlink_metadata(&capture_path)?;
            if metadata.file_type().is_symlink() {
                return Err(std::io::Error::other(format!(
                    "se rechazó un enlace simbólico en captures: {}",
                    capture_path.display()
                )));
            }
            if !metadata.is_file()
                || capture_path.extension().and_then(|value| value.to_str()) != Some("jsonl")
            {
                continue;
            }
            let Some(session_day) = capture_path
                .file_stem()
                .and_then(|value| value.to_str())
                .and_then(|value| value.parse::<i64>().ok())
            else {
                continue;
            };
            if session_day < cutoff {
                std::fs::remove_file(&capture_path)?;
                removed.push(capture_path);
            }
        }
    }
    Ok(removed)
}

fn directory_size(root: &Path) -> std::io::Result<u64> {
    let mut total = 0_u64;
    let mut entries = 0_usize;
    let mut pending = vec![(root.to_path_buf(), 0_usize)];
    while let Some((directory, depth)) = pending.pop() {
        if depth > MAX_DIRECTORY_DEPTH {
            return Err(std::io::Error::other(
                "árbol de almacenamiento demasiado profundo",
            ));
        }
        reject_directory_symlink(&directory)?;
        for entry in std::fs::read_dir(&directory)? {
            entries = checked_entry_count(entries)?;
            let path = entry?.path();
            let metadata = std::fs::symlink_metadata(&path)?;
            if metadata.file_type().is_symlink() {
                return Err(std::io::Error::other(format!(
                    "se rechazó un enlace simbólico al medir almacenamiento: {}",
                    path.display()
                )));
            }
            if metadata.is_dir() {
                pending.push((path, depth + 1));
            } else if metadata.is_file() {
                total = total
                    .checked_add(metadata.len())
                    .ok_or_else(|| std::io::Error::other("overflow al medir almacenamiento"))?;
            }
        }
    }
    Ok(total)
}

fn reject_directory_symlink(path: &Path) -> std::io::Result<()> {
    let metadata = std::fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(std::io::Error::other(format!(
            "directorio de almacenamiento inválido: {}",
            path.display()
        )));
    }
    Ok(())
}

fn checked_entry_count(current: usize) -> std::io::Result<usize> {
    let next = current.saturating_add(1);
    if next > MAX_DIRECTORY_ENTRIES {
        return Err(std::io::Error::other(
            "demasiadas entradas al inspeccionar almacenamiento",
        ));
    }
    Ok(next)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn temporary_dir(label: &str) -> PathBuf {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "options-storage-{label}-{}-{}",
            std::process::id(),
            nonce
        ))
    }

    #[test]
    fn aggregate_usage_is_measured_from_real_files_and_closed_at_the_boundary() {
        let root = temporary_dir("quota");
        let _ = std::fs::remove_dir_all(&root);
        crate::secure_fs::ensure_private_dir(&root).unwrap();
        crate::secure_fs::write_atomic(&root.join("one.bin"), &[1; 7]).unwrap();
        crate::secure_fs::write_atomic(&root.join("two.bin"), &[2; 5]).unwrap();
        let capacity = inspect_capacity(&root).unwrap();
        assert_eq!(capacity.used_bytes, 12);
        let exact = StorageLimits {
            max_total_bytes: 15,
            min_free_bytes: 0,
            capture_retention_days: 30,
        };
        assert!(capacity.allows(exact, 3));
        assert!(!capacity.allows(exact, 4));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn retention_removes_only_expired_numeric_market_captures() {
        let root = temporary_dir("retention");
        let _ = std::fs::remove_dir_all(&root);
        let ticker = root.join("market/GAL");
        crate::secure_fs::ensure_private_dir(&ticker).unwrap();
        for name in [
            "89.jsonl",
            "90.jsonl",
            "91.jsonl",
            "keep.txt",
            "invalid.jsonl",
        ] {
            let mut file = crate::secure_fs::open_private_append(&ticker.join(name)).unwrap();
            file.write_all(b"x").unwrap();
        }
        let now = (100 * 86_400) + (3 * 3_600);
        let removed = prune_expired_market_captures(&root, now, 10).unwrap();
        assert_eq!(removed, vec![ticker.join("89.jsonl")]);
        assert!(ticker.join("90.jsonl").exists());
        assert!(ticker.join("91.jsonl").exists());
        assert!(ticker.join("keep.txt").exists());
        assert!(ticker.join("invalid.jsonl").exists());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn telemetry_segments_are_stable_within_a_session_day() {
        let root = Path::new("/data");
        let first = daily_jsonl_path(root, "telemetry", "executions", 100 * 86_400);
        let same_day = daily_jsonl_path(root, "telemetry", "executions", 100 * 86_400 + 60);
        let next_day = daily_jsonl_path(root, "telemetry", "executions", 101 * 86_400);
        assert_eq!(first, same_day);
        assert_ne!(first, next_day);
        assert_eq!(
            first.parent(),
            Some(Path::new("/data/telemetry/executions"))
        );
        assert_eq!(
            first.extension().and_then(|value| value.to_str()),
            Some("jsonl")
        );
    }
}
