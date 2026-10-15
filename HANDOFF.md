# Handoff: the Plone end-to-end benchmark (B5)

Branch `claude/plone-zeo-zoe-locust-benchmark-pvkgea`. No pull request yet. No card: the work came as a direct request. When a card exists, move this file to `docs/handoffs/<card>.md`, as `CLAUDE.md` asks.

## The task

Build and run a reproducible Locust benchmark of a realistic Plone site, ZEO against ZOE, measuring application performance and what ZOE's conflict handling brings, with correctness checks, and end with a ranked list of optimizations. The full brief is in the session; its key rules: no forced winner, no invented "disabled" mode, constants recorded, ledger-based verification, read-only verification before any repair, no site data in the repository.

## State

- **Built and pushed:** the harness `bench/plone` (package `plonebench`), the recipe `just bench plone ...`, the section "The Plone end-to-end benchmark" of `bench/README.md`, the B5 item of the specification. 30 unit tests pass (`just bench plone test`); `ruff check` and `ruff format --check` are clean on `bench/plone`.
- **Ran:** a smoke run, then one repetition of `browse`, `catalog`, `edit`, `create-spread`, `create-shared`, `workflow` and `mixed` on `zeo` and `zoe-native` (steps 1, 4, 16 users × 45 s, warmup 45 s). All 14 passed verification. The human asked for one repetition only.
- **Interim findings:** `docs/findings/plone-end-to-end.md`, with the report and the summary JSON of those 14 runs in `docs/findings/plone-end-to-end/`.
- **Stopped on request** before the follow-up campaigns. Nothing runs; no process is left.

## What is not done

1. **Rerun the write scenarios with the word codes.** The 14 runs used a hex token in each title, which added a new word to the text index lexicons on each save and inflated the conflicts (see finding 8). The fix is pushed (`corpus.op_code`). Rerun:
   ```sh
   just bench plone campaign --stacks zeo,zoe-native --scenarios edit,create-spread,create-shared,mixed \
     --reps 1 --steps 1:45,4:45,16:45 --warmup 45 --label codes
   ```
   `browse`, `catalog` and `workflow` write no titles: their runs stand.
2. **The follow-up campaigns** (one repetition each, about 1.5 h in all):
   ```sh
   just bench plone campaign --stacks zeo,zoe-native,zoe-stock,zoe-asyncio \
     --scenarios contention-fields,contention-same,contention-folder,create-shared \
     --reps 1 --steps 4:45,16:45 --warmup 45 --label contention
   just bench plone campaign --stacks zeo,zoe-native --scenarios mixed-background \
     --reps 1 --steps 4:60,8:60 --warmup 45 --label background
   just bench plone campaign --stacks zeo,zoe-native --scenarios browse \
     --reps 1 --steps 4:60 --warmup 0 --trim 0 --label cold          # then: report --timeline 10
   just bench plone campaign --stacks zeo,zoe-native --scenarios browse \
     --reps 1 --steps 4:45 --warmup 45 --no-probe --label noprobe    # and the same without --no-probe
   just bench plone campaign --stacks zeo,zoe-native --scenarios create-spread \
     --reps 1 --steps 8:600 --warmup 30 --label sustained            # then: report --timeline 60
   ```
   `zoe-stock` (stock client) against `zoe-asyncio` isolates the `portal_uidgenerator` sequence; `zoe-asyncio` against `zoe-native` isolates the native client.
3. **The final findings:** replace the interim document with the numbers of 1 and 2: the contention tables per stack, the cost of the probe, the cold start, the trend of the sustained writes, and the ranked table updated.
4. **Open questions from the interim data**, each an experiment:
   - The exact mechanism of the `_wordinfo` conflicts: run `edit` with `--keep-store`, take the conflicting oids from `probe/*.jsonl`, and for each bucket compare the keys that the two transactions changed (same key, split, deletion). `inzope/oidmap.py` names the index of an oid; it walks the fixture, so oids created during a run stay unnamed unless it runs on the kept store.
   - Why `zoe serve` writes 5–20× more bytes per operation than ZEO, and why its fsync batch is always 1 (finding 7). Look at the sustained-write run and at the redb commit path. `crates/zoe-core/src/format/` is a red zone; a change there needs the human.
   - The read conflicts that only ZOE reports (`ReadConflictError` from `checkCurrentSerialInTransaction`, about 10–25 per step in writes).
5. **Not runnable here, record as such:** compression off (no option in `zoe serve`), free-threaded Plone (Plone 6.1 stops at CPython 3.13), cold filesystem pages (the container cannot drop caches). A `medium` fixture (`just bench plone prepare --preset medium`) is runnable and is the next step after the small one: the storage weighs more when the site does not fit in the caches.

## How to pick it up

The environment of this session had no `devenv` and no `just`; it used the Rust toolchain 1.99.0 through rustup (the pin of `devenv.nix`), `just` from nixpkgs, and the system CPython 3.13. In a normal setup: `devenv shell`, then the recipes.

```sh
just plone zeo build && just plone zoeclient build && just plone zeoclient build
just bench plone prepare            # builds fixture small-<hash> under target/bench/plone/fixtures
just bench plone fixtures           # lists it, with the equivalence verdict
just bench plone run --stack zoe-native --scenario edit --steps 4:30 --warmup 10   # a quick check
just bench plone report target/bench/plone/runs/<dir>... --out report.md --json summary.json
```

Everything generated lives under `target/bench/plone/` (ignored): fixtures, runs (`result.json`, ledger, probe files, samples, logs), campaign lists. A fresh machine must run `prepare` again; the fixture is deterministic, but its password is new.

## Things to know

- The instances run from the eggs of the buildout scenarios, but the runner writes their `zope.conf` and `wsgi.ini` itself (`stack.py`), so every stack has the same settings but the storage section. Ports: Zope 18080+, server 18100, admin 18199.
- The stock `runzeo` script of `plone.recipe.zeoserver` does not import the `zc` namespace packages on this interpreter, so the ZEO server starts through `probe/zeoserver/launch.py` with the interpreter of the zeo scenario. Debian's interpreter has its own `sitecustomize`, which is why the ZEO probe is a module that `launch.py` imports.
- plone.protect aborts a ZMI write without a token and the ZMI still redirects with its success message. The reindex role takes the token from a Plone page; do not trust a ZMI redirect as success.
- A write leaves a `pending` ledger record before its request, and Locust gets `--stop-timeout 120`, so an operation cut at the end of a run counts as ambiguous, not as a phantom.
- `zoe_conflicts_resolved_total` of `zoe serve` counts only conflicts of votes that pass: resolutions inside a vote that then fails do not count. The probe of the instances counts every resolution.
- Do not pkill with a pattern that the shell of the command itself matches: it kills the shell. Stop campaigns by the process group of their script.
