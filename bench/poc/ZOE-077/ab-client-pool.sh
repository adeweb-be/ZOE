#!/usr/bin/env bash
# The zoe-client storage with a pool of connections in the threads cells:
# each transaction takes its own connection, so the commits of the 16
# threads no longer wait for one another. Interleaved with one connection.
set -u
cd "$(dirname "$0")"
source ./common.sh
# Each store goes to tmpfs, where a sync costs nothing.
export POC_WORK=${POC_WORK:-/dev/shm/zoe-poc-077} POC_CLEANUP=1
OUT=${OUT:-results/ab-client-pool.jsonl}
BEST=(--env ZOE_POC_STORECOMPRESS=1 --env ZOE_POC_TIDONLY=1)
PAR=(--env ZOE_POC_PARCOMPRESS=8 --env ZOE_POC_TIDONLY=1)
one() {  # label scenario driver-args...
  local label=$1 sc=$2; shift 2
  $V driver.py --label "$label" "$@" --mode threads -c 16 --values 3 --scenarios $sc --results "$OUT" 2>>"$OUT.err" \
    | python3 show.py || echo "FAILED $label $sc" >> "$OUT.err"
}
for round in ${ROUNDS:-1 2}; do
  for sc in add update; do
    one zeo/stock $sc --server zeo
    one zeo-msgpack/rust $sc --server zeo-msgpack --client zoeclientx
    one zeo-msgpack/rust-pool16 $sc --server zeo-msgpack --client zoeclientx --client-pool 16
    one zoe-best/rust $sc --server zoe --binary "$ZOE" --client zoeclientx "${BEST[@]}"
    one zoe-best/rust-pool16 $sc --server zoe --binary "$ZOE" --client zoeclientx --client-pool 16 "${BEST[@]}"
    one zoe-par8/rust $sc --server zoe --binary "$ZOE" --client zoeclientx "${PAR[@]}"
    one zoe-par8/rust-pool16 $sc --server zoe --binary "$ZOE" --client zoeclientx --client-pool 16 "${PAR[@]}"
  done
done
echo DONE
