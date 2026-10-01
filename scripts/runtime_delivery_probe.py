#!/usr/bin/env python3
"""Compare frozen two-producer controls that verify every ordered delivery."""

import argparse
import json
import os
import statistics
from pathlib import Path

import runtime_baseline as baseline


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--before", type=Path, required=True)
    parser.add_argument("--after", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--cpus", required=True, help="CPU affinity shared by both variants")
    parser.add_argument("--rounds", type=int, default=6)
    args = parser.parse_args()
    cpus = {int(value) for value in args.cpus.split(",")}
    if not cpus or not cpus.issubset(os.sched_getaffinity(0)) or args.rounds < 1:
        raise ValueError("Select available CPUs and positive rounds")
    binaries = {label: getattr(args, label).resolve() for label in ("before", "after")}
    hashes = {label: baseline.digest(path) for label, path in binaries.items()}
    output = args.output.resolve()
    output.mkdir(parents=True, exist_ok=False)
    env = os.environ.copy()
    env["HYPHAE_WORKER_THREADS"] = "4"
    manifest = {
        "schema": 1, "complete": False, "host": baseline.host_identity(),
        "binaries": {label: {"path": str(path), "sha256": hashes[label]} for label, path in binaries.items()},
        "environment": {key: env.get(key) for key in ("RAYON_NUM_THREADS", "HYPHAE_WORKER_THREADS", "HYPHAE_WAVE_THREADS", "HYPHAE_WAVE_THRESHOLD")},
        "protocol": {"order": "ABBA", "rounds": args.rounds, "affinity": sorted(cpus), "threads": 2, "updates_per_thread": 20000},
        "runs": [], "descriptive_only": True,
        "interpretation": "Ordered no_coalesce synthetic control; no causal claim about coalescing-mode timings or production latency.",
    }
    for round_index in range(args.rounds):
        for position, label in enumerate(("before", "after", "after", "before"), 1):
            if baseline.digest(binaries[label]) != hashes[label]:
                raise ValueError("Frozen control executable changed")
            run_dir = output / f"round-{round_index + 1}-{position}-{label}"
            run_dir.mkdir()
            command = ["taskset", "-c", args.cpus, str(binaries[label]), "20000"]
            row = {"label": label, "command": command, "before": baseline.host_snapshot()}
            with (run_dir / "stdout.jsonl").open("w") as stdout, (run_dir / "stderr.txt").open("w") as stderr:
                code, resources, samples, timeout = baseline.run_measured(command, env, stdout, stderr, 120, 0.25)
            row.update(returncode=code, resources=resources, host_samples=samples, timed_out=timeout, after=baseline.host_snapshot())
            manifest["runs"].append(row)
            baseline.write_json(output / "manifest.json", manifest)
            if code != 0 or timeout:
                raise RuntimeError(f"Control failed; see {run_dir}")
            result = json.loads((run_dir / "stdout.jsonl").read_text())
            if (result["ops_per_thread"], result["delivered"], result["total_delivered"], result["order_failures"]) != (20000, [20000, 20000], 40000, [0, 0]):
                raise ValueError("Control delivered work differs from protocol")
            if type(result["elapsed_ns"]) is not int or result["elapsed_ns"] <= 0:
                raise ValueError("Control elapsed_ns must be a positive integer")
            row.update(result=result, raw=str((run_dir / "stdout.jsonl").relative_to(output)), raw_sha256=baseline.digest(run_dir / "stdout.jsonl"))
            baseline.write_json(output / "manifest.json", manifest)
    medians = {label: statistics.median(row["result"]["elapsed_ns"] for row in manifest["runs"] if row["label"] == label) for label in binaries}
    manifest.update(complete=True, median_ns=medians, descriptive_change_percent=100 * (medians["after"] / medians["before"] - 1))
    baseline.write_json(output / "manifest.json", manifest)
    print(json.dumps({key: manifest[key] for key in ("median_ns", "descriptive_change_percent")}, indent=2))


if __name__ == "__main__":
    main()
