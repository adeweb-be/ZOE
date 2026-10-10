# The single entry point of the project. Run the recipes inside
# `devenv shell`. CLAUDE.md files name these recipes, never raw commands.

set shell := ["bash", "-euo", "pipefail", "-c"]

# The interpreters that the Python tests run on.
pythons := "3.12 3.13 3.14 3.14t"

# List the recipes.
default:
    @just --list

# Check the format and run clippy and the Python lints.
check:
    cargo fmt --all --check
    cargo clippy --workspace --all-targets --locked -- -D warnings
    ruff check python ci/src
    ruff format --check python ci/src

# Run the Rust tests, then the Python tests on every interpreter.
test:
    cargo test --workspace --locked
    for py in {{ pythons }}; do \
        echo "== CPython $py"; \
        UV_PROJECT_ENVIRONMENT=.venv-$py uv run --locked --directory python --python $py pytest -q; \
    done

# Run the ZODB and ZEO conformance suites.
conformance:
    @echo "just conformance: runs ZODB.tests in-process and ZEO.tests against zoe serve. Not built yet."

# Build the Plone instances under plone/ with buildout.
plone:
    @echo "just plone: builds the filestorage, zeo, zoe and zeoclient scenarios under plone/. Not built yet."

# Run the benchmarks. `just bench` runs the zodbshootout baseline into
# target/bench/results, `just bench run --quick` runs the quick set and
# `just bench test` runs the tests of the harness. zoe-bench and the
# criterion benchmarks are not built yet.
bench *args="run":
    #!/usr/bin/env bash
    set -euo pipefail
    export UV_PYTHON="${UV_PYTHON:-3.13}"
    export UV_PROJECT_ENVIRONMENT="{{ justfile_directory() }}/target/bench/venv-$UV_PYTHON"
    # The RelStorage wheel links libstdc++, which the Nix interpreters do not
    # find on their own. The compiler of the shell knows where it is.
    if command -v cc >/dev/null; then
        libstdcxx="$(dirname "$(cc -print-file-name=libstdc++.so.6)")"
        export LD_LIBRARY_PATH="$libstdcxx${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
    fi
    shootout=(uv run --locked --project bench/shootout)
    if [ "{{ args }}" = test ]; then
        exec "${shootout[@]}" pytest -q bench/shootout/tests
    fi
    exec "${shootout[@]}" python -m shootout {{ args }}

# Build and run Loupe.
loupe:
    @echo "just loupe: builds and runs the Loupe desktop app from apps/loupe/. Not built yet."

# Build the documentation site from docs/ into target/book.
docs:
    mdbook build docs

# Run CI functions locally through Dagger. The default is check, test and docs.
ci *functions="check test docs":
    for f in {{ functions }}; do dagger call --mod ci "$f"; done

# Cut a release.
release:
    @echo "just release: bumps the versions in lockstep and builds the wheels, the binaries, the image and the Helm chart. Not built yet."
