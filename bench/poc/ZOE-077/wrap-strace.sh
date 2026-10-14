#!/usr/bin/env bash
# Run a command under strace -c. On SIGTERM, stop the traced child (the
# direct child of strace), so that strace writes its summary.
out=$1; shift
strace -f -c -o "$out" "$@" &
spid=$!
trap 'pkill -TERM -P "$spid"' TERM
wait "$spid"
wait "$spid"
