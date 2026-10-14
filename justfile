# The single entry point of the project. Run the recipes inside
# `devenv shell`. CLAUDE.md files name these recipes, never raw commands.

set shell := ["bash", "-euo", "pipefail", "-c"]

# The interpreters that the Python tests run on.
pythons := "3.12 3.13 3.14 3.14t"

# List the recipes.
default:
    @just --list

# Check the format and run clippy and the Python lints. The fuzz targets of
# zoe serve are a workspace of their own: check their format, and that their
# Cargo.lock needs no change, so `just fuzz` builds with --locked.
check:
    cargo fmt --all --check
    cargo fmt --manifest-path crates/zoe-server/fuzz/Cargo.toml --check
    cargo metadata --locked --format-version 1 --manifest-path crates/zoe-server/fuzz/Cargo.toml >/dev/null
    cargo clippy --workspace --all-targets --locked -- -D warnings
    ruff check python ci/src
    ruff format --check python ci/src

# Run the Rust tests, then the Python tests on every interpreter.
test:
    cargo test --workspace --locked
    # The bit-flip harness runs in release too. The debug assertions of redb
    # change what a damaged database file does. The harness prints its
    # counts.
    cargo test --release --locked -p zoe-core --test bit_flip_harness -- --show-output
    # The tests of the server run the `zoe` binary that cargo test built.
    export ZOE_BINARY="$(cd "${CARGO_TARGET_DIR:-target}" && pwd)/debug/zoe"; \
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

# Build the fuzz targets of zoe serve with libFuzzer, then run them one
# after the other for `seconds` in all. `just fuzz` runs each of the two
# targets for 30 seconds, and `just fuzz 600 frame` runs only `frame`, for
# 10 minutes. The targets and their seeds are in crates/zoe-server/fuzz. The
# build, the inputs that libFuzzer finds and the crash files go to
# target/fuzz.
fuzz seconds="60" *targets="frame handshake":
    #!/usr/bin/env bash
    set -euo pipefail
    fuzz=crates/zoe-server/fuzz
    out="${CARGO_TARGET_DIR:-target}/fuzz"
    # The flags of `cargo fuzz build`, so cargo-fuzz itself is not needed:
    # the coverage that libFuzzer reads, AddressSanitizer, debug assertions
    # and the cfg fuzzing. RUSTC_BOOTSTRAP lets the pinned stable toolchain
    # accept the unstable flags, as in `just tsan`. The explicit target
    # keeps the flags off the build scripts and the proc macros.
    export RUSTC_BOOTSTRAP=1
    export RUSTFLAGS="-Cpasses=sancov-module \
        -Cllvm-args=-sanitizer-coverage-level=4 \
        -Cllvm-args=-sanitizer-coverage-inline-8bit-counters \
        -Cllvm-args=-sanitizer-coverage-pc-table \
        -Cllvm-args=-sanitizer-coverage-trace-compares \
        -Cllvm-args=-sanitizer-coverage-stack-depth \
        -Cllvm-args=-simplifycfg-branch-fold-threshold=0 \
        --cfg fuzzing -Zsanitizer=address -Cdebug-assertions -Ccodegen-units=1"
    triple=x86_64-unknown-linux-gnu
    cargo build --manifest-path "$fuzz/Cargo.toml" --release --locked \
        --target "$triple" --target-dir "$out/build" \
        --config 'profile.release.debug="line-tables-only"'
    export ASAN_OPTIONS="detect_odr_violation=0"
    # The handshake target runs a server, which makes a store in TMPDIR.
    tmp="$(mktemp -d)"
    trap 'rm -rf "$tmp"' EXIT
    targets=({{ targets }})
    # A time of 0 has no end for libFuzzer, so each target gets 1 second at
    # least.
    each=$(( {{ seconds }} / ${#targets[@]} ))
    if [ "$each" -lt 1 ]; then each=1; fi
    for target in "${targets[@]}"; do
        echo "== $target"
        mkdir -p "$out/corpus/$target" "$out/artifacts/$target"
        options=()
        if [ "$target" = handshake ]; then
            # The server writes a line to stderr for each connection that it
            # closes, so its stderr goes away. The message of a panic goes
            # away too: to see it, run the target on the crash file without
            # -close_fd_mask, as
            # `target/fuzz/build/x86_64-unknown-linux-gnu/release/handshake
            # target/fuzz/artifacts/handshake/crash-...`. The threads of the
            # server free memory after the end of an input, so libFuzzer
            # would look for a leak after nearly each input, and run 20
            # times slower. LeakSanitizer still looks for leaks at the end
            # of the run.
            options=(-close_fd_mask=2 -detect_leaks=0)
        fi
        # libFuzzer adds the new inputs to the first directory. The seeds
        # in the repository do not change.
        TMPDIR="$tmp" "$out/build/$triple/release/$target" \
            -max_total_time="$each" -timeout=30 \
            -print_final_stats=1 "${options[@]}" \
            -artifact_prefix="$out/artifacts/$target/" \
            "$out/corpus/$target" "$fuzz/corpus/$target"
    done

# Run the test suites of ZODB in-process against zoe.Storage, on every
# interpreter. `just test` does not collect them. The ZEO suites against
# zoe serve are not built yet.
conformance:
    for py in {{ pythons }}; do \
        echo "== CPython $py"; \
        UV_PROJECT_ENVIRONMENT=.venv-$py uv run --locked --directory python --python $py pytest -q zoedb/tests/conformance; \
    done

# Build the Plone instance of a scenario under plone/ with buildout, and run
# it. `just plone zoe` builds the zoe scenario and starts its instance in the
# foreground, `just plone zoe build` only builds it, and `just plone zoe
# check` creates a Plone site by script, starts the instance and reads the
# site over HTTP. The other scenarios are filestorage, zeo, zeoclient and
# zeoclient-mount; their instances need a server. `just plone zeoclient
# check` builds the `zoe` binary, starts `zoe serve` on
# target/plone/zeoclient/var/zoe and stops it at the end. `just plone
# zeoclient-mount check` starts one `zoe serve` with the two named storages
# main and content, and puts the site in the mount point /content. The
# build goes to target/plone. ZOE_PLONE_ADMIN gives the admin user as
# user:password. Without it, the recipe makes a random password once, in
# target/plone/admin. The instance listens on 127.0.0.1, on the port
# ZOE_PLONE_PORT or 8080.
plone scenario="zoe" command="start":
    #!/usr/bin/env bash
    set -euo pipefail
    if [ ! -f "plone/{{ scenario }}.cfg" ]; then
        echo "just plone: unknown scenario {{ scenario }}. Use filestorage, zeo, zoe, zeoclient or zeoclient-mount." >&2
        exit 2
    fi
    target="{{ justfile_directory() }}/target/plone"
    instance="$target/{{ scenario }}/bin/instance"
    port="${ZOE_PLONE_PORT:-8080}"
    # The interpreters of the shell know only the C locales. Zope sets the
    # locale of the environment when it starts.
    export LC_ALL=C.UTF-8
    # The admin user of the instance, as user:password. ZOE_PLONE_ADMIN
    # gives it. Without it, the file that ZOE_PLONE_ADMIN_FILE names holds
    # it: the recipe writes a random password there once, and only this
    # user can read the file. The scripts read the password from one of
    # these two. It never goes on a command line or into a file of the
    # build: buildout gets only the user name.
    if [ -n "${ZOE_PLONE_ADMIN:-}" ]; then
        admin="$ZOE_PLONE_ADMIN"
        source=ZOE_PLONE_ADMIN
    else
        export ZOE_PLONE_ADMIN_FILE="$target/admin"
        if [ ! -s "$ZOE_PLONE_ADMIN_FILE" ]; then
            mkdir -p "$target"
            (umask 077 && python3.13 -c \
                'import secrets, sys; print(sys.argv[1] + ":" + secrets.token_urlsafe(18))' \
                admin >"$ZOE_PLONE_ADMIN_FILE")
        fi
        IFS= read -r admin <"$ZOE_PLONE_ADMIN_FILE" || true
        source="$ZOE_PLONE_ADMIN_FILE"
        echo "just plone: the admin user and its password are in $source"
    fi
    # Refuse a value without a colon, a user name or a password. Without a
    # colon, the name could be the password. The message does not show the
    # value.
    user="${admin%%:*}"
    if [ "$user" = "$admin" ] || [ -z "$user" ] || [ -z "${admin#*:}" ]; then
        echo "just plone: $source must be user:password, with a user name and a password." >&2
        exit 2
    fi
    unset admin
    # zc.buildout runs on CPython 3.13, the newest version that Plone 6.1
    # supports. The recipe makes its environment once.
    venv="$target/venv"
    if [ ! -x "$venv/bin/buildout" ]; then
        uv venv --python 3.13 "$venv"
        uv pip install --python "$venv/bin/python" -r plone/requirements.txt
    fi
    # Build the zoedb wheel and install it into target/plone/zoedb. A link in
    # the develop eggs of the scenario points there. Buildout then takes this
    # zoedb, with the files of this build, and does not look for it in the
    # index. The version of zoedb does not change between builds.
    rm -rf "$target/wheels" "$target/zoedb"
    uv build --directory python --package zoedb --wheel --python 3.13 \
        --out-dir "$target/wheels"
    uv pip install --python 3.13 --no-deps --target "$target/zoedb" \
        "$target"/wheels/zoedb-*.whl
    mkdir -p "$target/{{ scenario }}/develop-eggs"
    printf '%s\n.' "$target/zoedb" >"$target/{{ scenario }}/develop-eggs/zoedb.egg-link"
    # The cache of the extended files lets a later build run without the
    # network. Buildout needs its directory before it reads the
    # configuration. zope2instance makes the log directory only when it
    # writes zope.conf, and the zoe scenario writes its own zope.conf.
    mkdir -p "$target/extends" "$target/{{ scenario }}/var/log"
    # -N installs only what is missing. The user has an empty password, so
    # zope2instance writes no inituser file; set_admin.py gives the password
    # in the instance. Only this user reads the record of buildout.
    "$venv/bin/buildout" -N -c "plone/{{ scenario }}.cfg" \
        "buildout:extends-cache=$target/extends" \
        "instance:user=$user:" \
        "instance:http-address=127.0.0.1:$port"
    chmod 600 "$target/{{ scenario }}/.installed.cfg"
    case "{{ command }}" in
        build)
            ;;
        start)
            "$instance" run plone/set_admin.py
            exec "$instance" fg
            ;;
        check)
            # The scripts run in the instance with the storage of the
            # scenario. The instance must not run, because the storage
            # opens in one process only.
            # A new mark in the title of the site tells that read_site.py
            # reads this instance, and not another process on the port.
            mark="mark-$RANDOM$RANDOM"
            # At the exit, stop the instance, then the server, also when a
            # step fails. SIGTERM makes zoe serve close its store cleanly.
            pid=""
            server=""
            stop() {
                if [ -n "$pid" ]; then
                    kill "$pid" 2>/dev/null || true
                    wait "$pid" || true
                fi
                if [ -n "$server" ]; then
                    kill -TERM "$server" 2>/dev/null || true
                    wait "$server" || true
                fi
            }
            trap stop EXIT
            # The instance gives the site from its root, or from the mount
            # point /content of zeoclient-mount.
            folder=""
            case "{{ scenario }}" in
                zeoclient)
                    # zoe serve serves the store of the scenario under the
                    # storage name 1, the default of ZEO.
                    stores=(--path "$target/zeoclient/var/zoe")
                    ;;
                zeoclient-mount)
                    # One zoe serve serves the two databases of the
                    # instance: main at / and content at /content.
                    stores=(--storage "main=$target/zeoclient-mount/var/main"
                        --storage "content=$target/zeoclient-mount/var/content")
                    folder=/content
                    ;;
                *)
                    stores=()
                    ;;
            esac
            if [ "${#stores[@]}" -gt 0 ]; then
                # The instance is a ZEO client of zoe serve. The server is
                # the `zoe` binary of this build. It listens on the
                # zeo-address of zeoclient.cfg.
                cargo build --locked -p zoe-server --bin zoe
                zoe="$(cd "${CARGO_TARGET_DIR:-target}" && pwd)/debug/zoe"
                log="$target/{{ scenario }}/var/log/zoe-serve.log"
                # The server gets no ZOE_* variable of the caller, so only
                # the options here apply. The admin password does not go
                # to the server.
                (
                    for name in "${!ZOE_@}"; do
                        unset "$name"
                    done
                    exec "$zoe" serve "${stores[@]}" --listen 127.0.0.1:8100
                ) </dev/null >"$log" 2>&1 &
                server=$!
                # zoe serve writes `listening <address>` once it accepts
                # connections. Stop when the server exits or does not
                # listen, before a ZEO client waits for it.
                tries=0
                until grep -q '^listening ' "$log"; do
                    tries=$((tries + 1))
                    if ! kill -0 "$server" 2>/dev/null || [ "$tries" -gt 300 ]; then
                        cat "$log" >&2
                        echo "just plone: zoe serve does not listen, see $log" >&2
                        exit 1
                    fi
                    sleep 0.1
                done
                echo "just plone: zoe serve listens on 127.0.0.1:8100, its log is $log"
            fi
            "$instance" run plone/set_admin.py
            "$instance" run plone/create_site.py "$mark" ${folder:+"$folder"}
            "$instance" console &
            pid=$!
            # Wait for the instance, then read the page of the site, which
            # holds the title with the mark. Only the admin user can read
            # the management page of the site.
            site="http://127.0.0.1:$port$folder/Plone"
            python3.13 plone/read_site.py "$site" "Check site $mark"
            python3.13 plone/read_site.py "$site/manage_main" portal_catalog
            ;;
        *)
            echo "just plone: unknown command {{ command }}. Use start, build or check." >&2
            exit 2
            ;;
    esac

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

# Build and run Loupe. `just loupe` builds a release app and runs it,
# `just loupe dev` runs it on the Vite dev server, `just loupe check` lints
# the app, and `just loupe e2e` builds a debug app and runs the end-to-end
# tests under xvfb-run. Run them in the loupe profile of the shell:
# `devenv --profile loupe shell -- just loupe <command>`.
loupe command="run":
    #!/usr/bin/env bash
    set -euo pipefail
    if ! command -v cargo-tauri >/dev/null; then
        echo "just loupe: the Tauri toolchain is in the loupe profile of the shell." >&2
        echo "Run: devenv --profile loupe shell -- just loupe {{ command }}" >&2
        exit 1
    fi
    cd apps/loupe
    # Install the node modules when the lock file is newer than them.
    if [ ! node_modules/.package-lock.json -nt package-lock.json ]; then
        npm ci --no-audit --no-fund
    fi
    # Loupe is a Cargo workspace of its own. Its build goes to target/loupe.
    export CARGO_TARGET_DIR="{{ justfile_directory() }}/target/loupe"
    case "{{ command }}" in
        run)
            cargo tauri build --no-bundle -- --locked
            exec "$CARGO_TARGET_DIR/release/loupe"
            ;;
        dev)
            exec cargo tauri dev
            ;;
        check)
            npm run lint
            rustfmt --check --edition 2024 src-tauri/build.rs src-tauri/src/main.rs
            # The Tauri context embeds dist/, so clippy needs a build of it.
            npm run build
            cargo clippy --manifest-path src-tauri/Cargo.toml --all-targets --locked -- -D warnings
            cargo test --manifest-path src-tauri/Cargo.toml --locked
            ;;
        e2e)
            cargo tauri build --debug --no-bundle -- --locked
            # Xvfb has no GPU, so Mesa renders in software.
            LIBGL_ALWAYS_SOFTWARE=1 exec xvfb-run --auto-servernum npm run e2e
            ;;
        *)
            echo "just loupe: unknown command {{ command }}. Use run, dev, check or e2e." >&2
            exit 2
            ;;
    esac

# Build the documentation site from docs/ into target/book.
docs:
    mdbook build docs

# Run CI functions locally through Dagger. The default is check, test and docs.
ci *functions="check test docs":
    for f in {{ functions }}; do dagger call --mod ci "$f"; done

# Build the release artifacts. `just release` builds the static `zoe`
# binary and the server image, as an OCI tarball, into target/dist through
# the Dagger function release. `just release binary DIR` builds the static
# binary in the shell and copies it to DIR; the Dagger function zoe-binary
# runs it. `just release test` runs the tests of the Dagger functions
# against the Dagger engine. The version bump, the wheels and the Helm chart
# are not built yet.
release command="dist" dir="target/dist":
    #!/usr/bin/env bash
    set -euo pipefail
    case "{{ command }}" in
        dist)
            dagger call --mod ci release export --path "{{ dir }}"
            ;;
        binary)
            # musl links the binary statically, so it needs no C library
            # and no dynamic loader on the host.
            target=x86_64-unknown-linux-musl
            cargo build --release --locked --target "$target" \
                -p zoe-server --features serve --bin zoe
            mkdir -p "{{ dir }}"
            cp "${CARGO_TARGET_DIR:-target}/$target/release/zoe" "{{ dir }}/zoe"
            ;;
        test)
            # uv installs the Python SDK of the module from ci/sdk, which
            # dagger develop writes.
            dagger develop --mod ci
            uv run --locked --directory ci pytest -q tests
            ;;
        *)
            echo "just release: unknown command {{ command }}. Use dist, binary or test." >&2
            exit 2
            ;;
    esac
