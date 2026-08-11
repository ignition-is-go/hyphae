#!/usr/bin/env bash
set -euo pipefail
home_root=/home/trevor; repo_root="$(git rev-parse --show-toplevel)"; revision="${REVISION:-HEAD}"; commit="$(git -C "$repo_root" rev-parse "$revision")"
cache_root="${HYPHAE_EVIDENCE_CACHE_ROOT:-${XDG_CACHE_HOME:-$home_root/.cache}/hyphae-evidence}"; out="${OUTPUT_DIR:?set OUTPUT_DIR under /home}"; adapter="${HYPHAE_MAP_QUERY_ADAPTER:-$repo_root/tools/map_query_allocation_profile.rs}"; target="${CARGO_TARGET_DIR:-$out/target}"
ensure_home(){ local p; p="$(realpath -m -- "$1")"; case "$p" in "$home_root"|"$home_root"/*) ;; *) echo "path outside $home_root: $p" >&2; exit 2;; esac; }
ensure_home "$cache_root"; ensure_home "$out"; ensure_home "$target"
if [[ -e "$target" ]]; then echo "fresh nonexisting CARGO_TARGET_DIR required: $target" >&2; exit 2; fi
if [[ -d "$out" ]] && find "$out" -mindepth 1 -print -quit | grep -q .; then echo "OUTPUT_DIR must be empty: $out" >&2; exit 2; fi
mkdir -p "$cache_root" "$out/inputs"; cp "$adapter" "$out/inputs/adapter.rs"; cp "$0" "$out/inputs/inspect-map-query-codegen.sh"; adapter="$out/inputs/adapter.rs"
run="$(mktemp -d "$cache_root/codegen.XXXXXX")"; ensure_home "$run"; trap 'rm -rf "$run"' EXIT
git -C "$repo_root" archive "$commit" | tar -x -C "$run"; mkdir -p "$run/hyphae/examples"; cp "$adapter" "$run/hyphae/examples/map_query_allocation_profile.rs"
export CARGO_BUILD_JOBS=1 CARGO_TARGET_DIR="$target" RUSTFLAGS="${RUSTFLAGS:--Csymbol-mangling-version=v0 -Ccodegen-units=1 -Cdebuginfo=1}"
(cd "$run" && cargo rustc --locked --offline --release -p hyphae --example map_query_allocation_profile -- --emit=llvm-ir,asm) 2>&1 | tee "$out/build.txt"
mapfile -t ll < <(find "$target/release/examples" -maxdepth 1 -type f -name 'map_query_allocation_profile-*.ll' -print); mapfile -t asm < <(find "$target/release/examples" -maxdepth 1 -type f -name 'map_query_allocation_profile-*.s' -print)
[[ ${#ll[@]} -eq 1 && ${#asm[@]} -eq 1 ]] || { echo "expected exactly one IR and asm artifact" >&2; exit 3; }
cp "${ll[0]}" "$out/probe.ll"; cp "${asm[0]}" "$out/probe.s"; bin="$target/release/examples/map_query_allocation_profile"; [[ -x "$bin" ]]
llvm-objdump -Cd "$bin" > "$out/objdump.txt"
rg -n -C 12 'map_query_codegen_probe|call[^;]*(%|\*)|__rust_(alloc|realloc)|RawVec.*grow|MapDiff' "$out/probe.ll" "$out/objdump.txt" > "$out/probe-audit.txt" || true
sha256sum "$out/inputs/adapter.rs" "$out/inputs/inspect-map-query-codegen.sh" "$out/probe.ll" "$out/probe.s" "$out/objdump.txt" > "$out/sha256sums.txt"
printf 'revision=%s\ncommit=%s\nadapter_sha256=%s\n' "$revision" "$commit" "$(sha256sum "$adapter"|awk '{print $1}')" > "$out/environment.txt"
