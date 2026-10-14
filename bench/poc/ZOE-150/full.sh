#!/usr/bin/env bash
# The full run of ZOE-150 on the reference machine. Run it in devenv shell,
# under the exclusive lock of the machine. Not for merge.
set -euo pipefail
cd "$(dirname "$0")/../../.."
exec target/bench/venv-3.13/bin/python bench/poc/ZOE-150/run.py "$@"
