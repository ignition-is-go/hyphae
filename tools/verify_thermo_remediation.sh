#!/usr/bin/env bash
set -euo pipefail

repo_root="$(git rev-parse --show-toplevel)"
cd "$repo_root"

baseline_rev="${HYPHAE_THERMO_BASELINE_REV:-4465c6dbe364bfee9f97136d64420549af14d800}"

command -v cargo-semver-checks >/dev/null || {
	printf 'cargo-semver-checks is required\n' >&2
	exit 1
}

cargo semver-checks check-release \
	-p hyphae \
	--baseline-rev "$baseline_rev" \
	--release-type patch \
	--all-features

cargo test -p hyphae --lib --all-features bounded_output::tests
cargo test -p hyphae --lib --all-features traits::operators::finalize::tests
cargo test -p hyphae --lib --all-features map_query::compiler::tests
cargo test -p hyphae --lib --all-features cell_map::tests
cargo test -p hyphae --lib --all-features traits::collections::left_join::tests
cargo test -p hyphae --all-features --test arbitrary_n_public_join_region

cargo fmt --all --check
cargo clippy -p hyphae --all-targets --all-features -- -D warnings

if [[ "${1:-}" == "--full" ]]; then
	cargo test --workspace --all-features
	cargo clippy --workspace --all-targets --all-features -- -D warnings
fi
