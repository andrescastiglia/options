#!/usr/bin/env python3
"""Fail CI when a critical Rust module drops below its measured floor."""

import json
import pathlib
import sys


FLOORS = {
    "src/app.rs": {"lines": 78.0, "regions": 75.0},
    "src/broker.rs": {"lines": 99.0, "regions": 99.0},
    "src/build_identity.rs": {"lines": 100.0, "regions": 100.0},
    "src/config.rs": {"lines": 98.0, "regions": 97.0},
    "src/datasets.rs": {"lines": 99.0, "regions": 95.0},
    "src/iol_client.rs": {"lines": 86.0, "regions": 84.0},
    "src/main.rs": {"lines": 86.0, "regions": 81.0},
    "src/market.rs": {"lines": 94.0, "regions": 95.0},
    "src/market_calendar.rs": {"lines": 99.0, "regions": 98.0},
    "src/persistence.rs": {"lines": 97.0, "regions": 96.0},
    "src/redaction.rs": {"lines": 100.0, "regions": 100.0},
    "src/release_readiness.rs": {"lines": 99.0, "regions": 98.0},
    "src/risk.rs": {"lines": 100.0, "regions": 100.0},
    "src/secrets.rs": {"lines": 99.0, "regions": 95.0},
    "src/secure_fs.rs": {"lines": 99.0, "regions": 95.0},
    "src/storage.rs": {"lines": 84.0, "regions": 88.0},
    "src/time_reference.rs": {"lines": 100.0, "regions": 97.0},
    "src/vix.rs": {"lines": 100.0, "regions": 99.0},
}


def main() -> int:
    if len(sys.argv) != 2:
        print("uso: check_coverage.py <llvm-cov-summary.json>", file=sys.stderr)
        return 2
    report_path = pathlib.Path(sys.argv[1])
    report = json.loads(report_path.read_text(encoding="utf-8"))
    files = report.get("data", [{}])[0].get("files", [])
    summaries = {}
    for item in files:
        normalized = pathlib.Path(item["filename"]).as_posix()
        for relative in FLOORS:
            if normalized.endswith("/" + relative) or normalized == relative:
                summaries[relative] = item["summary"]

    failures = []
    for relative, metrics in FLOORS.items():
        summary = summaries.get(relative)
        if summary is None:
            failures.append(f"{relative}: ausente del reporte")
            continue
        values = []
        for metric, floor in metrics.items():
            actual = float(summary[metric]["percent"])
            values.append(f"{metric}={actual:.2f}% (piso {floor:.2f}%)")
            if actual + 1e-9 < floor:
                failures.append(
                    f"{relative}: {metric}={actual:.2f}% < {floor:.2f}%"
                )
        print(f"{relative}: " + ", ".join(values))

    if failures:
        print("\nRegresión de cobertura crítica:", file=sys.stderr)
        for failure in failures:
            print(f"- {failure}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
