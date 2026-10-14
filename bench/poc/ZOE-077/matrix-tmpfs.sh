#!/usr/bin/env bash
# The POC matrix with every store on tmpfs: a sync costs nothing.
set -u
cd "$(dirname "$0")"
source ./common.sh
# Each store goes to tmpfs, where a sync costs nothing.
export POC_WORK=${POC_WORK:-/dev/shm/zoe-poc-077} POC_CLEANUP=1
OUT=results/matrix-tmpfs.jsonl
run() {
  local label=$1 server=$2 bin=$3; shift 3
  local envs=()
  for e in "$@"; do envs+=(--env "$e"); done
  for mode in ${MODES:-threads processes}; do
    for sc in ${SCENARIOS:-add update}; do
      $V driver.py --server "$server" ${bin:+--binary "$bin"} --label "$label" "${envs[@]}" \
        --mode $mode -c 16 --values 3 --scenarios $sc --results "$OUT" >/dev/null 2>>"$OUT.err" \
        || echo "FAILED $label $mode $sc" >> "$OUT.err"
    done
  done
}
run zeo zeo ""
run zeo-msgpack zeo-msgpack ""
run zoe-base zoe "$ZOE"
run poc-profile zoe "$ZOE" $P
run tidonly zoe "$ZOE" $P ZOE_POC_TIDONLY=1
run precompress zoe "$ZOE" $P ZOE_POC_PRECOMPRESS=1
run nocompress zoe "$ZOE" $P ZOE_POC_COMPRESSION=none
run zstd1 zoe "$ZOE" $P ZOE_POC_COMPRESSION=zstd:1
run queue64 zoe "$ZOE" $P ZOE_POC_QUEUE=64
run combo zoe "$ZOE" $P ZOE_POC_TIDONLY=1 ZOE_POC_PRECOMPRESS=1 ZOE_POC_ZSTD_TLS=1
run combo-nocompress zoe "$ZOE" $P ZOE_POC_TIDONLY=1 ZOE_POC_COMPRESSION=none
run zoe-base-2 zoe "$ZOE"
run zeo-msgpack-2 zeo-msgpack ""
MODES=processes SCENARIOS=cold run zeo-cold zeo ""
MODES=processes SCENARIOS=cold run zeo-msgpack-cold zeo-msgpack ""
MODES=processes SCENARIOS=cold run zoe-base-cold zoe "$ZOE"
MODES=processes SCENARIOS=cold run zoe-tls-cold zoe "$ZOE" $P ZOE_POC_ZSTD_TLS=1
echo DONE
