#!/usr/bin/env python3
"""Capture assertion-backed lifecycle workloads from a frozen baseline build."""

import argparse
import json
import os
from pathlib import Path

import runtime_baseline as baseline


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--prepared", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--sizes", default="16,128")
    parser.add_argument("--iterations", type=int, default=1000)
    parser.add_argument("--warmup", type=int, default=50)
    parser.add_argument("--repeats", type=int, default=2)
    args = parser.parse_args()
    prepared_path = args.prepared.resolve()
    prepared = json.loads(prepared_path.read_text())
    if not prepared.get("complete") or prepared.get("schema") != 2:
        raise ValueError("A complete schema-2 prepared build is required")
    record = prepared["binaries"]["runtime_baseline"]
    binary = prepared_path.parent / record["path"]
    if baseline.digest(binary) != record["sha256"]:
        raise ValueError("Frozen workload binary changed")
    sizes = [int(size) for size in args.sizes.split(",")]
    if not sizes or min(sizes) < 1 or args.iterations < 1 or args.warmup < 0 or args.repeats < 1:
        raise ValueError("Sizes, iterations, and repeats must be positive; warmup must be nonnegative")
    output = args.output.resolve()
    output.mkdir(parents=True, exist_ok=False)
    manifest = {
        "schema": 1,
        "complete": False,
        "prepared": prepared,
        "prepared_sha256": baseline.digest(prepared_path),
        "host": baseline.host_identity(),
        "protocol": {"sizes": sizes, "iterations": args.iterations, "warmup": args.warmup, "repeats": args.repeats, "workers": 4},
        "runs": [],
        "interpretation": "Empirical phase latencies of repeatedly constructed synthetic graphs, not production end-to-end tails.",
    }
    env = os.environ.copy()
    env["HYPHAE_WORKER_THREADS"] = "4"
    for repeat in range(args.repeats):
        for size in sizes:
            run_dir = output / f"run-{repeat + 1}-size-{size}"
            run_dir.mkdir()
            invocation = [str(binary), "--workload", "all", "--size", str(size), "--iterations", str(args.iterations), "--warmup", str(args.warmup)]
            print(f"Workloads size={size}, repeat={repeat + 1}", flush=True)
            entry = {"command": invocation, "before": baseline.host_snapshot()}
            manifest["runs"].append(entry)
            with (run_dir / "samples.jsonl").open("w") as stdout, (run_dir / "stderr.txt").open("w") as stderr:
                returncode, resources, host_samples, timed_out = baseline.run_measured(invocation, env, stdout, stderr, 600, 0.25)
            entry.update(returncode=returncode, resources=resources, host_samples=host_samples, timed_out=timed_out)
            entry["after"] = baseline.host_snapshot()
            baseline.write_json(output / "manifest.json", manifest)
            if returncode != 0 or timed_out:
                raise RuntimeError(f"Workload failed; see {run_dir}")
            records = [json.loads(line) for line in (run_dir / "samples.jsonl").read_text().splitlines()]
            expected = {(name, phase) for name in ("lifecycle", "deep_chain", "wide_diamond", "event_no_coalesce", "source_fanout", "project_cell_churn") for phase in ("setup", "operation", "teardown")}
            actual = {(row["workload"], row["phase"]) for row in records}
            if actual != expected or len(records) != len(expected):
                raise RuntimeError("Missing or duplicate workload phase")
            for row in records:
                samples = row["samples"]
                if row["size"] != size or row["iterations"] != args.iterations or row["unit"] != "ns":
                    raise RuntimeError("Workload metadata differs from protocol")
                if len(samples) != args.iterations or any(type(value) is not int or value < 0 for value in samples):
                    raise RuntimeError("Invalid workload samples")
                ordered = sorted(samples)
                for quantile in (50, 95, 99):
                    if row[f"p{quantile}"] != ordered[(len(ordered) - 1) * quantile // 100]:
                        raise RuntimeError("Reported percentile differs from raw samples")
            entry["results"] = [{key: value for key, value in row.items() if key != "samples"} for row in records]
            entry["raw_sha256"] = baseline.digest(run_dir / "samples.jsonl")
            baseline.write_json(output / "manifest.json", manifest)
    manifest["complete"] = True
    baseline.write_json(output / "manifest.json", manifest)


if __name__ == "__main__":
    main()
