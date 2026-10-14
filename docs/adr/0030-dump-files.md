# 0030. Dump files: a format of ZOE's own, one file per tid range

Status: accepted. The human approved the layout with the pull request that builds `zoe backup` and `zoe restore`.

## Context

N15 asks for repozo-style dumps in phase 2: `zoe backup` writes a full dump, then incremental dumps by tid range through the iterator, and `zoe restore` rebuilds the store from them. A dump must restore a store with the same tids, oids, pickles, user, description and extension, the same blob files, and sequences that never give a value twice (ADR 0024: a copy through the iterator alone repeats values). The card asks for a checksum on each dump file, so that a damaged dump is found before anything is restored, and a backup never changes a dump that its directory holds.

A dump is a persistent format outside the store. Two layouts were weighed.

**Repozo's files.** repozo copies the bytes of a `Data.fs`: `<date>.fs` for a full backup, `<date>.deltafs` for the bytes that the file gained since, and `<date>.dat`, one line per file with its name, its start and end offsets and its MD5. An incremental backup appends its line to the `.dat` of the full backup. A restore concatenates the files. repozo copies no blob, and has nothing like a sequence.

For ZOE this means FileStorage transaction records, which `zoe-convert` already writes for `zoe convert --to-data-fs`. The full dump would be a `Data.fs`, and the dumps put end to end would be one: FileStorage, `fstest`, `fsdump` and `zodbconvert` would read them, and a restore would not need ZOE. But:

- The `.dat` of repozo changes a file of the full dump at each incremental backup. So ZOE needs a list of its own beside the files.
- A data record holds the offset of the previous record of its oid in the whole file. An incremental dump continues the offsets of the dumps before it, so each backup must rebuild the offset of the newest record of each oid from every earlier dump, or keep an index file of its own.
- A transaction record holds the lengths of the user, the description and the extension in 2 bytes each. A ZOE store takes longer ones, so some stores could not be dumped.
- A FileStorage record has no checksum. The checksums go into the list.
- The blobs need a blob tree per dump, and the sequences a file of their own.

So a dump would be four formats: the transaction records, the blob trees, the sequence file and the list. FileStorage tools read only the first one.

**A format of ZOE's own.** One file per dump holds the transactions, their records, their blobs and the ends of the sequences, and ends with a hash of its bytes.

## Decision

**A format of ZOE's own.** A dump is one file. The backup writes it once and never changes it. To get a `Data.fs`, restore the dumps and run `zoe convert --to-data-fs`.

**The backup directory.** A backup directory holds a chain of dump files. Each one covers the transactions after a tid `after` up to a tid `last`:

- The name is `<after>-<last>.zoedump`, each tid as 16 lowercase hexadecimal digits. Names of other forms do not count, and the commands leave such files alone.
- The first dump is the full dump: `after` is 0. Each later dump starts after the `last` of the dump before it. So the names sort in the order of the chain.
- Only the full dump of an empty store has `after` equal to `last`: `0000000000000000-0000000000000000.zoedump`.
- A dump that a backup still writes has the suffix `.tmp` after its name.

**The file.** All integers are big-endian, as ZODB writes oids and tids. A byte string is its length in 4 bytes, then its bytes.

| Part | Bytes |
| --- | --- |
| magic | `ZOEDUMP`, then the version `0x01`: 8 bytes |
| header | `after`, 8 bytes; `last`, 8 bytes |
| sequences | zero or more sequence entries |
| transactions | zero or more transaction entries, each followed by its records |
| end | the tag `E` (`0x45`), then the BLAKE3 hash of every byte of the file before the hash, the tag `E` included: 32 bytes |

| Entry | Tag | Fields after the tag |
| --- | --- | --- |
| sequence | `S` (`0x53`) | the name, as a byte string of UTF-8; the end, 8 bytes |
| transaction | `T` (`0x54`) | the tid, 8 bytes; the user, the description and the extension, as byte strings; the number of its records, 8 bytes |
| record | `R` (`0x52`) | the oid, 8 bytes; the pickle, as a byte string |
| deletion | `D` (`0x44`) | the oid, 8 bytes |
| record with a blob | `B` (`0x42`) | the oid, 8 bytes; the pickle, as a byte string; the size of the blob, 8 bytes; the BLAKE3 hash of the blob, 32 bytes; the bytes of the blob |

The rules of a file:

- The sequence entries come before the first transaction. The end of a sequence is the end of its row (ADR 0024): the first value that the store has not reserved.
- The tids of the transactions increase. Each one is after `after` and at or before `last`.
- The records of a transaction follow it, as many as it says, in increasing oid order. A transaction can have none.
- A pickle is never empty: a deletion is the entry `D`.
- The blob is the file of the blob record of `(oid, tid)` in the store: its size and its hash are those of the blob record (ADR 0023).
- Nothing follows the hash.

A reader refuses a file that breaks a rule, a blob whose bytes do not match its hash, and a file whose hash does not match.

**Versions.** The last byte of the magic is the version of the layout. A change of the layout takes the next version. Every later build reads every earlier version, so a dump never needs a conversion before a restore. A build refuses a version after its own.

**`zoe backup --path STORE --to DIR`.** The command opens the store read-only and writes one dump:

1. It creates `DIR` when it is missing, and takes an exclusive `flock` on a descriptor of `DIR` itself. So two backups never write into one directory together, and the lock adds no file. It removes the `.tmp` files that a stopped backup left.
2. It reads the names and the headers of the dumps, and checks the chain. A dump that does not follow the dump before it fails the backup: a dump is missing, or two dumps overlap.
3. It reads the last tid of the store, then the end of each sequence. A transaction up to the last tid committed after its values were reserved, so each value that it holds is below the end in the dump.
4. With dumps in `DIR` that end at the last tid of the store, it writes nothing. Otherwise the store must continue the dumps, else the backup fails and writes nothing:
   - The last tid of the dumps must be a tid of the store. Else the dumps are dumps of another store, or of another history of this store: a store restored from a part of the chain and then changed, for example.
   - The pack watermark of the store must be at or before the last tid of the dumps. A pack removes only records at or before its tid. So a pack at the last tid of the dumps removes only history that the dumps hold, and the next dump starts after that tid. A pack after it changed history that the next dump must hold. The watermark does not tell one store from another, so it never excuses a missing transaction.

   The message of both refusals asks for a new full dump in a new directory.
5. It writes the dump through the iterator into `<name>.tmp`, syncs it, renames it to its name and syncs `DIR`. A failed backup removes the `.tmp` file.

**`zoe restore --from DIR --path STORE`.** The command:

1. refuses a target that holds a store or other files, or that is not a directory: only a new or an empty directory takes a restore. It also refuses an empty directory on another filesystem than its parent, a mount point: the rename of step 6 cannot cross filesystems;
2. checks the BLAKE3 hash of every dump of the chain, before it writes anything. This pass reads each file as a stream and reads no length in it, so a damaged length allocates nothing. A file whose hash matches but that breaks a rule of the layout fails the restore later, while it builds the store;
3. creates the work directory `<STORE>.restore` beside the target, and fails when it exists: another restore can be writing into it;
4. opens a store in the work directory and restores each transaction in order with `tpc_begin_restore` under its own tid, each record with `restore`, and each blob with `restore_blob` after a check of its BLAKE3 hash;
5. raises each sequence to its end with `Sequence::restore_end`, which writes the end durably and never lowers it, so each sequence restarts at or above its end in the dumps;
6. closes the store, renames the work directory to the target and syncs the parent. Its report gives the last tid of the dumps and its time in UTC.

A restore that fails removes the work directory, so it leaves nothing at the target. A restore that a crash stopped leaves the work directory, never a store at the target.

**`zoe verify --path STORE --backup DIR`.** The command restores the dumps into a scratch directory in the temporary directory (`TMPDIR`), compares them with the store up to the last tid of the dumps, and removes the scratch directory:

- Each transaction and each record must be the same, with the same blob record. The dumps keep the records that a pack removed: at or before the pack watermark of the store, a record or a transaction that the store lacks is no difference.
- A pack watermark after the last tid of the dumps is a difference: the pack changed history that the next dump must hold, and the backup refuses to continue the dumps. A pack watermark equal to the last tid of the dumps is no difference.
- Otherwise, the current record of each oid at the last tid of the dumps must be the same in the store and in the dumps. So a store whose history diverged differs, also where the comparison of the transactions forgives a pack. A record that the store lacks is no difference when its serial is at or before the pack watermark: a pack removed an object that no reader could reach.
- The end of each sequence in the store must be at or above its end in the dumps.

A difference is a finding, and a dump that does not restore fails the command. A check names its scratch directory with the id of its process, and removes the scratch directories of a check whose process is gone, which a killed check leaves.

## Why a format of ZOE's own

- One file per dump, written once and renamed into place. A backup reads only the names and the headers of the earlier dumps, never their records.
- The hash covers every byte of the file, so a damaged dump fails its check, wherever the damage is. The hash of each blob, from its blob record, also names the blob that a damage hits.
- No field limit below 4 GiB: every transaction that the store took can be dumped.
- The blobs and the sequences are in the same file as the transactions that use them.
- The repozo route gives FileStorage tools the transaction records only, and costs a list, a blob tree per dump, a sequence file, and an offset index across dumps. `zoe convert --to-data-fs` gives a `Data.fs` from a restored store.

## Alternatives

- **repozo's files and FileStorage transaction records**, weighed above.
- **The record frame of ADR 0020 for each entry.** Each entry would carry its own xxh3-64 checksum. The frame is the format of the store, a red zone, and the dumps would follow each change of it. The restore checks the whole file before it writes, so one hash for the file is enough.
- **A dump as a directory**: the transactions in one file, the blobs as files beside it. A blob would not be in the hash of the file, and each blob file would need its own entry in a list.
- **A dump of the redb file.** A copy of `zoe.redb` and the blob directory is a full backup, but it gives no incremental dump by tid range and depends on the page format of redb.

## Consequences

- A restore reads each dump twice: once to check its hash, once to restore it.
- A restore commits each transaction with a sync, as every restore transaction does. A non-durable bulk restore is a card of its own.
- `zoe verify --backup` is as heavy as a restore: it needs free space for a full copy of the store in `TMPDIR`, it syncs once for each transaction, and it holds the lock of the store during the whole check. A scratch directory option and a non-durable restore are later cards.
- The dumps are not compressed. A record that the store keeps compressed goes into the dump as its pickle.
- A restored store was never packed: its pack watermark is 0, whatever the watermark of its source.
- A backup with nothing new writes nothing, also when a sequence moved. The values of a sequence that a dumped transaction holds are below the end in the dumps. A value given after the last dump can come again after a restore, as each change after the last dump is lost.
- The dumps keep the history that a later pack removes, so a restore gives back records that the store no longer has: the same content, with more history. A pack at or before the last tid of the dumps lets the backups go on. A pack never goes past the last tid of the store, so on a quiet site `zoe pack --days 0` right after a backup packs at the last tid of the dumps, and the next backup continues them. A pack after that tid stops them, and the next backup needs a new directory: `zoe backup` writes one chain per directory and has no option to start a new full dump. repozo starts a full backup when it sees a pack; ZOE does not, because each full dump copies every blob again. An option `--full` with several chains in one directory is a later card.
- A damaged dump in the middle of the chain makes the dumps after it useless: the restore stops at it. Start a new directory.
- A missing newest dump is not seen: the restore gives the older state of the dumps that remain, and exits with 0, as repozo does. The report of the restore gives the last tid and its UTC time, so the operator can see the point that the restore reached.
- A restore to an earlier point takes a prefix of the chain: copy the full dump and the dumps up to that point into another directory, and restore from it.
- A damaged store stops every backup: a record, a blob record or a blob file that does not read fails the backup, and it writes no dump until the store is repaired.
- A backup holds one transaction of the iterator in memory, all of its pickles, and a buffer of 64 KiB for a blob. A restore holds one record at a time and a buffer of 64 KiB for a blob; the staging area of the store keeps the pickles of one transaction in memory up to 16 MiB and spills the rest to a file. A blob of 200 MB took 12 MB of memory to back up and to restore.
- The dumps hold no `next_oid`. The restored store gives new oids above every oid of the dumps, as `finish` raises the next oid, not above an oid that the source gave without a commit.
- `zoe backup` opens the store read-only, so it takes the lock of the store: it cannot back up a store that a server holds. A backup through the server is a card of its own.
- A store whose blob directory is outside the store directory needs the option of the blob directory, which the command does not take yet, as for the other commands.
- No FileStorage tool reads a dump. A change of the layout raises the version byte.
