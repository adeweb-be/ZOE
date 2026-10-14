# Shared settings of the POC scripts. The environment can override each one.
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo="$(cd "$here/../../.." && pwd)"
# The Python of `just bench`: it has ZEO, ZODB, zodbshootout and msgpack.
V=${POC_PYTHON:-$repo/target/bench/venv-3.13/bin/python}
# The zoe binary of this branch. With no ZOE_POC_* toggle it runs as main.
ZOE=${POC_ZOE:-${CARGO_TARGET_DIR:-$repo/target}/release/zoe}
P=ZOE_POC_PROFILE=1
# The RelStorage wheel links libstdc++, which the Nix interpreters do not
# find on their own. The compiler of the devenv shell knows where it is.
if command -v cc >/dev/null; then
  libstdcxx="$(dirname "$(cc -print-file-name=libstdc++.so.6)")"
  export LD_LIBRARY_PATH="$libstdcxx${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
fi
# POC_LIBPQ: a directory with libpq and the libraries that it needs, without
# glibc. A psycopg2 built from source on a free-threaded interpreter needs it.
if [ -n "${POC_LIBPQ:-}" ]; then
  export LD_LIBRARY_PATH="$LD_LIBRARY_PATH:$POC_LIBPQ"
fi
