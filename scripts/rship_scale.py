#!/usr/bin/env python3
"""Run the rship-scale suite, store results, and compare with the baseline.

  scripts/rship_scale.py run                      # 5 runs of every scenario at scale 1
  scripts/rship_scale.py compare RESULTS.jsonl    # exit 1 on a regression
  scripts/rship_scale.py targets RESULTS.jsonl    # the absolute level-1 gates
  scripts/rship_scale.py baseline RESULTS.jsonl   # rewrite the checked-in baseline

See docs/benchmarks/rship-scale.md.
"""
import argparse
import fcntl
import json
import os
import statistics
import subprocess
import sys
import time
from collections import defaultdict
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
BASELINE = ROOT / "docs" / "benchmarks" / "rship-scale-baseline.json"
SCENARIOS = (
    "tick probe shared pulse burst edit churn setup overload fanout hotcone "
    "feedback writers diamond deep demand oracle"
).split()

# What a regression is, per kind of metric. A candidate regresses when its
# median over the runs is worse than the baseline median by more than the
# tolerance, and its best run is worse than the baseline's worst run. The
# tolerance is the larger of a floor and the baseline's own run-to-run range
# relative to its median, so a metric that was noisy when the baseline was
# taken is not flagged for being noisy again. docs/benchmarks/rship-scale.md
# has the measured ranges the floors come from.
FLOOR = {"p50": 0.25, "p99": 0.50, "ratio": 0.25, "time": 0.25, "higher": 0.25}

# (scenario, metric, kind, level-1 target or None). Latencies are
# microseconds. A target that grows with scale is given per unit of scale
# and marked "x". `higher` metrics are better when larger.
TRACKED = [
    ("tick", "tick.p50", "p50", None),
    ("tick", "tick.p99", "p99", "x3300"),
    ("tick", "setup.ms", "time", "x2000"),
    ("tick", "setup.rss_mb", "time", "x1000"),
    ("probe", "probe.idle.p50", "p50", None),
    ("probe", "probe.load.p50", "p50", None),
    ("probe", "probe.load.p99", "p99", "250"),
    ("shared", "shared.load.p99", "p99", "1000"),
    ("pulse", "pulse.p99", "p99", "250"),
    ("burst", "tick.unread.p99", "p99", "x3300"),
    ("edit", "edit.add.p99", "p99", "1000"),
    ("edit", "tick.p99", "p99", "x3300"),
    ("churn", "tick.p99", "p99", "x3300"),
    ("churn", "probe.load.p99", "p99", "250"),
    ("churn", "churn.value.p50", "p50", None),
    ("churn", "churn.key.p50", "p50", None),
    ("churn", "churn.key_over_value", "ratio", "3"),
    ("churn", "churn.request.p99", "p99", "1000"),
    ("setup", "setup.loaded_ms", "time", None),
    ("setup", "tick.build.p99", "p99", "x3300"),
    ("setup", "probe.build.p99", "p99", "250"),
    ("overload", "probe.over.p99", "p99", "250"),
    ("overload", "tick.over.p99", "p99", "x3300"),
    ("overload", "tick.after.p99", "p99", "x3300"),
    ("overload", "over.drain_ms", "p99", "100"),
    ("overload", "over.writes_per_s", "higher", None),
    ("fanout", "fan.p50", "p50", None),
    ("fanout", "fan.p99", "p99", "x3300"),
    ("hotcone", "hot.idle.p50", "p50", None),
    ("hotcone", "hot.idle.p99", "p99", "250"),
    ("hotcone", "hot.load.p99", "p99", "250"),
    ("hotcone", "hot.evals_per_write", "ratio", "2.05"),
    ("feedback", "fb.hop_ns", "p50", None),
    ("feedback", "fb.trip.p99", "p99", "x33333"),
    ("feedback", "probe.load.p99", "p99", "250"),
    ("writers", "wr.solo.p50", "p50", None),
    ("writers", "wr.all.p99", "p99", "250"),
    ("writers", "wr.all_over_solo_p99", "ratio", "2"),
    ("writers", "wr.thr1_per_s", "higher", None),
    ("writers", "wr.thr_all_per_s", "higher", None),
    ("diamond", "dia.p50", "p50", None),
    ("diamond", "dia.p99", "p99", "250"),
    ("diamond", "probe.four.p99", "p99", "250"),
    ("diamond", "dia.four_writes_per_s", "higher", None),
    ("deep", "deep.one.p50", "p50", None),
    ("deep", "deep.one.p99", "p99", "250"),
    ("deep", "deep.group.p99", "p99", "1056"),
    ("deep", "deep.200_over_100", "ratio", "2.5"),
    ("demand", "demand.rss_growth", "ratio", "0.05"),
    ("demand", "demand.zero_write_us.last", "p50", None),
    ("oracle", "probe.load.p99", "p99", None),
]


def git_commit():
    head = subprocess.run(
        ["git", "rev-parse", "--short=12", "HEAD"], cwd=ROOT, capture_output=True, text=True
    ).stdout.strip()
    dirty = subprocess.run(
        ["git", "status", "--porcelain", "--untracked-files=no"], cwd=ROOT, capture_output=True, text=True
    ).stdout.strip()
    return (head or "unknown") + ("-dirty" if dirty else "")


def load(paths):
    """(scenario, scale) -> list of metric dicts, None for a run that died."""
    points, commits = defaultdict(list), set()
    for path in paths:
        for line in Path(path).read_text().splitlines():
            row = json.loads(line)
            points[(row["scen"], row["scale"])].append(row.get("metrics"))
            commits.add(row.get("commit", "unknown"))
    return points, commits


def summarize(runs):
    """metric -> {median, min, max, n} over the runs that finished."""
    values = defaultdict(list)
    for metrics in runs:
        for name, value in (metrics or {}).items():
            if value is not None:
                values[name].append(value)
    return {
        name: {"median": statistics.median(v), "min": min(v), "max": max(v), "n": len(v)}
        for name, v in values.items()
    }


def cmd_run(args):
    out = Path(args.out or ROOT / "benchmark-results" / "rship-scale" / f"{git_commit()}.jsonl")
    out.parent.mkdir(parents=True, exist_ok=True)
    subprocess.run(
        ["cargo", "bench", "-p", "hyphae", "--features", "scale-bench", "--bench", "rship_scale", "--no-run"],
        cwd=ROOT, check=True,
    )
    lock = open(args.lock, "a") if args.lock else None
    env = dict(os.environ, HYPHAE_BENCH_COMMIT=git_commit())
    for scen in args.scenarios or SCENARIOS:
        for scale in args.scales:
            for _ in range(args.runs):
                # The lock is held for one run, so other benchmarks on the
                # machine alternate with this one run by run.
                if lock:
                    fcntl.flock(lock, fcntl.LOCK_EX)
                command = ["cargo", "bench", "-q", "-p", "hyphae", "--features", "scale-bench",
                           "--bench", "rship_scale", "--", "--scen", scen, "--scale", str(scale)]
                try:
                    done = subprocess.run(command, cwd=ROOT, env=env, capture_output=True, text=True, timeout=args.timeout)
                    line = next((l for l in reversed(done.stdout.splitlines()) if l.startswith("{")), None)
                    failure = f"exit {done.returncode}"
                except subprocess.TimeoutExpired:
                    line, failure = None, f"no result in {args.timeout} s"
                if line is None:
                    line = json.dumps({"suite": "rship_scale", "commit": env["HYPHAE_BENCH_COMMIT"],
                                       "scen": scen, "scale": scale, "failed": failure})
                with out.open("a") as handle:
                    handle.write(line + "\n")
                if lock:
                    fcntl.flock(lock, fcntl.LOCK_UN)
                print(f"{scen} x{scale}: {'ok' if 'failed' not in json.loads(line) else failure}", flush=True)
    print(f"results: {out}")


def cmd_baseline(args):
    points, commits = load(args.results)
    baseline = {
        "suite": "rship_scale",
        "commits": sorted(commits),
        "taken": time.strftime("%Y-%m-%d"),
        "machine": args.machine,
        "points": {
            f"{scen}@{scale}": {"runs": len(runs), "died": sum(1 for r in runs if r is None), "metrics": summarize(runs)}
            for (scen, scale), runs in sorted(points.items())
        },
    }
    BASELINE.write_text(json.dumps(baseline, indent=1, sort_keys=True) + "\n")
    print(f"wrote {BASELINE.relative_to(ROOT)} from {sum(len(r) for r in points.values())} runs")


def tolerance(kind, base):
    spread = (base["max"] - base["min"]) / abs(base["median"]) if base["median"] else 0.0
    return max(FLOOR[kind], spread)


def cmd_compare(args):
    baseline = json.loads(Path(args.baseline).read_text())
    points, commits = load(args.results)
    regressions, improvements, checked = [], [], 0
    for (scen, scale), runs in sorted(points.items()):
        base_point = baseline["points"].get(f"{scen}@{scale}")
        if base_point is None:
            continue
        died = sum(1 for r in runs if r is None)
        if died > base_point["died"]:
            regressions.append(f"{scen}@{scale}: {died} of {len(runs)} runs died (baseline {base_point['died']})")
        if len(runs) - died < args.min_runs:
            regressions.append(f"{scen}@{scale}: {len(runs) - died} finished runs, need {args.min_runs}")
            continue
        now = summarize(runs)
        # Correctness: a count that is 0 in the baseline must stay 0 in every run.
        for name, stats in now.items():
            if name.startswith("viol.") and stats["max"] > 0:
                was = base_point["metrics"].get(name, {"max": 0})["max"]
                if was == 0:
                    regressions.append(f"{scen}@{scale} {name}: {stats['max']:.0f}, baseline 0")
        for t_scen, metric, kind, _ in TRACKED:
            if t_scen != scen or metric not in now or metric not in base_point["metrics"]:
                continue
            base, cand = base_point["metrics"][metric], now[metric]["median"]
            checked += 1
            if not base["median"]:
                continue
            tol = tolerance(kind, base)
            change = cand / base["median"] - 1.0
            worse = -change if kind == "higher" else change
            text = (f"{scen}@{scale} {metric}: {base['median']:.4g} -> {cand:.4g} "
                    f"({change:+.0%}, tolerance {tol:.0%}, n={now[metric]['n']})")
            low, high = now[metric]["min"], now[metric]["max"]
            if kind == "higher":
                apart_worse, apart_better = high < base["min"], low > base["max"]
            else:
                apart_worse, apart_better = low > base["max"], high < base["min"]
            if worse > tol and apart_worse:
                regressions.append(text)
            elif worse < -tol and apart_better:
                improvements.append(text)
    print(f"baseline {', '.join(baseline['commits'])}; candidate {', '.join(sorted(commits))}; {checked} metrics compared")
    for title, rows in (("REGRESSIONS", regressions), ("improvements beyond tolerance", improvements)):
        print(f"\n{title}: {len(rows)}")
        for row in rows:
            print(f"  {row}")
    sys.exit(1 if regressions else 0)


def cmd_targets(args):
    points, _ = load(args.results)
    failed = 0
    print("| scenario@scale | quantity | target | median [min..max] | verdict |")
    print("| --- | --- | --- | --- | --- |")
    for (scen, scale), runs in sorted(points.items()):
        now = summarize(runs)
        for t_scen, metric, kind, target in TRACKED:
            if t_scen != scen or target is None or metric not in now:
                continue
            limit = float(target[1:]) * scale if target.startswith("x") else float(target)
            stats = now[metric]
            # Section 9.1: a gate passes only if every run passes.
            ok = stats["max"] <= limit and all(r is not None for r in runs)
            failed += not ok
            print(f"| {scen}@{scale} | {metric} | <= {limit:g} | {stats['median']:.4g} [{stats['min']:.4g}..{stats['max']:.4g}] | {'PASS' if ok else 'FAIL'} |")
        bad = sorted(name for name, stats in now.items() if name.startswith("viol.") and stats["max"] > 0)
        died = sum(1 for r in runs if r is None)
        ok = not bad and not died
        failed += not ok
        print(f"| {scen}@{scale} | correctness counts | all 0 | {', '.join(bad) or 'all 0'}{f', {died} runs died' if died else ''} | {'PASS' if ok else 'FAIL'} |")
    print(f"\n{failed} targets not met")


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = parser.add_subparsers(dest="command", required=True)
    run = sub.add_parser("run", help="run the bench and append one JSON line per run")
    run.add_argument("--runs", type=int, default=5)
    run.add_argument("--scales", type=int, nargs="+", default=[1])
    run.add_argument("--scenarios", nargs="+")
    run.add_argument("--out")
    run.add_argument("--timeout", type=int, default=300, help="seconds per run")
    run.add_argument("--lock", default=os.environ.get("HYPHAE_BENCH_LOCK"),
                     help="a file to flock around each block of runs, shared with other benchmarks on the machine")
    run.set_defaults(func=cmd_run)
    for name, func, text in (("compare", cmd_compare, "flag regressions against the baseline"),
                             ("targets", cmd_targets, "report the absolute level-1 gates"),
                             ("baseline", cmd_baseline, "rewrite the baseline from these results")):
        cmd = sub.add_parser(name, help=text)
        cmd.add_argument("results", nargs="+")
        cmd.set_defaults(func=func)
        if name == "compare":
            cmd.add_argument("--baseline", default=str(BASELINE))
            cmd.add_argument("--min-runs", type=int, default=5)
        if name == "baseline":
            cmd.add_argument("--machine", default=f"{os.uname().nodename}, {os.cpu_count()} cores")
    args = parser.parse_args()
    args.func(args)


if __name__ == "__main__":
    main()
