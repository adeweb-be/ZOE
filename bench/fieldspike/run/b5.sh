#!/usr/bin/env bash
# The B5 runs of the spike: one repetition of each scenario, 4 and 16 users.
set -u
cd "$HOME/.cache/zoe-build/ZOE-281/src"
source ../env.sh
export ZOE_BINARY="$HOME/.cache/zoe-build/ZOE-281/scripts/zoe-with-log"
for scenario in "$@"; do
    while [ "$(awk '/MemAvailable/{print $2}' /proc/meminfo)" -lt 8000000 ]; do sleep 30; done
    echo "== $scenario $(date -u +%H:%M:%S)"
    flock "$HOME/.cache/zoe-build/heavy.lock" devenv shell -- just bench plone run \
        --stack zoe-native-qi-kv --scenario "$scenario" --steps 4:60,16:60 \
        --port-offset 300 --keep-store --profile zoe.plone:default \
        > "../logs/b5-$scenario.log" 2>&1
    echo "exit $? $(date -u +%H:%M:%S)"
    tail -3 "../logs/b5-$scenario.log"
done
