#!/usr/bin/env bash
# A wrapper for `driver.py --client-wrap`: py-spy samples zodbshootout and
# its pyperf workers without stopping them. py-spy exits with an error when
# a worker ends first, so the exit code of py-spy does not count.
# Usage: --client-wrap "./wrap-pyspy.sh OUT.txt"
out=$1
shift
"${POC_PYSPY:-py-spy}" record --subprocesses --nonblocking --rate 250 --format raw --output "$out" -- "$@" 2>/dev/null
exit 0
