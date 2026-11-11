#!/usr/bin/env python3
"""Compare reactive evaluators at captured scene scale, with a 60 Hz offered load."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import random
import subprocess
import threading
import time

ROOT = Path(__file__).resolve().parents[1]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--repeats', type=int, default=3)
    parser.add_argument('--frames', type=int, default=120)
    parser.add_argument('--hz', type=int, default=60)
    parser.add_argument('--case', action='append', help='limit to named cases')
    parser.add_argument('--implementation', action='append', choices=['hyphae0', 'hyphae4', 'incremental', 'local'])
    args = parser.parse_args()
    if args.repeats < 1 or args.frames < 1 or args.hz < 0:
        parser.error('positive repeats/frames and nonnegative hz required')
    cases = {
        'scenes10': ['--scenes', '10'],
        'scenes20': ['--scenes', '20'],
        'observed28': ['--scenes', '28'],
        'clients14': ['--clients', '14'],
        'switch_storm': ['--clients', '14', '--switch-every', '1', '--dynamic-sinks', 'on'],
        'deep_expansion': ['--clients', '14', '--layers', '16'],
        'per_client_encode': ['--clients', '14', '--encode', 'per-client'],
        'batched_feedback': ['--feedback', 'batched'],
        'single_field': ['--fields', 'one'],
        'branch_select': ['--switch-every', '1', '--switch-mode', 'select'],
    }
    if args.case:
        unknown = set(args.case) - cases.keys()
        if unknown:
            parser.error(f'unknown cases: {sorted(unknown)}')
        cases = {key: value for key, value in cases.items() if key in args.case}
    build = subprocess.run(['cargo', 'bench', '-p', 'hyphae', '--features', 'scheduler,local-runtime',
                            '--bench', 'scene_workload', '--no-run', '--message-format=json'],
                           cwd=ROOT, text=True, capture_output=True, check=True)
    artifacts = [json.loads(line) for line in build.stdout.splitlines() if line.startswith('{')]
    executable = next(item['executable'] for item in artifacts
                      if item.get('reason') == 'compiler-artifact' and item.get('executable')
                      and item.get('target', {}).get('name') == 'scene_workload')
    implementations = {'hyphae0': ('hyphae', 0), 'hyphae4': ('hyphae', 4),
                       'incremental': ('incremental', 0), 'local': ('local', 0)}
    selected = [implementations[name] for name in (args.implementation or implementations)]
    jobs = [(repeat, name, backend, workers, options)
            for repeat in range(args.repeats) for name, options in cases.items()
            for backend, workers in selected]
    random.Random(2603).shuffle(jobs)
    digest = lambda name: hashlib.sha256((ROOT / name).read_bytes()).hexdigest()
    metadata = {'kind': 'metadata', 'source_commit': subprocess.check_output(
                    ['git', 'rev-parse', 'HEAD'], cwd=ROOT, text=True).strip(),
                'dirty': bool(subprocess.check_output(['git', 'status', '--porcelain'], cwd=ROOT, text=True)),
                'benchmark_sha256': digest('hyphae/benches/scene_workload.rs'),
                'runtime_sources_sha256': {name: digest(f'hyphae/src/local/{name}')
                                          for name in ['mod.rs', 'body.rs', 'runtime.rs']},
                'fixture_sha256': digest('hyphae/benches/fixtures/scene_topology.json'),
                'lock_sha256': digest('Cargo.lock'),
                'rustc': subprocess.check_output(['rustc', '-Vv'], cwd=ROOT, text=True).strip(),
                'platform': platform.platform(), 'affinity': sorted(os.sched_getaffinity(0)),
                'cpu': next((line.split(':', 1)[1].strip() for line in
                             Path('/proc/cpuinfo').read_text().splitlines()
                             if line.startswith('model name')), platform.processor()),
                'arguments': vars(args) | {'output': str(args.output)}, 'seed': 2603,
                'started_unix': time.time()}
    args.output.parent.mkdir(parents=True, exist_ok=True)
    failures = 0
    with args.output.open('w') as output:
        output.write(json.dumps(metadata) + '\n')
        for index, (repeat, name, backend, workers, options) in enumerate(jobs, 1):
            env = dict(os.environ, HYPHAE_WORKER_THREADS=str(workers), HYPHAE_WAVE_THRESHOLD='256')
            command = [executable, '--backend', backend, '--frames', str(args.frames),
                       '--hz', str(args.hz), *options]
            started = time.monotonic()
            record = {'repeat': repeat, 'case': name, 'backend': backend,
                      'workers': workers, 'command': command}
            proc = subprocess.Popen(command, cwd=ROOT, env=env, stdout=subprocess.PIPE,
                                    stderr=subprocess.STDOUT, text=True)
            timer = threading.Timer(120, proc.kill)
            timer.start()
            try:
                raw = proc.stdout.read()
                _, status, usage = os.wait4(proc.pid, 0)
                proc.returncode = os.waitstatus_to_exitcode(status)
            finally:
                timer.cancel()
                proc.stdout.close()
            record |= {'returncode': proc.returncode, 'elapsed_seconds': time.monotonic() - started,
                       'process_usage': {'user_seconds': usage.ru_utime,
                                         'system_seconds': usage.ru_stime,
                                         'max_rss_kib': usage.ru_maxrss}}
            try:
                record['result'] = json.loads(raw)
            except json.JSONDecodeError:
                record['stdout_and_stderr'] = raw
                if proc.returncode == 0:
                    failures += 1
            if proc.returncode:
                failures += 1
            output.write(json.dumps(record) + '\n')
            output.flush()
            print(f'{index}/{len(jobs)} {name} {backend} workers={workers} status={record.get("returncode", "timeout")}', flush=True)
    if failures:
        raise SystemExit(f'{failures} cases failed; inspect {args.output}')


if __name__ == '__main__':
    main()
