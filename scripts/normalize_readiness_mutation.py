#!/usr/bin/env python3
"""Normalize cargo-mutants text outcomes into pre-canary mutation evidence."""

import json
import pathlib
import re
import sys


SCOPE_FILES = {
    "app::order_recovery": {"src/app.rs"},
    "build_identity": {"src/build_identity.rs"},
    "config": {"src/config.rs"},
    "data_contracts": {"src/datasets.rs", "src/iol_client.rs", "src/market.rs"},
    "iol_client": {"src/iol_client.rs"},
    "main::authorization": {"src/main.rs"},
    "market_calendar": {"src/market_calendar.rs"},
    "persistence": {"src/persistence.rs"},
    "release_readiness": {"src/release_readiness.rs"},
    "risk": {"src/risk.rs"},
    "secrets": {"src/secrets.rs"},
    "secure_fs": {"src/secure_fs.rs"},
    "time_reference": {"src/time_reference.rs"},
    "vix": {"src/vix.rs"},
}
SOURCE = re.compile(r"(?:^|\s)(src/[A-Za-z0-9_./-]+\.rs):")


def main() -> int:
    if len(sys.argv) < 4:
        print(
            "uso: normalize_readiness_mutation.py <build-hash> <output.json> <mutants.out>...",
            file=sys.stderr,
        )
        return 2
    build_hash = sys.argv[1]
    if len(build_hash) != 64 or any(c not in "0123456789abcdefABCDEF" for c in build_hash):
        raise ValueError("build-hash debe ser SHA-256 hexadecimal")
    output_path = pathlib.Path(sys.argv[2])
    directories = [pathlib.Path(value) for value in sys.argv[3:]]
    by_file = {}
    seen_mutants = set()
    for directory in directories:
        for outcome in ("caught", "missed", "timeout"):
            path = directory / f"{outcome}.txt"
            if not path.is_file():
                raise ValueError(f"resultado cargo-mutants ausente: {path}")
            for line in path.read_text(encoding="utf-8").splitlines():
                line = line.strip()
                if not line:
                    continue
                match = SOURCE.search(line)
                if not match:
                    raise ValueError(f"mutante sin archivo reconocible en {path}: {line}")
                key = (match.group(1), line)
                if key in seen_mutants:
                    raise ValueError(f"mutante duplicado entre reportes: {line}")
                seen_mutants.add(key)
                counts = by_file.setdefault(match.group(1), {"caught": 0, "total": 0})
                counts["total"] += 1
                if outcome == "caught":
                    counts["caught"] += 1

    required_files = set().union(*SCOPE_FILES.values())
    missing = sorted(file for file in required_files if by_file.get(file, {}).get("total", 0) == 0)
    if missing:
        raise ValueError(f"módulos sin mutantes viables medidos: {missing}")

    def score(files: set[str]) -> float:
        caught = sum(by_file[file]["caught"] for file in files)
        total = sum(by_file[file]["total"] for file in files)
        return caught * 100.0 / total

    global_caught = sum(values["caught"] for values in by_file.values())
    global_total = sum(values["total"] for values in by_file.values())
    normalized = {
        "schema_version": 2,
        "build_hash": build_hash,
        "global_score_percentage": global_caught * 100.0 / global_total,
        "critical_scope_scores": {
            scope: score(files) for scope, files in SCOPE_FILES.items()
        },
    }
    output_path.write_text(
        json.dumps(normalized, indent=2, sort_keys=True) + "\n", encoding="utf-8"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
