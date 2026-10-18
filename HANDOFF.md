# Handoff: phase 4

This is the state of phase 4 when the session stopped (2026-10-18), and what the next session does. The cards keep their own notes in `docs/handoffs/<card>.md` on their branches. This file is the overview across them.

## Where phase 4 stands

**Merged to `main`:**

| PR | Card | Squash |
| --- | --- | --- |
| #421 | ZOE-208 | |
| #430 | ZOE-209 | |
| #418 | ZOE-203 | `18c440d` |
| #428 | ZOE-205 | `356c11d` |
| #432 | ZOE-206 | `67ce41e` |
| #419 | ZOE-204 | `73a6bd3` |
| #420 | ZOE-207 | `812cca0` |
| #443 | test fix | `98126b0` |
| #442 | ZOE-210 | `a7cd504`, now the head of `main` |

The CI fixes (#422 and the workflows `ad2a0bd`) are merged too.

**Phase gates:**
- **TSan** passed on `main` at `98126b0`: `just tsan` exits 0, and only the canary reports a race. Run it again on `main` once ZOE-219 merges, because ZOE-219 changes the vote.
- **The ConflictError gate (#440)** moved to B5, by the human's decision (comment on #440).
  - **The measure:** B5 `edit` and `create-spread` at 4 and 16 users, `zeo` against `zoe-native-qi`, counting the ConflictErrors raised to Zope per 1,000 operations.
  - **The limit:** at most 10 % of `zeo` in each scenario. B6 stays as a measure only.
  - **Today:** `edit` passes at 0 %. `create-spread` fails, at 25 % with 4 users and 14 % with 16. ZOE-219 is the fix: its projection is 0.6 % and 2.4 %.

**Open, in train order:** #444 (ZOE-218) first, then #446 (ZOE-219), stacked on `card/ZOE-218`.

## Next steps

### 1. ZOE-218, queued indexing: PR #444

- Worktree `/Users/antoineduchene/Projets/ZOE-card-218`, branch `card/ZOE-218`.
- ADR 0047 is accepted. The first review asked for changes (comment of 10:04 on #444).

**The builder was stopped after it committed the fixes, but before it pushed.**
- **Local commits,** not on GitHub: `5d16b40`, `8283724`, `845f00d`, `c9c2d32` and `875d6ee`, on top of `c000ed2` (origin). The tree is clean.
- **Done, according to its handoff:**
  - blocking findings B1 to B4;
  - the ten non-blocking findings;
  - serving mounted databases, as the human chose over a fallback. Each database with namespaces has a thread. A site's lease, queue and cookie wait live in its catalog's store. The cookie names the database.
- **Not confirmed:** that the full suite ran after these commits.

To do:
1. Check `git log c000ed2..HEAD` and read `docs/handoffs/ZOE-218.md`.
2. Run `just check`, `just plone zoe backends`, `just bench plone test` and `just test`.
3. Push, and post a comment on #444 that lists each review finding with its fix commit.
4. **Get the human's approval of the ADR 0047 text** that `845f00d` added: the item "Mounted databases" in §4, and in §6 the cookie also expiring after a wait that ran out. The human chose to serve mounted databases but has not read the text.
5. Re-review the delta `git diff c000ed2 HEAD` with a reviewer agent (`loops/prompts/reviewer.md`).
6. Squash-merge #444, without CI (see "CI" below).
7. Merge `main` into `card/ZOE-219`.

### 2. ZOE-219, the split merge: PR #446 (draft), card #445

- Worktree `/Users/antoineduchene/Projets/ZOE-card-219`, branch `card/ZOE-219`, at `ba41e25`, pushed.
- **Done:** steps 1 and 2.
  - The spike: `docs/spikes/concurrent-creations.md`.
  - ADR 0048, accepted by the human with the recommendation of each of its 10 questions (recorded on #446).
  - The planner's acceptance test, merged unchanged (`f6a4b39`): `python/zoedb/src/zoe/plone/tests/test_concurrent_creations.py`. It fails today in round 4, on the shadow storage tree of CMFEditions. Run it with `just plone zoe backends -t test_concurrent_creations`.
- The scope fence on #445 is widened to `crates/zoe-core/src/commit/**`, `crates/zoe-core/CLAUDE.md` and `crates/zoe-pickle/**` (ADR 0048, question 4).
- **Nothing is built yet.**

To do (a builder, `loops/prompts/builder.md`):
1. **Step 3: build ADR 0048.**
   - The three tier 1 rules and the order of key references, in those rules only.
   - The switch `split_merges` in the merge policy: `off` (the default), `shadow` and `on`.
   - In the acceptance test, fill only `mechanism_on` and `mechanism_off`, exactly as section 9 of the ADR shows.
   - The tests of section 10:
     - Rust vote tests for each rule and each no-answer case: two levels of nodes, a commit in flight, each mode;
     - the spike's model as a Python test against `zoe.Storage` and against `zoe serve`, with the native client and a stock `ZEO.ClientStorage`, plus `BTrees.check` and lookups from another connection and from another process.
   - Update `crates/zoe-core/CLAUDE.md` (the vote invariants), and section 5 of the spec and ADR 0042 where they say tier 1 is byte for byte ZODB's resolver.
2. **Step 4: measure.**
   - Add a B5 harness option that installs `split_merges: on` for `zoe-native-qi`.
   - Run B5 `edit` and `create-spread` at 4 and 16 users, on `zeo` and on `zoe-native-qi` with the switch, three repetitions, with `--port-offset 300`, under `caffeinate -ims`.
   - Report the #440 gate, and run `just bench native` and B2 against `main`.
   - Report the counts of each rule and the garbage bytes from the resolution log.
3. **Step 5:** update the spec. Then mark #446 ready, have a reviewer review it, and squash-merge it after #444.

### 3. Closing the phase

- Update the gate text in `docs/spec/technical-specification.md` to the B5 measure that the human set: section 11, the B6 paragraph ("the gate of phase 4") and the testing table row "success criterion". Do this after #444 merges, to avoid a conflict.
- Run `just tsan` on the final `main`.
- Close #440 when `create-spread` passes.
- **Follow-up cards to write:**
  - two creations in one folder: the `Folder` record and the order of plone.folder (ADR 0048, question 9);
  - `bench/replay` starting the indexer's batches (note `zoe.plone: indexing queue`) at their commits.
- **For the documenter:**
  - name `keys` in `crates/zoe-core/CLAUDE.md` and `CONTEXT.md`;
  - the fuzz target `fields` does not pick `keys` yet;
  - the sentence "Python has no order for other pairs" in `crates/zoe-pickle/src/python.rs` and the zoe-pickle `CLAUDE.md` is too broad: Python orders `(1, 'a') < (2, 3)`.
- **Other open cards:** ZOE-211 to ZOE-217 (#433 to #439).

## Decisions of the human in this session

- The orchestrator merges after reviewer approval and green CI. While hosted CI is blocked by billing, **merge without CI** (the human's words: "Merge without CI").
- ADR 0046 §11 (`keys`): confirmed as written.
- ADR 0047: approved with all 11 recommendations, then "serve mounted databases" for the review's B4. The amended text still needs the human's approval.
- ADR 0048: approved with all 10 recommendations, including the planner's three changes to the acceptance test.
- The gate of #440 is B5 `zeo` against `zoe-native-qi`, at most 10 % per scenario, with a CMFEditions card (ZOE-219) before the phase closes.
- Red zones still need the human: `format/`, `pack/`, `.github/` and `loops/`.

## Things to know

- **CI.** Hosted GitHub Actions jobs fail at start: "recent account payments have failed or your spending limit needs to be increased". Only the human can fix the billing. Run the checks locally with the recipes.
- **Recording approvals.** An auto-mode classifier refuses an edit by a subagent that writes "accepted" on the strength of an approval that the orchestrator relays. So the orchestrating session writes the status line and the PR comment itself, right after the human answers.
- **The machine.**
  - Other jobs use the default B5 ports: use `--port-offset 200` or `300`.
  - The machine sleeps when idle: run long campaigns under `caffeinate -ims`.
  - `devenv shell` needs `~/.nix-profile/bin` on `PATH`.
- **Disk.**
  - Free disk is 70 GiB. I deleted the build outputs of the merged worktrees (ZOE-card-203 to ZOE-card-209); their `target/bench` and `target/plone` stay.
  - Keep the B5 log for B6: `/Users/antoineduchene/Projets/ZOE/target/bench/plone/runs/20261017T023405Z-edit-zeo-r0`.
- **Spike and measure scripts** are in the session's scratchpad, which is temporary: `/private/tmp/claude-501/-Users-antoineduchene-Projets-ZOE/64e6d74e-d10f-4395-bd89-10814d2a705a/scratchpad/card218/` and `.../scratchpad/card219/`.
- **The user's rules:**
  - no attribution lines (Generated-with, Co-Authored-By, session links) in commits, PRs or any text;
  - no ticket ids in source code;
  - comments in Simplified Technical English, with no history.
