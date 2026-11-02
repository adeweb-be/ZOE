#!/usr/bin/env bash
# Dump a kept B5 store and compute the numbers of the spike.
set -eu
cd "$HOME/.cache/zoe-build/ZOE-281"
scenario="$1"
R=$(ls -d src/target/bench/plone/runs/*-"$scenario"-zoe-native-qi-kv-r0)
F=title,description,subject,creators,creation_date,modification_date,effective_date,expiration_date,id,portal_type,_plone.uuid,exclude_from_nav,cmf_uid
out="results/$scenario"
taskset -c 12-21 src/target/plone/zoeclient/parts/instance/bin/interpreter src/bench/fieldspike/dump.py "$R" "$out" 2>&1 | grep -v pkg_resources | tail -2
taskset -c 12-21 target/release/examples/fieldspike fields "$out/content.bin" "$out/content.jsonl" "$F"
taskset -c 12-21 target/release/examples/fieldspike fields "$out/conflicts.bin" "$out/conflicts.out.jsonl" "$F"
python3 src/bench/fieldspike/analyze.py "$R" "$out" "$F" > "$out/analysis.json"
taskset -c 12-21 src/target/plone/zoeclient/parts/instance/bin/interpreter src/bench/fieldspike/votes.py "$R" "$out" 2>&1 | grep -v pkg_resources | tail -1
taskset -c 12-21 target/release/examples/fieldspike fields "$out/votes.bin" "$out/votes.out.jsonl" "$F"
python3 src/bench/fieldspike/analyze_votes.py "$R" "$out" "$F" > "$out/votes-analysis.json"
