# Compatibility

ADR 0006 records the targets and their reasons.

| Component | Versions |
| --- | --- |
| CPython | 3.12, 3.13 and 3.14, and the free-threaded build 3.14t. 3.13t is not a target. 3.15 joins when it ships. |
| Operating system | Linux x86-64, and macOS on Apple silicon. A test run on either counts. CI runs on Linux. Windows is not a target. |
| ZODB | 6.0 or later. The wheel requires `ZODB>=6`. ZODB 5 is not a target. |
| ZEO | 6.0 or later, client and server. ZEO 5 is not a target. |
| Plone | 6.1 or later. The extra `zoedb[plone]` requires `Products.CMFPlone>=6.1`. A site on Plone 6.0 upgrades Plone first. |
| ZConfig | 4 or later. |
| zodburi | 3 or later, for the `zoe://` URI. |
| msgpack | 1 or later, on each stock ZEO client of `zoe serve`. The extra `zoedb[plone]` installs it. |

## Python packages

| Distribution | Import | Requires |
| --- | --- | --- |
| `zoedb` | `zoe` | `ZConfig>=4`, `ZODB>=6` |
| `zoedb[plone]` | `zoe.plone` | `Products.CMFPlone>=6.1`, `msgpack>=1` |
| `zoedb[uri]` | none | `zodburi>=3`, for the `zoe://` URI |

The wheel itself never imports Plone. Only `zoe.plone` does.

## Free-threaded CPython

The extension module `zoe._zoe` declares that it runs without the GIL. On CPython 3.14t, `persistent`, `BTrees` and `zodbpickle` turn the GIL back on unless they declare that they run without it. The workspace of the repository uses forks of these three packages that declare it (see [Known upstream issues](../findings/persistent-inline-values.md)).

## Platform differences

On macOS, a sync of the store is `fsync(2)`, as FileStorage does with `os.fsync`. A commit survives a crash of the process and of the system. A loss of power can lose the commits that the drive cache held. On Linux, a sync is `fsync` or `fdatasync` through the standard library of Rust.

The tests that read `/proc` run on Linux only. The static `zoe` binary of a release needs Linux to build.

## Data

- ZOE opens a store of `ZOE format 2`. It refuses a store of `ZOE format 1` (ADR 0033).
- `zoe convert` copies a `Data.fs` into a store and back, with tids, oids, pickles and blobs unchanged (see [The zoe command](cli.md#zoe-convert)).
- A `Data.fs` holds no KV namespace. `zoe convert --to-data-fs` refuses a store with the data of `zoe.plone` that a `Data.fs` cannot hold, unless `--force`.
