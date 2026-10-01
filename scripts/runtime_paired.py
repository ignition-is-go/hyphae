#!/usr/bin/env python3
"""Measure frozen builds in ABBA order with the existing capture protocol."""

import argparse
import json
from pathlib import Path
from types import SimpleNamespace

import runtime_baseline as baseline


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--before", type=Path, required=True)
    parser.add_argument("--after", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--workers", type=int, default=4)
    parser.add_argument("--benches", default=",".join(baseline.SUITE))
    parser.add_argument("--filter")
    args = parser.parse_args()
    output = args.output.resolve()
    output.mkdir(parents=True, exist_ok=False)
    captures = {"before": [], "after": []}
    for position, label in enumerate(("before", "after", "after", "before"), 1):
        directory = output / f"{position}-{label}"
        baseline.measure(SimpleNamespace(
            prepared=getattr(args, label), output=directory, label=label,
            workers=args.workers, benches=args.benches, filter=args.filter,
            repeats=1, timeout=600, host_sample_interval=0.25,
        ))
        captures[label].append(directory / "manifest.json")
    for label, paths in captures.items():
        first, second = (json.loads(path.read_text()) for path in paths)
        for key in ("protocol", "host", "environment", "prepared_manifest_sha256"):
            if first[key] != second[key]:
                raise ValueError(f"{label} repetitions differ in {key}")
        first["protocol"]["repeats"] = 2
        for repeat, (manifest, path) in enumerate(zip((first, second), paths), 1):
            for run in manifest["runs"]:
                directory = path.parent / f"run-{run['repeat']}" / run["benchmark"]
                for estimate in run["estimates"]:
                    estimate["raw"] = str((directory / estimate["raw"]).relative_to(output))
                run["capture_manifest"] = str(path.relative_to(output))
                run["repeat"] = repeat
        first["runs"].extend(second["runs"])
        first["after"] = second["after"]
        first["capture_manifests"] = [
            {"path": str(path.relative_to(output)), "sha256": baseline.digest(path)}
            for path in paths
        ]
        baseline.write_json(output / f"{label}.json", first)
    baseline.compare(SimpleNamespace(before=output / "before.json", after=output / "after.json"))


if __name__ == "__main__":
    main()
