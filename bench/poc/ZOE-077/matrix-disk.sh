#!/usr/bin/env bash
# Run the ZOE-077 POC matrix, one run at a time.
set -u
cd "$(dirname "$0")"
source ./common.sh
OUT=${OUT:-results/matrix-disk.jsonl}
run() {  # label server binary envs...
  local label=$1 server=$2 bin=$3; shift 3
  local envs=()
  for e in "$@"; do envs+=(--env "$e"); done
  for mode in threads processes; do
    for sc in add update; do
      $V driver.py --server "$server" ${bin:+--binary "$bin"} --label "$label" "${envs[@]}" \
        --mode $mode -c 16 --values 3 --scenarios $sc --results "$OUT" >/dev/null 2>>"$OUT.err" \
        || echo "FAILED $label $mode $sc" >> "$OUT.err"
      tail -1 "$OUT" | cut -c1-200
    done
  done
}
run zeo zeo ""
run zeo-msgpack zeo-msgpack ""
run zoe-base zoe "$ZOE"
run poc-profile zoe "$ZOE" $P
run tidonly zoe "$ZOE" $P ZOE_POC_TIDONLY=1
run zstd-tls zoe "$ZOE" $P ZOE_POC_ZSTD_TLS=1
run precompress-tls zoe "$ZOE" $P ZOE_POC_PRECOMPRESS=1 ZOE_POC_ZSTD_TLS=1
run nocompress zoe "$ZOE" $P ZOE_POC_COMPRESSION=none
run nosync zoe "$ZOE" $P ZOE_POC_NOSYNC=1
run queue64 zoe "$ZOE" $P ZOE_POC_QUEUE=64
run all3 zoe "$ZOE" $P ZOE_POC_TIDONLY=1 ZOE_POC_ZSTD_TLS=1 ZOE_POC_PRECOMPRESS=1
run zoe-base-2 zoe "$ZOE"
echo DONE
