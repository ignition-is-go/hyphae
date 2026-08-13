#!/usr/bin/env python3
import csv, json
from pathlib import Path

ROOT = Path(__file__).resolve().parent
KEY = ("scenario", "rows", "batch_size", "phase")
SIZES = {1, 10, 100, 1000, 10000}
SCENARIOS = {"projection_region", "two_join_region", "repeated_relation_four_join", "rekey_between_joins"}
PHASES = {"setup", "build", "materialize", "single_updates", "batch", "teardown"}

def load(revision):
    with (ROOT / "allocations" / revision / "results.csv").open(newline="") as f:
        rows = list(csv.DictReader(f))
    keyed = {}
    for row in rows:
        key = (row["scenario"], int(row["rows"]), int(row["batch_size"]), row["phase"])
        if key in keyed:
            raise SystemExit(f"duplicate {revision} key: {key}")
        keyed[key] = row
    expected = {(s, n, b, p) for s in SCENARIOS for n in SIZES for b in SIZES for p in PHASES}
    if set(keyed) != expected:
        raise SystemExit(f"{revision} grid mismatch: missing={expected-set(keyed)}, extra={set(keyed)-expected}")
    return rows, keyed

v3_rows, v3 = load("v3")
candidate_rows, candidate = load("candidate")
keys = sorted(v3)
output_rows_mismatches = sum(v3[k]["output_rows"] != candidate[k]["output_rows"] for k in keys)
output_checksum_mismatches = sum(v3[k]["output_checksum"] != candidate[k]["output_checksum"] for k in keys)

def teardown_mismatches(keyed):
    count = 0
    for scenario in SCENARIOS:
        for n in SIZES:
            for b in SIZES:
                entry = keyed[(scenario, n, b, "setup")]["live_bytes_before"]
                after = keyed[(scenario, n, b, "teardown")]["live_bytes_after"]
                count += entry != after
    return count

result = {
    "v3_rows": len(v3_rows), "candidate_rows": len(candidate_rows), "paired_rows": len(keys),
    "expected_rows_per_revision": 600,
    "key_duplicates_v3": 0, "key_duplicates_candidate": 0,
    "output_rows_mismatches": output_rows_mismatches,
    "output_checksum_mismatches": output_checksum_mismatches,
    "phases": sorted(PHASES), "scenarios": sorted(SCENARIOS),
    "rows_values": sorted(SIZES), "batch_values": sorted(SIZES),
    "v3_teardown_entry_mismatches": teardown_mismatches(v3),
    "candidate_teardown_entry_mismatches": teardown_mismatches(candidate),
}
print(json.dumps(result, indent=2))
if any(result[k] for k in ("output_rows_mismatches", "output_checksum_mismatches", "v3_teardown_entry_mismatches", "candidate_teardown_entry_mismatches")):
    raise SystemExit(1)
