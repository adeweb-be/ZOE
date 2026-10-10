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

# Run the benchmarks.
bench:
    @echo "just bench: runs zodbshootout, zoe-bench and the criterion benchmarks. Not built yet."

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
