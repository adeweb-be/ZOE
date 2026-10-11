# zoe-convert

Converters between `Data.fs`, RelStorage and ZOE stores. Specification: "Loupe", section "Converters", and F14.

## Invariants

- A conversion copies tids, oids and pickles unchanged.
- A conversion can resume from a tid.
- Every conversion verifies the transaction count, the last tid and a sample of records byte for byte, and writes a report.
- The RelStorage reader refuses a schema version that it does not know.

## `Data.fs` to ZOE

- The converter runs no interpreter. It reads the `Data.fs` and the blob files itself.
- The reader accepts only a regular file. It checks this before the open, because the open of a FIFO waits for a writer. Another path is `Io` with `InvalidInput`.
- The reader accepts only the magic `FS30`. Another magic is `NotDataFs`, and the store stays unchanged.
- The reader streams the file through a buffer. It holds one transaction header and one data record at a time. It reads the target of a back pointer and the redundant length with positioned reads, which do not move the buffer.
- The reader allocates the buffer of a record with `try_reserve_exact`. A length that the memory cannot hold is `Io` with `OutOfMemory`, never an abort. The reader checks the order of a size and an offset before it subtracts them, so no subtraction overflows.
- The reader checks each transaction before it gives it: the status, the header length, the redundant length, and that the tid is after the tid before it. It checks each data record: no version data, the tid and the offset of its transaction, and that the record stays inside its transaction. A failed check is `Corrupt` with the offset.
- A back pointer points before the record that holds it, to a record of the same oid. So a chain of back pointers always ends. A back pointer of 0 gives a record without data, and `restore` copies it as a deletion.
- The reader keeps the chain end of each record with a back pointer, at most 2^16 of them. A chain that reaches such a record stops there, so a long chain costs one step per record.
- The reader stops where ZODB stops, and ZODB cuts the file at the same offset when it opens it for writing:
  - before a whole last transaction with the status `c`, or a last one that a commit still writes: a commit in progress, which is not a problem;
  - before a transaction with the status `c` that data follows or that has a length that is not valid: a problem, because ZODB writes `c` only on the last transaction;
  - before a transaction that the end of the file cuts: a problem;
  - before a header that is not valid, when ZODB takes the tail for junk: a problem. The tail is junk when the number in the last 8 bytes of the file is less than the length of a header, or leads to a transaction before the header. So a tail of zeros is junk. A header that is not valid before a valid last transaction is `Corrupt`.
- The reader skips a transaction with the status `u`, as the index of ZODB does.
- Each transaction goes through one restore transaction under its own tid, with its user, description and extension bytes unchanged. The status `p` of a packed transaction is not copied, because the store has no such status.
- The conversion resumes after the last tid of the store. That tid must be a tid of the file. Otherwise the conversion fails with `Diverged` before it copies anything.
- A blob record is a record whose class pickle is the global `ZODB.blob.Blob`, as `is_blob_record` of ZODB decides. Its blob file is the file of `(oid, tid of the transaction)` in the layout that `.layout` names, bushy or lawn. A directory without `.layout` gets the layout that ZODB selects.
- A blob record goes through `store_blob`, with the current serial of its oid in the store as its serial. A blob record without its blob file is restored without a blob, and the report names it.
- The verification reads the file again, up to where the copy stopped. It compares the file with every transaction of the store: the tids, the user, the description, the extension and every pickle, byte for byte. It checks that the store has every blob, with the size of its blob file. It compares the first blob and then one blob in 16 byte for byte, and this read also checks the BLAKE3 hash of the store.
- Each difference is one line in `Report.problems`. The lines about the whole source come first, and no cap hides them: where the conversion stopped before the end of the file, and the blob records without a blob directory. At most 100 other lines follow, and a last line counts the others. An empty list means that the store holds the whole file.
- A conversion never gives an empty list when it stopped before the end of the file, except for a commit in progress at the end.
- The verification uses the transaction iterator of the store. That iterator holds the transaction rows and the record keys of the whole store in memory.

## ZOE to `Data.fs`

- The writer writes what FileStorage's `restore` writes. The file starts with the magic `FS30`. Each transaction of the store becomes one transaction record with the status ` ` and its tid, user, description and extension bytes unchanged. All numbers are big-endian.
- A transaction record is the tid (8 bytes), the length (8), the status (1), the lengths of the user, the description and the extension (2 each), these three fields, the data records, and the length again (8). The length counts from the tid to the end of the last data record.
- A field longer than 65535 bytes does not fit in its 2 bytes. The conversion then fails with `TooLong`, as FileStorage refuses such a transaction.
- A data record is the oid (8), the tid of its transaction (8), the offset of the previous record of its oid or 0 (8), the offset of its transaction (8), a version length of 0 (2), the length of the pickle (8), then the pickle. The writer writes each pickle inline, so the pickles are byte-identical. It writes no back pointer to data.
- A deletion is a data record with a pickle length of 0 and a back pointer of 0 (8 bytes), as FileStorage writes an undone creation.
- The records of a transaction come in ascending oid order, the order of the transaction iterator of the store. So a conversion writes the same bytes for the same store.
- The writer keeps the offset of the newest record of each oid, as the index of FileStorage does. It holds one transaction of the store at a time.
- The converter writes no `.index` file. FileStorage builds the index from the file when it opens it, and writes it at close.
- The blob of each record goes into the blob directory in the bushy layout: `0x<byte>/` for each of the 8 big-endian bytes of the oid, then `<tid>.blob` with the tid as ZODB's `tid_repr` writes it. The converter writes `.layout` with `bushy` before it creates `tmp/`, because ZODB gives the lawn layout to a directory that has `tmp/` and no `.layout`. A directory with the lawn layout is refused with `NotBushy`.
- The converter looks up a blob for every record that has a pickle, not only for a record whose class is `ZODB.blob.Blob`. So the blob of a subclass of `Blob` is copied too.
- A blob is copied through the reader of the store, which checks its size and its BLAKE3 hash. A blob that fails the check, or whose file is missing, is not written, and the report names it.
- A blob goes first into `tmp/` in the blob directory. The file is made read-only, as ZODB makes a committed blob file, and synced. Then it is renamed into place and its directory is synced. A directory that the converter creates is synced into its parent. A read-only temporary blob file that a crash left is removed before the copy.
- The conversion takes the lock of FileStorage, the file with the suffix `.lock`, with `flock`, as ZODB does. When another file handle holds it, the conversion fails with `Locked`. So the converter never writes a file that a FileStorage has open for writing.
- The conversion writes to the file with the suffix `.new`. It syncs that file, verifies it, renames it to the final name and syncs the directory. So the final name holds the old file or the whole new file, never a part. A failed conversion removes the `.new` file. The next conversion removes a `.new` file that a crash left.
- Every blob is durable before the rename of the file.
- An existing file is never changed in place. The conversion resumes after its last tid: it copies the file to the `.new` file and adds the later transactions there. A resume therefore reads and writes the whole file once. When the store has no later transaction, the conversion writes nothing and only verifies.
- A resume needs a file that ends with a whole transaction, else it fails with `Corrupt`. The last tid of the file must be a tid of the store, else it fails with `DataFsDiverged` before it writes anything. A resumed file is byte for byte the file of a conversion in one run.
- The verification reads the written file with the reader of `Data.fs` to ZOE, so the file passes every check of that reader. It compares the file with every transaction of the store up to the last tid at the start of the conversion: the tids, the user, the description, the extension and every pickle, byte for byte. It checks that the blob directory has every blob of the store with its size, and compares the first blob and then one blob in 16 byte for byte.
- The counts of the report describe the store. Each difference is one line in `Report.problems`, with the cap of the conversion from `Data.fs`.
