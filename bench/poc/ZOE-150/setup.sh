#!/usr/bin/env bash
# Build what the ZOE-150 run needs. Run it in devenv shell, from any
# directory. Not for merge.
#
# - target/zoe150/zoe-main: `zoe serve` as on main. ZOE_MAIN names a
#   release binary built from main; without it, the script builds main
#   from MAIN_REV (origin/main by default) in a temporary worktree.
# - target/zoe150/zoe-proto: this branch, main with the toggles of ZOE-077.
# - target/bench/venv-3.13 and venv-3.14t: zodbshootout, ZODB and ZEO from
#   the lock of bench/shootout, without zoedb and RelStorage.
# - the POC client of ZOE-077/rustclient for each interpreter.
# - target/zoe150/tools: py-spy.
set -euo pipefail
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo="$(cd "$here/../../.." && pwd)"
out="$repo/target/zoe150"
mkdir -p "$out"
cd "$repo"

cargo build --release --locked -p zoe-server --bin zoe
cp "${CARGO_TARGET_DIR:-target}/release/zoe" "$out/zoe-proto"

if [ -n "${ZOE_MAIN:-}" ]; then
  cp "$ZOE_MAIN" "$out/zoe-main"
else
  rev="${MAIN_REV:-origin/main}"
  tree="$out/main-src"
  git worktree remove --force "$tree" 2>/dev/null || true
  git worktree add --detach "$tree" "$rev"
  (cd "$tree" && CARGO_TARGET_DIR="$out/main-target" cargo build --release --locked -p zoe-server --bin zoe)
  cp "$out/main-target/release/zoe" "$out/zoe-main"
  git worktree remove --force "$tree"
fi

for py in ${PYTHONS:-3.13 3.14t}; do
  venv="$repo/target/bench/venv-$py"
  interp="python$py"
  UV_PROJECT_ENVIRONMENT="$venv" uv sync --locked --project bench/shootout \
    --python "$interp" --no-dev \
    --no-install-package zoedb --no-install-package RelStorage \
    --no-install-package psycopg2-binary
  POC_PYTHON="$venv/bin/python" CARGO_TARGET_DIR="$repo/target/poc-077-rustclient-$py" \
    "$here/../ZOE-077/rustclient/build.sh"
done

if [ ! -x "$out/tools/bin/py-spy" ]; then
  uv venv --quiet --python "$repo/target/bench/venv-3.13/bin/python" "$out/tools"
  uv pip install --quiet --python "$out/tools/bin/python" py-spy
fi

{
  echo "zoe-main $("$out/zoe-main" --version) $(sha256sum "$out/zoe-main" | cut -c1-16)"
  echo "zoe-proto $("$out/zoe-proto" --version) $(sha256sum "$out/zoe-proto" | cut -c1-16) $(git rev-parse --short=12 HEAD)"
} | tee "$out/binaries.txt"
