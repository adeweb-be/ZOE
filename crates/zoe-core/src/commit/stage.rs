//! The calls that stage the work of a transaction before its vote: the
//! stores, the blob stores, the deletions, the restores, the namespace
//! writes and the read-current marks. Each call checks the order of the
//! calls and the size limits before it stages anything.

use std::borrow::Cow;
use std::ops::Deref;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use super::{Encoded, Txn};
use crate::blob::{BlobFile, PendingBlob};
use crate::error::{Result, ZoeError};
use crate::format;
use crate::guard::SizeLimit;
use crate::kv::{self, Flavour};
use crate::store::Store;
use crate::types::{Oid, Tid};

impl<S: Deref<Target = Store>> Txn<S> {
    /// Stage a new version of `oid`. `serial` is the serial that the client
    /// read; it is `Tid::ZERO` for a new object.
    ///
    /// An empty pickle marks a deletion, so `store` refuses it with
    /// `Protocol`. A restore transaction takes `store` too, with the same
    /// serial check at the vote.
    ///
    /// After a vote, `store` takes only an oid that `vote_resolving` gave
    /// as a conflict. Then `serial` is the serial of the committed record.
    ///
    /// A blob that `store_blob` staged for `oid` stays, as in ZODB.
    ///
    /// A pickle longer than `max-record-size`, or a pickle that takes the
    /// transaction past `max-transaction-size`, fails with `TooLarge`. A
    /// restore transaction checks no size limit.
    pub fn store(&mut self, oid: Oid, serial: Tid, data: &[u8]) -> Result<()> {
        self.check_store(oid, data)?;
        let stored_bytes = self.check_size(oid, data)?;
        self.stage(oid, serial, data, false, stored_bytes)
    }

    /// Stage a deletion of `oid`: a record with an empty pickle. `serial`
    /// is the serial that the client read. This is `deleteObject` of ZODB.
    ///
    /// The vote checks `serial` as it does for `store`: a stale serial
    /// fails the vote with `Conflict`. `vote_resolving` never resolves a
    /// deletion. A restore transaction takes `delete` too, with the same
    /// check.
    ///
    /// The deletion replaces a pickle that the transaction staged for
    /// `oid`, and drops the staged blob of `oid`: a deleted object has no
    /// blob. After a vote, `delete` fails with `Protocol`.
    pub fn delete(&mut self, oid: Oid, serial: Tid) -> Result<()> {
        self.check_thread()?;
        if self.locked() {
            return Err(ZoeError::Protocol("delete after vote"));
        }
        check_oid(oid)?;
        let stored_bytes = self.check_size(oid, &[])?;
        self.stage(oid, serial, &[], false, stored_bytes)?;
        self.blobs.remove(&oid);
        Ok(())
    }

    /// Check that `store` or `store_blob` can stage `data` for `oid`. A
    /// refused call changes nothing.
    fn check_store(&self, oid: Oid, data: &[u8]) -> Result<()> {
        self.check_thread()?;
        if self.voted() {
            return Err(ZoeError::Protocol("store after vote"));
        }
        if self.locked() && !self.resolving.contains(&oid) {
            return Err(ZoeError::Protocol(
                "store after vote of an oid without conflict",
            ));
        }
        if data.is_empty() {
            return Err(ZoeError::Protocol("store of an empty pickle"));
        }
        check_oid(oid)
    }

    /// Check `data` for `oid` against `max-record-size` and
    /// `max-transaction-size`. Return the new value of `stored_bytes`,
    /// with `data` in it. A refused call changes nothing.
    ///
    /// The callers check the order of the call first, so a call out of
    /// order is `Protocol`, whatever its size.
    pub(super) fn check_size(&self, oid: Oid, data: &[u8]) -> Result<u64> {
        if self.is_restore() {
            return Ok(self.stored_bytes);
        }
        let guardrails = &self.store.guardrails;
        let size = data.len() as u64;
        guardrails.check(SizeLimit::Record, oid, size)?;
        // A pickle that replaces a staged pickle of the same oid counts
        // once. The check reads the length of the staged pickle, never its
        // bytes, so a spilled pickle costs no read.
        let staged = self
            .staged
            .record(oid)
            .map_or(0, |staged| staged.pickle_len() as u64);
        let stored_bytes = self
            .stored_bytes
            .saturating_sub(staged)
            .saturating_add(size);
        guardrails.check(SizeLimit::Transaction, oid, stored_bytes)?;
        Ok(stored_bytes)
    }

    /// Check a blob of `size` bytes for `oid` against `max-blob-size`. A
    /// restore transaction checks no size limit.
    fn check_blob_size(&self, oid: Oid, size: u64) -> Result<()> {
        if self.is_restore() {
            return Ok(());
        }
        self.store.guardrails.check(SizeLimit::Blob, oid, size)
    }

    /// Tell if this is a restore transaction. A restore transaction copies
    /// records as they are, so it checks no size limit: a copy never
    /// refuses what its source took.
    pub(super) fn is_restore(&self) -> bool {
        self.tid.is_some()
    }

    /// Stage a write of `value` for `key` in the namespace `name` of
    /// `flavour`. An ephemeral key needs `ttl`, more than 0 and at most
    /// `MAX_TTL_SECONDS`: the finish sets its expiry to the time of the
    /// write of its commit batch plus `ttl`. A durable key takes no TTL.
    ///
    /// The write takes no lock, and the vote checks nothing for it: when
    /// two transactions write one key, the commit with the later tid wins.
    /// A later write of the same key in this transaction replaces this
    /// one. An abort drops it.
    ///
    /// The value counts toward `max-record-size` and
    /// `max-transaction-size` as a pickle does: a refusal is `KvTooLarge`.
    /// A bad name, key or TTL is `InvalidKv`. A call from another thread is
    /// `WrongThread`. A call after the vote, or in a restore transaction,
    /// is `Protocol`. A refused call stages nothing.
    pub fn kv_set(
        &mut self,
        flavour: Flavour,
        name: &str,
        key: &[u8],
        value: &[u8],
        ttl: Option<Duration>,
    ) -> Result<()> {
        self.check_kv(name, key)?;
        let ttl_ms = kv::ttl_millis(flavour, ttl)?;
        let row = format::kv_key(name.as_bytes(), key);
        let stored_bytes = self.check_kv_size(flavour, &row, value.len())?;
        self.stage_kv(flavour, row, Some(value), ttl_ms, stored_bytes)
    }

    /// Stage a delete of `key` in the namespace `name` of `flavour`, with
    /// the rules of `kv_set`. The finish removes the row. A delete of a key
    /// that has no row does nothing.
    pub fn kv_delete(&mut self, flavour: Flavour, name: &str, key: &[u8]) -> Result<()> {
        self.check_kv(name, key)?;
        let row = format::kv_key(name.as_bytes(), key);
        let stored_bytes = self.check_kv_size(flavour, &row, 0)?;
        self.stage_kv(flavour, row, None, 0, stored_bytes)
    }

    /// Stage `data` for `oid` with `serial`, and make `stored_bytes` the
    /// transaction size: the size of the staged pickles with `data`, as
    /// `check_size` gives it. A stage that fails keeps the size.
    pub(super) fn stage(
        &mut self,
        oid: Oid,
        serial: Tid,
        data: &[u8],
        restored: bool,
        stored_bytes: u64,
    ) -> Result<()> {
        let area = &self.store.backend.staging;
        self.staged.put(area, oid, serial, data, restored)?;
        self.stored_bytes = stored_bytes;
        Ok(())
    }

    /// Stage a namespace write of `value`, or a delete for `None`, in the
    /// row `row` of `flavour` with a TTL of `ttl_ms`, and make
    /// `stored_bytes` the transaction size, as `check_kv_size` gives it. A
    /// stage that fails keeps the size.
    fn stage_kv(
        &mut self,
        flavour: Flavour,
        row: Vec<u8>,
        value: Option<&[u8]>,
        ttl_ms: u64,
        stored_bytes: u64,
    ) -> Result<()> {
        let area = &self.store.backend.staging;
        self.staged.put_kv(area, flavour, row, value, ttl_ms)?;
        self.stored_bytes = stored_bytes;
        Ok(())
    }

    /// Check that the transaction can stage a namespace write of `key` in
    /// the namespace `name`. A refused call changes nothing.
    fn check_kv(&self, name: &str, key: &[u8]) -> Result<()> {
        self.check_thread()?;
        if self.is_restore() {
            return Err(ZoeError::Protocol(
                "a namespace write in a restore transaction",
            ));
        }
        if self.locked() {
            return Err(ZoeError::Protocol("a namespace write after vote"));
        }
        kv::check_name(name)?;
        kv::check_key(key)
    }

    /// Check a value of `size` bytes for the row `row` of `flavour` against
    /// `max-record-size` and `max-transaction-size`, as `check_size` does
    /// for a pickle. The key of the row counts toward the transaction size
    /// with the value, and a write that replaces a staged write of the same
    /// row counts once. Return the new value of `stored_bytes`.
    fn check_kv_size(&self, flavour: Flavour, row: &[u8], size: usize) -> Result<u64> {
        let guardrails = &self.store.guardrails;
        guardrails.check_kv(SizeLimit::Record, size as u64)?;
        let size = (row.len() + size) as u64;
        let staged = self
            .staged
            .kv_record(flavour, row)
            .map_or(0, |staged| (row.len() + staged.value_len()) as u64);
        let stored_bytes = self
            .stored_bytes
            .saturating_sub(staged)
            .saturating_add(size);
        guardrails.check_kv(SizeLimit::Transaction, stored_bytes)?;
        Ok(stored_bytes)
    }

    /// Return the serial that the transaction read for `oid` and the pickle
    /// that it staged. Return `None` when the transaction did not stage
    /// `oid`.
    ///
    /// A pickle in the spill file is read from the file. A read that fails,
    /// or whose bytes do not match the hash of the pickle, fails the call
    /// with `Io`.
    pub fn staged(&self, oid: Oid) -> Result<Option<(Tid, Cow<'_, [u8]>)>> {
        self.staged.get(oid)
    }

    /// Return each oid that the transaction staged, in ascending oid order.
    /// This reads no pickle.
    pub fn staged_oids(&self) -> impl Iterator<Item = Oid> + '_ {
        self.staged.iter().map(|(oid, _)| oid)
    }

    /// Return the class of the pickle that the transaction staged for
    /// `oid`, as `module.name`. `zoe_pickle` reads it from the class
    /// pickle and never unpickles. This is the `class_name` of a ZODB
    /// `ConflictError`.
    ///
    /// Return `None` when the transaction did not stage `oid` or staged a
    /// deletion, when the class pickle does not decode or names no class,
    /// and when a spilled pickle does not read.
    pub fn staged_class_name(&self, oid: Oid) -> Option<String> {
        let (_, pickle) = self.staged(oid).ok()??;
        if pickle.is_empty() {
            return None;
        }
        zoe_pickle::decode_class_key(&pickle).ok()?
    }

    /// Attach `encoded` to the staged record of `oid`, so the finish writes
    /// it and does not encode the pickle. Return true when it is attached.
    ///
    /// The call attaches `encoded` only when it encodes the staged pickle
    /// of `oid` with the compression of the store: the length, the xxh3-64
    /// hash and the compression must be the same. Else it returns false,
    /// and the finish encodes the pickle inline. So the frame is always the
    /// frame of an inline encode. A later `store`, `delete` or `restore` of
    /// `oid` drops the payload.
    ///
    /// The call computes the hash of a pickle in memory. A call from
    /// another thread fails with `WrongThread`.
    pub fn set_encoded(&mut self, oid: Oid, encoded: Encoded) -> Result<bool> {
        self.check_thread()?;
        if encoded.compression != self.store.compression() {
            return Ok(false);
        }
        Ok(self.staged.attach(oid, encoded))
    }

    /// Tell if the transaction keeps the staged pickle of `oid` in memory.
    /// It is false for an oid that is not staged, and for a pickle in the
    /// spill file.
    pub fn stages_in_memory(&self, oid: Oid) -> bool {
        self.staged.in_memory_of(oid)
    }

    /// Return the bytes of the staged pickles that the transaction keeps in
    /// memory. The other staged pickles are in its spill file.
    pub fn staged_in_memory(&self) -> usize {
        self.staged.in_memory()
    }

    /// Stage a new version of `oid` with the blob in `file`, as `store`
    /// does, with the same checks. Then sync the file. `finish` renames it
    /// into the blob store. A second blob of `oid` replaces the first.
    ///
    /// `file` must come from `new_blob_file` of this store. A file of
    /// another store is refused with `Protocol`, and a file of a closed
    /// store with `Closed`. Then a blob longer than `max-blob-size` fails
    /// with `TooLarge`, before the sync, as a pickle over a size limit
    /// does. A restore transaction checks no size limit. A refused call
    /// stages nothing and drops `file`, which removes it.
    pub fn store_blob(&mut self, oid: Oid, serial: Tid, data: &[u8], file: BlobFile) -> Result<()> {
        self.check_store(oid, data)?;
        file.check_owner(&self.store.backend)?;
        let stored_bytes = self.check_size(oid, data)?;
        self.check_blob_size(oid, file.size())?;
        let blob = file.close(&self.store.backend)?;
        self.stage_blob(oid, serial, data, false, stored_bytes, blob)
    }

    /// Stage a new version of `oid` with the blob in the file at `path`, as
    /// `store_blob` does, with the same checks. The file must be in the
    /// blob temporary directory of this store, `Store::blob_temp_dir`, or
    /// in one of its subdirectories.
    ///
    /// The call takes the file by rename, never by copy: it syncs the file,
    /// renames it to a temporary name of the store and computes its BLAKE3
    /// hash. `finish` renames it into the blob store. A second blob of
    /// `oid` replaces the first.
    ///
    /// A refused call leaves the file in its place: a call from another
    /// thread, after the vote, with an empty pickle or a reserved oid, or
    /// with a path outside the blob temporary directory. A file that the
    /// transaction took belongs to it: an error after the rename, an abort
    /// or a drop of the transaction removes the file. So a blob longer
    /// than `max-blob-size`, which the call can measure only after it took
    /// the file, fails with `TooLarge` and removes the file.
    pub fn store_blob_at(&mut self, oid: Oid, serial: Tid, data: &[u8], path: &Path) -> Result<()> {
        self.check_store(oid, data)?;
        let stored_bytes = self.check_size(oid, data)?;
        let blob = self.store.take_blob_file(path)?;
        self.check_blob_size(oid, blob.record().size)?;
        self.stage_blob(oid, serial, data, false, stored_bytes, blob)
    }

    /// Stage a new version of `oid` with the blob in the file `name` of the
    /// directory of `oid` in the blob directory, as `store_blob_at` does,
    /// with the same checks. This is the `storeBlobShared` of ZEO: a client
    /// that shares the blob directory writes the file there, and gives its
    /// name.
    ///
    /// `name` is a name, not a path: it has no `/`, and it ends in `.tmp` or
    /// `.tmp-`. Another name is refused with `Protocol`. The call syncs the
    /// levels of the oid directory and the file, then takes the file by
    /// rename, as `store_blob_at` does. A refused call leaves the file in
    /// its place, and a file that the transaction took belongs to it.
    pub fn store_blob_shared(
        &mut self,
        oid: Oid,
        serial: Tid,
        data: &[u8],
        name: &str,
    ) -> Result<()> {
        self.check_store(oid, data)?;
        let stored_bytes = self.check_size(oid, data)?;
        self.store.writable()?;
        let blob =
            self.store
                .blobs()
                .take_shared(Arc::downgrade(&self.store.backend), oid, name)?;
        self.check_blob_size(oid, blob.record().size)?;
        self.stage_blob(oid, serial, data, false, stored_bytes, blob)
    }

    /// Stage `data` for `oid` with `serial`, and the blob `blob`.
    /// `stored_bytes` is the size of the staged pickles with `data`, as
    /// `check_size` gives it. A stage that fails drops `blob`, which
    /// removes its file.
    fn stage_blob(
        &mut self,
        oid: Oid,
        serial: Tid,
        data: &[u8],
        restored: bool,
        stored_bytes: u64,
        blob: PendingBlob,
    ) -> Result<()> {
        self.stage(oid, serial, data, restored, stored_bytes)?;
        self.blobs.insert(oid, blob);
        Ok(())
    }

    /// Stage a copy of a version of `oid` in a restore transaction. `None`
    /// copies a deletion. The version gets the tid of the transaction.
    ///
    /// The previous serial of the version is the current serial of `oid` in
    /// this store. The vote takes an exclusive lock on `oid`, as for
    /// `store`. If another transaction changes `oid` before the vote, the
    /// vote fails with `Conflict`.
    ///
    /// A blob that `store_blob` staged for `oid` stays, unless `data` is a
    /// deletion: a deleted object has no blob.
    ///
    /// A restore copies a record as it is: it checks no size limit.
    pub fn restore(&mut self, oid: Oid, data: Option<&[u8]>) -> Result<()> {
        let current = self.check_restore(oid)?;
        let area = &self.store.backend.staging;
        self.staged
            .put(area, oid, current, data.unwrap_or_default(), true)?;
        if data.is_none() {
            self.blobs.remove(&oid);
        }
        Ok(())
    }

    /// Stage a copy of a version of `oid` with the blob in `file`, in a
    /// restore transaction. This is `restore` with a blob, the
    /// `restoreBlob` of ZODB. Then sync the file. `finish` renames it into
    /// the blob store.
    ///
    /// As for `restore`, the version gets the tid of the transaction, and
    /// its previous serial is the current serial of `oid` in this store. A
    /// change of `oid` by another transaction before the vote fails the
    /// vote with `Conflict`, and `vote_resolving` never resolves it. So
    /// `restore_blob` differs from `store_blob` in a restore transaction:
    /// `store_blob` takes the serial of the client and checks it at the
    /// vote.
    ///
    /// `data` cannot be empty, because a deletion has no blob: an empty
    /// pickle is refused with `Protocol`. `file` must come from
    /// `new_blob_file` of this store, as for `store_blob`. A refused call
    /// stages nothing and drops `file`, which removes it.
    pub fn restore_blob(&mut self, oid: Oid, data: &[u8], file: BlobFile) -> Result<()> {
        let current = self.check_restore_blob(oid, data)?;
        let blob = file.close(&self.store.backend)?;
        self.stage_blob(oid, current, data, true, self.stored_bytes, blob)
    }

    /// Stage a copy of a version of `oid` with the blob in the file at
    /// `path`, as `restore_blob` does, with the same checks. The call takes
    /// the file by rename, as `store_blob_at` does, and a refused call
    /// leaves the file in its place.
    pub fn restore_blob_at(&mut self, oid: Oid, data: &[u8], path: &Path) -> Result<()> {
        let current = self.check_restore_blob(oid, data)?;
        let blob = self.store.take_blob_file(path)?;
        self.stage_blob(oid, current, data, true, self.stored_bytes, blob)
    }

    /// Check that `restore` can stage a copy of `oid`, and return the
    /// current serial of `oid`. A refused call changes nothing.
    fn check_restore(&self, oid: Oid) -> Result<Tid> {
        self.check_thread()?;
        if self.tid.is_none() {
            return Err(ZoeError::Protocol("restore outside a restore transaction"));
        }
        if self.locked() {
            return Err(ZoeError::Protocol("restore after vote"));
        }
        check_oid(oid)?;
        self.current_serial(oid)
    }

    /// Check that `restore_blob` can stage a copy of `oid` with `data`, and
    /// return the current serial of `oid`.
    fn check_restore_blob(&self, oid: Oid, data: &[u8]) -> Result<Tid> {
        let current = self.check_restore(oid)?;
        if data.is_empty() {
            return Err(ZoeError::Protocol("restore of a blob with an empty pickle"));
        }
        Ok(current)
    }

    /// Declare that the transaction read `oid` at `serial` and depends on
    /// it. This is `checkCurrentSerialInTransaction` of ZODB.
    ///
    /// The vote takes a shared lock on `oid` and checks that `serial` is
    /// still current. A stale serial fails the vote with `ReadConflict`.
    /// The lock stays until `finish` or `abort`, so no other transaction
    /// can change `oid` before this one finishes.
    pub fn check_current_serial(&mut self, oid: Oid, serial: Tid) -> Result<()> {
        self.check_thread()?;
        if self.locked() {
            return Err(ZoeError::Protocol("check_current_serial after vote"));
        }
        self.read_current.insert(oid, serial);
        Ok(())
    }
}

/// Check that a transaction can write `oid`. `Oid(u64::MAX)` is refused
/// with `Protocol`, because the next oid must stay greater than every
/// written oid.
fn check_oid(oid: Oid) -> Result<()> {
    if oid == Oid(u64::MAX) {
        return Err(ZoeError::Protocol("oid u64::MAX is reserved"));
    }
    Ok(())
}
