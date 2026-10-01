#!/usr/bin/env python3
"""Capture synthetic workload phases in ABBA order and compare matched runs."""

import argparse
import json
import statistics
import subprocess
import sys
from pathlib import Path

import runtime_baseline as baseline


def compare(paths):
    values = {label: {} for label in paths}
    reference = None
    identities = {}
    for label, captures in paths.items():
        for path in captures:
            manifest = json.loads(path.read_text())
            if manifest.get("schema") != 1 or not manifest.get("complete"):
                raise ValueError("A complete workload capture is required")
            prepared = manifest["prepared"]
            identity = (manifest["prepared_sha256"], prepared["binaries"]["runtime_baseline"]["sha256"])
            if label in identities and identities[label] != identity:
                raise ValueError("Workload artifact changed between repetitions")
            identities[label] = identity
            controls = {key: manifest[key] for key in ("protocol", "host", "environment")}
            controls.update({key: prepared[key] for key in ("toolchain", "build_args", "build_environment")})
            controls["workload_source"] = prepared["binaries"]["runtime_baseline"]["benchmark_source_sha256"]
            if reference is None:
                reference = controls
            elif reference != controls:
                raise ValueError("Workload comparison controls differ")
            for run in manifest["runs"]:
                if run["returncode"] != 0 or run["timed_out"]:
                    raise ValueError("Workload run failed")
                for row in run["results"]:
                    key = (row["workload"], row["size"], row["phase"])
                    values[label].setdefault(key, []).append({q: row[q] for q in ("p50", "p95", "p99")})
    if values["before"].keys() != values["after"].keys():
        raise ValueError("Workload result sets differ")
    rows = []
    for key in sorted(values["before"]):
        old, new = (values[label][key] for label in ("before", "after"))
        if len(old) != len(new):
            raise ValueError("Workload repetition counts differ")
        row = dict(zip(("workload", "size", "phase"), key))
        row.update(before_runs=old, after_runs=new)
        for quantile in ("p50", "p95", "p99"):
            old_value = statistics.median(run[quantile] for run in old)
            new_value = statistics.median(run[quantile] for run in new)
            row[quantile] = {"before_ns": old_value, "after_ns": new_value,
                             "descriptive_change_percent": 100 * (new_value / old_value - 1) if old_value else None}
        rows.append(row)
    return {"descriptive_only": True, "note": "Synthetic phase quantiles, not production tails or significance tests.", "results": rows}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--before", type=Path, required=True)
    parser.add_argument("--after", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--sizes", default="16,128")
    parser.add_argument("--iterations", type=int, default=1000)
    parser.add_argument("--warmup", type=int, default=50)
    args = parser.parse_args()
    output = args.output.resolve()
    output.mkdir(parents=True, exist_ok=False)
    captures = {"before": [], "after": []}
    for position, label in enumerate(("before", "after", "after", "before"), 1):
        directory = output / f"{position}-{label}"
        subprocess.run([sys.executable, str(Path(__file__).with_name("runtime_workloads.py")),
                        "--prepared", str(getattr(args, label)), "--output", str(directory),
                        "--sizes", args.sizes, "--iterations", str(args.iterations),
                        "--warmup", str(args.warmup), "--repeats", "1"], check=True)
        captures[label].append(directory / "manifest.json")
    baseline.write_json(output / "comparison.json", compare(captures))


if __name__ == "__main__":
    main()
