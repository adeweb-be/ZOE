#!/usr/bin/env bash
# The stock client with a timer slack of 1 µs: ZEO.ClientStorage sleeps
# 1e-6 s after each reply, and the default slack of 50 µs makes that sleep
# last about 53 µs. Threads cells, interleaved with the default slack.
set -u
cd "$(dirname "$0")"
source ./common.sh
# Each store goes to tmpfs, where a sync costs nothing.
export POC_WORK=${POC_WORK:-/dev/shm/zoe-poc-077} POC_CLEANUP=1
OUT=${OUT:-results/ab-timerslack.jsonl}
BEST=(--env ZOE_POC_STORECOMPRESS=1 --env ZOE_POC_TIDONLY=1)
one() {  # label scenario driver-args...
  local label=$1 sc=$2; shift 2
  $V driver.py --label "$label" "$@" --mode threads -c 16 --values 3 --scenarios $sc --results "$OUT" 2>>"$OUT.err" \
    | python3 show.py || echo "FAILED $label $sc" >> "$OUT.err"
}
for round in ${ROUNDS:-1 2}; do
  for sc in add update cold; do
    one zeo/stock $sc --server zeo
    one zeo/stock-slack1us $sc --server zeo --client-timerslack-ns 1000
    one zoe-best/stock $sc --server zoe --binary "$ZOE" "${BEST[@]}"
    one zoe-best/stock-slack1us $sc --server zoe --binary "$ZOE" "${BEST[@]}" --client-timerslack-ns 1000
  done
done
echo DONE
