use std::path::{Path, PathBuf};

use options_trading::{
    analytics::ANALYTICS_SCHEMA_VERSION,
    datasets::DATASET_MANIFEST_SCHEMA_VERSION,
    experiments::EXPERIMENT_SCHEMA_VERSION,
    iv_rank::IV_HISTORY_SCHEMA_VERSION,
    learning::{AUTHORIZATION_SCHEMA_VERSION, EVIDENCE_SCHEMA_VERSION},
    market::MARKET_CAPTURE_SCHEMA_VERSION,
    market_calendar::EXCHANGE_CALENDAR_SCHEMA_VERSION,
    persistence::{JOURNAL_SCHEMA_VERSION, SNAPSHOT_VERSION},
    release_readiness::RELEASE_READINESS_SCHEMA_VERSION,
};

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

#[test]
fn executable_environment_defaults_are_named_in_the_algorithm_contract() {
    let env_example = std::fs::read_to_string(root().join(".env.example")).unwrap();
    let algorithm = std::fs::read_to_string(root().join("docs/ALGORITMO.md")).unwrap();
    let undocumented = env_example
        .lines()
        .filter_map(environment_assignment)
        .map(|(name, _)| name)
        .filter(|name| !name.is_empty() && !algorithm.contains(&format!("`{name}`")))
        .collect::<Vec<_>>();
    assert!(
        undocumented.is_empty(),
        "variables ejecutables sin contrato en ALGORITMO.md: {undocumented:?}"
    );
}

fn environment_assignment(line: &str) -> Option<(&str, &str)> {
    let candidate = line
        .trim()
        .strip_prefix('#')
        .map(str::trim)
        .unwrap_or_else(|| line.trim());
    let (name, value) = candidate.split_once('=')?;
    let name = name.trim();
    (!name.is_empty()
        && name
            .bytes()
            .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit() || byte == b'_'))
    .then_some((name, value.trim()))
}

#[test]
fn standalone_environment_defaults_match_the_algorithm_table() {
    let env_example = std::fs::read_to_string(root().join(".env.example")).unwrap();
    let algorithm = std::fs::read_to_string(root().join("docs/ALGORITMO.md")).unwrap();
    let illustrative_values = ["IOL_USERNAME", "IOL_PASSWORD"];
    for (name, value) in env_example
        .lines()
        .filter(|line| !line.trim_start().starts_with('#'))
        .filter_map(|line| line.split_once('='))
        .map(|(name, value)| (name.trim(), value.trim()))
        .filter(|(name, _)| !illustrative_values.contains(name))
    {
        let exact_prefix = format!("| `{name}` |");
        if let Some(row) = algorithm
            .lines()
            .find(|line| line.starts_with(&exact_prefix))
        {
            let documented = row
                .split('|')
                .nth(2)
                .unwrap_or_default()
                .trim()
                .trim_matches('`');
            let numerically_equal = value
                .parse::<f64>()
                .ok()
                .zip(documented.parse::<f64>().ok())
                .is_some_and(|(left, right)| left == right);
            assert!(
                documented == value || numerically_equal,
                "default divergente para {name}={value}: {row}"
            );
        }
    }
}

#[test]
fn documented_core_schema_versions_match_rust_constants() {
    let algorithm = std::fs::read_to_string(root().join("docs/ALGORITMO.md")).unwrap();
    for expected in [
        format!("| Snapshot | v{SNAPSHOT_VERSION} |"),
        format!("| Journal | v{JOURNAL_SCHEMA_VERSION} |"),
        format!("| Evidencia | v{EVIDENCE_SCHEMA_VERSION} |"),
        format!("| Autorización | v{AUTHORIZATION_SCHEMA_VERSION} |"),
        format!("| Readiness pre-canary | v{RELEASE_READINESS_SCHEMA_VERSION} |"),
        format!("| Analytics | v{ANALYTICS_SCHEMA_VERSION} |"),
        format!("| Historial IV | v{IV_HISTORY_SCHEMA_VERSION} |"),
        format!("| Experimentos | v{EXPERIMENT_SCHEMA_VERSION} |"),
        format!("| Capture de mercado | v{MARKET_CAPTURE_SCHEMA_VERSION} |"),
        format!("| Manifiesto de dataset | v{DATASET_MANIFEST_SCHEMA_VERSION} |"),
        format!("| Manifiesto bursátil | v{EXCHANGE_CALENDAR_SCHEMA_VERSION} |"),
    ] {
        assert!(
            algorithm.contains(&expected),
            "contrato ausente: {expected}"
        );
    }
}

#[test]
fn documented_schema_compatibility_matches_executable_migration_policy() {
    let algorithm = std::fs::read_to_string(root().join("docs/ALGORITMO.md")).unwrap();
    for expected in [
        format!("| Snapshot | v{SNAPSHOT_VERSION} | v1–v4 | siempre v4; v1–v3 se normalizan al cargar |"),
        format!("| Journal | v{JOURNAL_SCHEMA_VERSION} | v1–v6 | v5 encadenado en readonly; v6 HMAC en live |"),
        format!("| Evidencia | v{EVIDENCE_SCHEMA_VERSION} | sólo v6 | v6; incompatible inicia otro epoch |"),
        format!("| Autorización | v{AUTHORIZATION_SCHEMA_VERSION} | sólo v3 | v3 efímera y de un solo uso |"),
        format!("| Readiness pre-canary | v{RELEASE_READINESS_SCHEMA_VERSION} | sólo v2 | v2 firmado y ligado al build; v1 se rechaza |"),
        format!("| Capture de mercado | v{MARKET_CAPTURE_SCHEMA_VERSION} | frame legado o envelope v1 | todo capture nuevo usa v1 |"),
    ] {
        assert!(algorithm.contains(&expected), "política de compatibilidad ausente: {expected}");
    }
}

#[test]
fn local_markdown_links_resolve() {
    let mut documents = vec![root().join("README.md")];
    documents.extend(
        std::fs::read_dir(root().join("docs"))
            .unwrap()
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| path.extension().is_some_and(|extension| extension == "md")),
    );
    for document in documents {
        check_local_links(&document);
    }
}

fn check_local_links(document: &Path) {
    let markdown = std::fs::read_to_string(document).unwrap();
    for fragment in markdown.split("](").skip(1) {
        let Some(raw_target) = fragment.split(')').next() else {
            continue;
        };
        let target = raw_target.trim().trim_matches(['<', '>']);
        if target.is_empty()
            || target.starts_with('#')
            || target.starts_with("https://")
            || target.starts_with("http://")
            || target.starts_with("mailto:")
        {
            continue;
        }
        let path_part = target.split('#').next().unwrap();
        let resolved = document.parent().unwrap().join(path_part);
        assert!(
            resolved.exists(),
            "link local roto en {}: {}",
            document.display(),
            target
        );
    }
}
