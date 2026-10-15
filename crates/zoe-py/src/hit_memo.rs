//! POC ZOE-187 (not for merge): the `bytes` of recent hits of the client
//! cache, so that a hit of the same entry gives the same `bytes` again and
//! copies nothing.
//!
//! The client cache decides every hit, as without the memo: the lookup,
//! the snapshot rules, the checks of the head of the entry and of the ring.
//! The memo only replaces the copy of the data. It keeps, for an oid, the
//! logical position of the entry that it copied and the `bytes` of that
//! copy, which the copy checked against the checksum of the data. Positions
//! never repeat in a cache, and the data of an entry never changes while a
//! lookup finds the entry intact. So when the lookup finds the entry at the
//! same position, the `bytes` hold its data, and the memo gives them.
//!
//! The memo holds at most `bound` bytes of data in all. A copy reserves its
//! bytes before it goes in. When the reserve fails, the copy drops the
//! oldest copies of its shard, and goes in only when the reserve then
//! succeeds. A `bytes` that a caller still holds after a drop belongs to
//! the caller, and is not in the bound.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Mutex, MutexGuard, PoisonError};

use pyo3::prelude::*;
use pyo3::types::PyBytes;

const SHARDS: usize = 16;

/// The memo of one client.
pub(crate) struct HitMemo {
    shards: Vec<Mutex<Shard>>,
    /// The most bytes of data of all copies.
    bound: usize,
    /// The bytes of data of all copies.
    held: AtomicUsize,
    /// The fewest bytes of data of a hit that the memo keeps.
    min_len: usize,
    /// The copies that found no room.
    refused: AtomicU64,
}

#[derive(Default)]
struct Shard {
    /// The position and the `bytes` of the copy of each oid.
    copies: HashMap<u64, (u64, Py<PyBytes>)>,
    /// The copies in the order of their insert, oldest first. An item whose
    /// oid now holds another position is stale.
    order: VecDeque<(u64, u64)>,
    /// The hits that the shard served, the copies that it took and the
    /// copies that it dropped for the bound.
    hits: u64,
    puts: u64,
    dropped: u64,
}

impl Shard {
    /// Remove the copy of `oid`. Return its `bytes`.
    fn remove(&mut self, oid: u64) -> Option<Py<PyBytes>> {
        self.copies.remove(&oid).map(|(_, data)| data)
    }
}

impl HitMemo {
    /// Return a memo of at most `bound` bytes of data, for hits of at least
    /// `min_len` bytes. A bound of 0 gives no memo.
    pub(crate) fn new(bound: usize, min_len: usize) -> Option<HitMemo> {
        (bound > 0).then(|| HitMemo {
            shards: (0..SHARDS).map(|_| Mutex::default()).collect(),
            bound,
            held: AtomicUsize::new(0),
            min_len,
            refused: AtomicU64::new(0),
        })
    }

    /// Return the memo of the environment variable `ZOE_POC_HIT_MEMO`:
    /// `BOUND` or `BOUND:MIN_LEN` in bytes. Without it, no memo.
    pub(crate) fn from_env() -> Option<HitMemo> {
        let text = std::env::var("ZOE_POC_HIT_MEMO").ok()?;
        let (bound, min_len) = text.split_once(':').unwrap_or((&text, "0"));
        HitMemo::new(bound.parse().ok()?, min_len.parse().ok()?)
    }

    /// Tell if the memo keeps a hit of `len` bytes.
    pub(crate) fn keeps(&self, len: usize) -> bool {
        len >= self.min_len && len <= self.bound
    }

    fn shard(&self, oid: u64) -> MutexGuard<'_, Shard> {
        let index = (oid.wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 60) as usize % SHARDS;
        self.shards[index]
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// Reserve `len` bytes of the bound. Return false when they do not fit.
    fn reserve(&self, len: usize) -> bool {
        self.held
            .try_update(Ordering::AcqRel, Ordering::Acquire, |held| {
                (held + len <= self.bound).then_some(held + len)
            })
            .is_ok()
    }

    fn release(&self, py: Python<'_>, data: &Py<PyBytes>) {
        self.held
            .fetch_sub(data.bind(py).as_bytes().len(), Ordering::AcqRel);
    }

    /// Return the `bytes` of the copy of `oid` at `pos`, if the memo holds it.
    pub(crate) fn get<'py>(
        &self,
        py: Python<'py>,
        oid: u64,
        pos: u64,
    ) -> Option<Bound<'py, PyBytes>> {
        let mut shard = self.shard(oid);
        let data = match shard.copies.get(&oid) {
            Some((held, data)) if *held == pos => data.bind(py).clone(),
            _ => return None,
        };
        shard.hits += 1;
        Some(data)
    }

    /// Keep `data`, the checked copy of the entry of `oid` at `pos`, in
    /// place of an older copy of `oid`. Drop the oldest copies of the shard
    /// while the bound has no room for it. The dropped `bytes` are released
    /// after the lock.
    pub(crate) fn put(&self, oid: u64, pos: u64, data: &Bound<'_, PyBytes>) {
        let py = data.py();
        let len = data.as_bytes().len();
        if !self.keeps(len) {
            return;
        }
        let mut dropped = Vec::new();
        {
            let mut shard = self.shard(oid);
            if let Some(old) = shard.remove(oid) {
                self.release(py, &old);
                dropped.push(old);
            }
            let mut room = self.reserve(len);
            while !room {
                let Some((old_oid, old_pos)) = shard.order.pop_front() else {
                    break;
                };
                if shard
                    .copies
                    .get(&old_oid)
                    .is_some_and(|(p, _)| *p == old_pos)
                    && let Some(old) = shard.remove(old_oid)
                {
                    self.release(py, &old);
                    shard.dropped += 1;
                    dropped.push(old);
                    room = self.reserve(len);
                }
            }
            if room {
                shard.copies.insert(oid, (pos, data.clone().unbind()));
                shard.order.push_back((oid, pos));
                shard.puts += 1;
            } else {
                self.refused.fetch_add(1, Ordering::Relaxed);
            }
            // Stale items of `order` stay until a drop reaches them. Keep
            // them below twice the copies.
            if shard.order.len() > 2 * shard.copies.len() + 16 {
                let Shard { copies, order, .. } = &mut *shard;
                order.retain(|(o, p)| copies.get(o).is_some_and(|(held, _)| held == p));
            }
        }
        drop(dropped);
    }

    /// Drop every copy.
    pub(crate) fn clear(&self, py: Python<'_>) {
        for shard in &self.shards {
            let copies = {
                let mut shard = shard.lock().unwrap_or_else(PoisonError::into_inner);
                shard.order.clear();
                std::mem::take(&mut shard.copies)
            };
            for (_, data) in copies.values() {
                self.release(py, data);
            }
        }
    }

    /// Return the copies and the bytes of data that the memo holds, and
    /// its counts of hits, puts, drops and refused copies.
    pub(crate) fn stats(&self) -> [u64; 6] {
        let mut out = [0u64; 6];
        for shard in &self.shards {
            let shard = shard.lock().unwrap_or_else(PoisonError::into_inner);
            out[0] += shard.copies.len() as u64;
            out[2] += shard.hits;
            out[3] += shard.puts;
            out[4] += shard.dropped;
        }
        out[1] = self.held.load(Ordering::Acquire) as u64;
        out[5] = self.refused.load(Ordering::Relaxed);
        out
    }
}
