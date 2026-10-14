#!/usr/bin/env bash
# RelStorage on PostgreSQL against ZEO and zoe serve, every store on tmpfs.
# PostgreSQL runs in the container of the CI baseline (bench/shootout), with
# its data on tmpfs. Run it in devenv shell: RelStorage needs libstdc++.
set -u
cd "$(dirname "$0")"
source ./common.sh
# Each store goes to tmpfs, where a sync costs nothing.
export POC_WORK=${POC_WORK:-/dev/shm/zoe-poc-077} POC_CLEANUP=1
OUT=${OUT:-results/ab-relstorage.jsonl}
BEST=(--env ZOE_POC_STORECOMPRESS=1 --env ZOE_POC_TIDONLY=1)
one() {  # label mode scenario driver-args...
  local label=$1 mode=$2 sc=$3; shift 3
  $V driver.py --label "$label" "$@" --mode $mode -c 16 --values 3 --scenarios $sc --results "$OUT" 2>>"$OUT.err" \
    | python3 show.py || echo "FAILED $label $mode $sc" >> "$OUT.err"
}
for round in ${ROUNDS:-1 2}; do
  for mode in ${MODES:-threads processes}; do
    for sc in ${SCENARIOS:-add update cold}; do
      one zeo/stock $mode $sc --server zeo
      one relstorage $mode $sc --server relstorage
      one relstorage-hf $mode $sc --server relstorage --history-free
      one zoe-base/stock $mode $sc --server zoe --binary "$ZOE"
      one zoe-best/stock $mode $sc --server zoe --binary "$ZOE" "${BEST[@]}"
      one zoe-best/rust $mode $sc --server zoe --binary "$ZOE" --client zoeclientx "${BEST[@]}"
      one zoe-best/rust-pool16 $mode $sc --server zoe --binary "$ZOE" --client zoeclientx --client-pool 16 "${BEST[@]}"
    done
  done
done
echo DONE
