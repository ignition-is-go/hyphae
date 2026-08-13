#!/usr/bin/env bash
set -euo pipefail

repo_root="$(git rev-parse --show-toplevel)"
revision="${REVISION:-HEAD}"
run_root="$(mktemp -d "${TMPDIR:-/tmp}/hyphae-operator-allocations.XXXXXX")"
timestamp="$(date -u +%Y%m%dT%H%M%SZ)"
output_dir="${OUTPUT_DIR:-$repo_root/benchmark-results/operator-allocations/$timestamp}"

cleanup() {
    rm -rf -- "$run_root"
}
trap cleanup EXIT

mkdir -p "$run_root/revision"
mkdir -p "$output_dir"
git -C "$repo_root" archive "$revision" | tar -x -C "$run_root/revision"
cp "$repo_root/tools/operator_allocation_profile.rs" "$output_dir/operator_allocation_profile.rs"
sha256sum "$output_dir/operator_allocation_profile.rs" > "$output_dir/harness.sha256"

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

run_revision() {
    local checkout="$1"
    local target_dir="$run_root/target"

    mkdir -p "$checkout/hyphae/examples"
    cp "$repo_root/tools/operator_allocation_profile.rs" \
        "$checkout/hyphae/examples/operator_allocation_profile.rs"

    (
        cd "$checkout"
        CARGO_BUILD_JOBS=1 \
        CARGO_TARGET_DIR="$target_dir" \
        HYPHAE_BENCH_REVISION="$revision" \
            cargo run --locked --offline --release -p hyphae \
                --example operator_allocation_profile
    )
}

echo "Building bounded v3 allocation profile: $revision" >&2
echo "Depths: 4, 8, 16 join stages (20, 40, 80 operators); build jobs: 1" >&2

run_revision "$run_root/revision" | tee "$output_dir/results.txt"
echo "Artifacts: $output_dir" >&2
