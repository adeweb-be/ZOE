//! The staging buffer of a transaction.
//!
//! A transaction keeps the pickles that it stages in memory, up to
//! `staging-memory` bytes. A pickle that does not fit goes to the spill
//! file of the transaction, under `staging/` in the store directory. An
//! index in memory keeps the serial of each staged oid and the place of
//! its pickle: in memory, or an offset and a length in the spill file.
//!
//! The spill file holds the spilled pickles one after the other, with
//! nothing between them. The index keeps the xxh3-64 hash of each spilled
//! pickle, and each read checks it. So the bytes that a read gives are
//! the bytes staged.
//!
//! A staged record can hold its payload, encoded before the finish
//! (`Encoded`). The finish then writes that payload, and reads no spilled
//! pickle for it. A later stage of the oid drops the payload with the
//! record. The payload is not counted in `staging-memory`.
//!
//! The writes of the KV namespaces stage in the same buffer: the key of
//! the row and the value count in `staging-memory`, and a value that does
//! not fit goes to the spill file; the key stays in memory. A later write
//! of the same key replaces the earlier one.
//!
//! A spill file is never part of the durable store. Nothing syncs it or
//! `staging/`: the transaction reads the file in the same process, and a
//! crash ends the transaction. A transaction removes its spill file when it
//! drops, so finish and abort remove it too. Opening a store removes the
//! spill files that a crash left.

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::io;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use redb::StorageBackend;
use xxhash_rust::xxh3::xxh3_64;

use super::Encoded;
use crate::backend::{NewPayload, Payload};
use crate::error::{Result, ZoeError};
use crate::format::Codec;
use crate::kv::Flavour;
use crate::types::{Oid, Tid};
use crate::vfs::Dir;

/// The directory of the spill files.
pub(crate) const STAGING_DIR: &str = "staging";

/// The staging area of a store: the limit of each transaction and the
/// directory of the spill files.
pub(crate) struct StagingArea {
    /// The store directory. The blob store shares it.
    dir: Arc<dyn Dir>,
    /// The most bytes of staged pickles that a transaction keeps in
    /// memory.
    memory: usize,
    /// True when the store is open for reads only. Then the area writes
    /// nothing.
    read_only: bool,
    /// Tells if `staging/` exists.
    ready: Mutex<bool>,
    /// The number of the next spill file.
    next: AtomicU64,
}

impl StagingArea {
    /// Return the staging area of the store directory `dir`. Each
    /// transaction keeps at most `memory` bytes of pickles in memory. The
    /// area of a read-only store writes nothing. This reads nothing:
    /// `open` checks the directory.
    pub(crate) fn new(dir: Arc<dyn Dir>, memory: usize, read_only: bool) -> Self {
        Self {
            dir,
            memory,
            read_only,
            ready: Mutex::new(false),
            next: AtomicU64::new(0),
        }
    }

    /// Remove the spill files that a crash left, when the store opens.
    /// Only regular files are removed, and other entries stay. The area of
    /// a read-only store removes nothing.
    ///
    /// Nothing syncs the removals. If a crash undoes one, the next open
    /// removes the file again.
    pub(crate) fn open(&self) -> Result<()> {
        if self.read_only {
            return Ok(());
        }
        let mut ready = self.lock_ready();
        if self.dir.exists(STAGING_DIR)? {
            for file in self.dir.list_files(STAGING_DIR)? {
                self.dir.remove(&format!("{STAGING_DIR}/{file}"))?;
            }
            *ready = true;
        }
        Ok(())
    }

    fn lock_ready(&self) -> std::sync::MutexGuard<'_, bool> {
        crate::lock(&self.ready)
    }

    /// Create a new, empty spill file. Create `staging/` first when it
    /// does not exist. A read-only store fails with `ReadOnly`.
    fn create(&self) -> Result<Spill> {
        if self.read_only {
            return Err(ZoeError::ReadOnly);
        }
        {
            let mut ready = self.lock_ready();
            if !*ready {
                if !self.dir.exists(STAGING_DIR)? {
                    self.dir.create_dir(STAGING_DIR)?;
                }
                *ready = true;
            }
        }
        // The creation fails when the name exists, as a file or as a
        // symbolic link. So a spill file is always new and empty, and a
        // write never goes through a link.
        let n = self.next.fetch_add(1, Ordering::Relaxed);
        let name = format!("{STAGING_DIR}/{n}.spill");
        let file = self.dir.create_new(&name)?;
        Ok(Spill { name, file, len: 0 })
    }
}

/// The spill file of a transaction. `Staging::remove_spill` removes it.
struct Spill {
    name: String,
    file: Box<dyn StorageBackend>,
    /// The number of bytes in the file.
    len: u64,
}

impl Spill {
    /// Write `data` at the end of the file and return its offset.
    ///
    /// Nothing syncs the file, because it is never durable data. The same
    /// process reads it back, and a crash ends the transaction.
    fn append(&mut self, data: &[u8]) -> io::Result<u64> {
        let offset = self.len;
        self.file.write(offset, data)?;
        self.len += data.len() as u64;
        Ok(offset)
    }
}

/// The place of a staged pickle.
enum Pickle {
    /// In memory.
    Memory(Vec<u8>),
    /// In the spill file, at `offset`, with `len` bytes whose xxh3-64 hash
    /// is `hash`.
    Spilled { offset: u64, len: usize, hash: u64 },
}

/// A record waiting for the commit.
pub(crate) struct Staged {
    /// The serial that the client read before it changed the object.
    pub(crate) serial: Tid,
    /// True when `restore` staged the record. A restored record is a copy,
    /// so a conflict on it cannot be resolved.
    pub(crate) restored: bool,
    pickle: Pickle,
    /// The payload of the pickle, encoded before the finish.
    encoded: Option<Encoded>,
}

impl Staged {
    /// Return the length of the pickle. This reads no pickle.
    pub(crate) fn pickle_len(&self) -> usize {
        match &self.pickle {
            Pickle::Memory(bytes) => bytes.len(),
            Pickle::Spilled { len, .. } => *len,
        }
    }
}

/// A write of a KV namespace waiting for the commit.
pub(crate) struct StagedKv {
    /// The value, or `None` for a delete.
    value: Option<Pickle>,
    /// The TTL of an ephemeral set, in milliseconds. It is 0 for a durable
    /// write and for a delete.
    pub(crate) ttl_ms: u64,
}

impl StagedKv {
    /// Return the length of the value. A delete has 0. This reads no
    /// value.
    pub(crate) fn value_len(&self) -> usize {
        match &self.value {
            Some(Pickle::Memory(bytes)) => bytes.len(),
            Some(Pickle::Spilled { len, .. }) => *len,
            None => 0,
        }
    }
}

/// A namespace write that the finish writes: the flavour, the key of the
/// row, the value or `None` for a delete, and the TTL in milliseconds.
pub(crate) type KvWrite<'a> = (Flavour, &'a [u8], Option<Cow<'a, [u8]>>, u64);

/// The staged records of one transaction.
///
/// The buffer does not hold its staging area: each call that needs it
/// takes it. The transaction calls `remove_spill` when it drops.
#[derive(Default)]
pub(crate) struct Staging {
    records: BTreeMap<Oid, Staged>,
    /// The namespace writes, by flavour and key of the row.
    kv: BTreeMap<(Flavour, Vec<u8>), StagedKv>,
    /// The bytes of the pickles in memory.
    in_memory: usize,
    /// The spill file. It exists after the first pickle that does not fit
    /// in memory.
    spill: Option<Spill>,
}

impl Staging {
    /// Stage `data` for `oid` with the serial `serial`, in place of an
    /// earlier record of `oid`. `restored` tells if `restore` staged it.
    ///
    /// The pickle stays in memory if it fits in the limit of `area`. Else
    /// it goes to the spill file, in `area`. The bytes of an earlier
    /// spilled pickle of `oid` stay in the file, unused. If the call
    /// fails, the earlier record stays.
    pub(crate) fn put(
        &mut self,
        area: &StagingArea,
        oid: Oid,
        serial: Tid,
        data: &[u8],
        restored: bool,
    ) -> Result<()> {
        let replaced = match self.records.get(&oid) {
            Some(Staged {
                pickle: Pickle::Memory(bytes),
                ..
            }) => bytes.len(),
            _ => 0,
        };
        let others = self.in_memory - replaced;
        let pickle = if others.saturating_add(data.len()) <= area.memory {
            self.in_memory = others + data.len();
            Pickle::Memory(data.to_vec())
        } else {
            let pickle = self.spill(area, data)?;
            self.in_memory = others;
            pickle
        };
        let staged = Staged {
            serial,
            restored,
            pickle,
            encoded: None,
        };
        self.records.insert(oid, staged);
        Ok(())
    }

    /// Stage a write of the row `row` of `flavour`: `value`, or a delete for
    /// `None`, with the TTL `ttl_ms`. It replaces an earlier write of the
    /// row. The key of the row counts in memory. The value stays in memory
    /// if it fits in the limit of `area` with the key, else it goes to the
    /// spill file. If the call fails, the earlier write stays.
    pub(crate) fn put_kv(
        &mut self,
        area: &StagingArea,
        flavour: Flavour,
        row: Vec<u8>,
        value: Option<&[u8]>,
        ttl_ms: u64,
    ) -> Result<()> {
        let key = (flavour, row);
        let replaced = match self.kv.get(&key) {
            Some(StagedKv {
                value: Some(Pickle::Memory(bytes)),
                ..
            }) => key.1.len() + bytes.len(),
            Some(_) => key.1.len(),
            None => 0,
        };
        // The key of the row stays in memory whatever happens to the value.
        let others = self.in_memory - replaced + key.1.len();
        let value = match value {
            None => {
                self.in_memory = others;
                None
            }
            Some(data) if others.saturating_add(data.len()) <= area.memory => {
                self.in_memory = others + data.len();
                Some(Pickle::Memory(data.to_vec()))
            }
            Some(data) => {
                let pickle = self.spill(area, data)?;
                self.in_memory = others;
                Some(pickle)
            }
        };
        self.kv.insert(key, StagedKv { value, ttl_ms });
        Ok(())
    }

    /// Return the staged write of the row `row` of `flavour`. This reads
    /// no value.
    pub(crate) fn kv_record(&self, flavour: Flavour, row: &[u8]) -> Option<&StagedKv> {
        self.kv.get(&(flavour, row.to_vec()))
    }

    /// Return the flavour, the length of the key of the row and the length
    /// of the value of each staged namespace write: 0 for a delete, with
    /// true for a delete. This reads no value.
    pub(crate) fn kv_lens(&self) -> impl Iterator<Item = (Flavour, usize, usize, bool)> + '_ {
        self.kv.iter().map(|((flavour, row), staged)| {
            (
                *flavour,
                row.len(),
                staged.value_len(),
                staged.value.is_none(),
            )
        })
    }

    /// Return the number of staged namespace writes.
    #[cfg(test)]
    pub(crate) fn kv_len(&self) -> usize {
        self.kv.len()
    }

    /// Return each staged namespace write, in the order of the flavour and
    /// of the key of the row. A spilled value is read when the iterator
    /// reaches it.
    pub(crate) fn kv_writes(&self) -> impl Iterator<Item = Result<KvWrite<'_>>> + '_ {
        self.kv.iter().map(|((flavour, row), staged)| {
            let value = match &staged.value {
                Some(pickle) => Some(self.read(pickle)?),
                None => None,
            };
            Ok((*flavour, row.as_slice(), value, staged.ttl_ms))
        })
    }

    /// Write `data` to the spill file, and create the file in `area` first
    /// when the transaction has none.
    fn spill(&mut self, area: &StagingArea, data: &[u8]) -> Result<Pickle> {
        let spill = match &mut self.spill {
            Some(spill) => spill,
            empty => empty.insert(area.create()?),
        };
        let offset = spill.append(data)?;
        Ok(Pickle::Spilled {
            offset,
            len: data.len(),
            hash: xxh3_64(data),
        })
    }

    /// Attach `encoded` to the staged record of `oid` when it encodes the
    /// staged pickle: the length and the xxh3-64 hash of the pickle are
    /// the same. Return true when it is attached. Else the record stays as
    /// it is. The hash of a pickle in memory is computed here. A spilled
    /// pickle has its hash, so the call reads no spill file.
    pub(crate) fn attach(&mut self, oid: Oid, encoded: Encoded) -> bool {
        let Some(staged) = self.records.get_mut(&oid) else {
            return false;
        };
        let same = match &staged.pickle {
            Pickle::Memory(bytes) => bytes.len() == encoded.len && xxh3_64(bytes) == encoded.hash,
            Pickle::Spilled { len, hash, .. } => *len == encoded.len && *hash == encoded.hash,
        };
        if same {
            staged.encoded = Some(encoded);
        }
        same
    }

    /// Tell if the staged pickle of `oid` is in memory.
    pub(crate) fn in_memory_of(&self, oid: Oid) -> bool {
        self.records
            .get(&oid)
            .is_some_and(|staged| matches!(staged.pickle, Pickle::Memory(_)))
    }

    /// Return the bytes of the pickles and of the namespace values in
    /// memory.
    pub(crate) fn in_memory(&self) -> usize {
        self.in_memory
    }

    /// Take `oid` out of the staging. The bytes of a spilled pickle stay in
    /// the file, unused.
    pub(crate) fn remove(&mut self, oid: Oid) {
        if let Some(Staged {
            pickle: Pickle::Memory(bytes),
            ..
        }) = self.records.remove(&oid)
        {
            self.in_memory -= bytes.len();
        }
    }

    /// Return the number of staged oids.
    pub(crate) fn len(&self) -> usize {
        self.records.len()
    }

    /// Return the greatest staged oid.
    pub(crate) fn last_oid(&self) -> Option<Oid> {
        self.records.keys().next_back().copied()
    }

    /// Return each staged oid and its record, in ascending oid order. This
    /// reads no pickle.
    pub(crate) fn iter(&self) -> impl Iterator<Item = (Oid, &Staged)> + '_ {
        self.records.iter().map(|(&oid, staged)| (oid, staged))
    }

    /// Return the record of `oid`, or `None` when `oid` is not staged.
    /// This reads no pickle.
    pub(crate) fn record(&self, oid: Oid) -> Option<&Staged> {
        self.records.get(&oid)
    }

    /// Return the serial and the pickle of `oid`, or `None` when `oid` is
    /// not staged. A spilled pickle is read from the spill file.
    pub(crate) fn get(&self, oid: Oid) -> Result<Option<(Tid, Cow<'_, [u8]>)>> {
        match self.records.get(&oid) {
            Some(staged) => Ok(Some((staged.serial, self.read(&staged.pickle)?))),
            None => Ok(None),
        }
    }

    /// Return each staged oid with its serial and its pickle, in ascending
    /// oid order. A spilled pickle is read when the iterator reaches it.
    #[cfg(test)]
    pub(crate) fn records(&self) -> impl Iterator<Item = Result<(Oid, Tid, Cow<'_, [u8]>)>> + '_ {
        self.records
            .iter()
            .map(|(&oid, staged)| Ok((oid, staged.serial, self.read(&staged.pickle)?)))
    }

    /// Return each staged oid with its serial and its payload, in
    /// ascending oid order: the attached payload of a record that has one,
    /// else its pickle. A spilled pickle is read when the iterator reaches
    /// it, only when its record needs it.
    pub(crate) fn payloads(&self) -> impl Iterator<Item = Result<NewPayload<'_>>> + '_ {
        self.records.iter().map(|(&oid, staged)| {
            let payload = match &staged.encoded {
                Some(Encoded {
                    packed: Some(packed),
                    ..
                }) => Payload::Encoded(Codec::Zstd, Cow::Borrowed(&packed[..])),
                Some(Encoded { packed: None, .. }) => {
                    Payload::Encoded(Codec::None, self.read(&staged.pickle)?)
                }
                None => Payload::Pickle(self.read(&staged.pickle)?),
            };
            Ok((oid, staged.serial, payload))
        })
    }

    /// Return the bytes of `pickle`, a pickle or a namespace value. A
    /// spilled one whose bytes do not match its hash fails with
    /// `InvalidData`.
    fn read<'a>(&'a self, pickle: &'a Pickle) -> io::Result<Cow<'a, [u8]>> {
        let (offset, len, hash) = match pickle {
            Pickle::Memory(bytes) => return Ok(Cow::Borrowed(bytes)),
            Pickle::Spilled { offset, len, hash } => (*offset, *len, *hash),
        };
        let spill = self
            .spill
            .as_ref()
            .expect("a spilled pickle has a spill file");
        let mut bytes = vec![0; len];
        spill.file.read(offset, &mut bytes)?;
        if xxh3_64(&bytes) != hash {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("a spilled pickle in {} does not match its hash", spill.name),
            ));
        }
        Ok(Cow::Owned(bytes))
    }

    /// Remove the spill file from `area`, if the transaction has one. A
    /// failed removal leaves the file to the next open.
    pub(crate) fn remove_spill(&mut self, area: &StagingArea) {
        if let Some(spill) = self.spill.take() {
            drop(spill.file);
            let _ = area.dir.remove(&spill.name);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::SystemClock;
    use crate::commit::Txn;
    use crate::store::{Options, Store};
    use crate::vfs::{MemVfs, Vfs};

    /// Return a staging area in a new filesystem. Each transaction keeps
    /// `memory` bytes of pickles in memory.
    fn area(memory: usize) -> (MemVfs, StagingArea) {
        let vfs = MemVfs::new();
        let area = StagingArea::new(Arc::new(vfs.clone()), memory, false);
        (vfs, area)
    }

    /// Open the store in `vfs`. Each transaction keeps `memory` bytes of
    /// pickles in memory.
    fn open(vfs: &MemVfs, memory: usize) -> Store {
        let options = Options {
            staging_memory: memory,
            ..Options::default()
        };
        Store::open_full(vfs.clone(), SystemClock, options).unwrap()
    }

    /// Return the names of the files in `staging/`.
    fn spill_files(vfs: &MemVfs) -> Vec<String> {
        let prefix = format!("{STAGING_DIR}/");
        let names = vfs.names().into_iter();
        names.filter(|name| name.starts_with(&prefix)).collect()
    }

    fn pickle(staging: &Staging, oid: Oid) -> Vec<u8> {
        staging.get(oid).unwrap().unwrap().1.into_owned()
    }

    #[test]
    fn namespace_values_share_the_limit_and_the_spill_file() {
        // A row key is 1 + 2 + 1 bytes here.
        let row = |key: &[u8]| crate::format::kv_key(b"ns", key);
        let (vfs, area) = area(12);
        let mut staging = Staging::default();
        staging.put(&area, Oid(1), Tid(1), b"aaaa", false).unwrap();
        staging
            .put_kv(&area, Flavour::Ephemeral, row(b"a"), Some(b"vvvv"), 1000)
            .unwrap();
        assert_eq!(staging.in_memory(), 12);
        assert!(!Vfs::exists(&vfs, STAGING_DIR).unwrap());
        // The next value does not fit, so it goes to the spill file. Its key
        // stays in memory.
        staging
            .put_kv(&area, Flavour::Durable, row(b"b"), Some(b"ww"), 0)
            .unwrap();
        assert_eq!(staging.in_memory(), 16);
        assert_eq!(spill_files(&vfs), ["staging/0.spill"]);
        // A later write of a key replaces the earlier one, and a delete
        // frees the memory of its value and keeps its key.
        staging
            .put_kv(&area, Flavour::Ephemeral, row(b"a"), None, 0)
            .unwrap();
        assert_eq!(staging.in_memory(), 12);
        assert_eq!(staging.kv_len(), 2);
        let writes: Vec<_> = staging
            .kv_writes()
            .map(|write| {
                let (flavour, row, value, ttl) = write.unwrap();
                (flavour, row.to_vec(), value.map(Cow::into_owned), ttl)
            })
            .collect();
        assert_eq!(
            writes,
            [
                (Flavour::Ephemeral, row(b"a"), None, 0),
                (Flavour::Durable, row(b"b"), Some(b"ww".to_vec()), 0),
            ]
        );
        assert_eq!(
            staging
                .kv_record(Flavour::Durable, &row(b"b"))
                .unwrap()
                .value_len(),
            2
        );
        staging.remove_spill(&area);
        assert!(spill_files(&vfs).is_empty());
    }

    #[test]
    fn pickles_past_the_limit_go_to_the_spill_file() {
        let (vfs, area) = area(8);
        let mut staging = Staging::default();
        staging.put(&area, Oid(1), Tid(1), b"aaaa", false).unwrap();
        staging.put(&area, Oid(2), Tid(2), b"bbbb", false).unwrap();
        assert!(!Vfs::exists(&vfs, STAGING_DIR).unwrap());
        staging.put(&area, Oid(4), Tid(4), b"cc", false).unwrap();
        // A pickle larger than the limit goes to the file too.
        staging
            .put(&area, Oid(3), Tid(3), b"dddddddddd", false)
            .unwrap();
        assert_eq!(staging.in_memory(), 8);
        assert_eq!(spill_files(&vfs), ["staging/0.spill"]);
        assert_eq!(vfs.file("staging/0.spill").len().unwrap(), 12);

        let records: Vec<_> = staging
            .records()
            .map(|record| {
                let (oid, serial, data) = record.unwrap();
                (oid, serial, data.into_owned())
            })
            .collect();
        assert_eq!(
            records,
            [
                (Oid(1), Tid(1), b"aaaa".to_vec()),
                (Oid(2), Tid(2), b"bbbb".to_vec()),
                (Oid(3), Tid(3), b"dddddddddd".to_vec()),
                (Oid(4), Tid(4), b"cc".to_vec()),
            ]
        );
        assert!(staging.get(Oid(5)).unwrap().is_none());
        assert_eq!((staging.len(), staging.last_oid()), (4, Some(Oid(4))));
    }

    #[test]
    fn a_later_put_of_an_oid_replaces_its_pickle() {
        let (_vfs, area) = area(4);
        let mut staging = Staging::default();
        staging.put(&area, Oid(1), Tid(1), b"aaa", false).unwrap();
        // The new pickle takes the place of the old one in memory.
        staging.put(&area, Oid(1), Tid(2), b"bbbb", false).unwrap();
        assert_eq!(staging.in_memory(), 4);
        staging.put(&area, Oid(1), Tid(3), b"ccccc", false).unwrap();
        assert_eq!(staging.in_memory(), 0);
        assert_eq!(pickle(&staging, Oid(1)), b"ccccc");
        staging.put(&area, Oid(1), Tid(4), b"d", false).unwrap();
        assert_eq!(staging.in_memory(), 1);
        assert_eq!(staging.get(Oid(1)).unwrap().unwrap().0, Tid(4));
        assert_eq!(pickle(&staging, Oid(1)), b"d");
        assert_eq!(staging.len(), 1);
    }

    #[test]
    fn each_buffer_has_its_own_spill_file() {
        let (vfs, area) = area(0);
        let mut first = Staging::default();
        let mut second = Staging::default();
        first.put(&area, Oid(1), Tid::ZERO, b"a", false).unwrap();
        second.put(&area, Oid(1), Tid::ZERO, b"b", false).unwrap();
        assert_eq!(spill_files(&vfs), ["staging/0.spill", "staging/1.spill"]);
        assert_eq!(pickle(&first, Oid(1)), b"a");
        assert_eq!(pickle(&second, Oid(1)), b"b");

        first.remove_spill(&area);
        assert_eq!(spill_files(&vfs), ["staging/1.spill"]);
        second.remove_spill(&area);
        second.remove_spill(&area);
        assert!(spill_files(&vfs).is_empty());
        assert!(Vfs::exists(&vfs, STAGING_DIR).unwrap());
    }

    #[test]
    fn an_empty_pickle_never_spills() {
        let (vfs, area) = area(0);
        let mut staging = Staging::default();
        staging.put(&area, Oid(1), Tid::ZERO, b"", false).unwrap();
        assert!(staging.get(Oid(1)).unwrap().unwrap().1.is_empty());
        assert!(!Vfs::exists(&vfs, STAGING_DIR).unwrap());
    }

    #[test]
    fn a_damaged_spill_file_fails_the_read() {
        let (vfs, area) = area(0);
        let mut staging = Staging::default();
        staging
            .put(&area, Oid(1), Tid::ZERO, b"abc", false)
            .unwrap();
        staging
            .put(&area, Oid(2), Tid::ZERO, b"def", false)
            .unwrap();
        let file = vfs.file("staging/0.spill");
        file.write(4, b"x").unwrap();
        assert_eq!(pickle(&staging, Oid(1)), b"abc");
        let error = staging.get(Oid(2)).unwrap_err();
        assert!(
            matches!(&error, ZoeError::Io(e) if e.kind() == io::ErrorKind::InvalidData),
            "{error:?}"
        );

        file.set_len(4).unwrap();
        assert!(staging.get(Oid(2)).is_err());
        assert!(staging.records().any(|record| record.is_err()));
    }

    #[test]
    fn a_store_that_never_spills_has_no_staging_directory() {
        let vfs = MemVfs::new();
        let store = open(&vfs, 1);
        let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
        txn.store(Oid(1), Tid::ZERO, b"a").unwrap();
        txn.vote().unwrap();
        txn.finish().unwrap();
        drop(store);
        open(&vfs, 1);
        assert!(!Vfs::exists(&vfs, STAGING_DIR).unwrap());
    }

    #[test]
    fn opening_removes_the_spill_files_of_a_dead_process() {
        let vfs = MemVfs::new();
        let store = open(&vfs, 0);
        let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
        txn.store(Oid(1), Tid::ZERO, b"lost").unwrap();
        // A dead process runs no destructor.
        std::mem::forget(txn);
        std::mem::forget(store);
        Vfs::create_dir(&vfs, "staging/stray").unwrap();
        assert_eq!(spill_files(&vfs), ["staging/0.spill"]);

        let store = open(&vfs, 0);
        assert!(spill_files(&vfs).is_empty());
        assert!(Vfs::exists(&vfs, "staging/stray").unwrap());

        // The next spill file takes the first number again.
        let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
        txn.store(Oid(1), Tid::ZERO, b"kept").unwrap();
        assert_eq!(spill_files(&vfs), ["staging/0.spill"]);
        txn.vote().unwrap();
        let tid = txn.finish().unwrap();
        assert_eq!(store.load(Oid(1)).unwrap(), (b"kept".to_vec(), tid));
        assert!(spill_files(&vfs).is_empty());
    }

    /// Return a new, empty directory for a store in the temporary
    /// directory of the system.
    fn temp_dir(name: &str) -> std::path::PathBuf {
        let name = format!("zoe-staging-{name}-{}", std::process::id());
        let dir = std::env::temp_dir().join(name);
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    /// A read-only store writes nothing in `staging/`. Its open keeps the
    /// spill files, and a pickle that would spill fails with `ReadOnly`.
    /// The next writer removes the spill files.
    #[test]
    fn a_read_only_store_writes_nothing_in_staging() {
        let dir = temp_dir("read-only");
        drop(Store::open(&dir).unwrap());
        let spill = dir.join(STAGING_DIR).join("0.spill");
        std::fs::create_dir(dir.join(STAGING_DIR)).unwrap();
        std::fs::write(&spill, b"left").unwrap();

        let store = Store::open_read_only(&dir).unwrap();
        let mut txn = Txn::begin(&store, b"", b"", b"");
        // A pickle in memory stages, and the vote refuses the transaction.
        txn.store(Oid(1), Tid::ZERO, b"small").unwrap();
        let large = vec![1; Options::default().staging_memory];
        let error = txn.store(Oid(2), Tid::ZERO, &large);
        assert!(matches!(error, Err(ZoeError::ReadOnly)), "{error:?}");
        assert!(matches!(txn.vote(), Err(ZoeError::ReadOnly)));
        drop(txn);
        drop(store);
        let names: Vec<_> = std::fs::read_dir(dir.join(STAGING_DIR))
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(names, ["0.spill"]);
        assert_eq!(std::fs::read(&spill).unwrap(), b"left");

        drop(Store::open(&dir).unwrap());
        assert!(!spill.exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A symbolic link under the name of a spill file never takes a write.
    /// The creation of the spill file fails, and the target of the link
    /// stays as it is. The next spill file takes the next number.
    #[test]
    fn a_spill_never_writes_through_a_link() {
        let dir = temp_dir("link");
        drop(Store::open(&dir).unwrap());
        std::fs::create_dir(dir.join(STAGING_DIR)).unwrap();
        let link = dir.join(STAGING_DIR).join("0.spill");
        std::os::unix::fs::symlink("../zoe.redb", &link).unwrap();
        let options = Options {
            staging_memory: 0,
            ..Options::default()
        };
        let store = Store::open_with(&dir, options).unwrap();
        // The open removes only regular files, so the link stays.
        assert!(link.symlink_metadata().unwrap().file_type().is_symlink());
        let database = std::fs::read(dir.join("zoe.redb")).unwrap();

        let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
        let error = txn.store(Oid(1), Tid::ZERO, b"a").unwrap_err();
        assert!(
            matches!(&error, ZoeError::Io(e) if e.kind() == io::ErrorKind::AlreadyExists),
            "{error:?}"
        );
        drop(txn);
        assert_eq!(std::fs::read(dir.join("zoe.redb")).unwrap(), database);

        let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
        txn.store(Oid(1), Tid::ZERO, b"a").unwrap();
        assert!(dir.join(STAGING_DIR).join("1.spill").is_file());
        txn.vote().unwrap();
        let tid = txn.finish().unwrap();
        assert_eq!(store.load(Oid(1)).unwrap(), (b"a".to_vec(), tid));
        drop(store);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// The crash simulation covers the spill files. Each state that a
    /// crash leaves opens with no spill file, and a commit of spilled
    /// records is there whole or not at all.
    #[test]
    fn a_crash_leaves_no_spill_file_and_no_part_of_a_commit() {
        let vfs = MemVfs::new();
        {
            // A first spill creates `staging/`. The record starts after it,
            // so it holds fewer pending operations and fewer images.
            let store = open(&vfs, 0);
            let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
            txn.store(Oid(9), Tid::ZERO, b"x").unwrap();
        }
        let vfs = vfs.durable_copy();
        let store = open(&vfs, 2);
        // The first pickle stays in memory, and the second one spills.
        let oids = [Oid(1), Oid(2)];
        let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
        for oid in oids {
            txn.store(oid, Tid::ZERO, &[oid.0 as u8; 2]).unwrap();
        }
        assert_eq!(txn.staged_in_memory(), 2);
        assert_eq!(spill_files(&vfs), ["staging/0.spill"]);
        txn.vote().unwrap();
        let tid = txn.finish().unwrap();
        vfs.mark("acknowledged");
        drop(store);

        let images = vfs.crash_images();
        let with_spill = images.iter().filter(|image| {
            let names = image.vfs().names();
            names.iter().any(|name| name.starts_with("staging/"))
        });
        assert!(with_spill.count() > 0);
        for image in &images {
            let image_vfs = image.vfs();
            let store = Store::open_vfs(image_vfs.clone())
                .unwrap_or_else(|e| panic!("{image:?} does not open: {e}"));
            assert!(spill_files(&image_vfs).is_empty(), "{image:?}");
            let found: Vec<_> = oids.iter().map(|&oid| store.load(oid).ok()).collect();
            if found[0].is_some() {
                for (oid, found) in oids.iter().zip(found) {
                    assert_eq!(found, Some((vec![oid.0 as u8; 2], tid)), "{image:?}");
                }
            } else {
                assert!(found.iter().all(Option::is_none), "{image:?}");
                assert!(!image.is_after("acknowledged"), "{image:?} lost a commit");
            }
        }
    }
}
