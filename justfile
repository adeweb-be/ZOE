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
# Cargo.lock needs no change, so `just fuzz` builds with --locked. The
# harnesses of bench/ that build zoedb must pin the forks of persistent,
# zodbpickle and BTrees that python/ pins.
check:
    cargo fmt --all --check
    cargo fmt --manifest-path crates/zoe-server/fuzz/Cargo.toml --check
    cargo metadata --locked --format-version 1 --manifest-path crates/zoe-server/fuzz/Cargo.toml >/dev/null
    cargo clippy --workspace --all-targets --locked -- -D warnings
    ruff check python ci/src deploy
    ruff format --check python ci/src deploy
    PYTHONPATH=bench/common/src python3.13 -m benchcommon.pins

# Run the Rust tests, then the Python tests on every interpreter, then the
# checks of the deployment files.
test:
    cargo test --workspace --locked
    # The bit-flip harness runs in release too. The debug assertions of redb
    # change what a damaged database file does. The harness prints its
    # counts.
    cargo test --release --locked -p zoe-core --test bit_flip_harness -- --show-output
    # The tests of the server run the `zoe` binary that cargo test built.
    ZOE_BINARY="$(cd "${CARGO_TARGET_DIR:-target}" && pwd)/debug/zoe" just _pytest "{{ pythons }}"
    just deploy check

# Run pytest in the uv workspace of python/ on each interpreter of `only`,
# with the pytest arguments that follow.
_pytest only *args:
    for py in {{ only }}; do \
        echo "== CPython $py"; \
        UV_PROJECT_ENVIRONMENT=.venv-$py uv run --locked --directory python --python $py pytest -q {{ args }}; \
    done

# Build the `zoe` binary in the profile `debug` or `release`, and print its
# absolute path. The recipes export it as ZOE_BINARY.
_zoe profile:
    #!/usr/bin/env bash
    set -euo pipefail
    case "{{ profile }}" in
        debug) flags=() ;;
        release) flags=(--release) ;;
        *) echo "just _zoe: the profile is debug or release." >&2; exit 2 ;;
    esac
    cargo build "${flags[@]}" --locked -p zoe-server --bin zoe >&2
    echo "$(cd "${CARGO_TARGET_DIR:-target}" && pwd)/{{ profile }}/zoe"

# Run tests and the race canary under ThreadSanitizer. `just tsan` runs the
# tests of zoe-core, and `just tsan zoe-core FILTER` only those whose name
# holds FILTER. `just tsan zoe-server FILTER` runs the tests of the library
# of zoe-server whose name holds FILTER. The canary test runs in each case.
tsan crate="zoe-core" filter="":
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
    # The sanitizer needs an explicit target. Use the target of the host.
    triple="$(rustc -vV | sed -n 's/^host: //p')"
    tsan=(cargo test -Zbuild-std --target "$triple" \
        --target-dir target/tsan --locked)
    if [ "{{crate}}" = zoe-core ]; then
        run=("${tsan[@]}" -p zoe-core -- "{{filter}}")
    else
        run=("${tsan[@]}" -p "{{crate}}" --lib -- "{{filter}}")
    fi
    # A report from a child process that a test kills does not change an
    # exit code. So also look for a report in the files and in the output.
    status=0
    "${run[@]}" 2>&1 | tee "$tmp/output" || status=$?
    if [ "$status" -ne 0 ] || grep -q -s 'SUMMARY: ThreadSanitizer:' "$tmp"/*; then
        cat "$tmp"/report.* 2>/dev/null || true
        echo "just tsan: the tests of {{crate}} failed under ThreadSanitizer." >&2
        exit 1
    fi
    # The canary test holds a deliberate data race. ThreadSanitizer must
    # report it, else the sanitizer is off.
    echo "== The canary test must make ThreadSanitizer report a data race"
    rm -f "$tmp"/*
    status=0
    "${tsan[@]}" -p zoe-core --test tsan_canary -- --ignored --exact \
        a_deliberate_data_race_is_reported >"$tmp/output" 2>&1 || status=$?
    if [ "$status" -eq 0 ]; then
        cat "$tmp/output"
        echo "just tsan: the canary test exited clean, so ThreadSanitizer is off." >&2
        exit 1
    fi
    # The stack of the report must name the canary test. Linux gives the
    # name demangled. macOS gives the v0 symbol of Rust.
    canary='tsan_canary::a_deliberate_data_race_is_reported'
    canary="$canary|11tsan_canary(s[0-9A-Za-z]*_)?34a_deliberate_data_race_is_reported"
    if ! grep -q -s 'SUMMARY: ThreadSanitizer: data race' "$tmp"/* \
        || ! grep -q -s -E "$canary" "$tmp"/*; then
        cat "$tmp"/*
        echo "just tsan: ThreadSanitizer did not report the race of the canary test." >&2
        exit 1
    fi
    grep -h '^SUMMARY: ThreadSanitizer' "$tmp"/*
    echo "just tsan: ThreadSanitizer reported the race of the canary test, as expected."

# Build the fuzz targets of zoe serve with libFuzzer, then run them one
# after the other for `seconds` in all. `just fuzz` runs each of the four
# targets for 15 seconds, and `just fuzz 600 frame` runs only `frame`, for
# 10 minutes. The targets and their seeds are in crates/zoe-server/fuzz. The
# build, the inputs that libFuzzer finds and the crash files go to
# target/fuzz.
fuzz seconds="60" *targets="frame handshake merge fields":
    #!/usr/bin/env bash
    set -euo pipefail
    fuzz=crates/zoe-server/fuzz
    out="${CARGO_TARGET_DIR:-target}/fuzz"
    # The flags of `cargo fuzz build`, so cargo-fuzz itself is not needed:
    # the coverage that libFuzzer reads, AddressSanitizer, debug assertions
    # and the cfg fuzzing. RUSTC_BOOTSTRAP lets the pinned stable toolchain
    # accept the unstable flags, as in `just tsan`. The explicit target, the
    # target of the host, keeps the flags off the build scripts and the proc
    # macros.
    triple="$(rustc -vV | sed -n 's/^host: //p')"
    # The depth of the stack needs the symbol __sancov_lowest_stack.
    # libFuzzer does not give it on macOS, so the flag is for Linux only.
    depth=""
    case "$triple" in
        *-linux-*) depth="-Cllvm-args=-sanitizer-coverage-stack-depth" ;;
    esac
    export RUSTC_BOOTSTRAP=1
    export RUSTFLAGS="-Cpasses=sancov-module \
        -Cllvm-args=-sanitizer-coverage-level=4 \
        -Cllvm-args=-sanitizer-coverage-inline-8bit-counters \
        -Cllvm-args=-sanitizer-coverage-pc-table \
        -Cllvm-args=-sanitizer-coverage-trace-compares \
        $depth \
        -Cllvm-args=-simplifycfg-branch-fold-threshold=0 \
        --cfg fuzzing -Zsanitizer=address -Cdebug-assertions -Ccodegen-units=1"
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
            # `target/fuzz/build/<host target>/release/handshake
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

# Run the test suites of ZODB in-process against zoe.Storage, and ZEO's
# suites against zoe serve, on every interpreter. `just test` does not
# collect them. The test forker of ZEO starts the `zoe` binary of the debug
# build. `just conformance PYTHONS TESTS` runs only the given interpreters
# and pytest arguments, for example
# `just conformance 3.13 zoedb/tests/conformance/test_zeo_suites.py`.
conformance only=pythons *tests="zoedb/tests/conformance":
    ZOE_BINARY="$(just _zoe debug)"; \
    export ZOE_BINARY; \
    just _pytest "{{ only }}" {{ tests }}

# Run the process kill loop against `zoe serve` for `runs` rounds, on
# CPython 3.13, then the kill loop under the simulator for `seeds` seeds.
# Each round of the process kill loop kills the server with SIGKILL while
# several writers commit objects and blobs, restarts it, stops it cleanly,
# runs `zoe verify` and loads every acknowledged commit. Without a seed, the
# recipe takes a random one. It prints the seed. The traceback of a failure
# shows its local variables, so it gives the round. `just crash RUNS SEED`
# repeats the kill times of a run, but not the order of the threads. The
# store and the blob caches go to TMPDIR: 10,000 rounds need about 3 GB
# there. The simulated kill loop (`tests/simulated_kill_loop.rs` of
# zoe-server, in release) crashes the simulated server with a power loss
# at each phase of a commit, for `seeds` seeds. Then the split writers of
# `tests/simulated_split_merges.rs` run for `split_seeds` seeds: they split
# one BTree with the split merge on, and the driver checks the tree after
# each recovery. A failing seed prints `ZOE_SIM_SEED=<seed>`, and
# `ZOE_SIM_SEED` runs that seed alone. `just test` runs both with their
# default seeds, and does not collect the process kill loop.
crash runs="200" seed="" seeds="200" split_seeds="200":
    #!/usr/bin/env bash
    set -euo pipefail
    seed="{{ seed }}"
    if [ -z "$seed" ]; then
        seed=$(( (RANDOM << 15) | RANDOM ))
    fi
    # The server is the `zoe` binary of this build.
    ZOE_BINARY="$(just _zoe debug)"
    export ZOE_BINARY
    export ZOE_KILL_LOOP_RUNS="{{ runs }}" ZOE_KILL_LOOP_SEED="$seed"
    echo "just crash: {{ runs }} rounds, seed $seed"
    status=0
    UV_PROJECT_ENVIRONMENT=.venv-3.13 uv run --locked --directory python --python 3.13 \
        pytest -q --showlocals zoedb/tests/crash || status=$?
    if [ "$status" -ne 0 ]; then
        echo "just crash: the kill loop failed. Run it again with: just crash {{ runs }} $seed" >&2
    fi
    for run in "simulated_kill_loop {{ seeds }}" "simulated_split_merges {{ split_seeds }}"; do
        read -r test count <<<"$run"
        echo "just crash: $test, $count seeds"
        simulated=0
        ZOE_SIM_SEEDS="$count" cargo test --release --locked -p zoe-server \
            --test "$test" || simulated=$?
        if [ "$simulated" -ne 0 ]; then
            echo "just crash: $test failed. Run the seed that it names with:" \
                "ZOE_SIM_SEED=<seed> cargo test --release -p zoe-server --test $test" >&2
            status=$simulated
        fi
    done
    exit "$status"

# Build the Plone instance of a scenario under plone/ with buildout, and run
# it. `just plone zoe` builds the zoe scenario and starts its instance in the
# foreground, `just plone zoe build` only builds it, and `just plone zoe
# check` creates a Plone site by script, starts the instance and reads the
# site over HTTP. The other scenarios are filestorage, zeo, zeoclient,
# zoeclient and zeoclient-mount. The instances of zeo, zeoclient, zoeclient
# and zeoclient-mount need their server. `just plone zeoclient check` builds
# the `zoe` binary, starts `zoe serve` on target/plone/zeoclient/var/zoe and
# stops it at the end; `just plone zoeclient check` does the same on
# target/plone/zoeclient/var/zoe. `just plone zeoclient-mount check` starts
# one `zoe serve` with the two named storages main and content, and puts
# the site in the mount point /content. `just plone zoe suites` runs the test suites of
# Products.CMFPlone, plone.app.contenttypes and plone.restapi with the
# `bin/test` of the scenario, on the layer of zoe.plone.testing; the options
# that follow go to `bin/test`. `just plone zeoclient suites` runs them
# against a `zoe serve` on a new store, which it starts and stops, and
# `just plone filestorage suites` runs them on the stock storages of
# plone.testing. `just plone zoe backends` runs the suite of plone.locking
# on the locking backend of zoe.plone, with the tests of zoe.plone; it
# builds the `zoe` binary first, for the tests that start `zoe serve`.
# `just plone <scenario> creators THREADS ITEMS SEED` creates a Plone site,
# sets the counter of portal_uidgenerator to SEED, and runs
# plone/creators.py: THREADS threads that create ITEMS Documents each. Its
# last line of stdout is a JSON report. It runs on zoe, zeoclient and
# zoeclient; for zeoclient and zoeclient, it starts `zoe serve` on a new
# store, target/plone/<scenario>/var/creators, and stops it at the end.
# `just plone <scenario> history THREADS` creates a Plone site and runs
# plone/history.py: the profile of zoe.plone writes the merge policy, the
# behaviour zoe.plone.history and its upgrade step, THREADS transitions of
# one Document from one snapshot, and two changes of two fields of it. Its
# last line of stdout is a JSON report. It runs on zoe and zoeclient; for
# zoeclient, it starts `zoe serve` on a new store,
# target/plone/zoeclient/var/history, and stops it at the end.
# The build goes to target/plone. ZOE_PLONE_ADMIN gives the
# admin user as user:password. Without it, the recipe makes a random
# password once, in target/plone/admin. The instance listens on 127.0.0.1,
# on the port ZOE_PLONE_PORT or 8080.
# `just plone test` runs the tests of plone/tests, which run these
# recipes, on CPython 3.13; `just plone test FILE...` runs the pytest
# arguments that follow, with paths of the repository.
[positional-arguments]
plone scenario="zoe" command="start" *args:
    #!/usr/bin/env bash
    set -euo pipefail
    # just gives the arguments of the recipe as positional arguments, each
    # unchanged: the scenario, the command, then the arguments that follow.
    args=("${@:3}")
    if [ "{{ scenario }}" = test ]; then
        # The arguments of pytest start at the command, which is start
        # when no argument follows test.
        tests=("${@:2}")
        if [ "${tests[*]}" = start ]; then
            tests=(plone/tests)
        fi
        export UV_PROJECT_ENVIRONMENT="{{ justfile_directory() }}/python/.venv-3.13"
        exec uv run --locked --project python --python 3.13 pytest -q "${tests[@]}"
    fi
    if [ ! -f "plone/{{ scenario }}.cfg" ]; then
        echo "just plone: unknown scenario {{ scenario }}. Use filestorage, zeo, zoe, zeoclient, zoeclient or zeoclient-mount." >&2
        exit 2
    fi
    target="{{ justfile_directory() }}/target/plone"
    instance="$target/{{ scenario }}/bin/instance"
    port="${ZOE_PLONE_PORT:-8080}"
    # The address of the server of a ZEO client instance: the zeo-address
    # of zeoclient.cfg, which templates/zoeclient.conf.in also gives.
    zeo_address=127.0.0.1:8100
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
    # The suites need the part `test` of base.cfg, the test script. The
    # suites run on the stock storages of plone.testing, on zoe.Storage or
    # on a ZEO client of zoe serve.
    parts=()
    if [ "{{ command }}" = suites ]; then
        case "{{ scenario }}" in
            filestorage | zoe | zeoclient)
                ;;
            *)
                echo "just plone: the suites run on filestorage, zoe or zeoclient." >&2
                exit 2
                ;;
        esac
        parts=("buildout:parts+=test")
    fi
    if [ "{{ command }}" = backends ]; then
        if [ "{{ scenario }}" != zoe ]; then
            echo "just plone: the suites of the backends run on zoe." >&2
            exit 2
        fi
        parts=("buildout:parts+=test")
    fi
    # -N installs only what is missing. The user has an empty password, so
    # zope2instance writes no inituser file; set_admin.py gives the password
    # in the instance. Only this user reads the record of buildout.
    "$venv/bin/buildout" -N -c "plone/{{ scenario }}.cfg" \
        "buildout:extends-cache=$target/extends" \
        "instance:user=$user:" \
        "instance:http-address=127.0.0.1:$port" \
        "${parts[@]}"
    chmod 600 "$target/{{ scenario }}/.installed.cfg"
    # At the exit, stop the instance, then the server, also when a step
    # fails. SIGTERM makes zoe serve close its store cleanly.
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
    # Start `zoe serve` in the background with the options that follow the
    # log file, and wait until it listens. Then `server` is its pid and
    # `address` the address that it listens on. The server is the `zoe`
    # binary of this build.
    serve() {
        local log="$1" tries=0
        shift
        zoe="$(just _zoe debug)"
        # The server gets no ZOE_* variable of the caller, so only the
        # options here apply. The admin password does not go to the
        # server.
        (
            for name in "${!ZOE_@}"; do
                unset "$name"
            done
            exec "$zoe" serve "$@"
        ) </dev/null >"$log" 2>&1 &
        server=$!
        # zoe serve writes `listening <address>` once it accepts
        # connections. Stop when the server exits or does not listen,
        # before a ZEO client waits for it.
        until grep -q '^listening ' "$log"; do
            tries=$((tries + 1))
            if ! kill -0 "$server" 2>/dev/null || [ "$tries" -gt 300 ]; then
                cat "$log" >&2
                echo "just plone: zoe serve does not listen, see $log" >&2
                exit 1
            fi
            sleep 0.1
        done
        address="$(sed -n 's/^listening //p' "$log" | head -n 1)"
        echo "just plone: zoe serve listens on $address, its log is $log"
    }
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
            # The instance gives the site from its root, or from the mount
            # point /content of zeoclient-mount.
            folder=""
            case "{{ scenario }}" in
                zeoclient | zoeclient)
                    # zoe serve serves the store of the scenario under the
                    # storage name 1, the default of ZEO.
                    stores=(--path "$target/{{ scenario }}/var/zoe")
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
                # The instance is a ZEO client of zoe serve.
                serve "$target/{{ scenario }}/var/log/zoe-serve.log" \
                    "${stores[@]}" --listen "$zeo_address"
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
        suites)
            # bin/test runs the suites unchanged. In the zoe and zeoclient
            # scenarios, its initialization installs the layer of
            # zoe.plone.testing, which gives plone.testing its base storage.
            if [ "{{ scenario }}" = zeoclient ]; then
                # A new store for each run. The layer reads the address of
                # the server from ZOE_PLONE_ZEO_ADDRESS.
                store="$target/zeoclient/var/suites"
                rm -rf "$store"
                serve "$target/zeoclient/var/log/zoe-serve-suites.log" \
                    --path "$store" --listen 127.0.0.1:0
                export ZOE_PLONE_ZEO_ADDRESS="$address"
            fi
            set -- "${args[@]}"
            "$target/{{ scenario }}/bin/test" -s Products.CMFPlone \
                -s plone.app.contenttypes -s plone.restapi "$@"
            ;;
        backends)
            # The suite of plone.locking, unchanged, on the locking backend
            # of zoe.plone, with the tests of zoe.plone: the layer of
            # zoe.plone.testing installs the backends that
            # ZOE_PLONE_BACKENDS names. The tests that start `zoe serve`
            # run the `zoe` binary of this build.
            set -- "${args[@]}"
            zoe="$(just _zoe debug)"
            ZOE_BINARY="$zoe" ZOE_PLONE_BACKENDS=locking "$target/{{ scenario }}/bin/test" \
                -s plone.locking -s zoe.plone "$@"
            ;;
        creators | history)
            # The script runs in the instance with the storage of the
            # scenario, after a new site. The instance must not run.
            # history.py checks the merge policy of zoe.plone and its
            # append-only workflow history.
            set -- "${args[@]}"
            if [ "{{ command }}" = creators ]; then
                usage="THREADS ITEMS SEED" count=3 clients=" zeoclient zoeclient "
                refusal="the creators run on zoe, zeoclient or zoeclient"
            else
                usage=THREADS count=1 clients=" zoeclient "
                refusal="the history check runs on zoe or zoeclient"
            fi
            if [ "$#" -ne "$count" ]; then
                echo "just plone: use just plone {{ scenario }} {{ command }} $usage." >&2
                exit 2
            fi
            if [ "{{ scenario }}" != zoe ]; then
                case "$clients" in
                    *" {{ scenario }} "*) ;;
                    *)
                        echo "just plone: $refusal." >&2
                        exit 2
                        ;;
                esac
                # A new store for each run, on the address of the
                # configuration of the instance.
                store="$target/{{ scenario }}/var/{{ command }}"
                rm -rf "$store"
                serve "$target/{{ scenario }}/var/log/zoe-serve-{{ command }}.log" \
                    --path "$store" --listen "$zeo_address"
            fi
            "$instance" run plone/set_admin.py
            "$instance" run plone/create_site.py "{{ command }}-$RANDOM$RANDOM"
            # The last line of stdout is the JSON report of the script.
            "$instance" run "plone/{{ command }}.py" "$@"
            ;;
        *)
            echo "just plone: unknown command {{ command }}. Use start, build, check, suites, creators or history." >&2
            exit 2
            ;;
    esac

# Run the benchmarks. `just bench` runs the zodbshootout baseline into
# target/bench/results, `just bench run --quick` runs the quick set,
# `just bench test` runs the tests of the harness and `just bench gate DIR`
# compares zoe serve with ZEO cell by cell: parity, read_hot left out
# (phase 2), and `--python cp314t` counts only that build.
# `just bench native` runs the phase 3 gate (ADR 0039): zoe serve through
# the native client against ZEO in alternating batches, in the 30 cells of
# the gate set. It writes target/bench/native/<started>.json, prints the
# report and fails when a cell is not faster. `--cell` and `--scenario`
# run a part of it. `just bench native report FILE` prints the report of a
# result and builds nothing.
# `run` runs each value of zodbshootout on a fresh store under
# target/bench/work, removes the store when the value ends, and fails a
# cell before it starts when that disk has less free space than one value
# needs. Each result records the GIL of each worker process. `run`, `test`
# and `native` build `zoe` in release for the storages zoe-server and
# zoe-native, unless ZOE_BINARY names a binary.
# `just bench test` runs the tests of bench/shootout and of bench/common,
# and `just bench test FILE...` runs only the tests of those files.
# `just bench index DIR` rewrites index.json of version 2 of a results
# tree, and `just bench publish [--main REF] SRC... CHECKOUT` copies the
# result documents of every family into a checkout of the results
# repository (ADR 0054). It refuses what is not publishable, never commits
# and never pushes. Neither builds anything.
# `just bench client` runs the client benchmark: ZEO.ClientStorage against
# the native client, through the storage API, against zoe serve and a ZEO
# server. It writes target/bench/client/<started>.json and prints the
# rows, and builds `zoe` in release first. `just bench client report FILE`
# prints the rows of a result and its verdict, and builds nothing.
# `just bench load` and `just bench saturate` build `zoe` and `zoe-bench`
# in release, start `zoe serve` on a new store in
# target/bench/work/zoe-bench, with no `ZOE_*` variable of the caller, run
# `zoe-bench load` or `zoe-bench saturate` with its default settings and the
# options that follow, and write the JSON result to stdout and to
# target/bench/zoe-bench/<command>-<started>.json.
# `just bench plone COMMAND` runs the Plone end-to-end benchmark of
# bench/plone: Locust against a synthetic Plone site on ZEO and on zoe
# serve (`prepare`, `run`, `campaign`, `report`, `serve`, `fixtures`), and
# `just bench plone test` runs its tests. It needs the Plone instances of
# `just plone zeo build`, `just plone zoeclient build` and
# `just plone zeoclient build`, and builds `zoe` in release first.
# `just bench replay RUN` runs the log replay of bench/replay (B6): the
# transactions of the store that the B5 run RUN kept (`--keep-store` on
# zeo), against ZEO and against zoe serve, in the interpreter of the
# zoeclient Plone instance. It writes target/bench/replay/<started>.json and
# prints the report, whose last line gives the ratio of the success
# criterion, a measure, not a gate. `--zoe-serve-arg ARG` passes ARG to
# zoe serve, such as `--zoe-serve-arg=--posting-dicts --zoe-serve-arg=on`.
# It needs `just plone zoeclient build`, whose package zoe the native
# client runs, and builds `zoe` in release first.
# `just bench replay report FILE` prints the report of a result, and
# `just bench replay test` runs the tests of the replay.
# `just bench showcase` runs the publishable set on this machine: the
# builds, the fixtures, the B5 runs of `small` and `medium` on `zeo` and
# `zoe-native-qi-kv`, one B5 summary for each fixture, and the native gate
# and the client benchmark on CPython 3.13 and 3.14t. `--set capacity`
# runs the capacity set instead: the builds, the fixture `xlarge`, its B5
# runs with 4 Zope instances on 4 cores, and its B5 summary. It writes
# target/bench/showcase/<started>/, which `just bench publish` takes, and
# resumes a directory that `--dir` gives. `--plan` prints the runs and
# their estimates and runs nothing. It builds nothing before it starts:
# the builds are the first runs of the set.
# Each benchmark holds the bench lock, an exclusive flock(2) on
# ZOE_BENCH_LOCK (target/bench/bench.lock by default), from before its
# build to its end, so one benchmark runs at a time: `run`, `native`,
# `client`, `load`, `saturate`, `plone prepare`, `plone run`,
# `plone campaign` and `replay RUN`. The showcase takes it for each of its
# runs. A child command of a holder takes it through the open file that
# ZOE_BENCH_LOCK_FD names, so it does not wait for its parent. The tests,
# the reports, `gate`, `index` and `publish` take no lock.
[positional-arguments]
bench *args="run":
    #!/usr/bin/env bash
    set -euo pipefail
    # just gives the arguments of the recipe as positional arguments, each
    # unchanged, such as `--zope-cpus "1;3"` or a path with a space.
    # `just bench showcase OPTIONS` stands for `just bench plone showcase
    # OPTIONS`, `just bench client OPTIONS` for `just bench client run
    # OPTIONS`, and `just bench native OPTIONS` for `just bench native run
    # OPTIONS`.
    if [ "${1:-}" = showcase ]; then
        set -- plone "$@"
    fi
    if { [ "${1:-}" = client ] || [ "${1:-}" = native ]; } && [ "${2:-}" != run ] && [ "${2:-}" != report ]; then
        command="$1"
        shift
        set -- "$command" run "$@"
    fi
    case "${1:-}" in
        plone | replay)
            family="$1"
            shift
            ;;
        load | saturate) family=zoe-bench ;;
        *) family=shootout ;;
    esac
    case "$family ${1:-} ${2:-}" in
        "shootout run "* | "shootout client run" | "shootout native run" | zoe-bench\ *) lock=true ;;
        "plone prepare "* | "plone run "* | "plone campaign "*) lock=true ;;
        "replay test "* | "replay report "*) lock=false ;;
        replay\ *) lock=true ;;
        *) lock=false ;;
    esac
    if [ "$lock" = true ]; then
        # The shell holds the lock through its open file 9 until the
        # benchmark ends. A parent that holds it gives its open file.
        if [ -z "${ZOE_BENCH_LOCK_FD:-}" ]; then
            lockfile="${ZOE_BENCH_LOCK:-target/bench/bench.lock}"
            mkdir -p "$(dirname "$lockfile")"
            exec 9>>"$lockfile"
            export ZOE_BENCH_LOCK_FD=9
        fi
        PYTHONPATH=bench/common/src python3.13 -m benchcommon.lock
    fi
    exec "{{ just_executable() }}" "_bench-$family" "$@"

# The Plone end-to-end benchmark of bench/plone, and its showcase.
[positional-arguments]
_bench-plone *args:
    #!/usr/bin/env bash
    set -euo pipefail
    export UV_PROJECT_ENVIRONMENT="{{ justfile_directory() }}/target/bench/plone/venv"
    # Locust loads greenlet, which links libstdc++.so.6. On Linux the
    # shell gives its directory in ZOE_CXX_LIBS.
    if [ -n "${ZOE_CXX_LIBS:-}" ]; then
        export LD_LIBRARY_PATH="$ZOE_CXX_LIBS${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
    fi
    plonebench=(uv run --locked --python 3.13 --project bench/plone)
    if [ "${1:-}" = test ]; then
        shift
        exec "${plonebench[@]}" pytest -q "${@:-bench/plone/tests}"
    fi
    case "${1:-}" in
        prepare | run | campaign | serve)
            ZOE_BINARY="${ZOE_BINARY:-$(just _zoe release)}"
            export ZOE_BINARY
            ;;
    esac
    exec "${plonebench[@]}" python -m plonebench "$@"

# The log replay of bench/replay (B6).
[positional-arguments]
_bench-replay *args:
    #!/usr/bin/env bash
    set -euo pipefail
    export UV_PROJECT_ENVIRONMENT="{{ justfile_directory() }}/target/bench/replay/venv"
    replay=(uv run --locked --python 3.13 --project bench/replay)
    if [ "${1:-}" = report ]; then
        shift
        exec "${replay[@]}" python -m replay report "$@"
    fi
    if [ "${1:-}" != test ]; then
        run="${1:?just bench replay: give the directory of a B5 run that kept its store}"
        store="$run/store/filestorage/Data.fs"
        [ -f "$store" ] || { echo "just bench replay: $run kept no FileStorage: run B5 on zeo with --keep-store" >&2; exit 1; }
        ls "$run"/probe/instance*.jsonl >/dev/null 2>&1 || { echo "just bench replay: $run has no probe records in probe/" >&2; exit 1; }
        # The interpreter of the instance has the eggs of the site, so
        # the resolver of ZODB has the classes of Plone. It runs the
        # package zoe of the last `just plone zoeclient build`.
        interpreter="target/plone/zoeclient/parts/instance/bin/interpreter"
        [ -x "$interpreter" ] || { echo "just bench replay: run just plone zoeclient build first" >&2; exit 1; }
    fi
    ZOE_BINARY="${ZOE_BINARY:-$(just _zoe release)}"
    export ZOE_BINARY
    if [ "${1:-}" = test ]; then
        shift
        exec "${replay[@]}" pytest -q "${@:-bench/replay/tests}"
    fi
    shift
    out="target/bench/replay/$(date -u +%Y%m%dT%H%M%SZ).json"
    work="target/bench/work/replay"
    rm -rf "$work"
    # The interpreter of the instance finds the replay and bench/common
    # on PYTHONPATH.
    export PYTHONPATH="{{ justfile_directory() }}/bench/replay/src:{{ justfile_directory() }}/bench/common/src"
    # The interpreter warns that pkg_resources is deprecated.
    export PYTHONWARNINGS="ignore::UserWarning"
    exec "$interpreter" -m replay run --store "$store" --probes "$run/probe" \
        --workdir "$work" --out "$out" "$@"

# `zoe-bench load` and `zoe-bench saturate` against `zoe serve` on a new
# store.
[positional-arguments]
_bench-zoe-bench command *args:
    #!/usr/bin/env bash
    set -euo pipefail
    ZOE_BINARY="${ZOE_BINARY:-$(just _zoe release)}"
    export ZOE_BINARY
    cargo build --release --locked -p zoe-bench --bin zoe-bench
    zoe_bench="${CARGO_TARGET_DIR:-target}/release/zoe-bench"
    PYTHONPATH=bench/common/src exec python3.13 -m benchcommon.zoebench \
        "$zoe_bench" target/bench/work/zoe-bench target/bench/zoe-bench "$@"

# The zodbshootout baseline, the gate, the native gate, the client
# benchmark, and the results trees of bench/shootout.
[positional-arguments]
_bench-shootout *args:
    #!/usr/bin/env bash
    set -euo pipefail
    export UV_PYTHON="${UV_PYTHON:-3.13}"
    export UV_PROJECT_ENVIRONMENT="{{ justfile_directory() }}/target/bench/venv-$UV_PYTHON"
    # The RelStorage wheel links libstdc++.so.6, which the interpreters of
    # the shell do not find. On Linux the shell gives its directory in
    # ZOE_CXX_LIBS.
    if [ -n "${ZOE_CXX_LIBS:-}" ]; then
        export LD_LIBRARY_PATH="$ZOE_CXX_LIBS${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
    fi
    shootout=(uv run --locked --project bench/shootout)
    case "${1:-} ${2:-}" in
        "client report" | "native report") ;;
        run\ * | test\ * | client\ * | native\ *)
            ZOE_BINARY="${ZOE_BINARY:-$(just _zoe release)}"
            export ZOE_BINARY
            ;;
    esac
    if [ "${1:-}" = test ]; then
        shift
        if [ "$#" -eq 0 ]; then
            set -- bench/shootout/tests bench/common/tests
        fi
        exec "${shootout[@]}" pytest -q "$@"
    fi
    exec "${shootout[@]}" python -m shootout "$@"

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

# Check the deployment files in deploy/. `just deploy check` runs the
# tests of deploy/tests on CPython 3.13: the Tier 1 manifests, the systemd
# unit, the supervisor program and the Grafana dashboard. Its environment
# goes to target/deploy/venv.
deploy command="check":
    #!/usr/bin/env bash
    set -euo pipefail
    case "{{ command }}" in
        check)
            export UV_PROJECT_ENVIRONMENT="{{ justfile_directory() }}/target/deploy/venv"
            uv run --locked --project deploy --python 3.13 pytest -q deploy/tests
            ;;
        *)
            echo "just deploy: unknown command {{ command }}. Use check." >&2
            exit 2
            ;;
    esac

# Check the links of the book, then build the documentation site from docs/
# into target/book. The check fails on a link to a missing file, and on a
# link to a page of docs/ that SUMMARY.md does not list.
docs:
    docs-linkcheck docs
    mdbook build docs

# Run CI functions locally through Dagger. The default is check, test and docs.
ci *functions="check test docs":
    for f in {{ functions }}; do dagger call --mod ci "$f"; done

# Build the release artifacts. `just release` builds the static `zoe`
# binary and the server image, as an OCI tarball, into target/dist through
# the Dagger function release. `just release binary DIR` builds the static
# binary in the shell and copies it to DIR; the Dagger function zoe-binary
# runs it. `just release wheel DIR VERSION...` builds the zoedb wheel in the
# shell into DIR, one wheel for each CPython version, 3.13 by default, as
# `just plone` builds it. `just release test` runs the tests of the Dagger
# functions against the Dagger engine.
release command="dist" dir="target/dist" *versions="3.13":
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
        wheel)
            # The extension module is for one CPython version and one
            # platform, so each version gets its own wheel. uv runs the
            # build in python/, so the output directory is absolute.
            mkdir -p "{{ dir }}"
            out="$(cd "{{ dir }}" && pwd)"
            for py in {{ versions }}; do
                uv build --directory python --package zoedb --wheel --python "$py" \
                    --out-dir "$out"
            done
            ;;
        test)
            # uv installs the Python SDK of the module from ci/sdk, which
            # dagger develop writes.
            dagger develop --mod ci
            uv run --locked --directory ci pytest -q tests
            ;;
        *)
            echo "just release: unknown command {{ command }}. Use dist, binary, wheel or test." >&2
            exit 2
            ;;
    esac
