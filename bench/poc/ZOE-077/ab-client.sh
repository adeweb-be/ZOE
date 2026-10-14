#!/usr/bin/env bash
# Interleaved A/B of the clients on tmpfs: ZEO.ClientStorage against the
# zoe-client storage of rustclient/ (build it first with rustclient/build.sh).
# zoe-client speaks msgpack only, so its ZEO server runs with ZEO_MSGPACK=1.
set -u
cd "$(dirname "$0")"
source ./common.sh
# Each store goes to tmpfs, where a sync costs nothing.
export POC_WORK=${POC_WORK:-/dev/shm/zoe-poc-077} POC_CLEANUP=1
OUT=${OUT:-results/ab-client.jsonl}
BEST=(--env ZOE_POC_STORECOMPRESS=1 --env ZOE_POC_TIDONLY=1)
one() {  # label mode scenario driver-args...
  local label=$1 mode=$2 sc=$3; shift 3
  $V driver.py --label "$label" "$@" --mode $mode -c 16 --values 3 --scenarios $sc --results "$OUT" 2>>"$OUT.err" \
    | python3 show.py || echo "FAILED $label $mode $sc" >> "$OUT.err"
}
for round in ${ROUNDS:-1 2}; do
  for mode in threads processes; do
    for sc in add update cold; do
      one zeo/stock $mode $sc --server zeo
      one zeo-msgpack/rust $mode $sc --server zeo-msgpack --client zoeclientx
      one zoe-base/stock $mode $sc --server zoe --binary "$ZOE"
      one zoe-base/rust $mode $sc --server zoe --binary "$ZOE" --client zoeclientx
      one zoe-best/stock $mode $sc --server zoe --binary "$ZOE" "${BEST[@]}"
      one zoe-best/rust $mode $sc --server zoe --binary "$ZOE" --client zoeclientx "${BEST[@]}"
    done
  done
done
echo DONE
