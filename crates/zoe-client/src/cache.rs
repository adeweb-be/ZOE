//! The client cache: the layout of ADR 0034 (proposed) in one anonymous
//! mapping, with the semantics of `ZEO.cache.ClientCache`.
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

mod mapping;

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

// The words of the header. The magic, the size of the ring and the number
// of slots have a checksum. The words 4 and 5 hold the boot id of the
// kernel, the word 6 the clean flag and the word 9 the checksum of the
// state. An anonymous mapping leaves them at zero: only a file uses them.
const H_MAGIC: usize = 0;
const H_RING: usize = 1;
const H_SLOTS: usize = 2;
const H_STATIC_SUM: usize = 3;
const H_APPLIED: usize = 7;
const H_HEAD: usize = 8;

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
    /// The cache stores nothing more: it was closed, or its order was lost.
    disabled: AtomicBool,
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
            .ok_or_else(too_large)?;
        let cache = Cache {
            map: Mapping::anonymous(words)?,
            ring,
            slots,
            index: HEADER_WORDS,
            base: HEADER_WORDS + index_words,
            writer: Mutex::new(CacheCounts::default()),
            disabled: AtomicBool::new(false),
        };
        cache.init();
        Ok(cache)
    }

    /// Write the header of an empty cache.
    fn init(&self) {
        let mut fixed = [0u8; 24];
        for (chunk, word) in fixed
            .as_chunks_mut::<8>()
            .0
            .iter_mut()
            .zip([MAGIC, self.ring, self.slots])
        {
            *chunk = word.to_le_bytes();
        }
        self.word(H_MAGIC).store(MAGIC, Ordering::Relaxed);
        self.word(H_RING).store(self.ring, Ordering::Relaxed);
        self.word(H_SLOTS).store(self.slots, Ordering::Relaxed);
        self.word(H_STATIC_SUM)
            .store(xxh3_64(&fixed), Ordering::Relaxed);
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
        if self.disabled.load(Ordering::Relaxed) {
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
        if let Some(pos) = self.append(entry.oid, entry.start, 0, entry.prev, &data) {
            self.link(&mut counts, entry.oid, pos);
            counts.stored += 1;
            counts.copied += 1;
        }
    }

    /// Store a record that a load from the server gave: its data, its start
    /// tid and its end tid, or `None` for a current reply. `seen` is the
    /// position of the connection that carried the reply, taken when the
    /// reply arrived. A current reply goes in only when `applied` is not
    /// above `seen`. A record with an end always goes in. Return whether
    /// the cache took it.
    pub fn store(&self, oid: Oid, start: Tid, end: Option<Tid>, data: &[u8], seen: Tid) -> bool {
        if !self.fits(data.len()) {
            return false;
        }
        let mut counts = self.lock();
        if self.disabled.load(Ordering::Relaxed) {
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
        if start == 0 || (end != 0 && end <= start) || !self.fits(data.len()) {
            return false;
        }
        let mut prev = None;
        for entry in self.chain(oid) {
            prev.get_or_insert(entry.pos);
            if entry.dropped() {
                continue;
            }
            if entry.start == start {
                return false;
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
        self.link(counts, oid, pos);
        counts.stored += 1;
        true
    }

    /// Apply the commit `tid` of another client: end the current entry of
    /// each of `oids` at `tid`, then move `applied` to `tid`.
    pub fn apply(&self, tid: Tid, oids: &[Oid]) {
        let _counts = self.lock();
        for oid in oids {
            self.end_current(oid.0, tid.0);
        }
        self.set_applied(tid.0);
    }

    /// Apply the commit `tid` of this process. Each write ends the current
    /// entry of its oid at `tid`. A write with data then adds a current
    /// entry with that data. A write without data, such as an oid that the
    /// server resolved, adds nothing. Then move `applied` to `tid`.
    pub fn commit(&self, tid: Tid, writes: &[(Oid, Option<&[u8]>)]) {
        let mut counts = self.lock();
        let disabled = self.disabled.load(Ordering::Relaxed);
        for (oid, data) in writes {
            self.end_current(oid.0, tid.0);
            if let Some(data) = data
                && !disabled
            {
                self.store_locked(&mut counts, oid.0, tid.0, 0, data);
            }
        }
        self.set_applied(tid.0);
    }

    /// Drop the current entry of `oid`, as `invalidate(oid, None)` of ZEO
    /// does after a `ConflictError`. The entries with an end stay.
    pub fn drop_current(&self, oid: Oid) {
        let _counts = self.lock();
        for entry in self.chain(oid.0) {
            if entry.end == 0 {
                self.set_end(&entry, entry.start);
            }
        }
    }

    /// Drop every entry, and keep `applied`: move the head of the ring two
    /// rings forward.
    pub fn clear(&self) {
        let _counts = self.lock();
        self.clear_locked();
    }

    /// Drop every entry, and store nothing more. After the close, or after
    /// the loss of the order of the commits.
    pub fn disable(&self) {
        let _counts = self.lock();
        self.disabled.store(true, Ordering::Relaxed);
        self.clear_locked();
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
    /// first slot that is empty or whose entry the ring overwrote. The
    /// caller holds the writer lock.
    fn link(&self, counts: &mut CacheCounts, oid: u64, pos: u64) {
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
            }
            None => counts.unindexed += 1,
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
