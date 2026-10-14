#!/usr/bin/env bash
set -u
cd "$(dirname "$0")"
source ./common.sh
# Each store goes to tmpfs, where a sync costs nothing.
export POC_WORK=${POC_WORK:-/dev/shm/zoe-poc-077} POC_CLEANUP=1
show() { python3 -c "import json,sys; r=json.loads(sys.stdin.read()); sc='$1'; print(r['label'], r['mode'], sc, r[sc]['ops'], 'client cores', r['client_cores'], 'client cpu s', r['client_cpu_s'], 'server cpu s', r['server_cpu_s'])"; }
for mode in threads processes; do
  for sc in add update; do
    $V driver.py --server zeo --label client-zeo --mode $mode -c 16 --values 3 --scenarios $sc --results results/client-cpu.jsonl | show $sc
    $V driver.py --server zoe --binary "$ZOE" --label client-zoe-base --mode $mode -c 16 --values 3 --scenarios $sc --results results/client-cpu.jsonl | show $sc
    $V driver.py --server zoe --binary "$ZOE" --label client-zoe-best --env ZOE_POC_STORECOMPRESS=1 --env ZOE_POC_TIDONLY=1 --mode $mode -c 16 --values 3 --scenarios $sc --results results/client-cpu.jsonl | show $sc
  done
done
