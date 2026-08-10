#!/usr/bin/env bash
set -euo pipefail

repo_root="$(git rev-parse --show-toplevel)"
revision="${REVISION:-HEAD}"
run_root="$(mktemp -d "${TMPDIR:-/tmp}/hyphae-map-query-allocations.XXXXXX")"
timestamp="$(date -u +%Y%m%dT%H%M%SZ)"
output_dir="${OUTPUT_DIR:-$repo_root/benchmark-results/map-query-allocations/$timestamp}"

cleanup() {
    rm -rf -- "$run_root"
}
trap cleanup EXIT

mkdir -p "$run_root/revision"
mkdir -p "$output_dir"
git -C "$repo_root" archive "$revision" | tar -x -C "$run_root/revision"
cp "$repo_root/tools/map_query_allocation_profile.rs" \
    "$output_dir/map_query_allocation_profile.rs"
sha256sum "$output_dir/map_query_allocation_profile.rs" > "$output_dir/harness.sha256"

{
    echo "timestamp_utc=$timestamp"
    echo "revision=$revision"
    echo "commit=$(git -C "$repo_root" rev-parse "$revision")"
    echo "rustc=$(rustc --version)"
    echo "cargo=$(cargo --version)"
    echo "kernel=$(uname -srmo)"
    echo "cpu=$(lscpu | awk -F: '/Model name/ {sub(/^[[:space:]]+/, "", $2); print $2; exit}')"
    echo "build_jobs=1"
} > "$output_dir/environment.txt"

mkdir -p "$run_root/revision/hyphae/examples"
cp "$repo_root/tools/map_query_allocation_profile.rs" \
    "$run_root/revision/hyphae/examples/map_query_allocation_profile.rs"

echo "Building map-query allocation profile: $revision" >&2
(
    cd "$run_root/revision"
    CARGO_BUILD_JOBS=1 \
    CARGO_TARGET_DIR="$run_root/target" \
    HYPHAE_BENCH_REVISION="$revision" \
        cargo run --locked --offline --release -p hyphae \
            --example map_query_allocation_profile
) | tee "$output_dir/results.txt"

echo "Artifacts: $output_dir" >&2
