use ring::digest;

pub const BUILD_SOURCE_NAMES: &[&str] = &[
    "analytics.rs",
    "app.rs",
    "broker.rs",
    "build_identity.rs",
    "config.rs",
    "datasets.rs",
    "errors.rs",
    "experiments.rs",
    "iol_client.rs",
    "iv_rank.rs",
    "learning.rs",
    "learning_model.rs",
    "lib.rs",
    "main.rs",
    "market.rs",
    "market_calendar.rs",
    "multileg.rs",
    "number_format.rs",
    "option_analytics.rs",
    "pattern.rs",
    "persistence.rs",
    "portfolio.rs",
    "redaction.rs",
    "release_readiness.rs",
    "risk.rs",
    "secrets.rs",
    "secure_fs.rs",
    "storage.rs",
    "time_reference.rs",
    "time_utils.rs",
    "trading.rs",
    "tui.rs",
    "vix.rs",
];

const BUILD_SOURCES: &[(&str, &str)] = &[
    ("analytics.rs", include_str!("analytics.rs")),
    ("app.rs", include_str!("app.rs")),
    ("broker.rs", include_str!("broker.rs")),
    ("build_identity.rs", include_str!("build_identity.rs")),
    ("config.rs", include_str!("config.rs")),
    ("datasets.rs", include_str!("datasets.rs")),
    ("errors.rs", include_str!("errors.rs")),
    ("experiments.rs", include_str!("experiments.rs")),
    ("iol_client.rs", include_str!("iol_client.rs")),
    ("iv_rank.rs", include_str!("iv_rank.rs")),
    ("learning.rs", include_str!("learning.rs")),
    ("learning_model.rs", include_str!("learning_model.rs")),
    ("lib.rs", include_str!("lib.rs")),
    ("main.rs", include_str!("main.rs")),
    ("market.rs", include_str!("market.rs")),
    ("market_calendar.rs", include_str!("market_calendar.rs")),
    ("multileg.rs", include_str!("multileg.rs")),
    ("number_format.rs", include_str!("number_format.rs")),
    ("option_analytics.rs", include_str!("option_analytics.rs")),
    ("pattern.rs", include_str!("pattern.rs")),
    ("persistence.rs", include_str!("persistence.rs")),
    ("portfolio.rs", include_str!("portfolio.rs")),
    ("redaction.rs", include_str!("redaction.rs")),
    ("release_readiness.rs", include_str!("release_readiness.rs")),
    ("risk.rs", include_str!("risk.rs")),
    ("secrets.rs", include_str!("secrets.rs")),
    ("secure_fs.rs", include_str!("secure_fs.rs")),
    ("storage.rs", include_str!("storage.rs")),
    ("time_reference.rs", include_str!("time_reference.rs")),
    ("time_utils.rs", include_str!("time_utils.rs")),
    ("trading.rs", include_str!("trading.rs")),
    ("tui.rs", include_str!("tui.rs")),
    ("vix.rs", include_str!("vix.rs")),
];

const BUILD_METADATA: &[(&str, &[u8])] = &[
    ("Cargo.toml", include_bytes!("../Cargo.toml")),
    ("Cargo.lock", include_bytes!("../Cargo.lock")),
    (
        "rust-toolchain.toml",
        include_bytes!("../rust-toolchain.toml"),
    ),
];

/// Identidad content-addressed de todo el código Rust y de las entradas de
/// compilación fijadas. Se liga a evidencia y autorizaciones de dinero real.
pub fn executable_build_hash() -> String {
    assert!(
        build_manifest_is_consistent(BUILD_SOURCE_NAMES, BUILD_SOURCES),
        "el manifiesto de identidad del build es inconsistente"
    );
    let mut context = digest::Context::new(&digest::SHA256);
    for (name, source) in BUILD_SOURCES {
        update_component(&mut context, name, source.as_bytes());
    }
    for (name, bytes) in BUILD_METADATA {
        update_component(&mut context, name, bytes);
    }
    context
        .finish()
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn build_manifest_is_consistent(names: &[&str], sources: &[(&str, &str)]) -> bool {
    names.len() == sources.len()
        && names
            .iter()
            .enumerate()
            .all(|(index, name)| *name == sources[index].0 && !names[..index].contains(name))
}

fn update_component(context: &mut digest::Context, name: &str, bytes: &[u8]) {
    context.update(&(name.len() as u64).to_be_bytes());
    context.update(name.as_bytes());
    context.update(&(bytes.len() as u64).to_be_bytes());
    context.update(bytes);
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;

    #[test]
    fn declared_build_inputs_cover_every_rust_source_exactly_once() {
        let declared = BUILD_SOURCE_NAMES.iter().copied().collect::<BTreeSet<_>>();
        assert_eq!(declared.len(), BUILD_SOURCE_NAMES.len());
        let actual = std::fs::read_dir(concat!(env!("CARGO_MANIFEST_DIR"), "/src"))
            .unwrap()
            .filter_map(Result::ok)
            .filter_map(|entry| {
                let path = entry.path();
                (path.extension().is_some_and(|extension| extension == "rs"))
                    .then(|| path.file_name().unwrap().to_string_lossy().into_owned())
            })
            .collect::<BTreeSet<_>>();
        assert_eq!(
            declared,
            actual.iter().map(String::as_str).collect::<BTreeSet<_>>()
        );
    }

    #[test]
    fn build_hash_is_stable_and_has_sha256_shape() {
        let first = executable_build_hash();
        assert_eq!(first, executable_build_hash());
        assert_eq!(first.len(), 64);
        assert!(first.bytes().all(|byte| byte.is_ascii_hexdigit()));
    }

    #[test]
    fn component_encoding_matches_the_versioned_length_prefixed_contract() {
        let mut context = digest::Context::new(&digest::SHA256);
        update_component(&mut context, "a", b"bc");
        let encoded_digest = context
            .finish()
            .as_ref()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();

        // SHA-256 de: u64be(1) || "a" || u64be(2) || "bc".
        assert_eq!(
            encoded_digest,
            "3fafa1cf2f19a7c1129beb20cf0983f73a489a221fc0dd2f16d1be292d089205"
        );
    }

    #[test]
    fn build_manifest_rejects_missing_reordered_and_duplicate_sources() {
        let valid_names = ["a.rs", "b.rs"];
        let valid_sources = [("a.rs", "a"), ("b.rs", "b")];
        assert!(build_manifest_is_consistent(&valid_names, &valid_sources));
        assert!(!build_manifest_is_consistent(
            &valid_names[..1],
            &valid_sources
        ));
        assert!(!build_manifest_is_consistent(
            &valid_names,
            &[("b.rs", "b"), ("a.rs", "a")]
        ));
        assert!(!build_manifest_is_consistent(
            &["a.rs", "a.rs"],
            &[("a.rs", "a"), ("a.rs", "duplicate")]
        ));
    }
}
