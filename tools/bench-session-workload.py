#!/usr/bin/env python3
"""Run isolated, randomized session benchmark cases and preserve raw JSONL results."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import random
import subprocess
import time

ROOT = Path(__file__).resolve().parents[1]


def command(args):
    return subprocess.check_output(args, cwd=ROOT, text=True).strip()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--repeats', type=int, default=3)
    parser.add_argument('--ticks', type=int, default=400)
    parser.add_argument('--workers', type=int, default=0)
    parser.add_argument('--threshold', type=int, default=256)
    parser.add_argument('--sessions', type=int, nargs='+', default=[1, 2, 4])
    args = parser.parse_args()
    if min(args.repeats, args.ticks, *args.sessions) <= 0 or args.workers < 0 or args.threshold < 1:
        parser.error('counts must be positive; workers may be zero')
    build = subprocess.run(['cargo', 'bench', '-p', 'hyphae', '--features', 'scheduler',
                            '--bench', 'session_workload', '--no-run', '--message-format=json'],
                           cwd=ROOT, text=True, capture_output=True, check=True)
    artifacts = [json.loads(line) for line in build.stdout.splitlines() if line.startswith('{')]
    executable = next(item['executable'] for item in artifacts
                      if item.get('reason') == 'compiler-artifact' and item.get('executable')
                      and item.get('target', {}).get('name') == 'session_workload')
    env = dict(os.environ, HYPHAE_WORKER_THREADS=str(args.workers),
               HYPHAE_WAVE_THRESHOLD=str(args.threshold))
    cases = {
        'cheap': ['--width', '16', '--depth', '8'],
        'deep': ['--width', '4', '--depth', '48'],
        'wide': ['--width', '256', '--depth', '2'],
        'compute': ['--width', '16', '--depth', '8', '--work', '1024'],
        'observers': ['--width', '16', '--depth', '8', '--observers', '32', '--edit-every', '10'],
        'noisy_neighbor': ['--width', '16', '--depth', '8', '--hot-work', '4096'],
        'shared_edits': ['--width', '16', '--depth', '8', '--sharing', 'shared', '--edit-every', '10'],
    }
    jobs = [(repeat, name, mode, sessions, options)
            for repeat in range(args.repeats) for name, options in cases.items()
            for sessions in args.sessions for mode in ['serial', 'threads', 'processes']
            if not (name == 'shared_edits' and mode == 'processes')
            and not (name == 'noisy_neighbor' and sessions == 1)]
    random.Random(856).shuffle(jobs)
    metadata = {'kind': 'metadata', 'source_commit': command(['git', 'rev-parse', 'HEAD']),
                'dirty': bool(command(['git', 'status', '--porcelain'])),
                'benchmark_sha256': hashlib.sha256((ROOT / 'hyphae/benches/session_workload.rs').read_bytes()).hexdigest(),
                'cargo_lock_sha256': hashlib.sha256((ROOT / 'Cargo.lock').read_bytes()).hexdigest(),
                'rustc': command(['rustc', '-Vv']), 'platform': platform.platform(),
                'cpu': next((line.split(':', 1)[1].strip() for line in
                             Path('/proc/cpuinfo').read_text().splitlines()
                             if line.startswith('model name')), platform.processor()),
                'affinity': sorted(os.sched_getaffinity(0)), 'arguments': vars(args) | {'output': str(args.output)},
                'seed': 856, 'command': [executable], 'started_unix': time.time()}
    args.output.parent.mkdir(parents=True, exist_ok=True)
    failures = 0
    with args.output.open('w') as output:
        output.write(json.dumps(metadata) + '\n')
        for index, (repeat, name, mode, sessions, options) in enumerate(jobs, 1):
            cmd = [executable, '--mode', mode, '--sessions', str(sessions), '--ticks', str(args.ticks),
                   '--warmup', '40', *options]
            started = time.monotonic()
            try:
                run = subprocess.run(cmd, cwd=ROOT, env=env, text=True, capture_output=True, timeout=120)
                record = {'repeat': repeat, 'case': name, 'mode': mode, 'sessions': sessions,
                          'returncode': run.returncode, 'command': cmd,
                          'elapsed_seconds': time.monotonic() - started}
                try:
                    record['result'] = json.loads(run.stdout)
                except json.JSONDecodeError:
                    record['stdout'] = run.stdout
                if run.returncode:
                    record['stderr'] = run.stderr
                    failures += 1
            except subprocess.TimeoutExpired as error:
                record = {'repeat': repeat, 'case': name, 'mode': mode, 'sessions': sessions,
                          'error': str(error), 'command': cmd}
                failures += 1
            output.write(json.dumps(record) + '\n')
            output.flush()
            print(f'{index}/{len(jobs)} {name} {mode} sessions={sessions}', flush=True)
    if failures:
        raise SystemExit(f'{failures} cases failed; see {args.output}')


if __name__ == '__main__':
    main()
