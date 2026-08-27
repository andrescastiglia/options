#!/usr/bin/env python3
"""Normalize llvm-cov JSON into the signed pre-canary coverage contract."""

import json
import pathlib
import sys


SCOPE_FILES = {
    "app::order_recovery": ("src/app.rs",),
    "build_identity": ("src/build_identity.rs",),
    "config": ("src/config.rs",),
    "data_contracts": ("src/datasets.rs", "src/iol_client.rs", "src/market.rs"),
    "iol_client": ("src/iol_client.rs",),
    "main::authorization": ("src/main.rs",),
    "market_calendar": ("src/market_calendar.rs",),
    "persistence": ("src/persistence.rs",),
    "release_readiness": ("src/release_readiness.rs",),
    "risk": ("src/risk.rs",),
    "secrets": ("src/secrets.rs",),
    "secure_fs": ("src/secure_fs.rs",),
    "time_reference": ("src/time_reference.rs",),
    "vix": ("src/vix.rs",),
}


def percentage(summary: dict, metric: str) -> float:
    value = summary.get(metric)
    if not isinstance(value, dict) or int(value.get("count", 0)) <= 0:
        raise ValueError(f"métrica {metric} ausente o sin instrumentar")
    return float(value["percent"])


def main() -> int:
    if len(sys.argv) != 4:
        print(
            "uso: normalize_readiness_coverage.py <llvm-cov.json> <build-hash> <output.json>",
            file=sys.stderr,
        )
        return 2
    report_path, build_hash, output_path = map(pathlib.Path, sys.argv[1:])
    build_hash = str(build_hash)
    if len(build_hash) != 64 or any(c not in "0123456789abcdefABCDEF" for c in build_hash):
        raise ValueError("build-hash debe ser SHA-256 hexadecimal")
    report = json.loads(report_path.read_text(encoding="utf-8"))
    data = report.get("data", [])
    if len(data) != 1:
        raise ValueError("llvm-cov debe contener exactamente un bloque data")
    payload = data[0]
    files = {}
    for item in payload.get("files", []):
        normalized = pathlib.Path(item["filename"]).as_posix()
        for relative in {file for values in SCOPE_FILES.values() for file in values}:
            if normalized == relative or normalized.endswith("/" + relative):
                files[relative] = item["summary"]

    missing = sorted({file for values in SCOPE_FILES.values() for file in values} - files.keys())
    if missing:
        raise ValueError(f"módulos ausentes del reporte: {missing}")

    def metrics(summary: dict) -> dict:
        return {
            "lines_percentage": percentage(summary, "lines"),
            "regions_percentage": percentage(summary, "regions"),
            "branches_percentage": percentage(summary, "branches"),
        }

    scopes = {}
    for scope, scope_files in SCOPE_FILES.items():
        values = [metrics(files[file]) for file in scope_files]
        # Un scope compuesto recibe el peor porcentaje de sus módulos, nunca
        # un promedio que pueda ocultar una superficie débil.
        scopes[scope] = {
            key: min(value[key] for value in values)
            for key in ("lines_percentage", "regions_percentage", "branches_percentage")
        }
    normalized = {
        "schema_version": 2,
        "build_hash": build_hash,
        "global": metrics(payload["totals"]),
        "critical_scopes": scopes,
    }
    pathlib.Path(output_path).write_text(
        json.dumps(normalized, indent=2, sort_keys=True) + "\n", encoding="utf-8"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
