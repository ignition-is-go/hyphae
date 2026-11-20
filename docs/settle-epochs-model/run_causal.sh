#!/bin/bash
# usage: run_causal.sh SCEN A B [MUTANT] [SKIP]  (writer 0 sends A writes, writer 1 sends B)
B=/mnt/tank/targets/hyphae-causal-model/causal_model
s=$(date +%s)
out=$(timeout 1200 systemd-run --user --scope -q -p MemoryMax=16G -E SCEN="$1" -E SKIP="${5:-0}" ${4:+-E MUTANT=$4} "$B" "$2" "$3" 2>&1)
rc=$?
echo "== scen=$1 a=$2 b=$3 mutant=${4:-none} skip=${5:-0} rc=$rc $(( $(date +%s)-s ))s"
echo "$out" | cut -c1-220 | head -2
