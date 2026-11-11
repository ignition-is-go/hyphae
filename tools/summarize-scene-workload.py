#!/usr/bin/env python3
"""Report medians across repetitions of each run's quantiles; never pool p99s."""
import argparse
from collections import defaultdict
import json
from pathlib import Path
from statistics import median


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('results', type=Path)
    args = parser.parse_args()
    groups = defaultdict(list)
    for line in args.results.read_text().splitlines():
        row = json.loads(line)
        if row.get('kind') == 'metadata':
            continue
        if row.get('returncode') != 0:
            raise SystemExit(f'case failed: {row}')
        groups[row['case'], row['backend'], row['workers']].append(row)
    print('| Case | Backend / wave workers | Eval p99 ms | Switch p99 ms | Scheduled → fanout p99 ms | Rejected | >frame budget | Peak RSS MiB | CPU seconds |')
    print('| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |')
    for (case, backend, workers), rows in sorted(groups.items()):
        def result(key):
            return median(row['result'][key] for row in rows)
        def quantile(key):
            samples = [row['result'][key]['p99'] for row in rows
                       if row['result'][key]['p99'] is not None]
            return f'{median(samples) / 1e6:.3f}' if samples else '—'
        rss = median(row['process_usage']['max_rss_kib'] for row in rows) / 1024
        cpu = median(row['process_usage']['user_seconds'] + row['process_usage']['system_seconds']
                     for row in rows)
        print(f'| {case} | {backend} / {workers} | {quantile("evaluation_ns")} | '
              f'{quantile("switch_evaluation_ns")} | {quantile("scheduled_to_fanout_ns")} | '
              f'{result("events_rejected"):g} | {result("deadline_misses"):g} | {rss:.1f} | {cpu:.3f} |')


if __name__ == '__main__':
    main()
