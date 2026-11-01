# 0054. One published format for every benchmark: one layout, the index of version 2, the B5 summary and `just bench publish`

Status: accepted. It changes no on-disk format, no wire protocol and no public Python API. It changes the output of `just bench plone report --json` and the layout of the results repository.

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
