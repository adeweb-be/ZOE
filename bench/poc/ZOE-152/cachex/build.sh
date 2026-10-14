#!/usr/bin/env bash
# Build the sim binary and the Python module, and put the module in python/.
# Run it in devenv. POC_PYTHON is the interpreter of the module.
set -eu
cd "$(dirname "$0")"
py=${POC_PYTHON:-../../../../target/bench/venv-3.13/bin/python}
export CARGO_TARGET_DIR=${CARGO_TARGET_DIR:-../../../../target/poc-152}
cargo build --release --bins
PYO3_BUILD_EXTENSION_MODULE=1 PYO3_PYTHON="$py" cargo build --release --lib --features python
suffix=$("$py" -c "import sysconfig; print(sysconfig.get_config_var('EXT_SUFFIX'))")
cp "$CARGO_TARGET_DIR/release/libcachex.so" "python/_cachex$suffix"
