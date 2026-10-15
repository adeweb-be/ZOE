#!/usr/bin/env bash
# POC ZOE-187 (not for merge): sample the frequency of each CPU every 20 ms
# while a command runs. Print the median MHz of the busy P-cores and E-cores.
# A CPU counts as busy in a sample when its frequency is above 1.5 GHz.
out="$(mktemp)"
( while :; do
    paste -d' ' <(seq 0 21) <(cat /sys/devices/system/cpu/cpu{0..21}/cpufreq/scaling_cur_freq) >> "$out"
    sleep 0.02
  done ) &
sampler=$!
"$@" > /dev/null
kill "$sampler"
python3 - "$out" <<'PY'
import statistics, sys
p, e = [], []
for line in open(sys.argv[1]):
    cpu, khz = map(int, line.split())
    if khz > 1_500_000:
        (p if cpu < 12 else e).append(khz / 1000)
print(f"busy P-core MHz median {statistics.median(p):.0f} (n={len(p)})  busy E-core MHz median {statistics.median(e) if e else 0:.0f} (n={len(e)})")
PY
rm -f "$out"
