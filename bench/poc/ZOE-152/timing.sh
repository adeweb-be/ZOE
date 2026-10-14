#!/usr/bin/env bash
# The timing runs of the finding. Run them under the exclusive lock of the
# machine: flock -x LOCK ./timing.sh OUTDIR PICKLES
set -eu
cd "$(dirname "$0")"
out=$1
pk=$2
py=${POC_PYTHON:-../../../../target/bench/venv-3.13/bin/python}
bin=${CARGO_TARGET_DIR:-../../../../target/poc-152}/release
mkdir -p "$out"
export PYTHONDONTWRITEBYTECODE=1
{
  date -u +%FT%TZ | sed 's/^/# start /'
  uptime | sed 's/^/# /'
  "$bin/micro" codec "$pk/p1024.bin"
  "$bin/micro" codec "$pk/p102400.bin"
  "$bin/micro" lookup "$pk/p1024.bin"
  "$bin/micro" lookup "$pk/p102400.bin"
  "$bin/micro" lock "$out"
  "$py" hit.py --objects 1000 --rounds 30 --threads 1
  "$py" hit.py --objects 100 --rounds 30 --threads 16
  "$py" hit.py --rounds 15 --threads 1 --size 102400 --cache-mb 256 --objects 200 --kinds zeo,proto-shared,proto-shared-zstd
  uptime | sed 's/^/# /'
} > "$out/timing.jsonl" 2>&1
