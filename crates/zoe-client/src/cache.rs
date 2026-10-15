//! The client cache: the layout of ADR 0034 (proposed) in one mapping,
//! with the semantics of `ZEO.cache.ClientCache`. The mapping is anonymous
//! (`Cache::new`), or a file that one process maps (`Cache::open`, in
//! `file.rs`). The same code reads and writes both.
//!
//! The mapping holds a header, an index and a ring of entries. An entry is
//! the data of one revision of an oid, with its start tid and its end tid.
//! A current entry has no end: its end word is 0.
//!
//! - An entry `(start, end)` with an end is true for the snapshots in
//!   `[start, end)`, for ever.
//! - A current entry is true for the snapshots in `[start, applied]`.
//!   `applied` is the tid up to which the cache applied the commits.
//! - `apply` and `commit` end the current entries of the oids of a tid
//!   before they move `applied` to the tid.
//! - A current entry from a load goes in only when `applied` is not above
//!   the position of the connection that carried the reply.
//!
//! A hit takes no lock and writes nothing. It loads `applied`, then the
//! slot of the oid, then the entry. It copies the entry, checks both
//! checksums, and checks that the head of the ring stayed within one ring
//! of the entry. A doubt is a miss. The writers take one mutex.
//!
//! Every word of the mapping is an atomic. A reader that copies an entry
//! while a writer overwrites it sees the head of the ring move: the writer
//! moves the head, then a release fence, then writes the entry; the reader
//! copies the entry, then an acquire fence, then loads the head.

mod file;
mod mapping;

use std::fs::File;
use std::io;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering, fence};
use std::sync::{Mutex, MutexGuard, PoisonError, TryLockError};

use xxhash_rust::xxh3::xxh3_64;
use zoe_core::{Oid, Tid};

use crate::client::Record;
use mapping::Mapping;

/// The words of the header page.
const HEADER_WORDS: usize = 512;
/// The magic of the header: `ZOECACH` and the version of the layout.
const MAGIC: u64 = u64::from_le_bytes(*b"ZOECACH\x01");

// The words of the header. The static checksum covers the magic, the size
// of the ring, the number of slots and the name of the storage. The words 4
// and 5 hold the boot id of the kernel, the word 6 the clean flag and the
// word 9 the checksum of the state. An anonymous mapping leaves these four
// words at zero: only a file uses them.
const H_MAGIC: usize = 0;
const H_RING: usize = 1;
const H_SLOTS: usize = 2;
const H_STATIC_SUM: usize = 3;
const H_BOOT: usize = 4;
const H_CLEAN: usize = 6;
const H_APPLIED: usize = 7;
const H_HEAD: usize = 8;
const H_STATE_SUM: usize = 9;
/// The length of the name of the storage, then its bytes in 32 words.
const H_STORAGE: usize = 10;
/// The most bytes of the name of the storage.
const STORAGE_BYTES: usize = 256;
/// The words of the header that the static checksum covers.
const STATIC_WORDS: usize = 3 + 1 + STORAGE_BYTES / 8;

// The words of the head of an entry. `end` is the only word that changes in
// place. Its change writes the head checksum again.
const E_POS: usize = 0;
const E_OID: usize = 1;
const E_START: usize = 2;
const E_END: usize = 3;
const E_PREV: usize = 4;
/// The stored length in the low half, the raw length in the high half.
const E_LENGTHS: usize = 5;
/// The codec in the low half, the bytes of the entry in the high half.
const E_CODEC: usize = 6;
const E_DATA_SUM: usize = 7;
const E_HEAD_SUM: usize = 8;
const HEAD_WORDS: usize = 9;
const HEAD_BYTES: u64 = 72;

/// The codec of an entry whose data is raw.
const CODEC_RAW: u64 = 0;

/// The fewest index slots.
const MIN_SLOTS: u64 = 1024;
/// The bytes of the ring for each index slot.
const BYTES_PER_SLOT: u64 = 256;
/// The most slots that a lookup or an insert probes.
const MAX_PROBE: u64 = 32;
/// The most entries of one oid that a lookup follows.
const MAX_CHAIN: usize = 8;

/// The counts of the writers of a cache.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CacheCounts {
    /// The entries written: from loads, commits and copies of hits.
    pub stored: u64,
    /// The current replies that the cache refused: `applied` was above the
    /// position of their connection.
    pub refused: u64,
    /// The hits that the cache copied to the head of the ring.
    pub copied: u64,
    /// The entries that no index slot took. No lookup finds them.
    pub unindexed: u64,
}

/// The head of an entry, as a reader copied and checked it.
#[derive(Clone, Copy, Debug)]
struct Head {
    pos: u64,
    oid: u64,
    start: u64,
    end: u64,
    prev: u64,
    len: u64,
    total: u64,
    data_sum: u64,
    words: [u64; HEAD_WORDS],
}

impl Head {
    /// Tell if the entry was dropped: its end is its start, so it is true
    /// for no snapshot.
    fn dropped(&self) -> bool {
        self.end != 0 && self.end == self.start
    }
}

/// The client cache of one storage, in one process.
pub struct Cache {
    /// The cache file, which holds its lock, until the close. `None` for an
    /// anonymous mapping.
    file: Mutex<Option<File>>,
    map: Mapping,
    /// The bytes of the ring, a multiple of 8.
    ring: u64,
    slots: u64,
    /// The first word of the index.
    index: usize,
    /// The first word of the ring.
    base: usize,
    /// The lock of the writers, with their counts.
    writer: Mutex<CacheCounts>,
    /// The cache serves and writes nothing more: it was closed, or its
    /// order was lost.
    disabled: AtomicBool,
    /// The cache serves. A file with entries serves only after its
    /// verification against the server.
    verified: AtomicBool,
}

/// The layout of a cache: the bytes of the ring, the number of slots, the
/// first word of the ring, and the words of the mapping.
struct Layout {
    ring: u64,
    slots: u64,
    base: usize,
    words: usize,
}

impl Layout {
    /// Return the layout of a cache whose ring holds `size` bytes, rounded
    /// down to a multiple of 8.
    fn of(size: u64) -> io::Result<Layout> {
        let ring = size / 8 * 8;
        if ring == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "the cache holds at least 8 bytes",
            ));
        }
        let slots = (ring / BYTES_PER_SLOT).max(MIN_SLOTS).next_power_of_two();
        let too_large = || io::Error::new(io::ErrorKind::InvalidInput, "the cache is too large");
        let index_words = usize::try_from(slots * 2).map_err(|_| too_large())?;
        let ring_words = usize::try_from(ring / 8).map_err(|_| too_large())?;
        let words = HEADER_WORDS
            .checked_add(index_words)
            .and_then(|n| n.checked_add(ring_words))
            .filter(|words| words.checked_mul(8).is_some())
            .ok_or_else(too_large)?;
        Ok(Layout {
            ring,
            slots,
            base: HEADER_WORDS + index_words,
            words,
        })
    }

    /// Return the static words of the header for the storage `storage`:
    /// the magic, the size of the ring, the number of slots, the length of
    /// the name and its bytes. The name holds at most `STORAGE_BYTES`.
    fn static_words(&self, storage: &[u8]) -> io::Result<[u64; STATIC_WORDS]> {
        if storage.len() > STORAGE_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("the name of the storage is longer than {STORAGE_BYTES} bytes"),
            ));
        }
        let mut words = [0u64; STATIC_WORDS];
        words[H_MAGIC] = MAGIC;
        words[H_RING] = self.ring;
        words[H_SLOTS] = self.slots;
        words[3] = storage.len() as u64;
        for (word, chunk) in words[4..].iter_mut().zip(storage.chunks(8)) {
            let mut bytes = [0u8; 8];
            bytes[..chunk.len()].copy_from_slice(chunk);
            *word = u64::from_le_bytes(bytes);
        }
        Ok(words)
    }
}

/// The checksum of a slice of words.
fn words_sum(words: &[u64]) -> u64 {
    let bytes: Vec<u8> = words.iter().flat_map(|word| word.to_le_bytes()).collect();
    xxh3_64(&bytes)
}

fn align8(n: u64) -> u64 {
    n.div_ceil(8) * 8
}

/// The checksum of the head of an entry: its first 8 words.
fn head_sum(words: &[u64]) -> u64 {
    let mut bytes = [0u8; 64];
    for (chunk, word) in bytes.as_chunks_mut::<8>().0.iter_mut().zip(words) {
        *chunk = word.to_le_bytes();
    }
    xxh3_64(&bytes)
}

impl Cache {
    /// Create a cache whose ring holds `size` bytes, in an anonymous
    /// mapping. The header and the index come on top. `size` rounds down
    /// to a multiple of 8, and must hold at least one word.
    pub fn new(size: u64) -> io::Result<Cache> {
        let layout = Layout::of(size)?;
        let fixed = layout.static_words(b"")?;
        let cache = Cache::with(layout.words, &layout, None)?;
        cache.init(&fixed);
        Ok(cache)
    }

    /// Return a cache on a new mapping of `layout`: of `file`, else
    /// anonymous. It serves at once.
    fn with(words: usize, layout: &Layout, file: Option<File>) -> io::Result<Cache> {
        let map = match &file {
            Some(file) => Mapping::shared(file, words)?,
            None => Mapping::anonymous(words)?,
        };
        Ok(Cache {
            file: Mutex::new(file),
            map,
            ring: layout.ring,
            slots: layout.slots,
            index: HEADER_WORDS,
            base: layout.base,
            writer: Mutex::new(CacheCounts::default()),
            disabled: AtomicBool::new(false),
            verified: AtomicBool::new(true),
        })
    }

    /// Write the header of an empty cache, with the static words `fixed`.
    /// The other words of the mapping must be zero.
    fn init(&self, fixed: &[u64; STATIC_WORDS]) {
        for (i, word) in fixed.iter().enumerate() {
            let index = match i {
                0..3 => i,
                _ => H_STORAGE + i - 3,
            };
            self.word(index).store(*word, Ordering::Relaxed);
        }
        self.word(H_STATIC_SUM)
            .store(words_sum(fixed), Ordering::Relaxed);
        self.word(H_APPLIED).store(0, Ordering::Relaxed);
        // The positions start at one ring, so the position 0 is no entry.
        self.word(H_HEAD).store(self.ring, Ordering::Release);
    }

    /// Return the bytes of the ring.
    pub fn size(&self) -> u64 {
        self.ring
    }

    /// Return the tid up to which the cache applied the commits.
    pub fn applied(&self) -> Tid {
        Tid(self.word(H_APPLIED).load(Ordering::Acquire))
    }

    /// Return the counts of the writers.
    pub fn counts(&self) -> CacheCounts {
        *self.lock()
    }

    /// Tell if the cache serves: it is verified and not closed. A cache
    /// that a file gave serves only after `verify`.
    pub fn verified(&self) -> bool {
        self.verified.load(Ordering::Acquire) && !self.disabled.load(Ordering::Relaxed)
    }

    /// Tell if an entry with `len` bytes of data fits: an entry larger
    /// than half of the ring is never cached.
    pub fn fits(&self, len: usize) -> bool {
        align8(HEAD_BYTES.saturating_add(len as u64)) <= self.ring / 2
    }

    fn word(&self, index: usize) -> &AtomicU64 {
        &self.map.words()[index]
    }

    fn lock(&self) -> MutexGuard<'_, CacheCounts> {
        self.writer.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Return the first word of the entry at the position `pos`.
    fn at(&self, pos: u64) -> usize {
        // The offset is below the ring, so it fits the mapping.
        self.base + ((pos % self.ring) / 8) as usize
    }

    /// Tell if the bytes at `pos` are intact: the head of the ring stayed
    /// within one ring of `pos`. Call it after the copy of the bytes.
    fn intact(&self, pos: u64) -> bool {
        fence(Ordering::Acquire);
        let head = self.word(H_HEAD).load(Ordering::Relaxed);
        pos != 0 && pos < head && head <= pos.saturating_add(self.ring)
    }

    /// Copy and check the head of the entry at `pos`.
    fn entry(&self, pos: u64) -> Option<Head> {
        if pos == 0 || !pos.is_multiple_of(8) || pos % self.ring + HEAD_BYTES > self.ring {
            return None;
        }
        let at = self.at(pos);
        let mut words = [0u64; HEAD_WORDS];
        for (i, word) in words.iter_mut().enumerate() {
            *word = self.word(at + i).load(Ordering::Relaxed);
        }
        if !self.intact(pos) {
            return None;
        }
        let len = words[E_LENGTHS] & u64::from(u32::MAX);
        let raw = words[E_LENGTHS] >> 32;
        let codec = words[E_CODEC] & u64::from(u32::MAX);
        let total = words[E_CODEC] >> 32;
        let good = words[E_HEAD_SUM] == head_sum(&words[..E_HEAD_SUM])
            && words[E_POS] == pos
            && codec == CODEC_RAW
            && raw == len
            && total == align8(HEAD_BYTES + len)
            && pos % self.ring + total <= self.ring;
        good.then_some(Head {
            pos,
            oid: words[E_OID],
            start: words[E_START],
            end: words[E_END],
            prev: words[E_PREV],
            len,
            total,
            data_sum: words[E_DATA_SUM],
            words,
        })
    }

    /// Copy the data of `entry` into `buf`, then check its checksum and
    /// that the ring did not reach it during the copy.
    fn copy(&self, entry: &Head, buf: &mut [u8]) -> bool {
        if buf.len() as u64 != entry.len {
            return false;
        }
        let at = self.at(entry.pos) + HEAD_WORDS;
        for (i, chunk) in buf.chunks_mut(8).enumerate() {
            let word = self.word(at + i).load(Ordering::Relaxed).to_le_bytes();
            chunk.copy_from_slice(&word[..chunk.len()]);
        }
        xxh3_64(buf) == entry.data_sum && self.intact(entry.pos)
    }

    fn hash(&self, oid: u64) -> u64 {
        oid.wrapping_mul(0x9E37_79B9_7F4A_7C15).rotate_left(29) & (self.slots - 1)
    }

    fn slot_key(&self, slot: u64) -> &AtomicU64 {
        self.word(self.index + slot as usize * 2)
    }

    fn slot_pos(&self, slot: u64) -> &AtomicU64 {
        self.word(self.index + slot as usize * 2 + 1)
    }

    /// Return the slot of `oid` and the position that it holds.
    fn find_slot(&self, oid: u64) -> Option<(u64, u64)> {
        let key = oid.wrapping_add(1);
        let mut slot = self.hash(oid);
        for _ in 0..MAX_PROBE {
            let found = self.slot_key(slot).load(Ordering::Acquire);
            if found == 0 {
                return None;
            }
            if found == key {
                return Some((slot, self.slot_pos(slot).load(Ordering::Acquire)));
            }
            slot = (slot + 1) & (self.slots - 1);
        }
        None
    }

    /// Return the entries of `oid`, newest insert first, at most
    /// `MAX_CHAIN`. The chain stops at an entry that the ring overwrote.
    fn chain(&self, oid: u64) -> impl Iterator<Item = Head> + '_ {
        let mut pos = self.find_slot(oid).map_or(0, |(_, pos)| pos);
        let mut left = MAX_CHAIN;
        std::iter::from_fn(move || {
            if pos == 0 || left == 0 {
                return None;
            }
            left -= 1;
            let entry = self.entry(pos).filter(|entry| entry.oid == oid)?;
            pos = entry.prev;
            Some(entry)
        })
    }

    /// Find the entry of `oid` that holds the revision of the snapshots
    /// before `before`. `out(len, fill)` makes a buffer of `len` bytes and
    /// calls `fill` on it, which copies the data and checks it. Return the
    /// result of `out`, the start tid of the entry and its end tid, or
    /// `None` for a current entry. A current entry serves only `before`
    /// up to `applied + 1`.
    pub fn load_before_with<R>(
        &self,
        oid: Oid,
        before: Tid,
        out: impl FnOnce(usize, &mut dyn FnMut(&mut [u8]) -> bool) -> Option<R>,
    ) -> Option<(R, Tid, Option<Tid>)> {
        if !self.verified() {
            return None;
        }
        let applied = self.applied().0;
        let before = before.0;
        let entry = self.chain(oid.0).find(|entry| {
            entry.start < before
                && if entry.end == 0 {
                    before <= applied.saturating_add(1)
                } else {
                    !entry.dropped() && entry.end >= before
                }
        })?;
        self.read(&entry, out)
    }

    /// Find the current entry of `oid`, as `load` of `ZEO.ClientStorage`
    /// does, whatever `applied` is. `out` works as for `load_before_with`.
    /// Return the result of `out` and the start tid of the entry.
    pub fn load_with<R>(
        &self,
        oid: Oid,
        out: impl FnOnce(usize, &mut dyn FnMut(&mut [u8]) -> bool) -> Option<R>,
    ) -> Option<(R, Tid)> {
        if !self.verified() {
            return None;
        }
        let entry = self.chain(oid.0).find(|entry| entry.end == 0)?;
        self.read(&entry, out)
            .map(|(result, start, _)| (result, start))
    }

    /// Return the record of `oid` for the snapshots before `before`, as
    /// `load_before_with` finds it.
    pub fn load_before(&self, oid: Oid, before: Tid) -> Option<Record> {
        self.load_before_with(oid, before, to_vec)
            .map(|(data, tid, end)| Record { data, tid, end })
    }

    /// Return the data and the start tid of the current entry of `oid`.
    pub fn load(&self, oid: Oid) -> Option<(Vec<u8>, Tid)> {
        self.load_with(oid, to_vec)
    }

    fn read<R>(
        &self,
        entry: &Head,
        out: impl FnOnce(usize, &mut dyn FnMut(&mut [u8]) -> bool) -> Option<R>,
    ) -> Option<(R, Tid, Option<Tid>)> {
        let mut fill = |buf: &mut [u8]| self.copy(entry, buf);
        let result = out(entry.len as usize, &mut fill)?;
        if entry.end == 0 {
            self.refresh(entry);
        }
        let end = (entry.end != 0).then_some(Tid(entry.end));
        Some((result, Tid(entry.start), end))
    }

    /// Copy a current entry that a hit found in the oldest fifth of the
    /// ring to the head of the ring, as the `rearrange` of ZEO does. Only
    /// when the writer lock is free: a hit never waits.
    fn refresh(&self, entry: &Head) {
        let head = self.word(H_HEAD).load(Ordering::Relaxed);
        if entry.pos.saturating_add(self.ring / 5 * 4) > head || entry.total > self.ring / 10 {
            return;
        }
        let mut counts = match self.writer.try_lock() {
            Ok(counts) => counts,
            Err(TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
            Err(TryLockError::WouldBlock) => return,
        };
        if !self.verified() {
            return;
        }
        // Under the lock, the entry is still the newest of its oid and
        // still current.
        let Some(entry) = self.entry(entry.pos) else {
            return;
        };
        if entry.end != 0 || self.find_slot(entry.oid).map(|(_, pos)| pos) != Some(entry.pos) {
            return;
        }
        let mut data = vec![0; entry.len as usize];
        if !self.copy(&entry, &mut data) {
            return;
        }
        if let Some(pos) = self.append(entry.oid, entry.start, 0, entry.prev, &data)
            && self.link(&mut counts, entry.oid, pos)
        {
            counts.stored += 1;
            counts.copied += 1;
        }
    }

    /// Store a record that a load from the server gave: its data, its start
    /// tid and its end tid, or `None` for a current reply. `seen` is the
    /// position of the connection that carried the reply, taken when the
    /// reply arrived. A current reply goes in only when `applied` is not
    /// above `seen`. A record with an end always goes in. Return whether
    /// the cache took it: a lookup can then find it. A cache that does not
    /// serve stores nothing.
    pub fn store(&self, oid: Oid, start: Tid, end: Option<Tid>, data: &[u8], seen: Tid) -> bool {
        if !self.fits(data.len()) {
            return false;
        }
        let mut counts = self.lock();
        if !self.verified() {
            return false;
        }
        if end.is_none() && self.applied().0 > seen.0 {
            counts.refused += 1;
            return false;
        }
        self.store_locked(
            &mut counts,
            oid.0,
            start.0,
            end.map_or(0, |end| end.0),
            data,
        )
    }

    /// Store an entry. The caller holds the writer lock.
    fn store_locked(
        &self,
        counts: &mut CacheCounts,
        oid: u64,
        start: u64,
        end: u64,
        data: &[u8],
    ) -> bool {
        // The key of the oid `u64::MAX` would be 0, the key of an empty slot.
        if oid == u64::MAX || start == 0 || (end != 0 && end <= start) || !self.fits(data.len()) {
            return false;
        }
        let mut prev = None;
        for entry in self.chain(oid) {
            prev.get_or_insert(entry.pos);
            if entry.dropped() {
                continue;
            }
            if entry.start == start {
                // Keep an intact entry of the revision. Drop one whose data
                // fails its checksum, and store the revision again.
                let mut stored = vec![0; entry.len as usize];
                if self.copy(&entry, &mut stored) {
                    return false;
                }
                self.set_end(&entry, entry.start);
                continue;
            }
            if end == 0 && entry.end == 0 {
                if entry.start > start {
                    // The cache holds a newer current entry.
                    return false;
                }
                // The older revision ended at a tid in (entry.start, start],
                // but not surely at `start`: drop it.
                self.set_end(&entry, entry.start);
            }
        }
        let Some(pos) = self.append(oid, start, end, prev.unwrap_or(0), data) else {
            return false;
        };
        if !self.link(counts, oid, pos) {
            return false;
        }
        counts.stored += 1;
        true
    }

    /// Apply the commit `tid` of another client: end the current entry of
    /// each of `oids` at `tid`, then move `applied` to `tid`. Before its
    /// verification, a cache drops the current entry of each oid, and
    /// `applied` stays.
    pub fn apply(&self, tid: Tid, oids: &[Oid]) {
        let _counts = self.lock();
        if self.disabled.load(Ordering::Relaxed) {
            return;
        }
        if !self.verified.load(Ordering::Relaxed) {
            for oid in oids {
                self.drop_current_locked(oid.0);
            }
            return;
        }
        for oid in oids {
            self.end_current(oid.0, tid.0);
        }
        self.set_applied(tid.0);
    }

    /// Apply the commit `tid` of this process. Each write ends the current
    /// entry of its oid at `tid`. A write with data then adds a current
    /// entry with that data. A write without data, such as an oid that the
    /// server resolved, adds nothing. Then move `applied` to `tid`. Before
    /// its verification, a cache drops the current entry of each oid, and
    /// `applied` stays.
    pub fn commit(&self, tid: Tid, writes: &[(Oid, Option<&[u8]>)]) {
        let mut counts = self.lock();
        if self.disabled.load(Ordering::Relaxed) {
            return;
        }
        if !self.verified.load(Ordering::Relaxed) {
            for (oid, _) in writes {
                self.drop_current_locked(oid.0);
            }
            return;
        }
        for (oid, data) in writes {
            self.end_current(oid.0, tid.0);
            if let Some(data) = data {
                self.store_locked(&mut counts, oid.0, tid.0, 0, data);
            }
        }
        self.set_applied(tid.0);
    }

    /// Drop the current entry of `oid`, as `invalidate(oid, None)` of ZEO
    /// does after a `ConflictError`. The entries with an end stay.
    pub fn drop_current(&self, oid: Oid) {
        let _counts = self.lock();
        if !self.disabled.load(Ordering::Relaxed) {
            self.drop_current_locked(oid.0);
        }
    }

    /// Drop the current entries of `oid`. The caller holds the writer lock.
    fn drop_current_locked(&self, oid: u64) {
        for entry in self.chain(oid) {
            if entry.end == 0 {
                self.set_end(&entry, entry.start);
            }
        }
    }

    /// Return the entries that a lookup can find, as `(oid, start, end)`,
    /// with `None` as the end of a current entry, in the order of the
    /// index. A dropped entry is not in it. A cache that serves and writes
    /// nothing more has no entries. The writer lock keeps the entries
    /// still during the scan. `__len__` and `contents()` of the cache of
    /// ZEO give the same things.
    pub fn entries(&self) -> Vec<(Oid, Tid, Option<Tid>)> {
        let _counts = self.lock();
        if self.disabled.load(Ordering::Relaxed) {
            return Vec::new();
        }
        let mut entries = Vec::new();
        for slot in 0..self.slots {
            let key = self.slot_key(slot).load(Ordering::Acquire);
            if key == 0 {
                continue;
            }
            for entry in self.chain(key - 1).filter(|entry| !entry.dropped()) {
                let end = (entry.end != 0).then_some(Tid(entry.end));
                entries.push((Oid(entry.oid), Tid(entry.start), end));
            }
        }
        entries
    }

    /// Drop every entry, and keep `applied`: move the head of the ring two
    /// rings forward.
    pub fn clear(&self) {
        let _counts = self.lock();
        if !self.disabled.load(Ordering::Relaxed) {
            self.clear_locked();
        }
    }

    /// Verify the cache against a server whose last tid is `tid`, as
    /// `verify` of ZEO does after a connection. `changed` gives the oids
    /// that the commits after `applied` wrote: the current entry of each
    /// goes, and the other entries stay, and `applied` moves up to `tid`.
    /// `None` drops every entry, and `applied` becomes `tid`, also when it
    /// was above. A lower `tid` is only for a cache that does not serve
    /// yet: the verification of a file ahead of its server, at the open.
    /// Its `store` refused every reply until then. One hold of the writer
    /// lock does both, so a store that
    /// runs after it refuses a current reply older than `tid`. Then the
    /// cache serves.
    pub fn verify(&self, tid: Tid, changed: Option<&[Oid]>) {
        let _counts = self.lock();
        if self.disabled.load(Ordering::Relaxed) {
            return;
        }
        match changed {
            Some(oids) => {
                for oid in oids {
                    self.drop_current_locked(oid.0);
                }
                self.set_applied(tid.0);
            }
            None => {
                debug_assert!(
                    !self.verified.load(Ordering::Relaxed) || tid >= self.applied(),
                    "only a cache that does not serve moves applied down"
                );
                self.clear_locked();
                self.word(H_APPLIED).store(tid.0, Ordering::Release);
            }
        }
        self.verified.store(true, Ordering::Release);
    }

    /// Drop every entry, and serve and write nothing more. After the loss
    /// of the order of the commits.
    pub fn disable(&self) {
        let _counts = self.lock();
        if !self.disabled.load(Ordering::Relaxed) {
            self.clear_locked();
            self.disabled.store(true, Ordering::Relaxed);
        }
    }

    /// Close the cache: it serves and writes nothing more. An anonymous
    /// mapping drops every entry. A file keeps its entries: the close
    /// writes the mapping to the file, marks the file clean, and frees the
    /// lock of the file. A second close does nothing.
    pub fn close(&self) {
        let _counts = self.lock();
        let file = self
            .file
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        if self.disabled.load(Ordering::Relaxed) && file.is_none() {
            return;
        }
        match file {
            None => self.clear_locked(),
            Some(file) => {
                self.write_clean();
                // The mapping holds the open file of the lock until the
                // unmap, so the close of the file alone does not free it.
                let _ = file.unlock();
            }
        }
        self.disabled.store(true, Ordering::Relaxed);
    }

    fn clear_locked(&self) {
        let head = self.word(H_HEAD).load(Ordering::Relaxed);
        let next = (head / self.ring + 2) * self.ring;
        self.word(H_HEAD).store(next, Ordering::Release);
    }

    fn set_applied(&self, tid: u64) {
        let applied = self.word(H_APPLIED);
        if tid > applied.load(Ordering::Relaxed) {
            applied.store(tid, Ordering::Release);
        }
    }

    /// End the current entries of `oid` that start before `tid` at `tid`.
    /// The caller holds the writer lock.
    fn end_current(&self, oid: u64, tid: u64) {
        for entry in self.chain(oid) {
            if entry.end == 0 && entry.start < tid {
                self.set_end(&entry, tid);
            }
        }
    }

    /// Write the end tid of an entry, then its head checksum. A reader that
    /// sees one without the other finds a bad checksum, and misses. The
    /// caller holds the writer lock.
    fn set_end(&self, entry: &Head, end: u64) {
        let mut words = entry.words;
        words[E_END] = end;
        let at = self.at(entry.pos);
        self.word(at + E_END).store(end, Ordering::Relaxed);
        self.word(at + E_HEAD_SUM)
            .store(head_sum(&words[..E_HEAD_SUM]), Ordering::Release);
    }

    /// Write an entry at the head of the ring, and return its position.
    /// An entry never crosses the end of the ring. The caller holds the
    /// writer lock.
    fn append(&self, oid: u64, start: u64, end: u64, prev: u64, data: &[u8]) -> Option<u64> {
        let len = data.len() as u64;
        let total = align8(HEAD_BYTES + len);
        if total > self.ring / 2 || len > u64::from(u32::MAX) {
            return None;
        }
        let head = self.word(H_HEAD).load(Ordering::Relaxed);
        let mut pos = head;
        if pos % self.ring + total > self.ring {
            pos = (pos / self.ring + 1) * self.ring;
        }
        // Move the head first: a reader of an older entry in this space
        // then sees that the ring reached it.
        self.word(H_HEAD).store(pos + total, Ordering::Relaxed);
        fence(Ordering::Release);
        let mut words = [0u64; HEAD_WORDS];
        words[E_POS] = pos;
        words[E_OID] = oid;
        words[E_START] = start;
        words[E_END] = end;
        words[E_PREV] = prev;
        words[E_LENGTHS] = len | (len << 32);
        words[E_CODEC] = CODEC_RAW | (total << 32);
        words[E_DATA_SUM] = xxh3_64(data);
        words[E_HEAD_SUM] = head_sum(&words[..E_HEAD_SUM]);
        let at = self.at(pos);
        for (i, word) in words.iter().enumerate() {
            self.word(at + i).store(*word, Ordering::Relaxed);
        }
        for (i, chunk) in data.chunks(8).enumerate() {
            let mut bytes = [0u8; 8];
            bytes[..chunk.len()].copy_from_slice(chunk);
            self.word(at + HEAD_WORDS + i)
                .store(u64::from_le_bytes(bytes), Ordering::Relaxed);
        }
        Some(pos)
    }

    /// Point the index at `pos` for `oid`. Take the slot of `oid`, else the
    /// first slot that is empty or whose entry the ring overwrote. Return
    /// false when no slot takes it: no lookup finds the entry. The caller
    /// holds the writer lock.
    fn link(&self, counts: &mut CacheCounts, oid: u64, pos: u64) -> bool {
        let key = oid.wrapping_add(1);
        let mut slot = self.hash(oid);
        let mut take = None;
        for _ in 0..MAX_PROBE {
            let found = self.slot_key(slot).load(Ordering::Relaxed);
            if found == key {
                take = Some(slot);
                break;
            }
            if found == 0 {
                take.get_or_insert(slot);
                break;
            }
            if take.is_none() {
                let held = self.slot_pos(slot).load(Ordering::Relaxed);
                let live = self
                    .entry(held)
                    .is_some_and(|entry| entry.oid == found.wrapping_sub(1));
                if !live {
                    take = Some(slot);
                }
            }
            slot = (slot + 1) & (self.slots - 1);
        }
        match take {
            Some(slot) => {
                self.slot_pos(slot).store(pos, Ordering::Release);
                self.slot_key(slot).store(key, Ordering::Release);
                true
            }
            None => {
                counts.unindexed += 1;
                false
            }
        }
    }
}

impl std::fmt::Debug for Cache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Cache")
            .field("size", &self.ring)
            .field("applied", &self.applied())
            .finish_non_exhaustive()
    }
}

/// The `out` of a lookup that returns the data as a `Vec`.
fn to_vec(len: usize, fill: &mut dyn FnMut(&mut [u8]) -> bool) -> Option<Vec<u8>> {
    let mut data = vec![0; len];
    fill(&mut data).then_some(data)
}

#[cfg(test)]
mod tests;
