#!/usr/bin/env bash
# The zoe-client storage on zoe serve with a cheaper codec, threads and
# processes, interleaved with ZEO. One connection runs one commit at a
# time, so the compression at storea stays on the path of each commit.
set -u
cd "$(dirname "$0")"
source ./common.sh
# Each store goes to tmpfs, where a sync costs nothing.
export POC_WORK=${POC_WORK:-/dev/shm/zoe-poc-077} POC_CLEANUP=1
OUT=${OUT:-results/ab-client-codec.jsonl}
BEST=(--env ZOE_POC_STORECOMPRESS=1 --env ZOE_POC_TIDONLY=1)
one() {  # label mode scenario driver-args...
  local label=$1 mode=$2 sc=$3; shift 3
  $V driver.py --label "$label" "$@" --mode $mode -c 16 --values 3 --scenarios $sc --results "$OUT" 2>>"$OUT.err" \
    | python3 show.py || echo "FAILED $label $mode $sc" >> "$OUT.err"
}
for round in ${ROUNDS:-1 2}; do
  for mode in threads processes; do
    for sc in add update; do
      one zeo-msgpack/rust $mode $sc --server zeo-msgpack --client zoeclientx
      one zoe-best/rust $mode $sc --server zoe --binary "$ZOE" --client zoeclientx "${BEST[@]}"
      one zoe-best-zstd1/rust $mode $sc --server zoe --binary "$ZOE" --client zoeclientx "${BEST[@]}" --env ZOE_POC_COMPRESSION=zstd:1
      one zoe-best-nocomp/rust $mode $sc --server zoe --binary "$ZOE" --client zoeclientx "${BEST[@]}" --env ZOE_POC_COMPRESSION=none
    done
  done
done
echo DONE
