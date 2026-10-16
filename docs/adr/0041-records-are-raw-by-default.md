# 0041. Records are raw by default; zstd is a setting

Status: accepted. The human chose it after the phase 3 gate of ZOE-201.

## Context

N14 asked for zstd on every record, at level 3 by default. The store then decompresses each record that it reads. On the development laptop, the zstd decompression of one 100 KB record of zodbshootout costs about 60 µs, and a cold read of it through ZEO takes about 140 µs in all. With zstd, `zoe serve` through the native client ties with ZEO on `read_cold` at 100 KB and concurrency 1, and stored raw it is 1.25× faster. FileStorage and ZEO store their records raw.

zstd makes a store smaller, and a commit of small records writes fewer bytes. It costs CPU on each read and each write.

## Decision

The default compression of a store is `none`: each new record is written as it is, with codec 0. `Options::default()` of `zoe-core` has `Compression::None`.

The setting `compression` chooses it: `none`, `zstd` (zstd at level 3) or `zstd:LEVEL`. `zoe serve` takes it as `--compression` (`ZOE_COMPRESSION`, the key `compression` of the configuration file and of `<zoe-server>`), and the in-process storage as the key `compression` of `<zoe>` (`ZOE_COMPRESSION`, the query of `zoe://`) and the argument `compression` of `zoe.Storage`. One parser, `FromStr` for `Compression` in `zoe-core`, reads the text for all of them.

The format does not change. A store reads every codec, whatever its setting, so a store can change its setting between opens, and a store written with zstd stays readable.

## Consequences

- A new store is as large as its records. N14's target, a Plone store at least 3× smaller than `Data.fs`, holds with `compression zstd` only.
- The tests of zstd behaviour (the bit flips of zstd payloads, the attached payloads, the compression pool of `zoe serve`) open their stores with zstd.
- The phase 3 gate runs `zoe serve` with its defaults.

## Alternatives

- **zstd by default, `none` for the gate only.** The default then loses the cold reads of large records against ZEO, the reads that the gate measures.
- **zstd for small records and raw for large ones.** It is a policy that the specification does not name, and a store would then mix codecs by size.
