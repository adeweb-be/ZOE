# 0030. Dump files: a format of ZOE's own, one file per tid range

Status: accepted.

## Context

N15 asks for repozo-style dumps: `zoe backup` writes a full dump, then incremental dumps by tid range through the iterator, and `zoe restore` rebuilds the store from them. A restore must give the same tids, oids, pickles, user, description, extension and blob files, and sequences that never give a value twice (ADR 0024). A damaged dump must fail before anything is restored.

repozo copies the bytes of a `Data.fs`. For ZOE that means FileStorage transaction records, which FileStorage tools read. But the `.dat` list of repozo changes at each backup. A data record holds a back pointer across the whole file, so each incremental dump needs the offsets of every earlier dump. The user, description and extension have 2-byte lengths, so some stores cannot be dumped. A record has no checksum. Blobs and sequences need files of their own. So a dump would be four formats, and FileStorage tools read only one.

## Decision

A dump is one file in a format of ZOE's own. The backup writes it once and never changes it. To get a `Data.fs`, restore the dumps and run `zoe convert --to-data-fs`.

**The backup directory.** It holds a chain of dump files. Each covers the transactions after a tid `after` up to a tid `last`:

- The name is `<after>-<last>.zoedump`, each tid as 16 lowercase hexadecimal digits. The commands ignore and keep files with other names.
- The first dump is the full dump: `after` is 0. The `after` of each later dump is the `last` of the dump before it, so the names sort in chain order.
- Only the full dump of an empty store has `after` equal to `last`.
- A dump that a backup still writes has the suffix `.tmp`.

**The file.** All integers are big-endian. A byte string is its length in 4 bytes, then its bytes.

| Part | Bytes |
| --- | --- |
| magic | `ZOEDUMP`, then the version `0x02`: 8 bytes |
| header | `after`, 8 bytes; `last`, 8 bytes |
| sequences | zero or more sequence entries |
| durable rows | zero or more durable row entries, in key order: every row of the durable KV namespaces at `last`, generation rows included (ADR 0043) |
| transactions | zero or more transaction entries, each followed by its records |
| end | the tag `E` (`0x45`), then the BLAKE3 hash of every byte before the hash, the tag included: 32 bytes |

| Entry | Tag | Fields after the tag |
| --- | --- | --- |
| sequence | `S` (`0x53`) | the name, as a byte string of UTF-8; the end, 8 bytes |
| durable row | `K` (`0x4B`) | the namespace, the key and the frame payload, each as a byte string. A generation row has an empty key, and its payload is the tid of the last commit that wrote the namespace |
| transaction | `T` (`0x54`) | the tid, 8 bytes; the user, the description and the extension, as byte strings; the number of its records, 8 bytes |
| record | `R` (`0x52`) | the oid, 8 bytes; the pickle, as a byte string |
| deletion | `D` (`0x44`) | the oid, 8 bytes |
| record with a blob | `B` (`0x42`) | the oid, 8 bytes; the pickle, as a byte string; the blob size, 8 bytes; the BLAKE3 hash of the blob, 32 bytes; the blob bytes |

The rules of a file:

- The end of a sequence is the first value that the store has not reserved (ADR 0024).
- The tids increase, each after `after` and at or before `last`.
- The records of a transaction follow it, as many as it says, in increasing oid order. A transaction can have none.
- A pickle is never empty and never compressed. A deletion is the entry `D`.
- The size and hash of a blob are those of its blob record (ADR 0023).
- Nothing follows the hash.

A reader refuses a file that breaks a rule, a blob that does not match its hash, and a file whose hash does not match.

**Versions.** The last byte of the magic is the version. A change of the layout takes the next version. Each build reads every earlier version and refuses a later one. A dump of version `0x01` has no durable rows.

**The commands.** The rules of `zoe backup`, `zoe restore` and `zoe verify --backup`: specification, "Loupe: desktop inspector and editor", "`zoe backup` and `zoe restore`: dumps (N15)". These rules are not in the specification:

- `zoe backup` creates `DIR` when it is missing, and takes the `flock` on a descriptor of `DIR`, so the lock adds no file. It removes the `.tmp` files of a stopped backup. A chain with a missing dump or an overlap fails the backup. The pack watermark never excuses a last tid of the dumps that the store does not have. The backup syncs the dump, renames it, then syncs `DIR`. A failed backup removes its `.tmp` file.
- `zoe restore` refuses a target that is not a directory. Its hash pass reads no length, so a damaged length allocates nothing. It fails when the work directory `<STORE>.restore` exists. It restores with `tpc_begin_restore`, `restore` and `restore_blob`, and raises each sequence with `Sequence::restore_end`. A crash leaves the work directory, never a store at the target.
- `zoe verify --backup` writes nothing outside the scratch directory. It forgives a current record that the store lacks when its serial is at or before the pack watermark.

## Consequences

- The hash covers every byte, so a damaged dump fails its check wherever the damage is. The hash of each blob names the blob that a damage hits.
- No field limit below 4 GiB: every transaction of the store can be dumped.
- A backup reads only the names and headers of earlier dumps.
- A backup with nothing new writes nothing, also when a sequence moved. A value given after the last dump can come again after a restore.
- A pack at or before the last tid of the dumps lets the backups go on: on a quiet site, run `zoe pack --days 0` right after a backup. A later pack needs a new directory. ZOE does not start a new full dump at a pack, as repozo does, because each full dump copies every blob again.
- A damaged dump in the chain makes the dumps after it useless. Start a new directory.
- A damaged store stops every backup until it is repaired.
- A backup holds one transaction and a 64 KiB blob buffer in memory. A restore holds one record and the same buffer. A blob of 200 MB took 12 MB of memory to back up and restore.
- The dumps hold no `next_oid`. A restored store gives new oids above every oid of the dumps.
- `zoe backup` takes the lock of the store, so it cannot back up a store that a server holds. It takes no blob directory option.
- No FileStorage tool reads a dump.

## Alternatives

- **repozo's files with FileStorage records.** Four formats, an offset index across dumps, 2-byte field lengths and no checksum.
- **The record frame of ADR 0020 for each entry.** The dumps would follow each change of the store format. One hash for the file is enough.
- **A dump as a directory with blob files.** The blobs are outside the hash of the file, and each needs an entry in a list.
- **A copy of the redb file.** No incremental dump by tid range, and it depends on the page format of redb.
