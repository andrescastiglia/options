use std::{
    collections::BTreeSet,
    io::Read,
    path::{Path, PathBuf},
};

use ring::digest::{Context, SHA256};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{
    secrets::{sign_dataset_manifest_payload, verify_dataset_manifest_payload, SecretError},
    secure_fs::{ensure_private_dir, open_limited_read, read_private_limited, write_new},
};

pub const DATASET_MANIFEST_SCHEMA_VERSION: u32 = 1;
const MAX_DATASET_BYTES: u64 = 1_073_741_824;
const MAX_MANIFEST_BYTES: u64 = 262_144;
const MAX_REGISTRY_ENTRIES: usize = 10_000;
const HASH_BUFFER_BYTES: usize = 65_536;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DatasetRole {
    Research,
    Selection,
    SealedValidation,
    Shadow,
    Canary,
    Live,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DatasetPartition {
    pub role: DatasetRole,
    pub start_secs: i64,
    pub end_secs: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DatasetManifest {
    pub schema_version: u32,
    pub dataset_id: String,
    pub origin: String,
    pub license: String,
    pub interval_start_secs: i64,
    pub interval_end_secs: i64,
    pub instruments: Vec<String>,
    pub timezone: String,
    pub transformations: Vec<String>,
    pub source_schema_version: u32,
    pub created_at_secs: i64,
    pub partitions: Vec<DatasetPartition>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignedDatasetManifest {
    pub manifest: DatasetManifest,
    pub signature: String,
}

impl SignedDatasetManifest {
    pub fn signing_payload(&self) -> Result<Vec<u8>, serde_json::Error> {
        serde_json::to_vec(&self.manifest)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HoldoutConsumption {
    pub schema_version: u32,
    pub manifest_sha256: String,
    pub dataset_id: String,
    pub partition: DatasetPartition,
    pub evaluator_id: String,
    pub consumed_at_secs: i64,
}

#[derive(Debug, Error)]
pub enum DatasetError {
    #[error("manifiesto de dataset inválido: {0}")]
    InvalidManifest(String),
    #[error("firma de dataset inválida")]
    InvalidSignature,
    #[error("el dataset no coincide con el manifiesto: esperado {expected}, observado {actual}")]
    HashMismatch { expected: String, actual: String },
    #[error("registro de datasets inválido: {0}")]
    Registry(String),
    #[error("el holdout ya fue consumido")]
    HoldoutConsumed,
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Secret(#[from] SecretError),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MarkerWriteDisposition {
    Created,
    Existing,
}

pub fn sign_manifest(manifest: DatasetManifest) -> Result<SignedDatasetManifest, DatasetError> {
    validate_manifest(&manifest)?;
    let payload = serde_json::to_vec(&manifest)?;
    let signature = sign_dataset_manifest_payload(&payload)?;
    Ok(SignedDatasetManifest {
        manifest,
        signature,
    })
}

pub fn register_dataset(
    dataset_path: &Path,
    signed: &SignedDatasetManifest,
    registry_dir: &Path,
) -> Result<PathBuf, DatasetError> {
    validate_signed_manifest(dataset_path, signed)?;
    ensure_private_dir(registry_dir)?;
    let dataset_hex = signed
        .manifest
        .dataset_id
        .strip_prefix("sha256:")
        .ok_or_else(|| DatasetError::InvalidManifest("dataset_id sin prefijo SHA-256".into()))?;
    let path = registry_dir.join(format!("dataset-v1-{dataset_hex}.json"));
    if path.exists() {
        let existing: SignedDatasetManifest =
            serde_json::from_slice(&read_private_limited(&path, MAX_MANIFEST_BYTES)?)?;
        if existing != *signed {
            return Err(DatasetError::Registry(
                "el mismo dataset ya tiene un manifiesto o split diferente".into(),
            ));
        }
        return Ok(path);
    }
    reject_registry_overlap(registry_dir, signed)?;
    write_new(&path, &serde_json::to_vec_pretty(signed)?)?;
    Ok(path)
}

pub fn consume_sealed_holdout(
    dataset_path: &Path,
    signed: &SignedDatasetManifest,
    registry_dir: &Path,
    evaluator_id: &str,
    consumed_at_secs: i64,
) -> Result<HoldoutConsumption, DatasetError> {
    let evaluator_id = evaluator_id.trim();
    if evaluator_id.is_empty()
        || evaluator_id.len() > 128
        || evaluator_id.chars().any(char::is_control)
    {
        return Err(DatasetError::InvalidManifest(
            "evaluator_id debe tener 1–128 caracteres sin controles".into(),
        ));
    }
    if consumed_at_secs <= 0 {
        return Err(DatasetError::InvalidManifest(
            "consumed_at_secs debe ser positivo".into(),
        ));
    }
    validate_signed_manifest(dataset_path, signed)?;
    let holdouts = signed
        .manifest
        .partitions
        .iter()
        .filter(|partition| partition.role == DatasetRole::SealedValidation)
        .cloned()
        .collect::<Vec<_>>();
    if holdouts.len() != 1 {
        return Err(DatasetError::InvalidManifest(
            "se exige exactamente una partición sealed_validation".into(),
        ));
    }
    register_dataset(dataset_path, signed, registry_dir)?;
    let manifest_sha256 = digest_hex(&signed.signing_payload()?);
    let consumption = HoldoutConsumption {
        schema_version: DATASET_MANIFEST_SCHEMA_VERSION,
        manifest_sha256: manifest_sha256.clone(),
        dataset_id: signed.manifest.dataset_id.clone(),
        partition: holdouts[0].clone(),
        evaluator_id: evaluator_id.to_string(),
        consumed_at_secs,
    };
    let consumed_dir = registry_dir.join("consumed");
    ensure_private_dir(&consumed_dir)?;
    let path = consumed_dir.join(format!("holdout-v1-{manifest_sha256}.json"));
    match marker_write_disposition(write_new(&path, &serde_json::to_vec_pretty(&consumption)?))? {
        MarkerWriteDisposition::Created => Ok(consumption),
        MarkerWriteDisposition::Existing => {
            let existing: HoldoutConsumption = serde_json::from_slice(&read_private_limited(
                &path,
                MAX_MANIFEST_BYTES,
            )?)
            .map_err(|error| {
                DatasetError::Registry(format!("marca de holdout existente inválida: {error}"))
            })?;
            if existing == consumption {
                Err(DatasetError::HoldoutConsumed)
            } else {
                Err(DatasetError::Registry(
                    "marca de holdout existente no coincide con el consumo solicitado".into(),
                ))
            }
        }
    }
}

fn marker_write_disposition(
    result: std::io::Result<()>,
) -> std::io::Result<MarkerWriteDisposition> {
    match result {
        Ok(()) => Ok(MarkerWriteDisposition::Created),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            Ok(MarkerWriteDisposition::Existing)
        }
        Err(error) => Err(error),
    }
}

pub fn validate_signed_manifest(
    dataset_path: &Path,
    signed: &SignedDatasetManifest,
) -> Result<(), DatasetError> {
    validate_signature(signed)?;
    let actual = dataset_sha256(dataset_path)?;
    if actual != signed.manifest.dataset_id {
        return Err(DatasetError::HashMismatch {
            expected: signed.manifest.dataset_id.clone(),
            actual,
        });
    }
    Ok(())
}

fn validate_signature(signed: &SignedDatasetManifest) -> Result<(), DatasetError> {
    validate_manifest(&signed.manifest)?;
    if !verify_dataset_manifest_payload(&signed.signing_payload()?, &signed.signature)? {
        return Err(DatasetError::InvalidSignature);
    }
    Ok(())
}

pub fn dataset_sha256(path: &Path) -> Result<String, DatasetError> {
    let mut file = open_limited_read(path, MAX_DATASET_BYTES)?;
    let mut context = Context::new(&SHA256);
    let mut buffer = [0_u8; HASH_BUFFER_BYTES];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        context.update(&buffer[..read]);
    }
    Ok(format!("sha256:{}", hex(context.finish().as_ref())))
}

fn validate_manifest(manifest: &DatasetManifest) -> Result<(), DatasetError> {
    if manifest.schema_version != DATASET_MANIFEST_SCHEMA_VERSION {
        return Err(DatasetError::InvalidManifest(format!(
            "schema {} no soportado",
            manifest.schema_version
        )));
    }
    let digest = manifest.dataset_id.strip_prefix("sha256:").unwrap_or("");
    if digest.len() != 64
        || !digest
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(DatasetError::InvalidManifest(
            "dataset_id debe ser sha256:<64 hex minúsculas>".into(),
        ));
    }
    let instruments = manifest
        .instruments
        .iter()
        .map(|instrument| instrument.as_str())
        .collect::<BTreeSet<_>>();
    let transformations = manifest
        .transformations
        .iter()
        .map(|transformation| transformation.as_str())
        .collect::<BTreeSet<_>>();
    if manifest.origin.trim().is_empty()
        || manifest.license.trim().is_empty()
        || manifest.timezone.trim().is_empty()
        || manifest.instruments.is_empty()
        || manifest.instruments.iter().any(|instrument| {
            instrument.is_empty()
                || instrument.len() > 32
                || !instrument
                    .bytes()
                    .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit() || byte == b'.')
        })
        || instruments.len() != manifest.instruments.len()
        || manifest
            .transformations
            .iter()
            .any(|transformation| transformation.trim().is_empty())
        || transformations.len() != manifest.transformations.len()
        || manifest.source_schema_version == 0
        || manifest.created_at_secs <= 0
        || !inclusive_interval_is_ordered(manifest.interval_start_secs, manifest.interval_end_secs)
        || manifest.partitions.is_empty()
    {
        return Err(DatasetError::InvalidManifest(
            "procedencia, licencia, intervalo, instrumentos, zona, schema y particiones son obligatorios"
                .into(),
        ));
    }
    for partition in &manifest.partitions {
        if !inclusive_interval_is_ordered(partition.start_secs, partition.end_secs)
            || partition.start_secs < manifest.interval_start_secs
            || partition.end_secs > manifest.interval_end_secs
        {
            return Err(DatasetError::InvalidManifest(
                "partición fuera del intervalo declarado".into(),
            ));
        }
    }
    for (index, left) in manifest.partitions.iter().enumerate() {
        for right in manifest.partitions.iter().skip(index + 1) {
            if intervals_overlap(left, right) {
                return Err(DatasetError::InvalidManifest(
                    "las particiones de roles incompatibles se solapan".into(),
                ));
            }
        }
    }
    Ok(())
}

fn reject_registry_overlap(
    registry_dir: &Path,
    candidate: &SignedDatasetManifest,
) -> Result<(), DatasetError> {
    let mut entries = 0_usize;
    for entry in std::fs::read_dir(registry_dir)? {
        entries = next_registry_entry_count(entries)
            .ok_or_else(|| DatasetError::Registry("registro demasiado grande".into()))?;
        let path = entry?.path();
        if path.extension().and_then(|value| value.to_str()) != Some("json") {
            continue;
        }
        let existing: SignedDatasetManifest =
            serde_json::from_slice(&read_private_limited(&path, MAX_MANIFEST_BYTES)?)?;
        validate_signature(&existing)?;
        if instruments_overlap(&existing.manifest, &candidate.manifest)
            && selection_holdout_overlap(&existing.manifest, &candidate.manifest)
        {
            return Err(DatasetError::Registry(
                "sesiones solapadas entre selection y sealed_validation".into(),
            ));
        }
    }
    Ok(())
}

fn inclusive_interval_is_ordered(start_secs: i64, end_secs: i64) -> bool {
    start_secs <= end_secs
}

fn next_registry_entry_count(current: usize) -> Option<usize> {
    let next = current.saturating_add(1);
    (next <= MAX_REGISTRY_ENTRIES).then_some(next)
}

fn instruments_overlap(left: &DatasetManifest, right: &DatasetManifest) -> bool {
    let left = left.instruments.iter().collect::<BTreeSet<_>>();
    right.instruments.iter().any(|item| left.contains(item))
}

fn selection_holdout_overlap(left: &DatasetManifest, right: &DatasetManifest) -> bool {
    left.partitions.iter().any(|left_partition| {
        right.partitions.iter().any(|right_partition| {
            matches!(
                (left_partition.role, right_partition.role),
                (DatasetRole::Selection, DatasetRole::SealedValidation)
                    | (DatasetRole::SealedValidation, DatasetRole::Selection)
            ) && intervals_overlap(left_partition, right_partition)
        })
    })
}

fn intervals_overlap(left: &DatasetPartition, right: &DatasetPartition) -> bool {
    left.start_secs <= right.end_secs && right.start_secs <= left.end_secs
}

fn digest_hex(bytes: &[u8]) -> String {
    hex(ring::digest::digest(&SHA256, bytes).as_ref())
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::secrets::{initialize_master_key, MASTER_KEY_ENV, MASTER_KEY_ENVIRONMENT_LOCK};

    fn manifest(partitions: Vec<DatasetPartition>) -> DatasetManifest {
        DatasetManifest {
            schema_version: DATASET_MANIFEST_SCHEMA_VERSION,
            dataset_id: format!("sha256:{}", "a".repeat(64)),
            origin: "fixture contractual".into(),
            license: "uso de prueba".into(),
            interval_start_secs: 100,
            interval_end_secs: 399,
            instruments: vec!["GGAL".into()],
            timezone: "America/Argentina/Buenos_Aires".into(),
            transformations: vec!["anonimización documentada".into()],
            source_schema_version: 1,
            created_at_secs: 50,
            partitions,
        }
    }

    fn split() -> Vec<DatasetPartition> {
        vec![
            DatasetPartition {
                role: DatasetRole::Selection,
                start_secs: 100,
                end_secs: 249,
            },
            DatasetPartition {
                role: DatasetRole::SealedValidation,
                start_secs: 250,
                end_secs: 399,
            },
        ]
    }

    fn with_test_master_key<T>(run: impl FnOnce() -> T) -> T {
        let _guard = MASTER_KEY_ENVIRONMENT_LOCK.lock().unwrap();
        let directory = tempfile::tempdir().unwrap();
        let key_path = directory.path().join("master.key");
        initialize_master_key(&key_path).unwrap();
        let previous = std::env::var_os(MASTER_KEY_ENV);
        unsafe { std::env::set_var(MASTER_KEY_ENV, &key_path) };
        let result = run();
        match previous {
            Some(previous) => unsafe { std::env::set_var(MASTER_KEY_ENV, previous) },
            None => unsafe { std::env::remove_var(MASTER_KEY_ENV) },
        }
        result
    }

    fn signed_for(path: &Path, partitions: Vec<DatasetPartition>) -> SignedDatasetManifest {
        let mut manifest = manifest(partitions);
        manifest.dataset_id = dataset_sha256(path).unwrap();
        sign_manifest(manifest).unwrap()
    }

    #[test]
    fn signed_split_rejects_overlapping_roles_before_any_evaluation() {
        let invalid = manifest(vec![
            DatasetPartition {
                role: DatasetRole::Selection,
                start_secs: 100,
                end_secs: 250,
            },
            DatasetPartition {
                role: DatasetRole::SealedValidation,
                start_secs: 250,
                end_secs: 399,
            },
        ]);
        assert!(validate_manifest(&invalid).is_err());

        let valid = manifest(vec![
            DatasetPartition {
                role: DatasetRole::Selection,
                start_secs: 100,
                end_secs: 249,
            },
            DatasetPartition {
                role: DatasetRole::SealedValidation,
                start_secs: 250,
                end_secs: 399,
            },
        ]);
        assert!(validate_manifest(&valid).is_ok());
    }

    #[test]
    fn manifest_rejects_each_missing_or_ambiguous_identity_field() {
        let valid = manifest(split());
        assert!(validate_manifest(&valid).is_ok());

        for instrument in ["A".repeat(32), "GGAL1".into(), "GGAL.C".into()] {
            let mut boundary = valid.clone();
            boundary.instruments = vec![instrument];
            assert!(validate_manifest(&boundary).is_ok());
        }

        let mutations: &[fn(&mut DatasetManifest)] = &[
            |m| m.schema_version = 2,
            |m| m.dataset_id = "sha256:short".into(),
            |m| m.dataset_id = format!("sha256:{}", "A".repeat(64)),
            |m| m.origin.clear(),
            |m| m.license = " ".into(),
            |m| m.timezone.clear(),
            |m| m.instruments.clear(),
            |m| m.instruments[0].clear(),
            |m| m.instruments[0] = "ggal".into(),
            |m| m.instruments[0] = "A".repeat(33),
            |m| m.instruments.push("GGAL".into()),
            |m| m.transformations.push(" ".into()),
            |m| m.transformations.push(m.transformations[0].clone()),
            |m| m.source_schema_version = 0,
            |m| m.created_at_secs = 0,
            |m| m.interval_start_secs = m.interval_end_secs + 1,
            |m| m.partitions.clear(),
        ];
        for mutate in mutations {
            let mut candidate = valid.clone();
            mutate(&mut candidate);
            assert!(validate_manifest(&candidate).is_err());
        }
    }

    #[test]
    fn every_partition_must_be_ordered_contained_and_disjoint() {
        assert!(inclusive_interval_is_ordered(100, 100));
        assert!(!inclusive_interval_is_ordered(101, 100));

        let valid = manifest(split());
        for mutate in [
            |m: &mut DatasetManifest| m.partitions[0].start_secs = m.partitions[0].end_secs + 1,
            |m: &mut DatasetManifest| m.partitions[0].start_secs = m.interval_start_secs - 1,
            |m: &mut DatasetManifest| m.partitions[1].end_secs = m.interval_end_secs + 1,
            |m: &mut DatasetManifest| m.partitions[1].start_secs = m.partitions[0].end_secs,
        ] {
            let mut candidate = valid.clone();
            mutate(&mut candidate);
            assert!(validate_manifest(&candidate).is_err());
        }
    }

    #[test]
    fn interval_and_instrument_overlap_contracts_are_closed_at_boundaries() {
        let selection = DatasetPartition {
            role: DatasetRole::Selection,
            start_secs: 100,
            end_secs: 200,
        };
        let touching = DatasetPartition {
            role: DatasetRole::SealedValidation,
            start_secs: 200,
            end_secs: 300,
        };
        let disjoint = DatasetPartition {
            start_secs: 201,
            ..touching.clone()
        };
        assert!(intervals_overlap(&selection, &touching));
        assert!(!intervals_overlap(&selection, &disjoint));

        let left = manifest(vec![selection.clone()]);
        let mut right = manifest(vec![touching]);
        assert!(instruments_overlap(&left, &right));
        assert!(selection_holdout_overlap(&left, &right));
        right.instruments = vec!["YPFD".into()];
        assert!(!instruments_overlap(&left, &right));
        right.partitions[0].role = DatasetRole::Research;
        assert!(!selection_holdout_overlap(&left, &right));
    }

    #[test]
    fn registry_count_and_marker_errors_have_exact_boundaries() {
        assert_eq!(MAX_REGISTRY_ENTRIES, 10_000);
        assert_eq!(next_registry_entry_count(0), Some(1));
        assert_eq!(next_registry_entry_count(9_999), Some(10_000));
        assert_eq!(next_registry_entry_count(10_000), None);
        assert_eq!(next_registry_entry_count(usize::MAX), None);

        assert_eq!(
            marker_write_disposition(Ok(())).unwrap(),
            MarkerWriteDisposition::Created
        );
        assert_eq!(
            marker_write_disposition(Err(std::io::Error::from(std::io::ErrorKind::AlreadyExists)))
                .unwrap(),
            MarkerWriteDisposition::Existing
        );
        let permission_denied = marker_write_disposition(Err(std::io::Error::from(
            std::io::ErrorKind::PermissionDenied,
        )))
        .unwrap_err();
        assert_eq!(
            permission_denied.kind(),
            std::io::ErrorKind::PermissionDenied
        );
    }

    #[test]
    fn dataset_hash_reads_the_exact_file_and_rejects_oversize_input() {
        assert_eq!(MAX_DATASET_BYTES, 1_073_741_824);
        assert_eq!(MAX_MANIFEST_BYTES, 262_144);
        assert_eq!(HASH_BUFFER_BYTES, 65_536);

        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("dataset.jsonl");
        crate::secure_fs::write_new(&path, b"first\nsecond\n").unwrap();
        assert_eq!(
            dataset_sha256(&path).unwrap(),
            format!("sha256:{}", digest_hex(b"first\nsecond\n"))
        );

        let oversized = directory.path().join("oversized.jsonl");
        let file = std::fs::File::create(&oversized).unwrap();
        file.set_len(MAX_DATASET_BYTES + 1).unwrap();
        assert!(dataset_sha256(&oversized).is_err());
    }

    #[test]
    fn signed_registry_is_idempotent_and_rejects_hash_signature_and_split_changes() {
        with_test_master_key(|| {
            let directory = tempfile::tempdir().unwrap();
            let dataset = directory.path().join("dataset.jsonl");
            crate::secure_fs::write_new(&dataset, b"contractual dataset\n").unwrap();
            let registry = directory.path().join("registry");
            let signed = signed_for(&dataset, split());

            let registered = register_dataset(&dataset, &signed, &registry).unwrap();
            assert_eq!(
                register_dataset(&dataset, &signed, &registry).unwrap(),
                registered
            );

            let mut bad_signature = signed.clone();
            bad_signature.signature.push('A');
            assert!(matches!(
                validate_signed_manifest(&dataset, &bad_signature),
                Err(DatasetError::InvalidSignature) | Err(DatasetError::Secret(_))
            ));

            let changed_dataset = directory.path().join("changed.jsonl");
            crate::secure_fs::write_new(&changed_dataset, b"changed bytes\n").unwrap();
            assert!(matches!(
                validate_signed_manifest(&changed_dataset, &signed),
                Err(DatasetError::HashMismatch { .. })
            ));

            let mut changed_split = signed.manifest.clone();
            changed_split.partitions[0].end_secs = 199;
            changed_split.partitions[1].start_secs = 200;
            let changed_split = sign_manifest(changed_split).unwrap();
            assert!(matches!(
                register_dataset(&dataset, &changed_split, &registry),
                Err(DatasetError::Registry(_))
            ));
        });
    }

    #[test]
    fn registry_rejects_cross_dataset_selection_holdout_leakage() {
        with_test_master_key(|| {
            let directory = tempfile::tempdir().unwrap();
            let registry = directory.path().join("registry");
            let first = directory.path().join("first.jsonl");
            let second = directory.path().join("second.jsonl");
            let third = directory.path().join("third.jsonl");
            crate::secure_fs::write_new(&first, b"first\n").unwrap();
            crate::secure_fs::write_new(&second, b"second\n").unwrap();
            crate::secure_fs::write_new(&third, b"third\n").unwrap();

            let first_signed = signed_for(&first, split());
            register_dataset(&first, &first_signed, &registry).unwrap();
            std::fs::write(registry.join("ignored.txt"), b"not a manifest").unwrap();

            let mut overlapping = signed_for(
                &second,
                vec![
                    DatasetPartition {
                        role: DatasetRole::Selection,
                        start_secs: 200,
                        end_secs: 299,
                    },
                    DatasetPartition {
                        role: DatasetRole::SealedValidation,
                        start_secs: 300,
                        end_secs: 399,
                    },
                ],
            );
            overlapping.manifest.interval_start_secs = 100;
            overlapping = sign_manifest(overlapping.manifest).unwrap();
            assert!(matches!(
                register_dataset(&second, &overlapping, &registry),
                Err(DatasetError::Registry(_))
            ));

            let mut independent_manifest = overlapping.manifest.clone();
            independent_manifest.dataset_id = dataset_sha256(&third).unwrap();
            independent_manifest.instruments = vec!["YPFD".into()];
            let independent = sign_manifest(independent_manifest).unwrap();
            assert!(register_dataset(&third, &independent, &registry).is_ok());
        });
    }

    #[test]
    fn sealed_holdout_is_validated_before_registration_and_consumed_exactly_once() {
        with_test_master_key(|| {
            let directory = tempfile::tempdir().unwrap();
            let dataset = directory.path().join("dataset.jsonl");
            crate::secure_fs::write_new(&dataset, b"sealed dataset\n").unwrap();
            let registry = directory.path().join("registry");
            let signed = signed_for(&dataset, split());

            assert!(consume_sealed_holdout(&dataset, &signed, &registry, " ", 100).is_err());
            assert!(consume_sealed_holdout(&dataset, &signed, &registry, "eval\nid", 100).is_err());
            assert!(
                consume_sealed_holdout(&dataset, &signed, &registry, &"e".repeat(129), 100)
                    .is_err()
            );
            assert!(consume_sealed_holdout(&dataset, &signed, &registry, "eval", 0).is_err());
            assert!(consume_sealed_holdout(&dataset, &signed, &registry, "eval", -1).is_err());
            assert!(!registry.exists());

            let max_evaluator_registry = directory.path().join("max-evaluator-registry");
            let max_evaluator = "e".repeat(128);
            let max_consumption = consume_sealed_holdout(
                &dataset,
                &signed,
                &max_evaluator_registry,
                &max_evaluator,
                1,
            )
            .unwrap();
            assert_eq!(max_consumption.evaluator_id, max_evaluator);
            assert_eq!(max_consumption.consumed_at_secs, 1);

            let no_holdout = signed_for(
                &dataset,
                vec![DatasetPartition {
                    role: DatasetRole::Research,
                    start_secs: 100,
                    end_secs: 399,
                }],
            );
            assert!(consume_sealed_holdout(&dataset, &no_holdout, &registry, "eval", 100).is_err());
            assert!(!registry.exists());

            let consumption =
                consume_sealed_holdout(&dataset, &signed, &registry, " eval ", 100).unwrap();
            assert_eq!(consumption.evaluator_id, "eval");
            assert!(matches!(
                consume_sealed_holdout(&dataset, &signed, &registry, "eval", 100),
                Err(DatasetError::HoldoutConsumed)
            ));
            assert!(matches!(
                consume_sealed_holdout(&dataset, &signed, &registry, "different", 101),
                Err(DatasetError::Registry(_))
            ));

            let marker = registry
                .join("consumed")
                .join(format!("holdout-v1-{}.json", consumption.manifest_sha256));
            std::fs::write(marker, b"corrupt").unwrap();
            assert!(matches!(
                consume_sealed_holdout(&dataset, &signed, &registry, "eval", 100),
                Err(DatasetError::Registry(_))
            ));
        });
    }
}
