# Shared settings of the POC scripts. The environment can override each one.
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo="$(cd "$here/../../.." && pwd)"
# The Python of `just bench`: it has ZEO, ZODB, zodbshootout and msgpack.
V=${POC_PYTHON:-$repo/target/bench/venv-3.13/bin/python}
# The zoe binary of this branch. With no ZOE_POC_* toggle it runs as main.
ZOE=${POC_ZOE:-${CARGO_TARGET_DIR:-$repo/target}/release/zoe}
P=ZOE_POC_PROFILE=1
