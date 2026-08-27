use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
    process::Command,
    time::{SystemTime, UNIX_EPOCH},
};

const REQUIRED_FILES: &[&str] = &[
    "src/app.rs",
    "src/build_identity.rs",
    "src/config.rs",
    "src/datasets.rs",
    "src/iol_client.rs",
    "src/main.rs",
    "src/market.rs",
    "src/market_calendar.rs",
    "src/persistence.rs",
    "src/release_readiness.rs",
    "src/risk.rs",
    "src/secrets.rs",
    "src/secure_fs.rs",
    "src/time_reference.rs",
    "src/vix.rs",
];

fn temporary_directory(label: &str) -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let path = std::env::temp_dir().join(format!(
        "options-readiness-{label}-{}-{nonce}",
        std::process::id()
    ));
    fs::create_dir_all(&path).unwrap();
    path
}

fn metric(percent: f64, count: u64) -> serde_json::Value {
    serde_json::json!({"count": count, "covered": count, "percent": percent})
}

fn summary(lines: f64, regions: f64, branches: f64) -> serde_json::Value {
    serde_json::json!({
        "lines": metric(lines, 10),
        "regions": metric(regions, 10),
        "branches": metric(branches, 10),
    })
}

fn run_python(script: &str, args: &[&Path]) -> std::process::Output {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut command = Command::new("python3");
    command.arg(root.join(script));
    command.args(args);
    command.output().unwrap()
}

#[test]
fn coverage_normalizer_uses_real_branch_counts_and_the_weakest_composite_module() {
    let directory = temporary_directory("coverage");
    let input = directory.join("raw.json");
    let output = directory.join("normalized.json");
    let build = Path::new("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
    let files = REQUIRED_FILES
        .iter()
        .map(|file| {
            let values = if *file == "src/market.rs" {
                summary(91.0, 92.0, 93.0)
            } else {
                summary(99.0, 98.0, 97.0)
            };
            serde_json::json!({"filename": file, "summary": values})
        })
        .collect::<Vec<_>>();
    fs::write(
        &input,
        serde_json::to_vec(&serde_json::json!({
            "data": [{"files": files, "totals": summary(96.0, 95.0, 94.0)}]
        }))
        .unwrap(),
    )
    .unwrap();

    let result = run_python(
        "scripts/normalize_readiness_coverage.py",
        &[&input, build, &output],
    );
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let normalized: serde_json::Value =
        serde_json::from_slice(&fs::read(&output).unwrap()).unwrap();
    assert_eq!(
        normalized["critical_scopes"]["data_contracts"]["branches_percentage"],
        93.0
    );

    let mut raw: serde_json::Value = serde_json::from_slice(&fs::read(&input).unwrap()).unwrap();
    raw["data"][0]["totals"]["branches"]["count"] = 0.into();
    fs::write(&input, serde_json::to_vec(&raw).unwrap()).unwrap();
    let rejected = run_python(
        "scripts/normalize_readiness_coverage.py",
        &[&input, build, &output],
    );
    assert!(!rejected.status.success());
    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn mutation_normalizer_counts_missed_and_timeout_as_surviving_mutants() {
    let directory = temporary_directory("mutation");
    let report = directory.join("mutants.out");
    fs::create_dir(&report).unwrap();
    let caught = REQUIRED_FILES
        .iter()
        .map(|file| format!("{file}:1: replace expression"))
        .collect::<Vec<_>>()
        .join("\n");
    fs::write(report.join("caught.txt"), format!("{caught}\n")).unwrap();
    fs::write(
        report.join("missed.txt"),
        "src/risk.rs:2: negate condition\n",
    )
    .unwrap();
    fs::write(report.join("timeout.txt"), "src/vix.rs:3: replace loop\n").unwrap();
    let output = directory.join("normalized.json");
    let build = Path::new("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb");

    let result = run_python(
        "scripts/normalize_readiness_mutation.py",
        &[build, &output, &report],
    );
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let normalized: serde_json::Value =
        serde_json::from_slice(&fs::read(&output).unwrap()).unwrap();
    let caught = REQUIRED_FILES.len() as f64;
    assert_eq!(
        normalized["global_score_percentage"],
        caught / (caught + 2.0) * 100.0
    );
    assert_eq!(normalized["critical_scope_scores"]["risk"], 50.0);
    assert_eq!(normalized["critical_scope_scores"]["vix"], 50.0);
    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn mutation_workflow_partitions_every_rust_source_exactly_once() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let workflow = fs::read_to_string(root.join(".github/workflows/mutation.yml")).unwrap();
    let declared = workflow
        .lines()
        .filter_map(|line| line.trim().strip_prefix("-f "))
        .map(str::to_owned)
        .collect::<Vec<_>>();
    let unique = declared.iter().cloned().collect::<BTreeSet<_>>();
    assert_eq!(
        declared.len(),
        unique.len(),
        "un módulo aparece en dos dominios"
    );

    let actual = fs::read_dir(root.join("src"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.extension().is_some_and(|extension| extension == "rs"))
        .map(|path| format!("src/{}", path.file_name().unwrap().to_string_lossy()))
        .collect::<BTreeSet<_>>();
    assert_eq!(unique, actual);
}
