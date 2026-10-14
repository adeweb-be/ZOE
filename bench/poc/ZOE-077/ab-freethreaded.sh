#!/usr/bin/env bash
# The threads cells with the client on a free-threaded interpreter. Set
# POC_PYTHON to a 3.14t bench environment, POC_ZEO_PYTHON to the Python of
# the ZEO server, and POC_LIBPQ for psycopg2. Run it in devenv shell.
# driver.py records the modules that turned the GIL back on.
set -u
cd "$(dirname "$0")"
source ./common.sh
# Each store goes to tmpfs, where a sync costs nothing.
export POC_WORK=${POC_WORK:-/dev/shm/zoe-poc-077} POC_CLEANUP=1
OUT=${OUT:-results/ab-freethreaded.jsonl}
BEST=(--env ZOE_POC_STORECOMPRESS=1 --env ZOE_POC_TIDONLY=1)
one() {  # label scenario driver-args...
  local label=$1 sc=$2; shift 2
  $V driver.py --label "$label" "$@" --mode threads -c 16 --values 3 --scenarios $sc --results "$OUT" 2>>"$OUT.err" \
    | python3 show.py || echo "FAILED $label $sc" >> "$OUT.err"
}
for round in ${ROUNDS:-1 2}; do
  for sc in ${SCENARIOS:-add update cold}; do
    one inproc $sc --server mapping
    one zeo/stock $sc --server zeo
    one relstorage $sc --server relstorage
    one relstorage-hf $sc --server relstorage --history-free
    one zoe-best/stock $sc --server zoe --binary "$ZOE" "${BEST[@]}"
    one zoe-best/rust $sc --server zoe --binary "$ZOE" --client zoeclientx "${BEST[@]}"
    one zoe-best/rust-pool16 $sc --server zoe --binary "$ZOE" --client zoeclientx --client-pool 16 "${BEST[@]}"
  done
done
echo DONE
