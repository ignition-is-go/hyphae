#!/bin/bash
# usage: run_model.sh MODES K [MUTANT]
B=/mnt/tank/targets/hyphae-epoch-model/epoch_model3
s=$(date +%s)
out=$(timeout 1200 systemd-run --user --scope -q -p MemoryMax=16G -E MODES="$1" -E STRICT_SWITCH=1 ${3:+-E MUTANT=$3} "$B" "$2" 2>&1)
rc=$?
echo "== modes=$1 k=$2 mutant=${3:-none} rc=$rc $(( $(date +%s)-s ))s"
echo "$out" | grep -vE '^ok' | cut -c1-200 | head -3
