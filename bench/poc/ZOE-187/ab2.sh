#!/usr/bin/env bash
# POC ZOE-187 (not for merge): the regression A/B of the memo of hits,
# interleaved as ab.sh: in each round, each scenario and cell runs zeo,
# zoe-native without the memo (<scenario>-off) and with it (<scenario>-on),
# in an order that rotates. Usage: ab2.sh ROUNDS OUT.jsonl.
#
# - cold:     read_cold, the population of an empty cache (memo 20 MiB).
# - warm:     read_warm, commits with invalidations (memo 20 MiB).
# - memo4m:   read_hot, a memo of 4 MiB under a working set of 10 MB.
# - ring6m:   read_hot, a ring of 6 MB under a working set of 10 MB.
# - big:      read_hot, threads 16, a cache of 256 MB for both clients
#             (memo 256 MiB), so the hot reads can hit.
set -euo pipefail
rounds="$1"; out="$2"
py=target/bench/venv-3.14t/bin/python
drive=bench/poc/ZOE-187/drive.py
variants=(zeo off on)
# scenario | memo | cells | extra options
specs=(
    "cold|20971520|threads:1:102400 processes:16:102400|--bench cold"
    "warm|20971520|threads:16:102400 processes:16:102400|--bench warm"
    "memo4m|4194304|threads:1:102400 processes:16:102400|"
    "ring6m|20971520|threads:1:102400 processes:16:102400|--cache-size 6000000"
    "big|268435456|threads:16:102400|--cache-size 268435456"
)
for r in $(seq 0 $((rounds - 1))); do
    for spec in "${specs[@]}"; do
        IFS='|' read -r name memo cells extra <<<"$spec"
        for cell in $cells; do
            for i in 0 1 2; do
                v="${variants[$(((r + i) % 3))]}"
                # shellcheck disable=SC2086
                case "$v" in
                    zeo) "$py" "$drive" --cells "$cell" --values 1 --storages zeo --label "$name-off" --out "$out" $extra ;;
                    off) "$py" "$drive" --cells "$cell" --values 1 --storages zoe-native --label "$name-off" --out "$out" $extra ;;
                    on) "$py" "$drive" --cells "$cell" --values 1 --storages zoe-native \
                            --env "ZOE_POC_HIT_MEMO=$memo" --label "$name-on" --out "$out" $extra ;;
                esac >/dev/null
            done
        done
    done
done
