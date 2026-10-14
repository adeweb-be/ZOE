#!/usr/bin/env bash
# The reduced run of ZOE-150. Run it in devenv shell, on a machine that
# runs nothing else. Not for merge.
#
# Without ZOE150_SERVER_CPUS and ZOE150_CLIENT_CPUS, the server gets the
# first quarter of the CPUs and the clients the rest. The runs go to
# results/run.jsonl and the log to results/run.log, or to ZOE150_LOG.
set -euo pipefail
cd "$(dirname "$0")/../../.."
n=$(nproc)
s=$(( n / 4 > 2 ? n / 4 : 2 ))
export ZOE150_SERVER_CPUS="${ZOE150_SERVER_CPUS:-0-$((s - 1))}"
export ZOE150_CLIENT_CPUS="${ZOE150_CLIENT_CPUS:-$s-$((n - 1))}"
log="${ZOE150_LOG:-bench/poc/ZOE-150/results/run.log}"
mkdir -p "$(dirname "$log")"
target/bench/venv-3.13/bin/python bench/poc/ZOE-150/run.py "$@" 2>&1 | tee -a "$log"
