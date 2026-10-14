#!/usr/bin/env bash
# The race tests of ZODB, with and without the rule, on 3.13 and 3.14t.
# Each batch of runs takes the lock POC_LOCK as shared, so a timing run on
# the same machine waits for the end of a batch. Set POC_PYTHONS and
# POC_ZOE. RUNS_ON and RUNS_OFF give the runs of each test.
set -u
cd "$(dirname "$0")"
OUT=${OUT:-results/race.jsonl}
LOCK=${POC_LOCK:-/tmp/zoe-poc-bench.lock}
BATCH=${BATCH:-100}
batch() {  # python pool rule runs tests...
  local py=$1 pool=$2 rule=$3 runs=$4; shift 4
  local done=0
  while [ "$done" -lt "$runs" ]; do
    local n=$(( runs - done < BATCH ? runs - done : BATCH ))
    flock -s "$LOCK" "$py" racex.py --binary "$POC_ZOE" --pool "$pool" --rule "$rule" \
      --runs "$n" --tests "$@" --results "$OUT" | grep -v ' pass ' || true
    done=$(( done + n ))
  done
}
for py in $POC_PYTHONS; do
  batch "$py" 4 on "${RUNS_ON:-1000}" loadopen external external-touch
  batch "$py" 16 on "${RUNS_ON16:-200}" loadopen external external-touch
  batch "$py" 1 on "${RUNS_ON16:-200}" loadopen external external-touch
  batch "$py" 4 off "${RUNS_OFF:-200}" loadopen external external-touch
  batch "$py" 1 off "${RUNS_OFF:-200}" loadopen external external-touch
done
echo DONE
