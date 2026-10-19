# 0054. One published format for every benchmark: one layout, the index of version 2, the B5 summary and `just bench publish`

Status: proposed. The planner of ZOE-243 wrote it from the decisions of the human on the card. The human confirms it, with an answer to each question at its end. It changes no on-disk format, no wire protocol and no public Python API. It changes the output of `just bench plone report --json` and the layout of the results repository.

## Context

The results repository, `adeweb-be/zoe-bench-results`, holds an `index.json` of version 1 with no result, the dashboard page and a README. Its layout, `results/<machine>/<python>/<storage>/<started>-<commit>.json`, fits zodbshootout only. Four families of results exist:

| Family | Command | Document | Schema |
| --- | --- | --- | --- |
| `shootout` | `just bench run` | one per storage and run | `bench/schema/results.schema.json` |
| `native` | `just bench native` | one per run of the native gate | `bench/schema/native.schema.json` |
| `client` | `just bench client` | one per run of the client benchmark | `bench/schema/client.schema.json` |
| `plone` | `just bench plone campaign`, `report` | `result.json` of each B5 run; `report --json` | none |

What each family records today:

- The first three share `machine` (`name`, `cpu`, `logical_cpus`, `memory_bytes`, `os`, `kernel`, `arch`, `disk`), `python` (`implementation`, `version`, `free_threaded`, `compiler`), a flat `software`, and `run.started` (ISO 8601 in UTC) and `run.commit`. None records whether the tree held changes that the commit does not. Only zodbshootout has a quick set (`run.quick`, `run.set`).
- A B5 run records in `result.json` the commit and `dirty` (`git status --porcelain --untracked-files=no`), the fixture (its name, its `SiteSpec`, and its equivalence with the counts of objects and catalog entries), the settings, the stack, the steps, the numbers of each step, the storage calls that the probe counts (`loads` and `stores` of the requests of a step), and the verdict of the verification. Its `machine` has no `name` and no `arch`, the Python of its instances has no `implementation`, and its start is a Unix time. It also holds paths: the `zope_conf` of the stack and the traceback of a failed run. So `result.json` is not a document to publish.
- `report --json` writes the internal summary of the report, keyed `scenario|stack`. It has no machine, software, commit, fixture or settings, no objects per second and no schema. Nothing reads it.

The decisions of the human, on the card:

- The results repository publishes every family, not only zodbshootout: B5 (operations per second and `ConflictError`s on a Plone site), the native gate and the client benchmark (B7).
- The page shows every result as plain tables, with no headline section (ZOE-245).
- On B5, ZOE is the stack `zoe-native-qi-kv`, labelled with what it turns on, beside `zeo`.
- Nothing goes to the public repository without the approval of the human of that push.

## Decision

### 1. The families

The families are `shootout`, `native`, `client` and `plone`. A document names its family by one key at its top level: `storage` (shootout), `cells` (native), `rows` (client) or `fixture` (plone). A document with none of these keys, or with more than one, is of no known family.

### 2. The layout

```text
index.json
results/shootout/<machine>/<python>/<storage>/<started>-<commit>.json
results/native/<machine>/<python>/<started>-<commit>.json
results/client/<machine>/<python>/<started>-<commit>.json
results/plone/<machine>/<python>/<fixture>/<started>-<commit>.json
```

- `<machine>` is `machine.name`. The names of the families are not machine names.
- `<python>` is the short name of `python`, as today: `cp313`, or `cp314t` for a free-threaded 3.14. For `plone` it is the Python of the Plone instances.
- `<started>` is `run.started` without `-` and `:`: `YYYYMMDDTHHMMSSZ`. `<commit>` is the first 12 characters of `run.commit`.
- `<fixture>` is `fixture.name`, such as `small-ba01ff255313`.
- A result in the layout of version 1, `results/<machine>/<python>/<storage>/...`, is a `shootout` document. It keeps its path.
- The harness of zodbshootout keeps its own tree under `target/bench/results`: `results/` and `quick/` in the layout of version 1. `just bench gate` reads that tree. Only `publish` writes the layout above.

### 3. `index.json` of version 2

```json
{
  "index_version": 2,
  "results": [
    {
      "family": "plone",
      "path": "results/plone/reference/cp313/small-ba01ff255313/20260101T030000Z-d939b3f58063.json",
      "machine": "reference",
      "python": "cp313",
      "stacks": ["zeo", "zoe-native-qi-kv"],
      "fixture": "small-ba01ff255313",
      "scenarios": ["edit"],
      "started": "2026-01-01T03:00:00Z",
      "commit": "d939b3f580637ba1f457c9041a12b2f26b0a5273"
    }
  ]
}
```

| Key | Holds |
| --- | --- |
| `family` | `shootout`, `native`, `client` or `plone` |
| `path` | the path of the document under the root of the checkout, with `/` |
| `machine` | `machine.name` |
| `python` | the short name of the Python build |
| `stacks` | what the document compares, each name once, in the order of the document: `shootout` `[storage]`; `native` the keys of `workers` of its cells (`zeo`, `zoe-native`); `client` the `client` of its rows; `plone` the `name` of its stacks |
| `fixture` | `plone`: `fixture.name`; else null |
| `scenarios` | each name once, in the order of the document: `shootout` the `name` of its scenarios; `native` the `name` of the scenarios of its cells; `client` the `workload` of its rows; `plone` the `name` of its scenarios |
| `started` | `run.started` |
| `commit` | `run.commit` |

The entries are in the order of `started`, then of `path`: oldest first. `just bench index DIR` writes the index from the tree: each `*.json` file under `results/`. The first directory under `results/` names the family of a file. A file under another directory is a `shootout` document in the layout of version 1. `index` never reads the old `index.json`, so it reads a checkout of either version and writes version 2. An index of version 1 has no family: its documents are `shootout`.

### 4. What is publishable

A document is publishable when each of these holds:

1. It is JSON, of a known family, and valid against the schema of its family.
2. It is not a quick run: for `shootout`, `run.quick` is false and `run.set` is not `quick`. The other families have no quick set.
3. `run.commit` is not null, and it is on the branch REF: `git merge-base --is-ancestor COMMIT REF` succeeds in the ZOE repository of the harness. REF is `origin/main`, unless `--main` gives another.
4. `run.dirty` is false. True is a run of a dirty tree. An absent key or null is no record of a clean tree.
5. For `plone`: the verification of each run of `runs` is `pass`.
6. Its path in the checkout is free or holds the same bytes, and no other document of the same publish goes to that path with other bytes.

So that rule 4 has evidence, the harness of zodbshootout, the native gate and the client benchmark record `run.dirty` as B5 and the replay do: true when `git status --porcelain --untracked-files=no` lists a change, false when it lists none, null when git fails. The three schemas get `run.dirty` as an optional key of type boolean or null. So each document that validates today stays valid.

### 5. `just bench publish`

```sh
just bench publish [--main REF] SRC... CHECKOUT
```

It runs `python -m shootout publish` in the environment of `bench/shootout`, which the `bench` recipe gives to every command that it does not know, as it does for `index`.

- SRC is a document, or a directory: the `*.json` files directly in it are documents, and its subdirectories are not read.
- CHECKOUT is an existing directory, a checkout of the results repository.

The command:

1. reads and checks every document (section 4). When one is not publishable, it prints each reason of each document, exits with 1 and writes nothing;
2. copies the bytes of each document to its path in CHECKOUT. A path that holds the same bytes stays as it is;
3. copies each file of `bench/dashboard/` to the same relative path in CHECKOUT, when that directory exists (ZOE-245);
4. writes `index.json` of version 2 from the whole tree of CHECKOUT (section 3);
5. prints the path of each document in CHECKOUT, then the path of `index.json`, and exits with 0.

It writes nothing outside CHECKOUT. It never runs git in CHECKOUT, never commits and never pushes.

| Exit | When |
| --- | --- |
| 0 | every document is in CHECKOUT, and `index.json` is of version 2 |
| 1 | a document is not publishable; CHECKOUT is unchanged, byte for byte |
| 2 | a usage error: CHECKOUT is not a directory, a SRC does not exist or holds no document, or argparse refuses the arguments; CHECKOUT is unchanged |

A refusal prints one line on stderr for each reason, `publish: SRC: REASON`, where SRC is the path of the document as the command line gave it, or the path of the file in a directory SRC. The last line is `publish: refused N of M documents; nothing changed in CHECKOUT`.

| Rule | REASON |
| --- | --- |
| 1 | `is not a JSON document`; `not a document of a known family`; `not valid against SCHEMA: MESSAGE`, with the file name of the schema and the message of jsonschema |
| 2 | `a quick run` |
| 3 | `no commit`; `commit COMMIT12 is not on REF` |
| 4 | `a run of a dirty tree`; `no record of a clean tree` |
| 5 | `the verification of run RUN is VERDICT`, for each run that did not pass |
| 6 | `PATH exists in CHECKOUT with other content`; `another document goes to PATH` |

A usage error prints `publish: CHECKOUT is not a directory`, `publish: SRC does not exist` or `publish: SRC holds no document`, with the path.

### 6. The B5 summary

`just bench plone report RUN... --json FILE` writes one document per campaign: the runs of one fixture. It validates the document against `bench/schema/plone.schema.json` before it writes it, so `bench/plone` gets `jsonschema` as a dependency. The runs of one document share one fixture, one commit, one machine name and the `settings` of the table below. When they do not, `report` prints the key and its values on stderr, exits with 2 and writes no FILE. The Markdown report keeps its behaviour. The document holds no host name, user name, path or password: no `zope_conf`, no traceback and no message of a check of the verification.

Every object of the schema has `additionalProperties: false`, except the maps: `software`, `machine.durability`, `machine.core_kinds`, `fixture.spec`, `scenarios[].mix` and `stacks[].merge_policy`. `bench/shootout/tests/data/publish/plone.json` is a valid document. A number that its counter does not give is null, never 0.

| Key | Holds |
| --- | --- |
| `schema_version` | 1 |
| `machine` | `name`: the name that the runs recorded, else `ZOE_BENCH_MACHINE`, else the CPU model as a slug, as the harness of zodbshootout does; `cpu`; `logical_cpus`; `memory_bytes` (or null); `os`; `kernel`; `core_kinds` (or null); `cpu_pinning`; `durability` (a text for each server); `store_disk`: `rotational` and `filesystem`, each or null |
| `python` | the interpreter of the Plone instances: `implementation` (`cpython` when the runs did not record it), `version`, `free_threaded`, `gil_enabled` (or null) |
| `software` | a version for each name: the eggs of the runs, `zoedb`, `zoe` and `locust`. A version that the runs did not record is absent. `Plone`, `ZODB` and `ZEO` are required |
| `run` | `started`: the first start of the runs, `YYYY-MM-DDTHH:MM:SSZ`; `commit` (or null); `dirty`: true when a run had a dirty tree, null when a run has no record, else false |
| `fixture` | `name`; `preset`: `small`, `medium` or `large` when the spec of the fixture equals the spec of that preset, else `custom`; `objects` and `catalog_entries` from the equivalence of the fixture; `spec`: the `SiteSpec`, numbers and lists of numbers |
| `settings` | from `stack.Settings`, without the ports and the switches: `instances`, `threads`, `zodb_cache_objects`, `pool_size`, `client_cache`, `server_cpus`, `zope_cpus` (a list), `locust_cpus`, `probe`, `zeo_fsync`, `zoe_cache_size` (or null), `zoe_compression` (or null); from the run: `warmup_seconds`, `warmup_users`, `trim_seconds`, `think`, `client_retries` |
| `stacks` | one per stack, in the order of the report: `name`; `description`; `server` (`zeo`, `zoe`); `client` (`zeoclient`, `zoeclient`); `native` (or null); `switches`: `backends` (a list), `member_properties`, `split_merges` and `folder_orders` (`on`, `shadow`, `off` or null); `merge_policy`: the policy that the runs of the stack recorded, or null |
| `scenarios` | one per scenario: `name`, `description`, `interface`, `mix`, `background` |
| `steps` | one per scenario, stack and step: `scenario`, `stack`, `step`, `users`, `seconds` (the measured seconds of one repetition of the step, after the trim), `repetitions`, `operations`, `ok_per_second` (`median`, `min`, `max`), `p50_seconds`, `p95_seconds`, `p99_seconds` (pooled over the repetitions), `p99_reliable`, `failure_rate`, `first_attempt_share`, `conflicts_per_1000_ops`, `exhausted`, `zope_cpu_ms_per_op`, `server_cpu_ms_per_op`, `store_write_kb_per_op`, `objects_loaded_per_second`, `objects_stored_per_second` |
| `runs` | one per run, in the order of their start: `id` (the run id), `scenario`, `stack`, `repetition`, `verification` (`pass`, `fail`, or `not run` for a run without a verdict), `violations` (or null) |

Each number of `steps` is the number that `report` prints, in the unit of its name. `objects_loaded_per_second` is the median, over the repetitions, of the `loadBefore` calls that the probe counts for the operations of the step (`loads` of `all.server` of the step), divided by the seconds of the step. `objects_stored_per_second` is the same for `stores`. A run without the numbers of its steps, such as a run that failed, is in `runs` and has no row in `steps`.

## Consequences

- One command fills a checkout of the results repository with every family. The human reviews the change of the checkout, then commits and pushes it.
- A document of today, without `run.dirty`, stays valid but is not publishable. A run in the container of the Dagger function `bench`, which has no `.git`, records `dirty` as null, so it is not publishable either (question 1).
- `publish` needs git and the history of REF in the ZOE repository. On a card branch, `--main HEAD` accepts the runs of the branch; ZOE-244 checks its tree that way.
- The tree of the harness of zodbshootout, `just bench gate` and the Dagger function `bench` do not change.
- `report --json` writes the document of section 6 in place of the internal summary.
- ZOE-244 writes one B5 summary per fixture in this format, beside the results of the native gate and of the client benchmark. ZOE-245 reads `index.json` of version 2 and the documents, and `publish` copies its page from `bench/dashboard/`.
- The acceptance tests: `bench/shootout/tests/test_publish.py` (`just bench test bench/shootout/tests/test_publish.py`), with one sample document of each family in `bench/shootout/tests/data/publish/`, and `bench/plone/tests/test_published_summary.py` (`just bench plone test bench/plone/tests/test_published_summary.py`), with two sample B5 run directories in `bench/plone/tests/data/runs/`. The samples hold numbers about a synthetic corpus and the first commit of the repository, which every branch holds.

## Alternatives

- **A uv project of its own for `publish`.** Its environment is lighter, but it adds a project, a lock file and a recipe outside the scope of the card, and it splits `publish` from the layout and from `index`, which live in `bench/shootout`.
- **`publish` in `bench/plone`.** It has no layout and no `jsonschema`, and it would also split `publish` from `index`.
- **Move the results of version 1 under `results/shootout/`.** The repository holds none, and a move changes paths that a link can name.
- **Publish the `result.json` of each B5 run.** It holds paths, and it has no number over the repetitions.
- **Know the family from the path of SRC, or from the first schema that validates.** A document can be anywhere, and a document that no schema validates then has no reason to give.
- **Accept a document with no record of its tree.** A run of a dirty tree then passes for a clean one.

## Questions for the human

1. **A document with no record of its tree is refused.** A run in the container of CI has no `.git`, so it records `dirty` as null. Recommendation: refuse it; a publication from CI gets a card of its own. Alternative: accept null when `ZOE_BENCH_COMMIT` names the commit, as CI sets it.
2. **A part of the native gate is publishable.** A run with `--cell` or `--scenario`, or a client run with other `--objects`, holds fewer cells or rows, and its document shows them. Recommendation: publish it as it is. Alternative: refuse a native run without the 30 cells of the gate set.
3. **The commit of main is a check with git.** `publish` runs `git merge-base --is-ancestor` against `origin/main`, and `--main REF` names another branch. Recommendation: yes. Alternative: no check, and the human checks the commit before the push.
