#!/usr/bin/env bash
set -euo pipefail

repo_root="$(git rev-parse --show-toplevel)"
home_root=/home/trevor
ensure_under_home() {
    local path
    path="$(realpath -m -- "$1")"
    case "$path" in "$home_root"|"$home_root"/*) ;; *) echo "path must be under $home_root: $path" >&2; exit 2;; esac
}
revision="${REVISION:-HEAD}"
cache_root="${HYPHAE_EVIDENCE_CACHE_ROOT:-${XDG_CACHE_HOME:-$home_root/.cache}/hyphae-evidence}"
ensure_under_home "$cache_root"
mkdir -p "$cache_root"
run_root="$(mktemp -d "$cache_root/map-query-allocations.XXXXXX")"
cleanup() { rm -rf -- "$run_root"; }
trap cleanup EXIT
timestamp="$(date -u +%Y%m%dT%H%M%SZ)"
output_dir="${OUTPUT_DIR:-$repo_root/benchmark-results/map-query-allocations/$timestamp}"
adapter="${HYPHAE_MAP_QUERY_ADAPTER:-$repo_root/tools/map_query_allocation_profile.rs}"
target_dir="${CARGO_TARGET_DIR:-$run_root/target}"
ensure_under_home "$run_root"
ensure_under_home "$output_dir"
ensure_under_home "$target_dir"

mkdir -p "$run_root/revision"
mkdir -p "$output_dir"
git -C "$repo_root" archive "$revision" | tar -x -C "$run_root/revision"
cp "$adapter" \
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
    echo "adapter=$adapter"
    echo "adapter_sha256=$(sha256sum "$adapter" | awk '{print $1}')"
    echo "evidence_rows=${HYPHAE_EVIDENCE_ROWS:-1000}"
    echo "evidence_batch=${HYPHAE_EVIDENCE_BATCH:-100}"
    echo "evidence_single_updates=${HYPHAE_EVIDENCE_SINGLE_UPDATES:-100}"
    echo "evidence_scenario=${HYPHAE_EVIDENCE_SCENARIO:-all}"
} > "$output_dir/environment.txt"

mkdir -p "$run_root/revision/hyphae/examples"
cp "$adapter" \
    "$run_root/revision/hyphae/examples/map_query_allocation_profile.rs"

echo "Building map-query allocation profile: $revision" >&2
(
    cd "$run_root/revision"
    CARGO_BUILD_JOBS=1 \
    CARGO_TARGET_DIR="$target_dir" \
    HYPHAE_BENCH_REVISION="$revision" \
        cargo run --locked --offline --release -p hyphae \
            --example map_query_allocation_profile
) | tee "$output_dir/results.txt"

echo "Artifacts: $output_dir" >&2
