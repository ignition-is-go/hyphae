#!/usr/bin/env python3
"""Prepare, run, and descriptively compare provenance-bound benchmarks."""
import argparse
import hashlib
import json
import os
import platform
import shutil
import signal
import statistics
import subprocess
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
SUITE = ("scheduler", "subscriber_registry", "scheduler_contention", "compiled_map_queries")
CRITERION_ARGS = (
    "--bench", "--noplot", "--sample-size", "30",
    "--warm-up-time", "1", "--measurement-time", "2",
)

def command(args):
    return subprocess.check_output(args, cwd=ROOT, text=True).strip()

def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()

def write_json(path, value):
    path.write_text(json.dumps(value, indent=2, sort_keys=True) + "\n")

def source_manifest():
    scopes = (
        "hyphae", "Cargo.toml", "Cargo.lock", ".cargo",
        "rust-toolchain", "rust-toolchain.toml",
    )
    names = command(["git", "ls-files", "--cached", "--others", "--exclude-standard", "--", *scopes]).splitlines()
    return {name: digest(ROOT / name) for name in names if (ROOT / name).is_file()}

def host_identity():
    cpu = "\n".join(line for line in command(["lscpu"]).splitlines() if "scaling MHz" not in line)
    return {
        "uname": platform.uname()._asdict(),
        "cpu": cpu,
        "affinity": sorted(os.sched_getaffinity(0)),
        "governors": {
            str(path): path.read_text().strip()
            for path in Path("/sys/devices/system/cpu").glob("cpu*/cpufreq/scaling_governor")
        },
    }

def host_snapshot(full=True):
    paths = [
        "/proc/stat", "/proc/meminfo", "/proc/loadavg",
        "/proc/pressure/cpu", "/proc/pressure/memory", "/proc/pressure/io",
    ]
    value = {
        "time_ns": time.time_ns(),
        "proc": {name: Path(name).read_text() for name in paths if Path(name).exists()},
    }
    if full:
        value["processes"] = command(["ps", "-eo", "pid,comm,pcpu,pmem", "--sort=-pcpu"])
    return value

def read_binaries(build_log):
    binaries = {}
    for line in build_log.read_text().splitlines():
        try:
            event = json.loads(line)
        except json.JSONDecodeError:
            continue
        if event.get("reason") == "compiler-artifact" and event.get("executable") and "bench" in event["target"]["kind"]:
            binaries[event["target"]["name"]] = Path(event["executable"])
    return binaries

def prepare(args):
    output = args.output.resolve()
    output.mkdir(parents=True, exist_ok=False)
    selected = [name for name in args.benches.split(",") if name]
    if not selected or len(selected) != len(set(selected)):
        raise ValueError("Select at least one benchmark, without duplicates")
    before = {
        "revision": command(["git", "rev-parse", "HEAD"]),
        "status": command(["git", "status", "--short"]),
        "source_sha256": source_manifest(),
    }
    build_args = [
        "cargo", "bench", "-p", "hyphae", "--features", args.features,
        "--no-run", "--message-format=json-render-diagnostics",
    ]
    for name in selected:
        build_args.extend(("--bench", name))
    build_log, build_stderr = output / "build.jsonl", output / "build.stderr.txt"
    manifest = {
        "schema": 2,
        "kind": "prepared-benchmarks",
        "complete": False,
        "before": before,
        "build_args": build_args,
        "build_environment": {key: os.environ.get(key) for key in ("RUSTFLAGS", "CARGO_TARGET_DIR")},
        "toolchain": {"rustc": command(["rustc", "-Vv"]), "cargo": command(["cargo", "-V"])},
        "benches": selected,
    }
    manifest_path = output / "manifest.json"
    write_json(manifest_path, manifest)
    with build_log.open("w") as stdout, build_stderr.open("w") as stderr:
        result = subprocess.run(build_args, cwd=ROOT, env=os.environ.copy(), stdout=stdout, stderr=stderr)
    manifest["build_returncode"] = result.returncode
    after = {
        "revision": command(["git", "rev-parse", "HEAD"]),
        "status": command(["git", "status", "--short"]),
        "source_sha256": source_manifest(),
    }
    manifest["after"] = after
    if result.returncode:
        write_json(manifest_path, manifest)
        raise RuntimeError(f"Build failed; inspect {build_stderr}")
    if before != after:
        write_json(manifest_path, manifest)
        raise RuntimeError("Source state changed during the build")
    binaries = read_binaries(build_log)
    frozen = {}
    (output / "bin").mkdir()
    for name in selected:
        binary = binaries.get(name)
        if not binary or not binary.is_file():
            raise ValueError(f"Missing built benchmark: {name}")
        target = output / "bin" / name
        shutil.copy2(binary, target)
        frozen[name] = {
            "path": str(target.relative_to(output)),
            "sha256": digest(target),
            "benchmark_source_sha256": digest(ROOT / "hyphae" / "benches" / f"{name}.rs"),
        }
    manifest.update(build_log_sha256=digest(build_log), binaries=frozen, complete=True)
    write_json(manifest_path, manifest)
    print(f"Saved {manifest_path}", flush=True)

def summarize(directory):
    result = []
    for path in sorted(directory.glob("criterion/**/new/estimates.json")):
        estimates = json.loads(path.read_text())
        metadata = json.loads((path.parent / "benchmark.json").read_text())
        result.append({
            "benchmark": metadata["full_id"],
            "mean_ns": estimates["mean"]["point_estimate"],
            "median_ns": estimates["median"]["point_estimate"],
            "mean_ci_ns": estimates["mean"]["confidence_interval"],
            "raw": str(path.relative_to(directory)),
        })
    return result

def usage_dict(usage):
    return {
        "user_seconds": usage.ru_utime,
        "system_seconds": usage.ru_stime,
        "max_rss_kib": usage.ru_maxrss,
        "minor_faults": usage.ru_minflt,
        "major_faults": usage.ru_majflt,
        "voluntary_switches": usage.ru_nvcsw,
        "involuntary_switches": usage.ru_nivcsw,
    }

def run_measured(invocation, env, stdout, stderr, timeout, sample_interval):
    process = subprocess.Popen(invocation, cwd=ROOT, env=env, stdout=stdout, stderr=stderr, start_new_session=True)
    deadline, next_sample, samples = time.monotonic() + timeout, time.monotonic(), []
    while True:
        pid, status, usage = os.wait4(process.pid, os.WNOHANG)
        if pid:
            process.returncode = os.waitstatus_to_exitcode(status)
            return process.returncode, usage_dict(usage), samples, False
        now = time.monotonic()
        if sample_interval and now >= next_sample:
            samples.append(host_snapshot(full=False))
            next_sample = now + sample_interval
        if now >= deadline:
            os.killpg(process.pid, signal.SIGKILL)
            _, status, usage = os.wait4(process.pid, 0)
            process.returncode = os.waitstatus_to_exitcode(status)
            return process.returncode, usage_dict(usage), samples, True
        time.sleep(0.05)

def measure(args):
    if args.repeats < 1 or args.timeout < 1 or args.workers < 0 or args.host_sample_interval < 0:
        raise ValueError("Invalid repetition, timeout, worker, or sampling parameters")
    prepared_path = args.prepared.resolve()
    prepared = json.loads(prepared_path.read_text())
    if prepared.get("schema") != 2 or prepared.get("kind") != "prepared-benchmarks" or not prepared.get("complete"):
        raise ValueError("Prepared manifest is incomplete or unsupported")
    current_toolchain = {"rustc": command(["rustc", "-Vv"]), "cargo": command(["cargo", "-V"])}
    if prepared["toolchain"] != current_toolchain:
        raise ValueError("Current toolchain differs from the prepared build")
    selected = [name for name in args.benches.split(",") if name]
    if not selected or len(selected) != len(set(selected)):
        raise ValueError("Select at least one benchmark, without duplicates")
    binaries = {}
    for name in selected:
        record = prepared["binaries"].get(name)
        binary = prepared_path.parent / record["path"] if record else None
        if not binary or not binary.is_file() or digest(binary) != record["sha256"]:
            raise ValueError(f"Prepared benchmark is missing or changed: {name}")
        binaries[name] = binary
    output = args.output.resolve()
    output.mkdir(parents=True, exist_ok=False)
    protocol = {
        "benches": selected,
        "filter": args.filter,
        "worker_threads": args.workers,
        "repeats": args.repeats,
        "timeout_seconds": args.timeout,
        "criterion_args": list(CRITERION_ARGS),
        "host_sample_interval_seconds": args.host_sample_interval,
    }
    manifest = {
        "schema": 2,
        "kind": "benchmark-measurement",
        "complete": False,
        "label": args.label,
        "protocol": protocol,
        "prepared_manifest_sha256": digest(prepared_path),
        "prepared": prepared,
        "host": host_identity(),
        "environment": {
            key: os.environ.get(key)
            for key in ("RAYON_NUM_THREADS", "HYPHAE_WORKER_THREADS", "HYPHAE_WAVE_THREADS")
        },
        "before": host_snapshot(),
        "runs": [],
    }
    manifest_path = output / "manifest.json"
    write_json(manifest_path, manifest)
    for repeat in range(args.repeats):
        for name in selected:
            run = output / f"run-{repeat + 1}" / name
            run.mkdir(parents=True)
            env = os.environ.copy()
            env["HYPHAE_WORKER_THREADS"] = str(args.workers)
            env["CRITERION_HOME"] = str(run / "criterion")
            invocation = [str(binaries[name]), *CRITERION_ARGS]
            if args.filter:
                invocation.append(args.filter)
            entry = {"benchmark": name, "repeat": repeat + 1, "command": invocation, "before": host_snapshot()}
            started = time.monotonic()
            print(f"Running {name}, repeat {repeat + 1}", flush=True)
            with (run / "stdout.txt").open("w") as stdout, (run / "stderr.txt").open("w") as stderr:
                returncode, resources, samples, timed_out = run_measured(invocation, env, stdout, stderr, args.timeout, args.host_sample_interval)
            write_json(run / "resources.json", resources)
            write_json(run / "host-samples.json", samples)
            entry.update(
                returncode=returncode,
                timed_out=timed_out,
                wall_seconds=time.monotonic() - started,
                after=host_snapshot(),
                estimates=summarize(run),
            )
            manifest["runs"].append(entry)
            write_json(manifest_path, manifest)
            if timed_out or returncode or not entry["estimates"]:
                raise RuntimeError(f"{name} failed, timed out, or produced no estimates; inspect {run}")
    manifest.update(after=host_snapshot(), complete=True)
    write_json(manifest_path, manifest)
    print(f"Saved {manifest_path}", flush=True)

def compare(args):
    before, after = (json.loads(path.read_text()) for path in (args.before, args.after))
    for manifest in (before, after):
        if manifest.get("schema") != 2 or manifest.get("kind") != "benchmark-measurement" or not manifest.get("complete"):
            raise ValueError("Both inputs must be complete, provenance-bound measurements")
    def bench_sources(manifest):
        return {
            name: record["benchmark_source_sha256"]
            for name, record in manifest["prepared"]["binaries"].items()
            if name in manifest["protocol"]["benches"]
        }

    checks = {
        "protocol": (before["protocol"], after["protocol"]),
        "host": (before["host"], after["host"]),
        "toolchain": (before["prepared"]["toolchain"], after["prepared"]["toolchain"]),
        "build arguments": (before["prepared"]["build_args"], after["prepared"]["build_args"]),
        "build environment": (before["prepared"]["build_environment"], after["prepared"]["build_environment"]),
        "run environment": (before["environment"], after["environment"]),
        "benchmark sources": (bench_sources(before), bench_sources(after)),
    }
    mismatches = [name for name, pair in checks.items() if pair[0] != pair[1]]
    if mismatches:
        raise ValueError("Incomparable measurements: " + ", ".join(mismatches))

    def collect(manifest):
        values = {}
        for run in manifest["runs"]:
            for estimate in run["estimates"]:
                values.setdefault(estimate["benchmark"], []).append({"repeat": run["repeat"], **estimate})
        return values

    old, new = collect(before), collect(after)
    if old.keys() != new.keys():
        raise ValueError("Benchmark result sets differ")
    rows = []
    for name in sorted(old):
        old_median = statistics.median(item["mean_ns"] for item in old[name])
        new_median = statistics.median(item["mean_ns"] for item in new[name])
        rows.append({
            "benchmark": name,
            "before_median_run_mean_ns": old_median,
            "after_median_run_mean_ns": new_median,
            "descriptive_change_percent": 100 * (new_median / old_median - 1),
            "before_runs": old[name],
            "after_runs": new[name],
        })
    print(json.dumps({
        "descriptive_only": True,
        "note": "No significance or tail-latency claim; inspect every repeat, confidence interval, and host sample.",
        "before_revision": before["prepared"]["before"]["revision"],
        "after_revision": after["prepared"]["before"]["revision"],
        "results": rows,
    }, indent=2))

def main():
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    build = commands.add_parser("prepare", help="compile and freeze provenance-bound benchmark executables")
    build.add_argument("--output", type=Path, required=True)
    build.add_argument("--benches", default=",".join(SUITE))
    build.add_argument("--features", default="scheduler")
    build.set_defaults(action=prepare)
    run = commands.add_parser("measure", help="measure executables from a prepare manifest")
    run.add_argument("--prepared", type=Path, required=True, help="path to prepare manifest.json")
    run.add_argument("--output", type=Path, required=True)
    run.add_argument("--label", required=True)
    run.add_argument("--benches", default=",".join(SUITE))
    run.add_argument("--filter")
    run.add_argument("--workers", type=int, default=4)
    run.add_argument("--repeats", type=int, default=2)
    run.add_argument("--timeout", type=int, default=600)
    run.add_argument("--host-sample-interval", type=float, default=0.25, help="seconds; zero disables sampling")
    run.set_defaults(action=measure)
    diff = commands.add_parser("compare", help="descriptively compare compatible measurements")
    diff.add_argument("before", type=Path)
    diff.add_argument("after", type=Path)
    diff.set_defaults(action=compare)
    args = parser.parse_args()
    args.action(args)

if __name__ == "__main__":
    main()
