# ZOE

ZOE, the Zope Object Engine: a ZODB storage written in Rust, drop-in for Plone 6.1 and later, in-process or behind a server that speaks the ZEO protocol.

- What ZOE is and how it works: `docs/spec/technical-specification.md`.
- How ZOE gets built (roles, cards, loops, roadmap): `docs/spec/working-methods.md`.
- Decisions and their reasons: `docs/adr/`.
- Vocabulary: `CONTEXT.md`. Use its terms.
- Invariants of a crate: `crates/<crate>/CLAUDE.md`. Read it before you change the crate.

The specification governs what you build. The working-methods document governs how you work. Where they disagree, or are silent on something you need, stop and ask. Do not invent.

## Environment and recipes

Enter the environment with `devenv shell`, then run `just <recipe>`. Use recipes, never raw commands. If you need a command that has no recipe, add the recipe in the same pull request.

| Recipe | Does |
| --- | --- |
| `just check` | `cargo fmt` check, clippy with warnings as errors, ruff lint and format check |
| `just test` | Rust tests, then the Python tests on CPython 3.12, 3.13, 3.14 and 3.14t |
| `just tsan` | `zoe-core` tests under ThreadSanitizer, then the canary test, which passes only when ThreadSanitizer reports its deliberate race; builds into `target/tsan` |
| `just conformance` | ZODB and ZEO conformance suites (not built yet) |
| `just plone` | builds the Plone instances under `plone/` (not built yet) |
| `just bench` | benchmarks (not built yet) |
| `just loupe` | builds and runs Loupe (not built yet) |
| `just docs` | builds the documentation site into `target/book` |
| `just ci` | runs the Dagger functions `check`, `test` and `docs` locally; `just ci <function>` runs one |
| `just release` | cuts a release (not built yet) |

`just ci` reproduces a CI result. Do not use it for the edit-test loop.

## Hard constraints

- Targets: CPython 3.12, 3.13, 3.14 and 3.14t (free-threaded); Linux x86-64; ZODB 6.0 or later and ZEO 6.0 or later only; Plone 6.1 or later. No macOS, no Windows, no ZODB 5, no CPython 3.15 yet.
- Rust: the stable toolchain that `devenv.nix` pins. `zoe-core` is synchronous: no tokio, no Python inside. Only `zoe-server` uses tokio. PyO3 0.28 or later, with free-threading support. redb is the first backend.
- Tools: devenv is the only environment, `just` the only entry point, uv for the packages under `python/`, zc.buildout for the Plone instances under `plone/` (no mxdev), pytest for the Python tests, Dagger for CI (`ci/`), thin GitHub Actions workflows that only call Dagger, GitHub Issues and Projects as the board, GitHub Pages for the documentation.
- Names: crates `zoe-core`, `zoe-server`, `zoe-client`, `zoe-py`, `zoe-catalog`, `zoe-vectors`, `zoe-mcp`, `zoe-bench`, `zoe-loupe`, `zoe-pickle`, `zoe-convert`; PyPI `zoedb`, import `zoe`; Plone extra `zoedb[plone]`, import `zoe.plone`; CLI `zoe`; stanza `<zoe>`; URI `zoe://`; environment `ZOE_*`; image `ghcr.io/adeweb-be/zoe`.
- Nothing site-specific: no production data, no credentials, no secrets in the repository or in CI logs. Test corpora are synthetic. Keep every text generic: no company, place or natural-language names. Nothing is hosted on a private domain.
- No durations and no dates in plans, cards or documents. Tests gate the phases.

## Red zones

A pull request that touches a red zone needs the human's approval. `CODEOWNERS` lists them.

- `crates/zoe-core/src/format/`: the on-disk format. A change to the data model (a key space, a `meta` key, a frame field) is a format change wherever its code lives.
- `crates/zoe-core/src/pack/`: pack, because it deletes data.
- `.github/` (the workflows and `CODEOWNERS`) and `loops/` (the role briefs). Only a human changes these.

Everything else, `ci/` and `devenv.nix` included, merges on the reviewer's approval with the acceptance test, the crash simulation and TSan green.

## Stop and ask

Stop and write a question on the card, instead of guessing, when:

- a change would alter the on-disk format, the wire protocol, a public Python API, a trait signature that another crate uses, or an acceptance test;
- the budget of the card is spent;
- the only way to make the test green is to weaken it.

Then write `docs/handoffs/<card>.md`: the state, what you tried, what failed, the next step. Do not try harder.

## Card lifecycle

1. A card is a GitHub issue with the task-card template of the working-methods document. It cites a requirement id and names its acceptance tests.
2. Board columns: Backlog, Ready, Building, Review, Verifying, Human, Done. The human moves a card to Ready.
3. The builder works on the branch `card/<id>`, one card per branch and per pull request, in its own worktree.
4. The builder opens a draft pull request and marks it ready when the acceptance test is green. The description ends with the claims-and-evidence section of `.github/pull_request_template.md`.
5. A reviewer agent reviews. CI and the verifier post the numbers.
6. Merge is a squash merge, after reviewer approval, a green verifier, and the human's approval when a red zone is touched. No force pushes.
7. The documenter updates `docs/` after the merge.

Definition of Ready: a requirement id, an acceptance test that fails today, a scope fence, written invariants, a budget, no dependency on an unmerged card.

Definition of Done: acceptance test green; unit tests added; TSan and the pull-request harness tier green; benchmark numbers posted and inside the regression gate; claims and evidence list what was tested and what was not; reviewer approval; human approval for a red zone; the specification or an ADR updated when behaviour changed.

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
| `deploy/` | manifests and the Helm chart |
| `bench/` | the benchmark harness |
| `ci/` | the Dagger module |
| `loops/` | the role briefs |
| `docs/` | the specification, the ADRs, spike findings, handoffs; the mdBook source |
