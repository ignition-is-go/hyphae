#!/usr/bin/env python3
"""Run the `emission` bench several times and compare result files.

    tools/emission-bench.py run [--runs 5] [--cpus 6-11] [--out FILE] [filter...]
    tools/emission-bench.py compare BEFORE AFTER

`run` builds `hyphae/benches/emission.rs` in release, runs it `--runs` times
pinned with `taskset` (separate processes, so process-level noise is in the
spread) and writes one JSON file. Per case it records the median allocations,
work and errors, and the median of the per-run median ns/op with the min and
max of those run medians. `noise_pct` is (max - min) / median of the run
medians.

`compare` prints per-case deltas. A time delta is flagged when it exceeds the
larger noise_pct of the two files; an allocation delta when it exceeds the
larger allocation spread plus 0.01. A work change beyond the larger work spread
(concurrent writers coalesce, so their work varies) and any nonzero error count
are always flagged, because they mean the case did different work.
"""
import argparse
import json
import os
import statistics
import subprocess
import sys
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]


def git(*args):
    return subprocess.check_output(["git", *args], cwd=ROOT, text=True).strip()


def run(args):
    build = ["cargo", "bench", "-q", "-p", "hyphae", "--bench", "emission", "--no-run",
             "--message-format=json"]
    out = subprocess.check_output(build, cwd=ROOT, text=True, stderr=subprocess.DEVNULL)
    exe = None
    for line in out.splitlines():
        msg = json.loads(line)
        if msg.get("reason") == "compiler-artifact" and msg["target"]["name"] == "emission" \
                and msg.get("executable"):
            exe = msg["executable"]
    if not exe:
        sys.exit("could not locate the emission bench binary")
    load = Path("/proc/loadavg").read_text().split()[:3]
    started = time.time()
    runs = []
    for index in range(args.runs):
        cmd = ["taskset", "-c", args.cpus, exe, *args.filter] if args.cpus else [exe, *args.filter]
        text = subprocess.check_output(cmd, cwd=ROOT / "hyphae", text=True)
        runs.append([json.loads(line) for line in text.splitlines() if line.startswith("{")])
        print(f"run {index + 1}/{args.runs} done", file=sys.stderr)
    cases = {}
    for case in runs[0]:
        name = case["case"]
        rows = [next(r for r in rs if r["case"] == name) for rs in runs]
        medians = [statistics.median(r["ns_per_op"]) for r in rows]
        allocs = [r["allocs_per_op"] for r in rows]
        mid = statistics.median(medians)
        cases[name] = {
            "covers": case["covers"],
            "allocs_per_op": statistics.median(allocs),
            "allocs_spread": max(allocs) - min(allocs),
            "bytes_per_op": statistics.median(r["bytes_per_op"] for r in rows),
            "work_per_op": statistics.median(r["work_per_op"] for r in rows),
            "work_spread": max(r["work_per_op"] for r in rows) - min(r["work_per_op"] for r in rows),
            "errors": sum(r["errors"] for r in rows),
            "ns_per_op": round(mid, 2),
            "ns_min": round(min(medians), 2),
            "ns_max": round(max(medians), 2),
            "noise_pct": round(100 * (max(medians) - min(medians)) / mid, 2),
            "run_medians": [round(m, 2) for m in medians],
        }
    result = {
        "commit": git("rev-parse", "HEAD"),
        "dirty": bool(git("status", "--porcelain", "--", "hyphae")),
        "rustc": subprocess.check_output(["rustc", "--version"], cwd=ROOT, text=True).strip(),
        "cpus": args.cpus,
        "runs": args.runs,
        "loadavg_before": load,
        "loadavg_after": Path("/proc/loadavg").read_text().split()[:3],
        "seconds": round(time.time() - started, 1),
        "cases": cases,
    }
    out_path = Path(args.out) if args.out else (
        ROOT / "benchmark-results" / "emission" / f"{time.strftime('%Y%m%dT%H%M%S')}.json")
    out_path.parent.mkdir(parents=True, exist_ok=True)
    out_path.write_text(json.dumps(result, indent=2) + "\n")
    table(result)
    print(f"wrote {out_path}", file=sys.stderr)


def table(result):
    print(f"{'case':28} {'allocs/op':>10} {'ns/op':>10} {'min..max':>19} {'noise%':>7} "
          f"{'work/op':>9} {'err':>4}")
    for name, c in result["cases"].items():
        print(f"{name:28} {c['allocs_per_op']:10.2f} {c['ns_per_op']:10.1f} "
              f"{c['ns_min']:9.1f}..{c['ns_max']:<9.1f} {c['noise_pct']:7.1f} "
              f"{c['work_per_op']:9.4f} {c['errors']:4}")


def compare(args):
    before = json.loads(Path(args.before).read_text())["cases"]
    after = json.loads(Path(args.after).read_text())["cases"]
    print(f"{'case':28} {'allocs b->a':>17} {'d':>8} {'ns b->a':>19} {'d%':>7} {'noise%':>7}  flags")
    for name, b in before.items():
        a = after.get(name)
        if a is None:
            print(f"{name:28} missing in after")
            continue
        flags = []
        d_alloc = a["allocs_per_op"] - b["allocs_per_op"]
        if abs(d_alloc) > max(a["allocs_spread"], b["allocs_spread"]) + 0.01:
            flags.append("ALLOCS")
        d_pct = 100 * (a["ns_per_op"] - b["ns_per_op"]) / b["ns_per_op"]
        noise = max(a["noise_pct"], b["noise_pct"])
        if abs(d_pct) > noise:
            flags.append("TIME")
        work_noise = max(a.get("work_spread", 0), b.get("work_spread", 0), 1e-3)
        if abs(a["work_per_op"] - b["work_per_op"]) > work_noise:
            flags.append(f"WORK {b['work_per_op']:.4g}->{a['work_per_op']:.4g}")
        if a["errors"] or b["errors"]:
            flags.append(f"ERRORS {b['errors']}->{a['errors']}")
        print(f"{name:28} {b['allocs_per_op']:8.2f}->{a['allocs_per_op']:<8.2f} {d_alloc:+8.2f} "
              f"{b['ns_per_op']:9.1f}->{a['ns_per_op']:<9.1f} {d_pct:+7.1f} {noise:7.1f}  "
              f"{' '.join(flags)}")


def main():
    parser = argparse.ArgumentParser(description=__doc__,
                                     formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = parser.add_subparsers(dest="cmd", required=True)
    r = sub.add_parser("run")
    r.add_argument("--runs", type=int, default=5)
    r.add_argument("--cpus", default=os.environ.get("EMISSION_CPUS", "6-11"),
                   help="taskset CPU list; empty string disables pinning")
    r.add_argument("--out")
    r.add_argument("filter", nargs="*")
    c = sub.add_parser("compare")
    c.add_argument("before")
    c.add_argument("after")
    args = parser.parse_args()
    run(args) if args.cmd == "run" else compare(args)


if __name__ == "__main__":
    main()
