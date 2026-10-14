//! Group commit.
//!
//! Each finish joins a commit batch. A finish that finds no leader becomes
//! the leader: it takes every finish that waits, writes them all in one
//! backend write transaction under the commit sequencer, and commits it
//! with one sync. Finishes that arrive while the leader writes wait, and
//! form the next batch. When the batch is durable or failed, the leader
//! gives each member its outcome and wakes the waiting finishes. One of
//! them then leads the next batch.
//!
//! The members of a batch commit in arrival order. Each member gets a tid
//! after the tid of the member before it, so the commit order is the batch
//! order. The leader publishes the last tid of the batch after the whole
//! batch is visible to readers.
//!
//! A batch whose members are all deferred restores commits without a
//! sync. It is visible at once, and the next durable commit makes it
//! durable. Each deferred member keeps its intent file in `blobs/tmp/`
//! until then, so a crash before it leaves no blob file of a lost commit.
//! A batch with another member commits with a sync, which also makes every
//! earlier deferred commit durable.
//!
//! The queue mutex is held with no other lock. The leader takes the commit
//! sequencer after it releases the queue mutex.
//!
//! The write of a batch and its sync run in a `tracing` span `fsync`, with
//! the field `commits`, the number of members that have a tid. The span
//! is a child of the span of the leader's finish. A batch of deferred
//! restores commits without a sync, so it has no span `fsync`.

use std::collections::{BTreeMap, HashMap};
use std::io;
use std::mem;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock};

use super::staging::Staging;
use super::{Finished, Observer, check_restore_tid, next_tid};
use crate::backend::{TxnMeta, TxnStatus};
use crate::blob::PendingBlob;
use crate::clock::timestamp;
use crate::error::{Result, ZoeError};
use crate::store::Store;
use crate::types::{Oid, Tid};

/// The part of a transaction that the leader of its batch writes.
pub(super) struct Member {
    /// The tid of a restore transaction. A normal transaction gets its tid
    /// in the batch.
    pub(super) tid: Option<Tid>,
    /// True for a restore transaction whose finish commits without a sync.
    pub(super) deferred: bool,
    pub(super) staged: Staging,
    pub(super) blobs: BTreeMap<Oid, PendingBlob>,
    pub(super) user: Vec<u8>,
    pub(super) description: Vec<u8>,
    pub(super) extension: Vec<u8>,
}

/// The outcome of one finish. The member goes back to its transaction. It
/// is `None` only after a panic of the leader.
pub(super) type Outcome = (Option<Member>, Result<Finished>);

/// The finishes of a store that wait for a batch.
#[derive(Default)]
struct Queue {
    /// The finishes that wait for a leader, with their tickets, in arrival
    /// order.
    waiting: Vec<(u64, Member)>,
    /// The outcome of each finish of an ended batch, by ticket.
    ended: HashMap<u64, Outcome>,
    /// True while a leader writes a batch.
    leading: bool,
    /// The ticket of the next finish.
    next: u64,
}

/// The commit batches of a store.
#[derive(Default)]
pub(crate) struct GroupCommit {
    queue: Mutex<Queue>,
    /// Signals the end of a batch.
    changed: Condvar,
    /// The observer of the store. A leader tells it the size of each
    /// batch that it made durable.
    pub(super) observer: OnceLock<Arc<dyn Observer>>,
}

impl GroupCommit {
    fn lock(&self) -> MutexGuard<'_, Queue> {
        // The queue stays valid after a panic: `Lead` ends the lead when
        // it unwinds.
        self.queue.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Commit `member` in a batch of `store`. Return the member and its
    /// outcome when its batch is durable or failed.
    pub(super) fn finish(&self, store: &Store, member: Member) -> Outcome {
        let mut queue = self.lock();
        let ticket = queue.next;
        queue.next += 1;
        queue.waiting.push((ticket, member));
        loop {
            if let Some(outcome) = queue.ended.remove(&ticket) {
                return outcome;
            }
            if !queue.leading {
                // The ticket is still in `waiting`: only a leader takes it
                // out, and a leader gives its outcome before it stops.
                queue.leading = true;
                let batch = mem::take(&mut queue.waiting);
                drop(queue);
                let mut lead = Lead {
                    group: self,
                    tickets: batch.iter().map(|(ticket, _)| *ticket).collect(),
                    outcomes: None,
                };
                lead.outcomes = Some(write_batch(store, batch));
                drop(lead);
                queue = self.lock();
                continue;
            }
            queue = self.changed.wait(queue).unwrap_or_else(|e| e.into_inner());
        }
    }

    /// Return the number of finishes that joined a batch since the store
    /// opened.
    #[cfg(test)]
    pub(super) fn joined(&self) -> u64 {
        self.lock().next
    }
}

/// The batch of a leader. When it drops, it gives each member its outcome,
/// ends the lead and wakes the waiting finishes. After a panic of the
/// leader, the outcome of each member is unknown.
struct Lead<'a> {
    group: &'a GroupCommit,
    tickets: Vec<u64>,
    outcomes: Option<Vec<Outcome>>,
}

impl Drop for Lead<'_> {
    fn drop(&mut self) {
        let outcomes = self.outcomes.take();
        let mut queue = self.group.lock();
        match outcomes {
            Some(outcomes) => queue
                .ended
                .extend(self.tickets.iter().copied().zip(outcomes)),
            None => queue.ended.extend(self.tickets.iter().map(|&ticket| {
                let error = "the leader of the commit batch panicked";
                (ticket, (None, Err(ZoeError::Backend(error.into()))))
            })),
        }
        queue.leading = false;
        drop(queue);
        self.group.changed.notify_all();
    }
}

/// Write the members of a batch under the commit sequencer, and return
/// the outcome of each, in batch order.
///
/// Each member gets its tid in batch order. A tid that fails its check
/// fails only its member, with `Protocol`. The other members go in one
/// backend write transaction. An error of that write fails each of them,
/// and the store halts. The write commits without a sync when each of
/// these members is a deferred restore.
fn write_batch(store: &Store, batch: Vec<(u64, Member)>) -> Vec<Outcome> {
    let mut sequencer = store.lock_sequencer();
    let Some(start) = *sequencer else {
        return batch
            .into_iter()
            .map(|(_, member)| (Some(member), Err(ZoeError::Halted)))
            .collect();
    };
    let now = timestamp(store.clock.now());
    let mut last = start;
    // The number of commits that the sync of the batch made durable.
    let mut synced = 0;
    let mut members: Vec<(Member, Result<Finished>)> = batch
        .into_iter()
        .map(|(_, member)| {
            let tid = match member.tid {
                Some(tid) => check_restore_tid(tid, last).map(|()| tid),
                None => next_tid(now, last),
            };
            let finished = tid.map(|tid| Finished {
                tid,
                previous: mem::replace(&mut last, tid),
            });
            (member, finished)
        })
        .collect();
    if last != start {
        let deferred = members
            .iter()
            .all(|(member, finished)| member.deferred || finished.is_err());
        // The sequencer stays empty if the write below fails.
        *sequencer = None;
        // Only a batch that syncs gets the span `fsync`.
        let span = if deferred {
            tracing::Span::none()
        } else {
            let commits = members.iter().filter(|(_, f)| f.is_ok()).count();
            tracing::info_span!("fsync", commits)
        };
        match span.in_scope(|| write(store, &mut members, deferred)) {
            Ok(intents) => {
                if deferred {
                    // A crash before the next durable commit loses this
                    // batch. The intent files let the next open remove
                    // its blob files.
                    for intent in intents {
                        store.blobs().keep(intent);
                    }
                } else {
                    for intent in intents {
                        store.blobs().finished(intent);
                    }
                    // The sync of this batch covers the deferred commits
                    // before it.
                    store.blobs().release_kept();
                }
                for (member, finished) in &members {
                    if let Ok(finished) = finished {
                        // A running pack marks what these records refer to
                        // before its next removal.
                        let oids = member.staged.iter().map(|(oid, _)| oid);
                        store.packing.note(finished.tid, oids);
                    }
                }
                *sequencer = Some(last);
                if !deferred {
                    synced = members.iter().filter(|(_, f)| f.is_ok()).count();
                }
            }
            Err(error) => fail(&mut members, error),
        }
    }
    drop(sequencer);
    if synced > 0
        && let Some(observer) = store.observer()
    {
        observer.batch_synced(synced);
    }
    members
        .into_iter()
        .map(|(member, finished)| (Some(member), finished))
        .collect()
}

/// Place the blobs of the members that have a tid, then write these
/// members in one backend write transaction and commit it, without a sync
/// when `deferred` is true. Return the intent files of the blobs.
fn write(
    store: &Store,
    members: &mut [(Member, Result<Finished>)],
    deferred: bool,
) -> Result<Vec<Option<String>>> {
    let mut intents = Vec::new();
    for (member, finished) in members.iter_mut() {
        let Ok(finished) = finished else { continue };
        if let Some(oid) = member.staged.last_oid() {
            store.raise_next_oid(oid);
        }
        // The blob files are durable in their place before the backend
        // commit, so a blob record never lacks its file.
        intents.push(store.blobs().place(finished.tid, &mut member.blobs)?);
    }
    let mut write = store.backend.begin_write()?;
    for (member, finished) in members.iter_mut() {
        let Ok(finished) = finished else { continue };
        let tid = finished.tid;
        // A spilled pickle is read back here, one at a time.
        write.put_records(tid, &mut member.staged.records())?;
        for (oid, _) in member.staged.iter() {
            if let Some(blob) = member.blobs.get(&oid) {
                write.put_blob(oid, tid, blob.record())?;
            }
        }
        write.put_txn(&TxnMeta {
            tid,
            user: mem::take(&mut member.user),
            description: mem::take(&mut member.description),
            extension: mem::take(&mut member.extension),
            oid_count: member.staged.len() as u64,
            status: TxnStatus::Committed,
        })?;
    }
    if deferred {
        write.commit_deferred()?;
    } else {
        write.commit()?;
    }
    Ok(intents)
}

/// Fail each member that has a tid with `error`. The last such member gets
/// `error` itself, and the others a copy.
fn fail(members: &mut [(Member, Result<Finished>)], error: ZoeError) {
    let mut written = members
        .iter_mut()
        .map(|(_, finished)| finished)
        .filter(|finished| finished.is_ok())
        .peekable();
    let mut error = Some(error);
    while let Some(finished) = written.next() {
        let own = match (written.peek(), error.as_ref()) {
            (Some(_), Some(shared)) => copy(shared),
            _ => error.take().expect("the last member takes the error"),
        };
        *finished = Err(own);
    }
}

/// Return a copy of `error` for another member of a failed batch. The copy
/// has the message of `error`. An I/O error keeps its kind.
fn copy(error: &ZoeError) -> ZoeError {
    match error {
        ZoeError::Io(error) => ZoeError::Io(io::Error::new(error.kind(), error.to_string())),
        ZoeError::Backend(inner) => ZoeError::Backend(inner.to_string().into()),
        other => ZoeError::Backend(other.to_string().into()),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::io::{self, Read};
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::thread;
    use std::time::{Duration, Instant};

    use super::*;
    use crate::commit::Txn;
    use crate::vfs::{DirLock, MemFile, MemVfs, Vfs, WriteFile};

    /// The database file of a store.
    const DATABASE: &str = "zoe.redb";

    /// A `MemVfs` that counts the syncs and the writes of the database
    /// file.
    #[derive(Clone, Default)]
    struct Counting {
        vfs: MemVfs,
        syncs: Arc<AtomicU64>,
        writes: Arc<AtomicU64>,
    }

    /// A file of `Counting`.
    #[derive(Debug)]
    struct CountedFile {
        file: MemFile,
        /// The counters of the database file, `None` for another file.
        counters: Option<(Arc<AtomicU64>, Arc<AtomicU64>)>,
    }

    impl Counting {
        fn counted(&self, name: &str, file: MemFile) -> CountedFile {
            let counters = (name == DATABASE).then(|| (self.syncs.clone(), self.writes.clone()));
            CountedFile { file, counters }
        }

        fn syncs(&self) -> u64 {
            self.syncs.load(Ordering::SeqCst)
        }
    }

    impl redb::StorageBackend for CountedFile {
        fn len(&self) -> io::Result<u64> {
            self.file.len()
        }

        fn read(&self, offset: u64, out: &mut [u8]) -> io::Result<()> {
            self.file.read(offset, out)
        }

        fn set_len(&self, len: u64) -> io::Result<()> {
            self.file.set_len(len)
        }

        fn sync_data(&self) -> io::Result<()> {
            if let Some((syncs, _)) = &self.counters {
                syncs.fetch_add(1, Ordering::SeqCst);
            }
            self.file.sync_data()
        }

        fn write(&self, offset: u64, data: &[u8]) -> io::Result<()> {
            if let Some((_, writes)) = &self.counters {
                writes.fetch_add(1, Ordering::SeqCst);
            }
            self.file.write(offset, data)
        }
    }

    impl Vfs for Counting {
        type File = CountedFile;

        fn exists(&self, name: &str) -> io::Result<bool> {
            self.vfs.exists(name)
        }

        fn open(&self, name: &str) -> io::Result<CountedFile> {
            Ok(self.counted(name, self.vfs.open(name)?))
        }

        fn create_new(&self, name: &str) -> io::Result<CountedFile> {
            Ok(self.counted(name, self.vfs.create_new(name)?))
        }

        fn rename(&self, from: &str, to: &str) -> io::Result<()> {
            self.vfs.rename(from, to)
        }

        fn remove(&self, name: &str) -> io::Result<()> {
            self.vfs.remove(name)
        }

        fn sync_dir(&self) -> io::Result<()> {
            self.vfs.sync_dir()
        }

        fn sync_dir_at(&self, dir: &str) -> io::Result<()> {
            self.vfs.sync_dir_at(dir)
        }

        fn create_dir(&self, name: &str) -> io::Result<()> {
            self.vfs.create_dir(name)
        }

        fn list_files(&self, dir: &str) -> io::Result<Vec<String>> {
            self.vfs.list_files(dir)
        }

        fn list_entries(&self, dir: &str) -> io::Result<Vec<String>> {
            self.vfs.list_entries(dir)
        }

        fn create_file(&self, name: &str) -> io::Result<Box<dyn WriteFile>> {
            self.vfs.create_file(name)
        }

        fn read_file(&self, name: &str) -> io::Result<Box<dyn Read + Send>> {
            self.vfs.read_file(name)
        }

        fn sync_file(&self, name: &str) -> io::Result<()> {
            self.vfs.sync_file(name)
        }

        fn set_read_only(&self, name: &str) -> io::Result<()> {
            self.vfs.set_read_only(name)
        }

        fn lock_dir(&self, name: &str) -> io::Result<DirLock> {
            self.vfs.lock_dir(name)
        }

        fn path(&self, name: &str) -> PathBuf {
            self.vfs.path(name)
        }
    }

    /// Wait until `done` is true. Fail the test after 10 s.
    fn wait_until(done: impl Fn() -> bool) {
        let start = Instant::now();
        while !done() {
            assert!(start.elapsed() < Duration::from_secs(10), "timed out");
            thread::sleep(Duration::from_millis(1));
        }
    }

    /// Begin a transaction that writes `data` for the new oid `oid`, and
    /// vote.
    fn voted<'s>(store: &'s Store, oid: Oid, data: &[u8]) -> Txn<&'s Store> {
        let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
        txn.store(oid, Tid::ZERO, data).unwrap();
        txn.vote().unwrap();
        txn
    }

    /// Finish `txns`, each on a thread of its own, while the test holds the
    /// commit sequencer. The first finish leads a batch of its own and
    /// waits for the sequencer. The others join in the order of `txns`
    /// and wait for the next batch. Call `first` when the first finish
    /// returns. Return the outcomes in the order of `txns`.
    fn finish_in_two_batches<'s>(
        store: &'s Store,
        txns: Vec<Txn<&'s Store>>,
        first: impl FnOnce(),
    ) -> Vec<Result<Finished>> {
        let sequencer = store.lock_sequencer();
        let joined = store.group.joined();
        thread::scope(|scope| {
            let mut finishes = Vec::new();
            for (n, txn) in (1..).zip(txns) {
                finishes.push(scope.spawn(move || txn.finish_after()));
                wait_until(|| store.group.joined() == joined + n);
            }
            drop(sequencer);
            let mut finishes = finishes.into_iter();
            let mut outcomes = vec![finishes.next().unwrap().join().unwrap()];
            first();
            outcomes.extend(finishes.map(|finish| finish.join().unwrap()));
            outcomes
        })
    }

    #[test]
    fn concurrent_finishes_share_one_sync() {
        const FINISHES: u64 = 16;
        let vfs = Counting::default();
        let store = Store::open_in(vfs.clone(), crate::clock::SystemClock).unwrap();
        let txns = (0..FINISHES)
            .map(|_| voted(&store, store.new_oid().unwrap(), b"pickle"))
            .collect();
        let before = vfs.syncs();
        let finished = finish_in_two_batches(&store, txns, || {});
        let syncs = vfs.syncs() - before;

        // One sync for the first batch, one for the others. A file that
        // grows can add one more.
        assert!(
            syncs <= 3,
            "{FINISHES} concurrent finishes took {syncs} syncs of the database",
        );
        // Each finish committed after the one before it.
        let mut previous = finished[0].as_ref().unwrap().previous;
        for finish in &finished {
            let finish = finish.as_ref().unwrap();
            assert_eq!(finish.previous, previous);
            assert!(finish.tid > previous);
            previous = finish.tid;
        }
        assert_eq!(store.last_transaction(), previous);
        assert!(store.locks.is_empty());
    }

    #[test]
    fn a_restore_tid_that_fails_its_check_fails_only_its_member() {
        let store = Store::open_vfs(MemVfs::new()).unwrap();
        // The restore tid is far after the clock, so it is after the first
        // commit also on a slow build.
        let tid = timestamp(std::time::SystemTime::now() + Duration::from_secs(3600));
        let first = store.new_oid().unwrap();
        let txns = vec![
            voted(&store, first, b"first"),
            restore(&store, tid, Oid(100), b"a"),
            restore(&store, tid, Oid(101), b"b"),
            voted(&store, store.new_oid().unwrap(), b"after"),
        ];
        let finished = finish_in_two_batches(&store, txns, || {});

        assert_eq!(finished[1].as_ref().unwrap().tid, tid);
        assert!(matches!(finished[2], Err(ZoeError::Protocol(_))));
        let after = finished[3].as_ref().unwrap();
        assert_eq!(after.previous, tid);
        assert!(after.tid > tid);
        assert!(matches!(
            store.load(Oid(101)),
            Err(ZoeError::NotFound { .. })
        ));
        assert!(store.locks.is_empty());
        // The store does not halt.
        voted(&store, store.new_oid().unwrap(), b"later")
            .finish()
            .unwrap();
    }

    /// Begin a restore at `tid` that restores `data` for `oid`, and vote.
    fn restore<'s>(store: &'s Store, tid: Tid, oid: Oid, data: &[u8]) -> Txn<&'s Store> {
        let mut txn = store.tpc_begin_restore(tid, b"", b"", b"").unwrap();
        txn.restore(oid, Some(data)).unwrap();
        txn.vote().unwrap();
        txn
    }

    /// Begin a deferred restore at `tid` that restores `data` for `oid`,
    /// and vote.
    fn deferred<'s>(store: &'s Store, tid: Tid, oid: Oid, data: &[u8]) -> Txn<&'s Store> {
        let mut txn = store
            .tpc_begin_restore_deferred(tid, b"", b"", b"")
            .unwrap();
        txn.restore(oid, Some(data)).unwrap();
        txn.vote().unwrap();
        txn
    }

    /// An observer that keeps the size of each synced batch.
    #[derive(Default)]
    struct Batches(std::sync::Mutex<Vec<usize>>);

    impl Observer for Batches {
        fn batch_synced(&self, commits: usize) {
            self.0.lock().unwrap().push(commits);
        }
    }

    #[test]
    fn a_batch_of_deferred_restores_does_not_sync() {
        let vfs = Counting::default();
        let store = Store::open_in(vfs.clone(), crate::clock::SystemClock).unwrap();
        let batches = Arc::new(Batches::default());
        assert!(store.set_observer(batches.clone()));
        let before = vfs.syncs();
        for n in 1..=10 {
            deferred(&store, Tid(n), Oid(n), b"pickle")
                .finish()
                .unwrap();
        }
        assert_eq!(vfs.syncs(), before);
        assert_eq!(store.last_transaction(), Tid(10));
        assert_eq!(store.load(Oid(10)).unwrap(), (b"pickle".to_vec(), Tid(10)));
        // A batch without a sync and `Store::sync` call no observer method.
        store.sync().unwrap();
        assert!(vfs.syncs() > before);
        assert!(batches.0.lock().unwrap().is_empty());
        assert_eq!(store.last_transaction(), Tid(10));
    }

    #[test]
    fn a_batch_with_a_durable_member_makes_its_deferred_members_durable() {
        let vfs = MemVfs::new();
        let store = Store::open_vfs(vfs.clone()).unwrap();
        // The restore tid is far after the clock, so the normal commit
        // after it gets a later tid.
        let tid = timestamp(std::time::SystemTime::now() + Duration::from_secs(3600));
        // The first restore commits in a batch of its own, without a sync.
        // The second one shares its batch with a normal commit.
        let txns = vec![
            deferred(&store, Tid(5), Oid(100), b"alone"),
            deferred(&store, tid, Oid(101), b"shared"),
            voted(&store, store.new_oid().unwrap(), b"durable"),
        ];
        let finished = finish_in_two_batches(&store, txns, || {});
        let last = finished[2].as_ref().unwrap().tid;
        assert!(last > tid);

        let image = vfs.crash_now(|_| 0);
        let reopened = Store::open_vfs(image).unwrap();
        assert_eq!(reopened.last_transaction(), last);
        assert_eq!(reopened.load(Oid(100)).unwrap().1, Tid(5));
        assert_eq!(reopened.load(Oid(101)).unwrap().1, tid);
        drop(store);
    }

    #[test]
    fn a_copy_of_an_error_keeps_its_message() {
        let backend = ZoeError::Backend(Box::new(io::Error::other("disk full")));
        assert_eq!(copy(&backend).to_string(), "backend error: disk full");
        let full = ZoeError::Io(io::Error::new(io::ErrorKind::StorageFull, "disk full"));
        let copied = copy(&full);
        assert!(matches!(&copied, ZoeError::Io(e) if e.kind() == io::ErrorKind::StorageFull));
        assert_eq!(copied.to_string(), full.to_string());
        let corrupt = ZoeError::CorruptTxn { tid: Tid(7) };
        assert_eq!(
            copy(&corrupt).to_string(),
            "backend error: corrupt transaction row: tid Tid(7)"
        );
    }

    #[test]
    fn every_crash_image_keeps_each_acknowledged_batch() {
        // The record of the filesystem starts after the store is created.
        let created = MemVfs::new();
        drop(Store::open_vfs(created.clone()).unwrap());
        let vfs = created.durable_copy();
        let store = Store::open_vfs(vfs.clone()).unwrap();
        let oids: Vec<Oid> = (0..4).map(|_| store.new_oid().unwrap()).collect();
        let txns = oids
            .iter()
            .map(|&oid| voted(&store, oid, b"pickle"))
            .collect();
        // The first batch is acknowledged before the second one ends.
        let mark = || vfs.mark("first acknowledged");
        for finish in finish_in_two_batches(&store, txns, mark) {
            finish.unwrap();
        }
        vfs.mark("acknowledged");

        // A crash does not close the store, so the images end here.
        let images = vfs.crash_images();
        drop(store);
        assert!(images.iter().any(|image| image.is_after("acknowledged")));
        let all: BTreeSet<Oid> = oids.iter().copied().collect();
        let first: BTreeSet<Oid> = oids[..1].iter().copied().collect();
        for image in &images {
            let store = Store::open_vfs(image.vfs())
                .unwrap_or_else(|e| panic!("{image:?} does not open: {e}"));
            let found: BTreeSet<Oid> = oids
                .iter()
                .copied()
                .filter(|&oid| store.load(oid).is_ok())
                .collect();
            // A batch is whole or absent, and no acknowledged commit is
            // lost.
            if image.is_after("acknowledged") {
                assert_eq!(found, all, "{image:?}");
            } else if image.is_after("first acknowledged") {
                assert!(found == first || found == all, "{image:?}: {found:?}");
            } else {
                assert!(
                    found.is_empty() || found == first || found == all,
                    "{image:?}: {found:?}"
                );
            }
        }
    }
}
