#!/usr/bin/env bash
# POC ZOE-187 (not for merge): the A/B of the memo of hits, interleaved per
# cell: in each round, each cell runs zeo, zoe-native without the memo
# (label ab-off) and zoe-native with it (ab-on), in an order that rotates
# from round to round. So a variant never always follows a heavy cell.
# Usage: ab.sh ROUNDS OUT.jsonl [MEMO]. MEMO is ZOE_POC_HIT_MEMO of ab-on,
# by default 20 MiB (the default cache size) for hits of any size.
set -euo pipefail
rounds="$1"; out="$2"; memo="${3:-20971520}"
py=target/bench/venv-3.14t/bin/python
drive=bench/poc/ZOE-187/drive.py
cells=(threads:1:1024 threads:1:102400 threads:16:1024 threads:16:102400 processes:16:1024 processes:16:102400)
variants=(zeo off on)
for r in $(seq 0 $((rounds - 1))); do
    for cell in "${cells[@]}"; do
        for i in 0 1 2; do
            v="${variants[$(((r + i) % 3))]}"
            case "$v" in
                zeo) "$py" "$drive" --cells "$cell" --values 1 --storages zeo --label ab-off --out "$out" ;;
                off) "$py" "$drive" --cells "$cell" --values 1 --storages zoe-native --label ab-off --out "$out" ;;
                on) "$py" "$drive" --cells "$cell" --values 1 --storages zoe-native \
                        --env "ZOE_POC_HIT_MEMO=$memo" --label ab-on --out "$out" ;;
            esac >/dev/null
        done
    done
done
