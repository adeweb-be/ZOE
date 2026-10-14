#!/usr/bin/env bash
# The threads cells add and update at concurrency 16 and 64 on 3.14t, with
# one connection, the pool of ZOE-077 without the rule, and the pool with
# the rule. Set POC_PYTHON to a 3.14t bench environment. Each round runs
# under the lock POC_LOCK, taken exclusive, so no other heavy job of the
# machine runs during a round. The rounds interleave the configurations.
set -u
cd "$(dirname "$0")"
source ./common.sh
export POC_WORK=${POC_WORK:-/dev/shm/zoe-poc-151} POC_CLEANUP=1
OUT=${OUT:-results/ab-concurrent.jsonl}
LOCK=${POC_LOCK:-/tmp/zoe-poc-bench.lock}
BEST=(--env ZOE_POC_STORECOMPRESS=1 --env ZOE_POC_TIDONLY=1)
one() {  # label scenario concurrency driver-args...
  local label=$1 sc=$2 c=$3; shift 3
  $V driver.py --label "$label" "$@" --mode threads -c "$c" --values 3 --scenarios "$sc" \
    --server zoe --binary "$ZOE" --client zoeclientx "${BEST[@]}" --results "$OUT" 2>>"$OUT.err" \
    | python3 show.py || echo "FAILED $label $sc $c" >> "$OUT.err"
}
round() {
  for sc in ${SCENARIOS:-add update}; do
    for c in ${CONCURRENCY:-16 64}; do
      one one-connection "$sc" "$c" --client-pool 1
      one pool-without-rule "$sc" "$c" --client-pool "$c"
      one pool-with-rule "$sc" "$c" --client-pool "$c" --client-rule
    done
  done
}
if [ "${1:-}" = --round ]; then
  round
  exit 0
fi
for r in ${ROUNDS:-1 2 3}; do
  echo "== round $r"
  flock -x "$LOCK" "$0" --round
done
echo DONE
