#!/usr/bin/env bash
# A prefix of the zodbshootout command: py-spy samples zodbshootout and its
# pyperf worker. py-spy fails when a worker ends first, so its exit code
# does not count. Usage: wrap-pyspy.sh PY-SPY OUT.txt COMMAND...
spy=$1 out=$2
shift 2
"$spy" record --subprocesses --nonblocking --rate 250 --format raw --output "$out" -- "$@" 2>/dev/null
exit 0
