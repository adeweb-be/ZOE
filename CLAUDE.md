# ZOE

ZOE, the Zope Object Engine: a ZODB storage written in Rust, drop-in for Plone 6.1 and later, in-process or behind a server that speaks the ZEO protocol.

- What ZOE is and how it works: `docs/spec/technical-specification.md`.
- How ZOE gets built (cards, tools, roadmap): `docs/spec/working-methods.md`.
- Decisions and their reasons: `docs/adr/`.
- Vocabulary: `CONTEXT.md`. Use its terms.
- Invariants of a crate: `crates/<crate>/CLAUDE.md`. Read it before you change the crate.

The specification governs what you build. The working-methods document governs how you work. Where they disagree, or are silent on something you need, stop and ask. Do not invent.

## Environment and recipes

Enter the environment with `devenv shell`, then run `just <recipe>`. Use recipes, never raw commands. If you need a command that has no recipe, add the recipe in the same pull request.

| Recipe | Does |
| --- | --- |
| `just check` | `cargo fmt` check, the `Cargo.lock` of the fuzz targets, clippy with warnings as errors, ruff lint and format check |
| `just test` | Rust tests, then the Python tests on CPython 3.12, 3.13, 3.14 and 3.14t, then `just deploy check` |
| `just tsan` | `zoe-core` tests under ThreadSanitizer, then the canary test, which passes only when ThreadSanitizer reports its deliberate race; builds into `target/tsan`; `just tsan zoe-server FILTER` runs the library tests of `zoe-server` whose name holds FILTER instead |
| `just fuzz` | builds the fuzz targets of `zoe serve` (`crates/zoe-server/fuzz`) with libFuzzer and AddressSanitizer into `target/fuzz`, then runs them for 60 seconds in all; `just fuzz 3600` runs them for an hour, `just fuzz 600 frame` runs only `frame` |
| `just crash` | the process kill loop against `zoe serve` for 200 rounds, then the kill loop and the split writers under the simulator; `just crash RUNS SEED` repeats the kill times of a run |
| `just conformance` | ZODB's storage suites and test modules in-process against `zoe.Storage`, on every interpreter |
| `just plone` | builds the Plone instance of a scenario under `plone/` into `target/plone` and starts it; `just plone zoe check` creates a Plone site on the zoe scenario by script and reads it over HTTP |
| `just bench` | the benchmarks of `bench/`, which `bench/README.md` describes: the zodbshootout baseline by default; `native` the phase 3 gate (ADR 0039), `client` the client benchmark, `load` and `saturate` through `zoe-bench`, `plone` the Plone end-to-end benchmark (B5), `replay` the log replay (B6), `showcase` the publishable set, `publish` the copy of results into a checkout of the results repository (ADR 0054) |
| `just loupe` | builds Loupe in release and runs it; `just loupe dev` runs it on the Vite dev server, `just loupe check` lints it, `just loupe e2e` runs its end-to-end tests. They run in the `loupe` profile of the shell: `devenv --profile loupe shell -- just loupe` |
| `just deploy check` | the tests of `deploy/tests` on the deployment files: the Tier 1 manifests, the host units, the Grafana dashboard; `just test` runs it at its end |
| `just docs` | checks the links of the book, then builds the documentation site into `target/book`; a link to a missing file or to a page of `docs/` that `SUMMARY.md` does not list fails it |
| `just ci` | runs the Dagger functions `check`, `test` and `docs` locally; `just ci <function>` runs one |
| `just release` | builds the static `zoe` binary and the server image into `target/dist` through Dagger; `just release binary DIR` builds the binary in the shell; `just release wheel DIR VERSION...` builds the `zoedb` wheel in the shell, one for each CPython version, 3.13 by default; `just release test` runs the tests of `ci/`. Publishing is not built yet |

`just ci` reproduces a CI result. Do not use it for the edit-test loop.

## Hard constraints

- Targets: CPython 3.12, 3.13, 3.14 and 3.14t (free-threaded); Linux x86-64 and macOS on Apple silicon; ZODB 6.0 or later and ZEO 6.0 or later only; Plone 6.1 or later. No Windows, no ZODB 5, no CPython 3.15 yet. A green run on macOS is evidence, as a run on Linux is (ADR 0038). `just release binary` needs Linux, and CI runs on Linux.
- Rust: the stable toolchain that `devenv.nix` pins. `zoe-core` is synchronous: no tokio, no Python inside. Only `zoe-server`, `zoe-client`, `zoe-bench`, `zoe-mcp`, the server mode of Loupe and the native client in `zoe-py` use tokio. The extension module (`zoe-py`) links tokio only through `zoe-client`, starts its threads when a client opens, never at import, and never links rmcp: `cargo tree -p zoe-py -e normal` lists tokio, not rmcp (ADR 0035). PyO3 0.28 or later, with free-threading support. redb is the first backend.
- Tools: devenv is the only environment, `just` the only entry point, uv for the packages under `python/`, zc.buildout for the Plone instances under `plone/` (no mxdev), pytest for the Python tests, Dagger for CI (`ci/`), thin GitHub Actions workflows that only call Dagger, GitHub Issues and Projects as the board, GitHub Pages for the documentation.
- Names: crates `zoe-core`, `zoe-server`, `zoe-client`, `zoe-py`, `zoe-catalog`, `zoe-vectors`, `zoe-mcp`, `zoe-bench`, `zoe-loupe`, `zoe-pickle`, `zoe-convert`, `zoe-wire`; PyPI `zoedb`, import `zoe`; Plone extra `zoedb[plone]`, import `zoe.plone`; CLI `zoe`; stanza `<zoe>`; URI `zoe://`; environment `ZOE_*`; image `ghcr.io/adeweb-be/zoe`.
- Nothing site-specific: no production data, no credentials, no secrets in the repository or in CI logs. Test corpora are synthetic. Keep every text generic: no company, place or natural-language names. Nothing is hosted on a private domain.
- No durations and no dates in plans, cards or documents. Tests gate the phases.

## Red zones

The red zones need care. `CODEOWNERS` lists them. A pull request that touches one merges when CI is green, as any other, unless it carries a design decision (see "Stop and ask").

- `crates/zoe-core/src/format/`: the on-disk format. A change to the data model (a key space, a `meta` key, a frame field) is a format change wherever its code lives.
- `crates/zoe-core/src/pack/`: pack, because it deletes data.
- `.github/` (the workflows and `CODEOWNERS`) and `loops/` (the role briefs). Only a human changes these.

Everything else, `ci/` and `devenv.nix` included, merges when CI is green.

The human takes the big design decisions only: a change to the on-disk format, the wire protocol or a public Python API, and a new ADR. The orchestrator merges the rest.

## Stop and ask

Stop and write a question on the card, instead of guessing, when:

- a change would alter the on-disk format, the wire protocol, a public Python API, a trait signature that another crate uses, or an acceptance test;
- the only way to make the test green is to weaken it.

## Cards

1. A card is a GitHub issue: one mechanism, one pull request. It names its acceptance test and its scope fence.
2. The builder works on the branch `card/<id>` in its own worktree, and opens a pull request when the work is done. The description gives the card, what changed in one paragraph, and what did not run.
3. The pull request merges by squash when CI is green. No force pushes.
4. When `main` moves and the pull request conflicts, the builder merges `main` into its branch.
5. A card that stops unfinished leaves `docs/handoffs/<card>.md`: the state and the next step.

CI runs `check`, `test` and `docs` on each pull request. The conformance and Plone suites, the crash loop, the benchmarks and `just tsan` run when the human asks, as a rule at the end of a phase.

Done means: the acceptance test is green in CI, and the specification or an ADR says what changed in behaviour.

## Rules

- Never edit an acceptance test to make it pass.
- Stay inside the scope fence of the card.
- Prefer the simplest thing that the specification allows. Add no feature that the specification does not name.
- `docs/spec/*.md` are living documents. When a card changes behaviour, its pull request updates them.
- Code comments use Simplified Technical English: short sentences, one idea each, active voice. A comment describes the code as it is now: no history, no card or issue ids in source code.

## Layout

| Path | Holds |
| --- | --- |
| `crates/` | the Rust crates |
| `python/` | the uv workspace; `python/zoedb/` is the wheel (`zoe` package, extension `zoe._zoe`) |
| `plone/` | the buildout scenarios for the Plone instances |
| `apps/loupe/` | the Loupe window code |
| `deploy/` | the Tier 1 manifests, the systemd and supervisor units, the Grafana dashboard and their tests |
| `bench/` | the benchmark harness |
| `ci/` | the Dagger module |
| `loops/` | the role briefs |
| `docs/` | the specification, the ADRs, spike findings, handoffs; the mdBook source |
