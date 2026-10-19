# ZOE benchmark results

This repository holds the published benchmark results of [ZOE](https://github.com/adeweb-be/ZOE), the Zope Object Engine. The results are numbers from a synthetic corpus, never data. The dashboard is the GitHub Pages site of this repository: <https://adeweb-be.github.io/zoe-bench-results/>.

Do not edit the page or this README here. Their source is [`bench/dashboard/`](https://github.com/adeweb-be/ZOE/tree/main/bench/dashboard) in the ZOE repository, and `just bench publish` copies the files that git tracks there into a checkout of this repository.

## Layout

One layout holds every family of results (ADR 0054 of the ZOE repository):

```text
index.json
index.html, data.js, README.md          the page: the tracked files of bench/dashboard/
results/shootout/<machine>/<python>/<storage>/<started>-<commit>.json
results/native/<machine>/<python>/<started>-<commit>.json
results/client/<machine>/<python>/<started>-<commit>.json
results/plone/<machine>/<python>/<fixture>/<started>-<commit>.json
```

| Family | What it measures | Command | Schema |
| --- | --- | --- | --- |
| `shootout` | zodbshootout against each storage (B1) | `just bench` | [`results.schema.json`](https://github.com/adeweb-be/ZOE/blob/main/bench/schema/results.schema.json) |
| `native` | the native gate: `zoe serve` through the native client against ZEO 6 | `just bench native` | [`native.schema.json`](https://github.com/adeweb-be/ZOE/blob/main/bench/schema/native.schema.json) |
| `client` | the client benchmark: `ZEO.ClientStorage` against the native client (B7) | `just bench client` | [`client.schema.json`](https://github.com/adeweb-be/ZOE/blob/main/bench/schema/client.schema.json) |
| `plone` | the Plone end-to-end benchmark, one summary per campaign and fixture (B5) | `just bench plone report --json` | [`plone.schema.json`](https://github.com/adeweb-be/ZOE/blob/main/bench/schema/plone.schema.json) |

- `<machine>` is the name of the machine. `<python>` is a short name for the Python build: `cp313`, or `cp314t` for a free-threaded 3.14. For `plone` it is the Python of the Plone instances.
- `<started>` is the UTC start of the run, `YYYYMMDDTHHMMSSZ`. `<commit>` is the first 12 characters of the ZOE commit. `<fixture>` is the name of the fixture of the campaign, such as `small-ba01ff255313`.
- A result in the layout of version 1, `results/<machine>/<python>/<storage>/<started>-<commit>.json`, is a `shootout` document. It keeps its path.

## `index.json`

`index.json` of version 2 lists every document, oldest first (by `started`, then by `path`). Each entry holds:

| Key | Holds |
| --- | --- |
| `family` | `shootout`, `native`, `client` or `plone` |
| `path` | the path of the document under the root of this repository |
| `machine` | the name of the machine |
| `python` | the short name of the Python build |
| `stacks` | what the document compares: the storage (`shootout`), the storages of its cells (`native`), the clients of its rows (`client`), its stacks (`plone`) |
| `fixture` | the name of the fixture for `plone`, else null |
| `scenarios` | its scenarios, or the workloads of the client benchmark |
| `started` | the start of the run, ISO 8601 in UTC |
| `commit` | the ZOE commit of the run |

`just bench index CHECKOUT` writes the index again from each `*.json` file under `results/`. An index of version 1 has no family: its documents are `shootout`.

## Publish

In the ZOE repository, inside `devenv shell`:

```sh
just bench showcase --plan                                  # the set and its estimate; runs nothing
just bench showcase                                         # B5, the native gate, the client benchmark
just bench publish target/bench/showcase/<started> CHECKOUT
just bench publish target/bench/results/results/<machine>/<python>/<storage> CHECKOUT
```

`just bench publish [--main REF] SRC... CHECKOUT` checks each document first. It takes a document only when it is valid against the schema of its family, is not a quick run, has its commit on `origin/main` (or on REF), comes from a tree with no change beside its commit, and, for B5, has every run verified. When it refuses one, it names the document and the reason, and changes nothing. Else it copies the documents into the layout, copies the files that git tracks in `bench/dashboard/`, and writes `index.json` of version 2, last. It warns when a file of the page differs from the commit, because it copies the file of the working tree. When a write fails after the checks, it names the files that it wrote and exits with 3. It never commits and never pushes. Review the change of the checkout, then commit it. A push to this repository needs the approval of the maintainer. GitHub Pages serves the dashboard from `main`.

## The dashboard

`index.html` is one static page with no build step. `data.js` reads `index.json`, then each document that it lists, and gives the page its data. The page loads Chart.js from cdnjs at a pinned version, and nothing else from outside this repository.

- A selector of the machine and of the Python build applies to every section.
- For each family of tables, the page shows the latest document (for B5, the latest of each fixture), and a selector gives the earlier ones. zodbshootout shows every run.
- **Plone end-to-end (B5).** Each stack with what it turns on: the server, the client, the backends of `zoe.plone`, the switches and the merge policy. One table per scenario, with one group of rows per step and one row per stack: the ok operations per second, p50, p95 and p99, the `ConflictError`s per 1,000 operations, the first-attempt successes, the exhausted retries, the CPU per operation of Zope and of the server, the store KB written per operation, the objects loaded and stored per second, and the verdict of the verification. Below each number of a stack other than `zeo`: its ratio, the number of the stack divided by the number of `zeo` in the same step. For `edit` and `create-spread`, a chart of the users against the operations per second and against the `ConflictError`s per 1,000 operations.
- **Native gate.** Each cell and scenario with its ratio `t(zeo) / t(zoe-native)` (above 1, zoe-native is faster), its interval and its verdict, as the document holds them. A dash marks a result that the document does not hold.
- **Client benchmark.** Each row of the document.
- **zodbshootout.** For each cell and metric, one chart per scenario over every run, with one line per storage. A point links to its ZOE commit.
- **The method** of each document: the machine, Python, the software, the commit, and for B5 the fixture with its object counts, the settings and the scenarios.

The page computes no number that a document does not hold, except the ratio of a number of a B5 step to the number of `zeo` in the same step. It rounds the numbers to three significant digits and scales their units (seconds to milliseconds, shares to percent).

## License

Apache-2.0. See [LICENSE](LICENSE).
