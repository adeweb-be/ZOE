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

# Run the zoe-core tests and the race canary under ThreadSanitizer.
tsan:
    #!/usr/bin/env bash
    set -euo pipefail
    # ThreadSanitizer writes each report to a file in this directory. A test
    # that captures the output of a child process cannot hide the report.
    tmp="$(mktemp -d)"
    trap 'rm -rf "$tmp"' EXIT
    # Set all the options here. Then options of the caller cannot suppress
    # a report.
    export TSAN_OPTIONS="log_path=$tmp/report"
    # RUSTC_BOOTSTRAP lets the pinned stable toolchain accept the unstable
    # flags. -Zbuild-std rebuilds the standard library with the sanitizer
    # from the rust-src component. The build goes to target/tsan, apart
    # from the normal build.
    export RUSTC_BOOTSTRAP=1
    export RUSTFLAGS="-Zsanitizer=thread"
    export RUSTDOCFLAGS="-Zsanitizer=thread"
    tsan=(cargo test -Zbuild-std --target x86_64-unknown-linux-gnu \
        --target-dir target/tsan --locked -p zoe-core)
    # A report from a child process that a test kills does not change an
    # exit code. So also look for a report in the files and in the output.
    status=0
    "${tsan[@]}" 2>&1 | tee "$tmp/output" || status=$?
    if [ "$status" -ne 0 ] || grep -q -s 'SUMMARY: ThreadSanitizer:' "$tmp"/*; then
        cat "$tmp"/report.* 2>/dev/null || true
        echo "just tsan: the zoe-core tests failed under ThreadSanitizer." >&2
        exit 1
    fi
    # The canary test holds a deliberate data race. ThreadSanitizer must
    # report it, else the sanitizer is off.
    echo "== The canary test must make ThreadSanitizer report a data race"
    rm -f "$tmp"/*
    status=0
    "${tsan[@]}" --test tsan_canary -- --ignored --exact \
        a_deliberate_data_race_is_reported >"$tmp/output" 2>&1 || status=$?
    if [ "$status" -eq 0 ]; then
        cat "$tmp/output"
        echo "just tsan: the canary test exited clean, so ThreadSanitizer is off." >&2
        exit 1
    fi
    # The stack of the report must name the canary test.
    if ! grep -q -s 'SUMMARY: ThreadSanitizer: data race' "$tmp"/* \
        || ! grep -q -s 'tsan_canary::a_deliberate_data_race_is_reported' "$tmp"/*; then
        cat "$tmp"/*
        echo "just tsan: ThreadSanitizer did not report the race of the canary test." >&2
        exit 1
    fi
    grep -h '^SUMMARY: ThreadSanitizer' "$tmp"/*
    echo "just tsan: ThreadSanitizer reported the race of the canary test, as expected."

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
