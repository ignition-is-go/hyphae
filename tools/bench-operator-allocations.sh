#!/usr/bin/env bash
set -euo pipefail

repo_root="$(git rev-parse --show-toplevel)"
baseline_revision="${BASELINE_REVISION:-v2.0.1}"
candidate_revision="${CANDIDATE_REVISION:-HEAD}"
run_root="$(mktemp -d "${TMPDIR:-/tmp}/hyphae-operator-allocations.XXXXXX")"

cleanup() {
    rm -rf -- "$run_root"
}
trap cleanup EXIT

mkdir -p "$run_root/baseline" "$run_root/candidate"
git -C "$repo_root" archive "$baseline_revision" | tar -x -C "$run_root/baseline"
git -C "$repo_root" archive "$candidate_revision" | tar -x -C "$run_root/candidate"

run_revision() {
    local label="$1"
    local checkout="$2"
    local legacy_cfg="$3"
    local target_dir="$run_root/target-$label"

    mkdir -p "$checkout/hyphae/examples"
    cp "$repo_root/tools/operator_allocation_profile.rs" \
        "$checkout/hyphae/examples/operator_allocation_profile.rs"

    (
        cd "$checkout"
        CARGO_BUILD_JOBS=1 \
        CARGO_TARGET_DIR="$target_dir" \
        HYPHAE_BENCH_REVISION="$label" \
        RUSTFLAGS="${RUSTFLAGS:-} $legacy_cfg" \
            cargo run --locked --offline --release -p hyphae \
                --example operator_allocation_profile
    )
}

echo "Building bounded allocation profile: $baseline_revision vs $candidate_revision" >&2
echo "Depths: 4, 8, 16 join stages (20, 40, 80 operators); build jobs: 1" >&2

run_revision "baseline-$baseline_revision" "$run_root/baseline" "--cfg hyphae_legacy"
run_revision "candidate-$candidate_revision" "$run_root/candidate" ""
