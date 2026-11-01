//! Sequences: the named counters of a store.
//!
//! A sequence gives values that are unique and strictly increasing per
//! store and per name. The first value is 1. Within one open store, the
//! values of a name are consecutive, across threads.
//!
//! The store reserves the values in blocks of `BLOCK`. A reservation is a
//! backend write transaction of its own. It writes the end of the block to
//! the row of the name, and the write is durable before the sequence gives
//! a value of the block. An open store reads the stored end at the first
//! use of a name and starts there. So a crash or a reopen never gives a
//! value twice. Each reopen skips at most `BLOCK` values of a name. The
//! skips of reopens in a row add up.
//!
//! An allocation is not part of a transaction: an abort does not give its
//! values back. It takes no per-OID lock and not the commit sequencer. It
//! waits only for the mutex of its name. A reservation holds that mutex
//! and also waits for the single writer of the backend.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};

use crate::error::{Result, ZoeError};
use crate::store::Store;

/// The number of values that one reservation reserves.
const BLOCK: u64 = 100;

/// The values of one name that the store can give without a reservation:
/// `next` up to `end`, `end` excluded.
#[derive(Clone, Copy)]
struct Block {
    next: u64,
    end: u64,
}

/// The state of one name in an open store.
struct Counter {
    name: String,
    /// The reserved values that are not given yet. It is `None` until the
    /// first use of the name reads the stored end. The mutex orders the
    /// allocations of the name, and a reservation holds it.
    block: Mutex<Option<Block>>,
}

/// The sequences of an open store.
#[derive(Default)]
pub(crate) struct Sequences {
    counters: Mutex<HashMap<String, Arc<Counter>>>,
}

impl Sequences {
    /// Return the state of `name`. Make it on the first call.
    fn counter(&self, name: &str) -> Arc<Counter> {
        let mut counters = crate::lock(&self.counters);
        if let Some(counter) = counters.get(name) {
            return Arc::clone(counter);
        }
        let counter = Arc::new(Counter {
            name: name.to_owned(),
            block: Mutex::new(None),
        });
        counters.insert(name.to_owned(), Arc::clone(&counter));
        counter
    }
}

/// A sequence of a store: a named counter.
///
/// `Store::sequence` gives it. Two `Sequence` values of the same name share
/// the same counter.
pub struct Sequence<'a> {
    store: &'a Store,
    counter: Arc<Counter>,
}

impl<'a> Sequence<'a> {
    pub(crate) fn new(store: &'a Store, name: &str) -> Self {
        Self {
            store,
            counter: store.sequences.counter(name),
        }
    }

    /// Return the next value of the sequence.
    ///
    /// When the reserved values are all given, the call reserves the next
    /// block and waits for its write. When no value below `u64::MAX` is
    /// left, it fails with `Protocol`. A read-only store fails with
    /// `ReadOnly`. A failed call gives no value and loses none: the next
    /// call gives the same value.
    pub fn next(&self) -> Result<u64> {
        self.store.writable()?;
        let mut slot = self.lock();
        let mut block = match *slot {
            Some(block) => block,
            None => {
                let block = self.load()?;
                *slot = Some(block);
                block
            }
        };
        if block.next >= block.end {
            block.end = self.reserve(block.next)?;
        }
        let value = block.next;
        block.next += 1;
        *slot = Some(block);
        Ok(value)
    }

    /// Raise the sequence, so that its next value is at or above `end`.
    ///
    /// A restore calls this with the end of the sequence in a copy of
    /// another store, so that no value of that store comes again. The call
    /// lowers nothing: when the next value is at or above `end`, it changes
    /// nothing. When the reserved values end below `end`, the call writes
    /// `end` as the stored end, and the write is durable before the call
    /// returns. Otherwise it only skips the reserved values below `end`.
    /// The stored end never decreases. A read-only store fails with
    /// `ReadOnly`. A failed write can reach the disk all the same, so after
    /// it the open store forgets its reserved values, and the next call
    /// reads the stored end.
    pub fn restore_end(&self, end: u64) -> Result<()> {
        self.store.writable()?;
        let mut slot = self.lock();
        let mut block = match *slot {
            Some(block) => block,
            None => self.load()?,
        };
        if block.next < end {
            if block.end < end {
                if let Err(error) = self.write_end(end) {
                    *slot = None;
                    return Err(error);
                }
                block.end = end;
            }
            block.next = end;
        }
        *slot = Some(block);
        Ok(())
    }

    /// Write `end` as the stored end of the name, in a backend write
    /// transaction of its own.
    fn write_end(&self, end: u64) -> Result<()> {
        let mut write = self.store.backend.begin_write()?;
        write.put_sequence_end(&self.counter.name, end)?;
        write.commit()
    }

    fn lock(&self) -> MutexGuard<'_, Option<Block>> {
        // A panic cannot leave a block half changed: `next` writes it back
        // only after the reservation succeeds.
        crate::lock(&self.counter.block)
    }

    /// Return the empty block at the stored end of the name. A name with no
    /// row starts at 1.
    fn load(&self) -> Result<Block> {
        let stored = self.store.backend.sequence_end(&self.counter.name)?;
        let end = stored.unwrap_or(1).max(1);
        Ok(Block { next: end, end })
    }

    /// Reserve the block that starts at `start`. Return its end after the
    /// end is durable.
    ///
    /// `start` is the next value of the name, so every value given is
    /// below it. A reservation that failed after its write reached the
    /// disk wrote the same end, so a retry does not raise the stored end
    /// further.
    fn reserve(&self, start: u64) -> Result<u64> {
        if start == u64::MAX {
            return Err(ZoeError::Protocol("no value is left in the sequence"));
        }
        let end = start.saturating_add(BLOCK);
        self.write_end(end)?;
        Ok(end)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::mpsc;
    use std::thread;
    use std::time::Duration;

    use super::*;
    use crate::types::{Oid, Tid};
    use crate::vfs::MemVfs;

    fn store() -> Store {
        Store::open_vfs(MemVfs::new()).unwrap()
    }

    /// Return the stored end of `name`.
    fn stored(store: &Store, name: &str) -> Option<u64> {
        store.backend.sequence_end(name).unwrap()
    }

    #[test]
    fn a_reservation_writes_the_end_before_the_first_value_of_its_block() {
        let store = store();
        let sequence = store.sequence("intids");
        assert_eq!(stored(&store, "intids"), None);
        assert_eq!(sequence.next().unwrap(), 1);
        assert_eq!(stored(&store, "intids"), Some(1 + BLOCK));
        for value in 2..=BLOCK {
            assert_eq!(sequence.next().unwrap(), value);
        }
        // The block is spent, but the next block is not reserved yet.
        assert_eq!(stored(&store, "intids"), Some(1 + BLOCK));
        assert_eq!(sequence.next().unwrap(), BLOCK + 1);
        assert_eq!(stored(&store, "intids"), Some(1 + 2 * BLOCK));
    }

    #[test]
    fn a_reservation_changes_no_tid_and_no_oid() {
        let store = store();
        store.sequence("intids").next().unwrap();
        assert_eq!(store.last_transaction(), Tid::ZERO);
        assert_eq!(store.new_oid().unwrap(), Oid(1));
        assert_eq!(store.iterator(None, None).unwrap().count(), 0);
    }

    #[test]
    fn two_sequences_of_one_name_share_the_values() {
        let store = store();
        let (a, b) = (store.sequence("intids"), store.sequence("intids"));
        assert_eq!(
            [a.next(), b.next(), a.next()].map(Result::unwrap),
            [1, 2, 3]
        );
    }

    #[test]
    fn an_open_store_starts_at_the_stored_end() {
        let vfs = MemVfs::new();
        {
            let store = Store::open_vfs(vfs.clone()).unwrap();
            let sequence = store.sequence("intids");
            for _ in 0..3 {
                sequence.next().unwrap();
            }
        }
        let store = Store::open_vfs(vfs).unwrap();
        assert_eq!(store.sequence("intids").next().unwrap(), 1 + BLOCK);
        assert_eq!(stored(&store, "intids"), Some(1 + 2 * BLOCK));
    }

    #[test]
    fn the_skips_of_reopens_in_a_row_add_up_but_no_value_comes_twice() {
        // Session 1 gives 1 to 5 and closes with the stored end at 101.
        let vfs = MemVfs::new();
        {
            let store = Store::open_vfs(vfs.clone()).unwrap();
            for _ in 0..5 {
                store.sequence("intids").next().unwrap();
            }
        }
        vfs.mark("session 2 opens");
        // Session 2 reserves up to 201 and gives 101.
        {
            let store = Store::open_vfs(vfs.clone()).unwrap();
            assert_eq!(store.sequence("intids").next().unwrap(), 1 + BLOCK);
            vfs.mark("returned 101");
        }

        // Session 3 opens each crash image of session 2.
        let mut largest = 0;
        let images = vfs.crash_images();
        for image in images
            .iter()
            .filter(|image| image.is_after("session 2 opens"))
        {
            let returned = if image.is_after("returned 101") {
                1 + BLOCK
            } else {
                5
            };
            let store = Store::open_vfs(image.vfs()).unwrap();
            let first = store.sequence("intids").next().unwrap();
            assert!(
                first > returned,
                "{image:?}: {first} came again after {returned}"
            );
            largest = largest.max(first - returned - 1);
        }
        // An image where session 2 died after its reservation and before it
        // gave 101 shows a caller 5, then 201.
        assert_eq!(largest, 195);
    }

    #[test]
    fn the_last_value_is_below_u64_max() {
        let store = store();
        let mut write = store.backend.begin_write().unwrap();
        write.put_sequence_end("intids", u64::MAX - 2).unwrap();
        write.commit().unwrap();

        let sequence = store.sequence("intids");
        assert_eq!(sequence.next().unwrap(), u64::MAX - 2);
        assert_eq!(sequence.next().unwrap(), u64::MAX - 1);
        assert_eq!(stored(&store, "intids"), Some(u64::MAX));
        for _ in 0..2 {
            assert!(matches!(sequence.next(), Err(ZoeError::Protocol(_))));
        }
    }

    #[test]
    fn a_restored_end_raises_a_new_sequence_durably() {
        let vfs = MemVfs::new();
        {
            let store = Store::open_vfs(vfs.clone()).unwrap();
            store.sequence("intids").restore_end(500).unwrap();
            assert_eq!(stored(&store, "intids"), Some(500));
            assert_eq!(store.sequence("intids").next().unwrap(), 500);
        }
        // The reopen starts at the end of the block that `next` reserved.
        let store = Store::open_vfs(vfs).unwrap();
        assert_eq!(store.sequence("intids").next().unwrap(), 500 + BLOCK);
    }

    #[test]
    fn a_restored_end_lowers_nothing() {
        let store = store();
        let sequence = store.sequence("intids");
        for _ in 0..3 {
            sequence.next().unwrap();
        }
        sequence.restore_end(2).unwrap();
        assert_eq!(sequence.next().unwrap(), 4);
        assert_eq!(stored(&store, "intids"), Some(1 + BLOCK));

        // A stored end above the restored end stays.
        let mut write = store.backend.begin_write().unwrap();
        write.put_sequence_end("other", 1_000).unwrap();
        write.commit().unwrap();
        store.sequence("other").restore_end(10).unwrap();
        assert_eq!(stored(&store, "other"), Some(1_000));
        assert_eq!(store.sequence("other").next().unwrap(), 1_000);
    }

    #[test]
    fn a_restored_end_inside_the_block_writes_nothing() {
        let store = store();
        let sequence = store.sequence("intids");
        assert_eq!(sequence.next().unwrap(), 1);
        sequence.restore_end(50).unwrap();
        assert_eq!(stored(&store, "intids"), Some(1 + BLOCK));
        assert_eq!(sequence.next().unwrap(), 50);
        // Past the block, the next reservation starts at the next value.
        sequence.restore_end(1 + 3 * BLOCK).unwrap();
        assert_eq!(stored(&store, "intids"), Some(1 + 3 * BLOCK));
        assert_eq!(sequence.next().unwrap(), 1 + 3 * BLOCK);
        assert_eq!(stored(&store, "intids"), Some(1 + 4 * BLOCK));
    }

    #[test]
    fn a_read_only_store_restores_no_end() {
        let dir = std::env::temp_dir().join(format!("zoe-sequence-ro-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        drop(Store::open(&dir).unwrap());
        let store = Store::open_read_only(&dir).unwrap();
        let restored = store.sequence("intids").restore_end(10);
        assert!(matches!(restored, Err(ZoeError::ReadOnly)), "{restored:?}");
        assert_eq!(stored(&store, "intids"), None);
        drop(store);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_allocation_does_not_wait_for_the_commit_sequencer() {
        let store = Arc::new(store());
        // The commit sequencer stays locked, as during a finish.
        let sequencer = store.lock_sequencer();
        let (sent, received) = mpsc::channel();
        let allocator = Arc::clone(&store);
        thread::spawn(move || {
            let sequence = allocator.sequence("intids");
            let values: Vec<u64> = (0..=BLOCK).map(|_| sequence.next().unwrap()).collect();
            let _ = sent.send(values);
        });
        let values = received
            .recv_timeout(Duration::from_secs(10))
            .expect("the allocation waited for the commit sequencer");
        assert_eq!(values, (1..=BLOCK + 1).collect::<Vec<_>>());
        drop(sequencer);
    }
}
