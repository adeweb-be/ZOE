#!/usr/bin/env bash
# Wait for run1, then for a machine without cargo or pytest, then time run2
# under the exclusive lock. Inside the hold, check the load again.
S=${BENCH_LOCK_DIR:?the directory of bench.lock}
D=$(cd "$(dirname "$0")" && pwd)
busy() { ps -eo pid=,args= | grep -v -E '^ *(103308|123628) ' | grep -E ' (cargo|[^ ]*/cargo) |pytest|/deps/zoe_|racex' | grep -v grep | grep -q .; }
until [ "$(grep -c '^# ' $D/results/run1/timing.jsonl 2>/dev/null)" -ge 3 ]; do sleep 10; done
echo "run1 done $(date -u +%T)"
while true; do
  until ! busy; do sleep 15; done
  rc=0
  flock -x $S/bench.lock bash -c '
    busy() { ps -eo pid=,args= | grep -v -E "^ *(103308|123628) " | grep -E " (cargo|[^ ]*/cargo) |pytest|/deps/zoe_|racex" | grep -v grep | grep -q .; }
    if busy; then exit 42; fi
    { ps -eo pid,etime,pcpu,args --sort=-pcpu | head -8; ps -o pid,ni,etime,pcpu,time,args -p 103308,123628; } > '"$D"'/results/run2.ps-before.txt
    cd '"$D"' && POC_PYTHON=/home/aduchene/Projects/zoe/target/bench/venv-3.13/bin/python CARGO_TARGET_DIR=/home/aduchene/.cache/zoe-build/ZOE-152-poc/src/target/poc-152 ./timing.sh '"$D"'/results/run2 '"$S"'/z152/pickles
    { ps -eo pid,etime,pcpu,args --sort=-pcpu | head -8; ps -o pid,ni,etime,pcpu,time,args -p 103308,123628; } > '"$D"'/results/run2.ps-after.txt
  ' || rc=$?
  if [ $rc -eq 42 ]; then echo "busy inside the hold, retry $(date -u +%T)"; sleep 30; continue; fi
  echo "run2 done rc=$rc $(date -u +%T)"; break
done
