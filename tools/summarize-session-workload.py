#!/usr/bin/env python3
"""Summarize per-session quantiles without pretending they are pooled percentiles."""
import argparse
from collections import defaultdict
import json
from pathlib import Path
from statistics import median


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('results', type=Path)
    parser.add_argument('--sessions', type=int, default=4)
    args = parser.parse_args()
    groups = defaultdict(list)
    for line in args.results.read_text().splitlines():
        row = json.loads(line)
        if row.get('kind') == 'metadata' or row['sessions'] != args.sessions:
            continue
        if row.get('returncode') != 0:
            raise SystemExit(f'failed case: {row}')
        result = row['result']
        runs = (result['results'] if row['mode'] != 'processes' else
                [child['results'][0] for child in result['results']])
        duration = (sum(run['elapsed_ns'] for run in runs) if row['mode'] == 'serial'
                    else max(run['elapsed_ns'] for run in runs))
        cheap = [run['output_latency_ns']['p99'] / 1000 for run in runs if run['work'] == 0]
        groups[row['case'], row['mode']].append((
            median(run['output_latency_ns']['p99'] for run in runs) / 1000,
            sum(run['ticks'] for run in runs) * 1e9 / duration,
            median(cheap) if cheap else None))
    print('| Case | Mode | Median session p99, µs | Approx. completed ticks/s | Cheap-session p99, µs |')
    print('| --- | --- | ---: | ---: | ---: |')
    for (case, mode), values in sorted(groups.items()):
        cheap = [item[2] for item in values if item[2] is not None]
        cheap_text = f'{median(cheap):.1f}' if cheap else '—'
        print(f'| {case} | {mode} | {median(item[0] for item in values):.1f} | '
              f'{median(item[1] for item in values):,.0f} | {cheap_text} |')


if __name__ == '__main__':
    main()
