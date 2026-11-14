# 0054. One published format for every benchmark: one layout, the index of version 2, the B5 summary and `just bench publish`

Status: accepted. It changes no on-disk format, no wire protocol and no public Python API. It changes the output of `just bench plone report --json` and the layout of the results repository. The amendment at the end, the family `catalog`, is accepted.

## Context

The results repository, `adeweb-be/zoe-bench-results`, must publish every family of results: zodbshootout (B1), the native gate, the client benchmark (B7) and B5. Its layout of version 1, `results/<machine>/<python>/<storage>/<started>-<commit>.json`, fits zodbshootout only. The `result.json` of a B5 run holds paths and tracebacks, and no number over the repetitions, so it is not a document to publish.

The page shows every result as plain tables, and does not apply the rule of the native gate. On B5 it shows `zoe-native-qi-kv` beside `zeo`. A push to the public repository needs the approval of the human.

## Decision

The specification states the result ("Performance and testing harness", "Regression tracking"). `bench/README.md`, "The results repository", gives the layout, the index and the steps of `publish`.

### 1. The families

The families are `shootout`, `native`, `client` and `plone`. A document names its family by one key at its top level: `storage`, `cells`, `rows` or `fixture`. A document with none of these keys, or with more than one, is of no known family.

### 2. The layout

```text
index.json
results/shootout/<machine>/<python>/<storage>/<started>-<commit>.json
results/native/<machine>/<python>/<started>-<commit>.json
results/client/<machine>/<python>/<started>-<commit>.json
results/plone/<machine>/<python>/<fixture>/<started>-<commit>.json
```

- `<machine>` is `machine.name`, never a family name. `<python>` is the short name of the build (`cp313`, `cp314t`); for `plone`, of the Plone instances. `<started>` is `YYYYMMDDTHHMMSSZ`. `<commit>` is 12 characters. `<fixture>` is `fixture.name`.
- A path of version 1 holds a `shootout` document, and keeps its path.
- The harness of zodbshootout keeps its own tree of version 1 under `target/bench/results`, which `just bench gate` reads. Only `publish` writes the layout above.

### 3. `index.json` of version 2

`{"index_version": 2, "results": [...]}`. Each entry has `family`, `path`, `machine`, `python`, `stacks`, `fixture` (null except for `plone`), `scenarios`, `started` and `commit`, in the order of `started`, then of `path`. `just bench index DIR` writes it from each `*.json` file under `results/`. The first directory under `results/` names the family; a file under another directory is a `shootout` document of version 1. `index` never reads the old index, so it reads a checkout of either version.

### 4. What is publishable

A document is publishable when each of these holds:

1. It is JSON, of a known family, and valid against the schema of its family.
2. It is not a quick run of zodbshootout (`run.quick` is true, or `run.set` is `quick`).
3. `run.commit` is on REF: `git merge-base --is-ancestor COMMIT REF` succeeds in the ZOE repository. REF is `origin/main`, or `--main REF` (section 7, point 3).
4. `run.dirty` is false. Absent or null is no record of a clean tree (section 7, point 1).
5. For `plone`: the verification of each run of `runs` is `pass`.
6. Its path in the checkout is free or holds the same bytes, and no other document of the publish goes there with other bytes. Each part of the path is a name (a letter or a digit, then letters, digits, `.`, `_` or `-`), no part is a symbolic link or a file, and an existing path is a regular file. So a document never writes outside `results/`.

Every harness records `run.dirty`: true when `git status --porcelain --untracked-files=no` lists a change, false when it lists none, null when git fails or `ZOE_BENCH_COMMIT` names another commit than the tree. It is an optional key of the schemas, so older documents stay valid.

### 5. `just bench publish`

```sh
just bench publish [--main REF] SRC... CHECKOUT
```

It runs `python -m shootout publish` in the environment of `bench/shootout`. A SRC is a document, or a directory whose `*.json` files are documents. CHECKOUT is an existing checkout of the results repository.

- It checks every document, every path and the new index before it writes. A refusal prints one line for each reason and changes nothing.
- It copies the documents, then the files of `bench/dashboard/` that git tracks (no dotfiles, `__pycache__` or symbolic links, never to `results/` or `index.json`), then writes `index.json` last. Each file goes to a random temporary name, then replaces its path.
- It warns about a page file that differs from `HEAD`, and copies the working tree.
- It writes nothing outside CHECKOUT, never runs git there, never commits and never pushes.
- Exit 0: done. Exit 1: a document or a path is refused; CHECKOUT is unchanged. Exit 2: a usage error, such as a REF that git cannot read; CHECKOUT is unchanged. Exit 3: a write failed after the checks; `index.json` is not written.

### 6. The B5 summary

`just bench plone report RUN... --json FILE` writes one document per campaign. It validates the document against `bench/schema/plone.schema.json`, which holds its fields, before it writes it. `bench/shootout/tests/data/publish/plone.json` is a sample.

- The runs share one fixture, commit, machine name, Python of the instances and settings. Else `report` prints the key and its values, exits with 2 and writes no FILE. It writes the Markdown report in every case.
- The machine name is the name that the runs recorded, else `ZOE_BENCH_MACHINE` of the report, else a slug of the CPU model.
- A number that its counter does not give is null, never 0. A run without step numbers is in `runs`, not in `steps`.
- Each entry of `runs` holds `zope_clock_mhz`, the mean clock of the Zope CPUs during the steps, and `zope_throttle_seconds`, the throttle time of their cores during the steps, when a run of the campaign records them; each is null for a run that does not give it. They are optional keys, so a document of runs that record neither stays as it is.
- The document holds no host name, user name, path or password.

### 7. A run without a record of its tree, a part of the gate, and the commit of main

1. **A document with no record of its tree is not publishable** (section 4, rule 4). A run in the container of CI has no `.git`, so nothing publishes from CI.
2. **A part of the native gate is publishable.** A run with `--cell` or `--scenario`, or a client run with other `--objects`, shows the cells or rows that it holds.
3. **The commit of main is a check with git** (section 4, rule 3). `--main REF` names another branch.

### 8. The outcome of a scenario of the native gate

Each scenario of a cell of a native result holds `ratio`, `low`, `high` and `verdict`, as `shootout.native.decide` gives them with the rule of the run (`bench/schema/native.schema.json`). A value that is not finite is null. The keys are optional. `just bench native report` applies the rule again, and its verdict agrees.

## Consequences

- One command fills a checkout with every family. The human reviews the change, then commits and pushes it.
- A document without `run.dirty` stays valid but is not publishable.
- `publish` needs git and the history of REF. On a card branch, `--main HEAD` accepts the runs of the branch.
- `just bench gate` and the Dagger function `bench` do not change.
- The acceptance tests: `bench/shootout/tests/test_publish.py` and `bench/plone/tests/test_published_summary.py`.

## Alternatives

- **A uv project of its own for `publish`.** One more lock file, and `publish` apart from `index`.
- **`publish` in `bench/plone`.** No layout and no `jsonschema` there.
- **Move the version 1 results under `results/shootout/`.** It breaks links.
- **Publish the `result.json` of each B5 run.** It holds paths and no number over the repetitions.
- **The family from the path, or from the first schema that validates.** A document can be anywhere, and an invalid one gets no reason.
- **Accept a document with no record of its tree.** A dirty run passes for a clean one.
- **Refuse a native run without the 30 cells of the gate set.** A document shows what it holds.
- **No commit check; the human checks before the push.** A commit off main passes when the human misses it.

## Amendment: the family `catalog`

Status: accepted. Card ZOE-300, decision Q14 of the plan of phase 6 (#597). It adds a fifth family for the results of `zoe-bench catalog` (ZOE-299). It changes no on-disk format, no wire protocol and no public Python API. The four families of sections 1 to 8 do not change.

1. **The family and its key** (section 1). The family `catalog` has one document for each run of `just bench catalog`, valid against `bench/schema/catalog.schema.json`. Its key is `catalog`: the fixture of the run, with its `name`, its `preset` (the preset whose spec equals the spec of the fixture, else `custom`) and its counts. No other family uses this key, and a catalog document holds none of the keys of the other families. In `plone` the fixture is the key `fixture`. A catalog document cannot use that key, because `fixture` names `plone`.
2. **The layout** (section 2). `results/catalog/<machine>/<fixture>/<started>-<commit>.json`. `<machine>` is `machine.name`, and `<fixture>` is `catalog.name`. `run.started` is in UTC to the second, so `<started>` is `YYYYMMDDTHHMMSSZ`. The path has no `<python>` part, where the four families have one. No Python runs on the measured path: the client is `zoe-bench`, the server is `zoe serve`. A Zope process makes the plans once and stores them with the fixture, so its Python build belongs to the fixture, not to the run. A `<python>` part would split the runs of one machine by a build that does not change their numbers. `catalog` joins the family names that a machine cannot take; no machine of the results repository has it.
3. **The entry in `index.json`** (section 3). `family` is `catalog`; `path`; `machine`; `python` is null, for the reason of point 2; `stacks` is `["zoe"]`; `fixture` is `catalog.name`; `scenarios` are the names of the scenarios, in the order of the document; `started` and `commit` come from `run`. So `fixture` is null except for `plone` and `catalog`. `stacks` lists the stacks that ran, as in every family. One stack runs, `zoe serve` with its mirror, and the document holds no list of stacks, so the entry gives the name `zoe`. `index_version` stays 2: an entry keeps its keys, and `family` gets one more value.
4. **What is publishable** (section 4). Rules 1, 3, 4 and 6 apply as they are: valid against the schema, a commit on REF, `run.dirty` false, a free path. Rule 2 is for `shootout` and rule 5 for `plone`. A catalog document has one more rule: it is a full run. `catalog.preset` is `catalog-1m`, and the document holds each scenario of the default set: `intersect`, `text`, `mix` and `cold`. So `publish` refuses a run of one scenario, such as `just bench catalog text`, and a run on `catalog-mini`, with the reason. This departs from section 7, point 2, where a part of the native gate is publishable. G2, G3 and G4 read one run of the whole set on `catalog-1m`, and the dashboard shows the latest document of a machine, so a part of a run would hide the scenarios that it does not hold.
5. **`text-rebuild`.** The rebuild of the text index with one thread is not in the default set, so a full run stays near 30 minutes. A document of the default set can also hold it. A document of `text-rebuild` alone is a part of a run, and `publish` refuses it.
6. **Refusals and errors.** The counts of the replies `f` and `e` are numbers of the run. They do not make its document unpublishable. The differential suite (G1) judges correctness, and a reply `not_ready` during `cold` is expected.
7. **No private data** (section 6). The document holds no host name, user name, path, address or password, as the B5 summary. So the paths of the fixture and of the store, and the address of `zoe serve`, are not in it.
8. **`publish`, `index` and the dashboard** (section 5). `just bench publish` and `just bench index` take a catalog document as they take the others. For each machine and fixture, the dashboard shows the latest catalog document as plain tables, with the earlier ones a selector away. It computes no number that the document does not hold.

### Consequences of the amendment

- `bench/README.md` ("The results repository"), `bench/dashboard/README.md` and "Regression tracking" of the specification name the fifth family.
- The acceptance tests: `bench/shootout/tests/test_publish_catalog.py`, the catalog cases of `bench/shootout/tests/test_dashboard.py`, and `crates/zoe-bench/tests/catalog_document.rs`.
- A checkout without a catalog document gets the same index as before.

### Alternatives of the amendment

- **The key `fixture`, as in `plone`.** One key would name two families.
- **A `<python>` part, with the build of the plans.** It names the build of a step of the fixture, not of the run.
- **A part of the set is publishable, as for the native gate.** The latest document of a machine would hide the scenarios that it does not hold.
- **`index_version` 3.** No key of an entry changes.

### Points settled at acceptance

1. **`python` of the entry.** Null. The other choice was the build of the plans.
2. **`text-rebuild` alone.** Refused. ZOE-299 runs `text-rebuild` alone, so its numbers publish only when `just bench catalog` can run it with the default set in one run. The other choice was: `text-rebuild` alone on `catalog-1m` is a full run of its own set, and the dashboard shows the latest document of each set.
