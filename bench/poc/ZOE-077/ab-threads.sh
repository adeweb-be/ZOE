#!/usr/bin/env bash
set -u
cd "$(dirname "$0")"
source ./common.sh
# Each store goes to tmpfs, where a sync costs nothing.
export POC_WORK=${POC_WORK:-/dev/shm/zoe-poc-077} POC_CLEANUP=1
OUT=results/ab-threads.jsonl
one() {
  local label=$1 server=$2 bin=$3 mode=$4 sc=$5; shift 5
  local envs=()
  for e in "$@"; do envs+=(--env "$e"); done
  $V driver.py --server "$server" ${bin:+--binary "$bin"} --label "$label" "${envs[@]}" \
    --mode $mode -c 16 --values 3 --scenarios $sc --results "$OUT" >/dev/null 2>>"$OUT.err" \
    || echo "FAILED $label $mode $sc" >> "$OUT.err"
}
for round in 1 2; do
  for sc in add update; do
    one zeo zeo "" threads $sc
    one base zoe "$ZOE" threads $sc $P
    one bufread zoe "$ZOE" threads $sc $P ZOE_POC_BUFREAD=1
    one best zoe "$ZOE" threads $sc $P ZOE_POC_BUFREAD=1 ZOE_POC_STORECOMPRESS=1 ZOE_POC_TIDONLY=1
  done
done
echo DONE
