use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use thiserror::Error;

pub const RELEASE_READINESS_SCHEMA_VERSION: u32 = 2;
pub const READINESS_MAX_AGE_SECS: i64 = 30 * 86_400;
pub const REQUIRED_CRITICAL_SCOPES: &[&str] = &[
    "app::order_recovery",
    "build_identity",
    "config",
    "data_contracts",
    "iol_client",
    "main::authorization",
    "market_calendar",
    "persistence",
    "release_readiness",
    "risk",
    "secrets",
    "secure_fs",
    "time_reference",
    "vix",
];

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct QualityMetrics {
    pub lines_percentage: f64,
    pub regions_percentage: f64,
    pub branches_percentage: f64,
    pub mutation_score_percentage: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct CoverageMetrics {
    pub lines_percentage: f64,
    pub regions_percentage: f64,
    pub branches_percentage: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CoverageEvidence {
    pub schema_version: u32,
    pub build_hash: String,
    pub global: CoverageMetrics,
    pub critical_scopes: BTreeMap<String, CoverageMetrics>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MutationEvidence {
    pub schema_version: u32,
    pub build_hash: String,
    pub global_score_percentage: f64,
    pub critical_scope_scores: BTreeMap<String, f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReleaseReadiness {
    pub schema_version: u32,
    pub build_hash: String,
    pub commit_hash: String,
    pub generated_at_secs: i64,
    pub global: QualityMetrics,
    pub critical_scopes: BTreeMap<String, QualityMetrics>,
    pub coverage_report_sha256: String,
    pub mutation_report_sha256: String,
    pub fuzz_corpus_sha256: String,
    pub fuzz_campaign_seconds: u64,
    #[serde(default)]
    pub signature: String,
}

#[derive(Debug, Error, PartialEq)]
pub enum ReadinessError {
    #[error("schema de readiness no soportado")]
    Schema,
    #[error("readiness pertenece a otro build")]
    Build,
    #[error("readiness futuro o vencido")]
    Time,
    #[error("hash o identificador inválido: {0}")]
    Identifier(&'static str),
    #[error("métrica global debajo del gate: {0}")]
    Global(&'static str),
    #[error("scope crítico ausente: {0}")]
    MissingScope(&'static str),
    #[error("scope crítico debajo del gate: {0}")]
    CriticalScope(String),
    #[error("scope crítico desconocido: {0}")]
    UnknownScope(String),
    #[error("campaña fuzz sin duración verificable")]
    FuzzCampaign,
    #[error("firma de readiness ausente o inválida")]
    Signature,
    #[error("hash del reporte {0} no coincide con sus bytes")]
    EvidenceHash(&'static str),
    #[error("contrato del reporte {0} inválido o divergente")]
    EvidenceContract(&'static str),
    #[error("no se pudo serializar readiness: {0}")]
    Serialization(String),
    #[error("no se pudo verificar la firma: {0}")]
    Secret(String),
}

impl ReleaseReadiness {
    pub fn signing_payload(&self) -> Result<Vec<u8>, ReadinessError> {
        let mut unsigned = self.clone();
        unsigned.signature.clear();
        serde_json::to_vec(&unsigned)
            .map_err(|error| ReadinessError::Serialization(error.to_string()))
    }

    pub fn validate_claims(
        &self,
        expected_build_hash: &str,
        now_secs: i64,
    ) -> Result<(), ReadinessError> {
        if self.schema_version != RELEASE_READINESS_SCHEMA_VERSION {
            return Err(ReadinessError::Schema);
        }
        if self.build_hash != expected_build_hash {
            return Err(ReadinessError::Build);
        }
        if self.generated_at_secs > now_secs.saturating_add(300)
            || now_secs.saturating_sub(self.generated_at_secs) > READINESS_MAX_AGE_SECS
        {
            return Err(ReadinessError::Time);
        }
        require_hex("build_hash", &self.build_hash)?;
        require_hex("commit_hash", &self.commit_hash)?;
        require_hex("coverage_report_sha256", &self.coverage_report_sha256)?;
        require_hex("mutation_report_sha256", &self.mutation_report_sha256)?;
        require_hex("fuzz_corpus_sha256", &self.fuzz_corpus_sha256)?;
        validate_metrics(self.global, 90.0, 85.0, 85.0, 80.0).map_err(ReadinessError::Global)?;

        let required = REQUIRED_CRITICAL_SCOPES
            .iter()
            .copied()
            .collect::<BTreeSet<_>>();
        for scope in REQUIRED_CRITICAL_SCOPES {
            let metrics = self
                .critical_scopes
                .get(*scope)
                .ok_or(ReadinessError::MissingScope(scope))?;
            validate_metrics(*metrics, 95.0, 95.0, 90.0, 90.0)
                .map_err(|metric| ReadinessError::CriticalScope(format!("{scope}: {metric}")))?;
        }
        if let Some(scope) = self
            .critical_scopes
            .keys()
            .find(|scope| !required.contains(scope.as_str()))
        {
            return Err(ReadinessError::UnknownScope(scope.clone()));
        }
        if self.fuzz_campaign_seconds == 0 {
            return Err(ReadinessError::FuzzCampaign);
        }
        Ok(())
    }

    pub fn verify_with_master_key(
        &self,
        master_key_path: &std::path::Path,
        expected_build_hash: &str,
        now_secs: i64,
    ) -> Result<(), ReadinessError> {
        self.validate_claims(expected_build_hash, now_secs)?;
        if self.signature.is_empty()
            || !crate::secrets::verify_release_readiness_payload_from(
                master_key_path,
                &self.signing_payload()?,
                &self.signature,
            )
            .map_err(|error| ReadinessError::Secret(error.to_string()))?
        {
            return Err(ReadinessError::Signature);
        }
        Ok(())
    }

    pub fn bind_evidence(
        &mut self,
        coverage_report: &[u8],
        mutation_report: &[u8],
        fuzz_corpus: &[u8],
    ) -> Result<(), ReadinessError> {
        if digest_hex(coverage_report) != self.coverage_report_sha256 {
            return Err(ReadinessError::EvidenceHash("coverage"));
        }
        if digest_hex(mutation_report) != self.mutation_report_sha256 {
            return Err(ReadinessError::EvidenceHash("mutation"));
        }
        if digest_hex(fuzz_corpus) != self.fuzz_corpus_sha256 {
            return Err(ReadinessError::EvidenceHash("fuzz corpus"));
        }
        let coverage: CoverageEvidence = serde_json::from_slice(coverage_report)
            .map_err(|_| ReadinessError::EvidenceContract("coverage"))?;
        let mutation: MutationEvidence = serde_json::from_slice(mutation_report)
            .map_err(|_| ReadinessError::EvidenceContract("mutation"))?;
        if coverage.schema_version != RELEASE_READINESS_SCHEMA_VERSION
            || coverage.build_hash != self.build_hash
            || coverage.global.lines_percentage != self.global.lines_percentage
            || coverage.global.regions_percentage != self.global.regions_percentage
            || coverage.global.branches_percentage != self.global.branches_percentage
            || coverage.critical_scopes.len() != self.critical_scopes.len()
            || self.critical_scopes.iter().any(|(scope, expected)| {
                coverage.critical_scopes.get(scope).is_none_or(|actual| {
                    actual.lines_percentage != expected.lines_percentage
                        || actual.regions_percentage != expected.regions_percentage
                        || actual.branches_percentage != expected.branches_percentage
                })
            })
        {
            return Err(ReadinessError::EvidenceContract("coverage"));
        }
        if mutation.schema_version != RELEASE_READINESS_SCHEMA_VERSION
            || mutation.build_hash != self.build_hash
            || mutation.global_score_percentage != self.global.mutation_score_percentage
            || mutation.critical_scope_scores.len() != self.critical_scopes.len()
            || self.critical_scopes.iter().any(|(scope, expected)| {
                mutation
                    .critical_scope_scores
                    .get(scope)
                    .is_none_or(|actual| *actual != expected.mutation_score_percentage)
            })
        {
            return Err(ReadinessError::EvidenceContract("mutation"));
        }
        Ok(())
    }
}

fn validate_metrics(
    metrics: QualityMetrics,
    minimum_lines: f64,
    minimum_regions: f64,
    minimum_branches: f64,
    minimum_mutation: f64,
) -> Result<(), &'static str> {
    for (name, value, minimum) in [
        ("lines", metrics.lines_percentage, minimum_lines),
        ("regions", metrics.regions_percentage, minimum_regions),
        ("branches", metrics.branches_percentage, minimum_branches),
        (
            "mutation",
            metrics.mutation_score_percentage,
            minimum_mutation,
        ),
    ] {
        if !value.is_finite() || value < minimum || value > 100.0 {
            return Err(name);
        }
    }
    Ok(())
}

fn require_hex(name: &'static str, value: &str) -> Result<(), ReadinessError> {
    if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(ReadinessError::Identifier(name));
    }
    Ok(())
}

pub fn digest_hex(bytes: &[u8]) -> String {
    ring::digest::digest(&ring::digest::SHA256, bytes)
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn passing(build_hash: &str) -> ReleaseReadiness {
        let metrics = QualityMetrics {
            lines_percentage: 95.0,
            regions_percentage: 95.0,
            branches_percentage: 90.0,
            mutation_score_percentage: 90.0,
        };
        ReleaseReadiness {
            schema_version: RELEASE_READINESS_SCHEMA_VERSION,
            build_hash: build_hash.into(),
            commit_hash: "a".repeat(64),
            generated_at_secs: 1_000,
            global: metrics,
            critical_scopes: REQUIRED_CRITICAL_SCOPES
                .iter()
                .map(|scope| ((*scope).into(), metrics))
                .collect(),
            coverage_report_sha256: digest_hex(b"coverage"),
            mutation_report_sha256: digest_hex(b"mutation"),
            fuzz_corpus_sha256: digest_hex(b"corpus"),
            fuzz_campaign_seconds: 1,
            signature: String::new(),
        }
    }

    fn evidence(readiness: &ReleaseReadiness) -> (Vec<u8>, Vec<u8>) {
        let coverage = CoverageEvidence {
            schema_version: RELEASE_READINESS_SCHEMA_VERSION,
            build_hash: readiness.build_hash.clone(),
            global: CoverageMetrics {
                lines_percentage: readiness.global.lines_percentage,
                regions_percentage: readiness.global.regions_percentage,
                branches_percentage: readiness.global.branches_percentage,
            },
            critical_scopes: readiness
                .critical_scopes
                .iter()
                .map(|(scope, metrics)| {
                    (
                        scope.clone(),
                        CoverageMetrics {
                            lines_percentage: metrics.lines_percentage,
                            regions_percentage: metrics.regions_percentage,
                            branches_percentage: metrics.branches_percentage,
                        },
                    )
                })
                .collect(),
        };
        let mutation = MutationEvidence {
            schema_version: RELEASE_READINESS_SCHEMA_VERSION,
            build_hash: readiness.build_hash.clone(),
            global_score_percentage: readiness.global.mutation_score_percentage,
            critical_scope_scores: readiness
                .critical_scopes
                .iter()
                .map(|(scope, metrics)| (scope.clone(), metrics.mutation_score_percentage))
                .collect(),
        };
        (
            serde_json::to_vec(&coverage).unwrap(),
            serde_json::to_vec(&mutation).unwrap(),
        )
    }

    #[test]
    fn exact_global_and_critical_boundaries_are_closed() {
        let build = "b".repeat(64);
        assert_eq!(READINESS_MAX_AGE_SECS, 2_592_000);
        let mut readiness = passing(&build);
        readiness.global = QualityMetrics {
            lines_percentage: 90.0,
            regions_percentage: 85.0,
            branches_percentage: 85.0,
            mutation_score_percentage: 80.0,
        };
        assert_eq!(readiness.validate_claims(&build, 1_000), Ok(()));

        readiness.global = QualityMetrics {
            lines_percentage: 100.0,
            regions_percentage: 100.0,
            branches_percentage: 100.0,
            mutation_score_percentage: 100.0,
        };
        for metrics in readiness.critical_scopes.values_mut() {
            *metrics = readiness.global;
        }
        assert_eq!(readiness.validate_claims(&build, 1_000), Ok(()));

        readiness.global.lines_percentage = 89.999;
        assert_eq!(
            readiness.validate_claims(&build, 1_000),
            Err(ReadinessError::Global("lines"))
        );
    }

    #[test]
    fn missing_scope_wrong_build_stale_and_evidence_changes_fail_closed() {
        let build = "b".repeat(64);
        let mut readiness = passing(&build);
        readiness.critical_scopes.remove("iol_client");
        assert_eq!(
            readiness.validate_claims(&build, 1_000),
            Err(ReadinessError::MissingScope("iol_client"))
        );

        readiness = passing(&build);
        assert_eq!(
            readiness.validate_claims(&"c".repeat(64), 1_000),
            Err(ReadinessError::Build)
        );
        assert_eq!(
            readiness.validate_claims(&build, 1_000 + READINESS_MAX_AGE_SECS + 1),
            Err(ReadinessError::Time)
        );
        let (coverage, mutation) = evidence(&readiness);
        readiness.coverage_report_sha256 = digest_hex(&coverage);
        readiness.mutation_report_sha256 = digest_hex(&mutation);
        assert_eq!(
            readiness.bind_evidence(&coverage, &mutation, b"corpus"),
            Ok(())
        );
        assert_eq!(
            readiness.bind_evidence(b"changed", &mutation, b"corpus"),
            Err(ReadinessError::EvidenceHash("coverage"))
        );
        let mut divergent: CoverageEvidence = serde_json::from_slice(&coverage).unwrap();
        divergent.global.lines_percentage = 94.0;
        let divergent = serde_json::to_vec(&divergent).unwrap();
        readiness.coverage_report_sha256 = digest_hex(&divergent);
        assert_eq!(
            readiness.bind_evidence(&divergent, &mutation, b"corpus"),
            Err(ReadinessError::EvidenceContract("coverage"))
        );
        readiness.coverage_report_sha256 = digest_hex(&coverage);
        assert_eq!(
            readiness.bind_evidence(&coverage, &mutation, b"changed"),
            Err(ReadinessError::EvidenceHash("fuzz corpus"))
        );
    }

    #[test]
    fn every_claim_boundary_and_unknown_scope_fails_closed() {
        let build = "b".repeat(64);
        let mut readiness = passing(&build);
        readiness.schema_version = RELEASE_READINESS_SCHEMA_VERSION - 1;
        assert_eq!(
            readiness.validate_claims(&build, 1_000),
            Err(ReadinessError::Schema)
        );
        readiness = passing(&build);
        readiness.generated_at_secs = 1_300;
        assert_eq!(readiness.validate_claims(&build, 1_000), Ok(()));
        readiness.generated_at_secs = 1_301;
        assert_eq!(
            readiness.validate_claims(&build, 1_000),
            Err(ReadinessError::Time)
        );
        readiness = passing(&build);
        assert_eq!(
            readiness.validate_claims(&build, 1_000 + READINESS_MAX_AGE_SECS),
            Ok(())
        );

        readiness = passing(&build);
        readiness.commit_hash = "not-hex".into();
        assert_eq!(
            readiness.validate_claims(&build, 1_000),
            Err(ReadinessError::Identifier("commit_hash"))
        );
        readiness = passing(&build);
        readiness.commit_hash = "z".repeat(64);
        assert_eq!(
            readiness.validate_claims(&build, 1_000),
            Err(ReadinessError::Identifier("commit_hash"))
        );
        readiness = passing(&build);
        readiness.global.regions_percentage = f64::NAN;
        assert_eq!(
            readiness.validate_claims(&build, 1_000),
            Err(ReadinessError::Global("regions"))
        );
        readiness = passing(&build);
        readiness.global.branches_percentage = 84.999;
        assert_eq!(
            readiness.validate_claims(&build, 1_000),
            Err(ReadinessError::Global("branches"))
        );
        readiness = passing(&build);
        readiness.global.mutation_score_percentage = 79.999;
        assert_eq!(
            readiness.validate_claims(&build, 1_000),
            Err(ReadinessError::Global("mutation"))
        );
        readiness = passing(&build);
        readiness.global.lines_percentage = 100.001;
        assert_eq!(
            readiness.validate_claims(&build, 1_000),
            Err(ReadinessError::Global("lines"))
        );
        readiness = passing(&build);
        readiness
            .critical_scopes
            .get_mut("risk")
            .unwrap()
            .mutation_score_percentage = 89.999;
        assert_eq!(
            readiness.validate_claims(&build, 1_000),
            Err(ReadinessError::CriticalScope("risk: mutation".into()))
        );
        readiness = passing(&build);
        readiness.critical_scopes.insert(
            "invented".into(),
            QualityMetrics {
                lines_percentage: 100.0,
                regions_percentage: 100.0,
                branches_percentage: 100.0,
                mutation_score_percentage: 100.0,
            },
        );
        assert_eq!(
            readiness.validate_claims(&build, 1_000),
            Err(ReadinessError::UnknownScope("invented".into()))
        );
        readiness = passing(&build);
        readiness.fuzz_campaign_seconds = 0;
        assert_eq!(
            readiness.validate_claims(&build, 1_000),
            Err(ReadinessError::FuzzCampaign)
        );
    }

    #[test]
    fn mutation_evidence_and_signature_are_bound_to_exact_claims() {
        let build = "b".repeat(64);
        let mut readiness = passing(&build);
        let (coverage, mutation) = evidence(&readiness);
        readiness.coverage_report_sha256 = digest_hex(&coverage);
        let mut divergent: MutationEvidence = serde_json::from_slice(&mutation).unwrap();
        divergent.critical_scope_scores.insert("vix".into(), 99.0);
        let divergent = serde_json::to_vec(&divergent).unwrap();
        readiness.mutation_report_sha256 = digest_hex(&divergent);
        assert_eq!(
            readiness.bind_evidence(&coverage, &divergent, b"corpus"),
            Err(ReadinessError::EvidenceContract("mutation"))
        );

        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory = std::env::temp_dir().join(format!(
            "options-readiness-signature-{}-{nonce}",
            std::process::id()
        ));
        crate::secure_fs::ensure_private_dir(&directory).unwrap();
        let key_path = directory.join("master.key");
        crate::secrets::initialize_master_key(&key_path).unwrap();
        readiness = passing(&build);
        assert_eq!(
            readiness.verify_with_master_key(&key_path, &build, 1_000),
            Err(ReadinessError::Signature)
        );
        readiness.signature = crate::secrets::sign_release_readiness_payload_from(
            &key_path,
            &readiness.signing_payload().unwrap(),
        )
        .unwrap();
        assert_eq!(
            readiness.verify_with_master_key(&key_path, &build, 1_000),
            Ok(())
        );
        let mut tampered_claim = readiness.clone();
        tampered_claim.commit_hash = "c".repeat(64);
        assert_eq!(
            tampered_claim.verify_with_master_key(&key_path, &build, 1_000),
            Err(ReadinessError::Signature)
        );
        let replacement = if readiness.signature.starts_with('A') {
            "B"
        } else {
            "A"
        };
        readiness.signature.replace_range(0..1, replacement);
        assert_eq!(
            readiness.verify_with_master_key(&key_path, &build, 1_000),
            Err(ReadinessError::Signature)
        );
        assert!(matches!(
            readiness.verify_with_master_key(&directory.join("missing.key"), &build, 1_000),
            Err(ReadinessError::Secret(_))
        ));
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn every_evidence_field_is_bound_and_malformed_reports_fail_closed() {
        let build = "b".repeat(64);
        let baseline = passing(&build);
        let (coverage_bytes, mutation_bytes) = evidence(&baseline);
        let coverage: CoverageEvidence = serde_json::from_slice(&coverage_bytes).unwrap();
        let mutation: MutationEvidence = serde_json::from_slice(&mutation_bytes).unwrap();

        let bind = |coverage: &CoverageEvidence, mutation: &MutationEvidence| {
            let coverage_bytes = serde_json::to_vec(coverage).unwrap();
            let mutation_bytes = serde_json::to_vec(mutation).unwrap();
            let mut readiness = baseline.clone();
            readiness.coverage_report_sha256 = digest_hex(&coverage_bytes);
            readiness.mutation_report_sha256 = digest_hex(&mutation_bytes);
            readiness.bind_evidence(&coverage_bytes, &mutation_bytes, b"corpus")
        };

        let mut changed = coverage.clone();
        changed.schema_version -= 1;
        assert_eq!(
            bind(&changed, &mutation),
            Err(ReadinessError::EvidenceContract("coverage"))
        );
        changed = coverage.clone();
        changed.build_hash = "c".repeat(64);
        assert_eq!(
            bind(&changed, &mutation),
            Err(ReadinessError::EvidenceContract("coverage"))
        );
        for metric in ["regions", "branches"] {
            changed = coverage.clone();
            match metric {
                "regions" => changed.global.regions_percentage -= 1.0,
                "branches" => changed.global.branches_percentage -= 1.0,
                _ => unreachable!(),
            }
            assert_eq!(
                bind(&changed, &mutation),
                Err(ReadinessError::EvidenceContract("coverage"))
            );
        }
        changed = coverage.clone();
        changed.critical_scopes.remove("vix");
        assert_eq!(
            bind(&changed, &mutation),
            Err(ReadinessError::EvidenceContract("coverage"))
        );
        changed = coverage.clone();
        let displaced = changed.critical_scopes.remove("vix").unwrap();
        changed.critical_scopes.insert("invented".into(), displaced);
        assert_eq!(
            bind(&changed, &mutation),
            Err(ReadinessError::EvidenceContract("coverage"))
        );
        for metric in ["lines", "regions", "branches"] {
            changed = coverage.clone();
            let value = changed.critical_scopes.get_mut("vix").unwrap();
            match metric {
                "lines" => value.lines_percentage -= 1.0,
                "regions" => value.regions_percentage -= 1.0,
                "branches" => value.branches_percentage -= 1.0,
                _ => unreachable!(),
            }
            assert_eq!(
                bind(&changed, &mutation),
                Err(ReadinessError::EvidenceContract("coverage"))
            );
        }

        let mut changed_mutation = mutation.clone();
        changed_mutation.schema_version -= 1;
        assert_eq!(
            bind(&coverage, &changed_mutation),
            Err(ReadinessError::EvidenceContract("mutation"))
        );
        changed_mutation = mutation.clone();
        changed_mutation.build_hash = "c".repeat(64);
        assert_eq!(
            bind(&coverage, &changed_mutation),
            Err(ReadinessError::EvidenceContract("mutation"))
        );
        changed_mutation = mutation.clone();
        changed_mutation.global_score_percentage -= 1.0;
        assert_eq!(
            bind(&coverage, &changed_mutation),
            Err(ReadinessError::EvidenceContract("mutation"))
        );
        changed_mutation = mutation.clone();
        changed_mutation.critical_scope_scores.remove("vix");
        assert_eq!(
            bind(&coverage, &changed_mutation),
            Err(ReadinessError::EvidenceContract("mutation"))
        );
        changed_mutation = mutation.clone();
        let displaced = changed_mutation
            .critical_scope_scores
            .remove("vix")
            .unwrap();
        changed_mutation
            .critical_scope_scores
            .insert("invented".into(), displaced);
        assert_eq!(
            bind(&coverage, &changed_mutation),
            Err(ReadinessError::EvidenceContract("mutation"))
        );
        changed_mutation = mutation.clone();
        *changed_mutation
            .critical_scope_scores
            .get_mut("vix")
            .unwrap() -= 1.0;
        assert_eq!(
            bind(&coverage, &changed_mutation),
            Err(ReadinessError::EvidenceContract("mutation"))
        );

        let mut readiness = baseline.clone();
        readiness.coverage_report_sha256 = digest_hex(b"not-json");
        readiness.mutation_report_sha256 = digest_hex(&mutation_bytes);
        assert_eq!(
            readiness.bind_evidence(b"not-json", &mutation_bytes, b"corpus"),
            Err(ReadinessError::EvidenceContract("coverage"))
        );
        readiness.coverage_report_sha256 = digest_hex(&coverage_bytes);
        readiness.mutation_report_sha256 = digest_hex(b"not-json");
        assert_eq!(
            readiness.bind_evidence(&coverage_bytes, b"not-json", b"corpus"),
            Err(ReadinessError::EvidenceContract("mutation"))
        );
        readiness.mutation_report_sha256 = digest_hex(b"changed");
        assert_eq!(
            readiness.bind_evidence(&coverage_bytes, &mutation_bytes, b"corpus"),
            Err(ReadinessError::EvidenceHash("mutation"))
        );
    }
}
