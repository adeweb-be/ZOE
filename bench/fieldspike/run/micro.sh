#!/usr/bin/env bash
set -u
cd "$HOME/.cache/zoe-build/ZOE-281"
F=title,description,subject,creators,creation_date,modification_date,effective_date,expiration_date,id,portal_type,_plone.uuid,exclude_from_nav,cmf_uid
echo "load $(cut -d' ' -f1-3 /proc/loadavg)"
taskset -c 8 target/release/examples/fieldspike bench results/sample/docs.bin results/sample/buckets.bin $F 51
for s in edit create-spread; do echo "== $s"; taskset -c 8 target/release/examples/fieldspike decode results/$s/indexer.bin 5; done
for v in 3.13 3.14t; do taskset -c 8 "${PYTHON_VENVS:?the directory of the venvs .venv-3.13 and .venv-3.14t}/.venv-$v/bin/python" src/bench/fieldspike/pybench.py results/sample/docs.bin $F 51; done
echo "load $(cut -d' ' -f1-3 /proc/loadavg)"
