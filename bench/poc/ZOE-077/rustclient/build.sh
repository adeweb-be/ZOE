#!/usr/bin/env bash
# Build the binding and put it next to python/zoeclientx. Run it in devenv.
set -eu
cd "$(dirname "$0")"
py=${POC_PYTHON:-../../../../target/bench/venv-3.13/bin/python}
export CARGO_TARGET_DIR=${CARGO_TARGET_DIR:-../../../../target/poc-077-rustclient}
PYO3_BUILD_EXTENSION_MODULE=1 PYO3_PYTHON="$py" cargo build --release
suffix=$("$py" -c "import sysconfig; print(sysconfig.get_config_var('EXT_SUFFIX'))")
cp "$CARGO_TARGET_DIR/release/lib_zoeclientx.so" "python/zoeclientx/_zoeclientx$suffix"
