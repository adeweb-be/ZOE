//! The invalidations of a storage.
//!
//! After each commit, every other client of the storage gets
//! `invalidateTransaction(tid, oids)`. The client that committed gets
//! `info`, then the reply of its `tpc_finish`, and no invalidation. Each
//! client gets these messages in commit order. So the tids that a client
//! sees, in the replies of its own commits and in the invalidations,
//! always increase. The cache of a stock client requires this.
//!
//! `finish_after` gives each commit the tid of the commit before it. A
//! commit waits in the hub until the hub publishes the commit before it.
//! The session that publishes a commit also publishes the commits that wait
//! for it. So no thread waits for the commit of another thread. A finish that
//! fails gives no tid, and no commit waits for it.
//!
//! The hub never waits for room in the queue of a connection: a frame that
//! does not fit closes the connection, and the client connects again and
//! asks `getInvalidations`. So a slow or dead client never stops a commit or
//! the other clients. The hub holds its mutex while it puts frames in the
//! queues, never while a frame goes out to the network.
//!
//! A `loadBefore` reply shows only the commits that the hub published, and
//! it takes its place in the order of its connection under the mutex of
//! the hub, when the hub published no commit since the read. So the reply
//! comes after the invalidation of each commit that it shows, and before
//! the invalidation of each later commit.
//!
//! `register`, `lastTransaction` and `getInvalidations` read the last
//! published tid under the mutex, and their replies take their places
//! later, outside it. So each of these replies comes after the
//! invalidation of each commit up to its tid. The invalidation of a later
//! commit can come before it. A stock client applies only the
//! invalidations after the tid of these replies, so it accepts that.
//!
//! Every commit of a store passes through the hub of that store exactly
//! once, and each store has one hub. A commit that bypasses the hub never
//! publishes its tid, and each later commit waits for it forever.
//!
//! `send_to_all` puts a frame that is not a commit, the `info` after a
//! pack, in the queue of every connection, under the same mutex. So it
//! takes its place among the frames of the commits.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use zoe_core::Tid;

use super::outbox::{Frame, Outbox, Place, Pushed, Room};

/// The id of a connection in the hub.
pub(crate) type ClientId = u64;

/// What the other clients of the storage get for one commit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Others {
    /// This frame: the `invalidateTransaction` of the commit. Its list of
    /// oids is empty for a commit that wrote no oid, as from ZEO.
    Send(Vec<u8>),
    /// A close of their connections, because the oids of the commit are
    /// not known. Each client verifies its cache when it connects again.
    Close,
}

/// One commit that the hub publishes.
pub(crate) struct Commit {
    /// The tid of the commit just before this one, in commit order.
    pub(crate) previous: Tid,
    /// The tid of the commit.
    pub(crate) tid: Tid,
    /// The connection that committed, when it is in the hub.
    pub(crate) origin: Option<ClientId>,
    /// The outbox of the connection that committed.
    pub(crate) outbox: Outbox,
    /// The frames for the connection that committed: `info`, then the
    /// reply of `tpc_finish`.
    pub(crate) own: Vec<Vec<u8>>,
    /// What the other clients get.
    pub(crate) others: Others,
}

/// A commit that is not published yet. Dropping it publishes it, also
/// when the thread panics before. So no later commit waits for it
/// forever.
pub(crate) struct Publication<'a> {
    hub: &'a Invalidations,
    commit: Option<Commit>,
}

impl Publication<'_> {
    /// Return the commit, to change its frames before the publication.
    pub(crate) fn commit(&mut self) -> &mut Commit {
        self.commit
            .as_mut()
            .expect("a publication holds its commit")
    }
}

impl Drop for Publication<'_> {
    fn drop(&mut self) {
        if let Some(commit) = self.commit.take() {
            self.hub.publish(commit);
        }
    }
}

/// The publication of the commits of one storage to its connections, in
/// commit order. All the connections of the storage share it.
pub(crate) struct Invalidations {
    state: Mutex<State>,
}

/// The state of the hub. Under its mutex, only a `loadBefore` place takes
/// another lock: the lock of the outbox of its connection, which no holder
/// keeps while it waits for the hub.
struct State {
    /// The tid of the last published commit.
    published: Tid,
    /// The commits that wait for the commit before them, by the tid of
    /// that commit.
    waiting: HashMap<Tid, Commit>,
    /// The outbox of each connection that registered.
    clients: BTreeMap<ClientId, Outbox>,
    /// The id of the next connection.
    next: ClientId,
}

impl Invalidations {
    /// Start with `last` published: the last tid of the store when the
    /// server opens it.
    pub(crate) fn new(last: Tid) -> Self {
        Self {
            state: Mutex::new(State {
                published: last,
                waiting: HashMap::new(),
                clients: BTreeMap::new(),
                next: 0,
            }),
        }
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Return the tid of the last published commit.
    pub(crate) fn published(&self) -> Tid {
        self.lock().published
    }

    /// Add a connection with its outbox. Return its id and the tid of the
    /// last published commit. The connection gets the frames of each later
    /// commit, and of no earlier one.
    pub(crate) fn subscribe(&self, outbox: Outbox) -> (ClientId, Tid) {
        let mut state = self.lock();
        let id = state.next;
        state.next += 1;
        state.clients.insert(id, outbox);
        (id, state.published)
    }

    /// Return the most frames that wait in the hub queue of one
    /// connection.
    pub(crate) fn deepest_queue(&self) -> usize {
        let state = self.lock();
        state
            .clients
            .values()
            .map(Outbox::hub_len)
            .max()
            .unwrap_or(0)
    }

    /// Remove the connection `id`.
    pub(crate) fn unsubscribe(&self, id: ClientId) {
        self.lock().clients.remove(&id);
    }

    /// Take the next place with `room` when the hub published no commit
    /// after `seen`. Otherwise, give the room back.
    ///
    /// A reader reads the last published tid, then the store, then takes
    /// its place here. With a place, its reply goes out before the
    /// invalidation of each later commit. Without one, a commit came in
    /// between, and the reader reads again.
    pub(crate) fn place_since(&self, seen: Tid, room: Room) -> Result<Place, Room> {
        let state = self.lock();
        if state.published == seen {
            Ok(room.place())
        } else {
            Err(room)
        }
    }

    /// Run `read` with the last published tid, then take the next place
    /// with `room`, both under the mutex of the hub. So the hub publishes
    /// no commit between the read and the place. The hub waits for the
    /// read, so a reader calls this only after `place_since` failed a few
    /// times.
    pub(crate) fn read_placed<T>(&self, room: Room, read: impl FnOnce(Tid) -> T) -> (T, Place) {
        let state = self.lock();
        let value = read(state.published);
        (value, room.place())
    }

    /// Return the publication of `commit`. Dropping the publication
    /// publishes the commit.
    pub(crate) fn prepare(&self, commit: Commit) -> Publication<'_> {
        Publication {
            hub: self,
            commit: Some(commit),
        }
    }

    /// Publish `commit` after the commit before it, and then each commit
    /// that waits for it.
    pub(crate) fn publish(&self, commit: Commit) {
        let overflows = {
            let mut state = self.lock();
            debug_assert!(commit.previous >= state.published, "a commit came twice");
            state.waiting.insert(commit.previous, commit);
            let mut overflows = 0;
            loop {
                let published = state.published;
                let Some(commit) = state.waiting.remove(&published) else {
                    break;
                };
                overflows += state.deliver(commit);
            }
            overflows
        };
        // Write the log line after the release of the mutex.
        log_overflows(overflows);
    }

    /// Put `frame` in the hub queue of each connection, and do not wait. The
    /// frame goes out after the frames of each published commit, and before
    /// the frames of each later commit. A full queue closes its connection,
    /// as for a commit.
    pub(crate) fn send_to_all(&self, frame: Vec<u8>) {
        let overflows = {
            let mut state = self.lock();
            let mut gone = Vec::new();
            let overflows = state.push_all(&Arc::new(frame), None, &mut gone);
            for id in gone {
                state.clients.remove(&id);
            }
            overflows
        };
        log_overflows(overflows);
    }
}

/// Write a log line for the connections that a full queue closed.
fn log_overflows(overflows: usize) {
    if overflows > 0 {
        tracing::warn!(
            connections = overflows,
            "closing connections: the client queue of each is full"
        );
    }
}

impl State {
    /// Put the frames of `commit` in the outboxes, and mark the commit
    /// published. Return the number of connections that a full outbox
    /// closed.
    fn deliver(&mut self, commit: Commit) -> usize {
        let Commit {
            tid,
            origin,
            outbox,
            own,
            others,
            ..
        } = commit;
        let mut gone = Vec::new();
        let mut overflows = 0;
        let invalidation = match others {
            Others::Send(bytes) => Some(Arc::new(bytes)),
            Others::Close => {
                for (&id, client) in &self.clients {
                    if Some(id) != origin {
                        client.close();
                        gone.push(id);
                    }
                }
                None
            }
        };
        if let Some(frame) = invalidation {
            overflows += self.push_all(&frame, origin, &mut gone);
        }
        for frame in own {
            let pushed = outbox.push(Arc::new(frame));
            if pushed == Pushed::Overflow {
                overflows += 1;
            }
            if pushed != Pushed::Queued {
                gone.extend(origin);
                break;
            }
        }
        for id in gone {
            self.clients.remove(&id);
        }
        self.published = tid;
        overflows
    }

    /// Put `frame` in the hub queue of each connection but `origin`. Add
    /// each connection that the push closed, or found closed, to `gone`.
    /// Return the number of queues that were full.
    fn push_all(&self, frame: &Frame, origin: Option<ClientId>, gone: &mut Vec<ClientId>) -> usize {
        let mut overflows = 0;
        for (&id, client) in &self.clients {
            if Some(id) == origin {
                continue;
            }
            match client.push(Arc::clone(frame)) {
                Pushed::Queued => {}
                Pushed::Overflow => {
                    overflows += 1;
                    gone.push(id);
                }
                Pushed::Gone => gone.push(id),
            }
        }
        overflows
    }
}

#[cfg(test)]
mod tests {
    use std::panic::{AssertUnwindSafe, catch_unwind};

    use super::super::outbox::{Closing, Inbox};
    use super::*;

    /// A connection of the tests: its id in the hub, its outbox, the
    /// frames that wait in it and its close.
    struct Client {
        id: ClientId,
        outbox: Outbox,
        frames: Inbox,
        closing: Closing,
    }

    impl Client {
        /// Add a connection whose hub queue holds `size` frames to `hub`.
        fn new(hub: &Invalidations, size: usize) -> Self {
            let (outbox, frames, closing) = Outbox::new(10, size);
            let (id, _) = hub.subscribe(outbox.clone());
            Self {
                id,
                outbox,
                frames,
                closing,
            }
        }

        /// Take the frames that wait, as text.
        fn take(&mut self) -> Vec<String> {
            let mut taken = Vec::new();
            while let Some(frame) = self.frames.try_recv() {
                taken.push(String::from_utf8(frame.to_vec()).unwrap());
            }
            taken
        }

        /// The commit `tid` of this connection, after `previous`, with
        /// the reply `reply <tid>` and the invalidation `inv <tid>`.
        fn commit(&self, previous: u64, tid: u64) -> Commit {
            Commit {
                previous: Tid(previous),
                tid: Tid(tid),
                origin: Some(self.id),
                outbox: self.outbox.clone(),
                own: vec![format!("info {tid}").into(), format!("reply {tid}").into()],
                others: Others::Send(format!("inv {tid}").into()),
            }
        }
    }

    fn text(frames: &[&str]) -> Vec<String> {
        frames.iter().map(|frame| (*frame).to_owned()).collect()
    }

    #[test]
    fn commits_go_out_in_commit_order_whatever_the_order_of_their_finish() {
        let hub = Invalidations::new(Tid(1));
        let (mut a, mut b, mut c) = (
            Client::new(&hub, 10),
            Client::new(&hub, 10),
            Client::new(&hub, 10),
        );
        // The commit 3 comes first: it waits for the commit 2.
        hub.publish(a.commit(2, 3));
        assert!(a.take().is_empty() && b.take().is_empty() && c.take().is_empty());
        assert_eq!(hub.published(), Tid(1));

        hub.publish(b.commit(1, 2));
        assert_eq!(hub.published(), Tid(3));
        // The client that commits gets its own frames and no invalidation.
        assert_eq!(a.take(), text(&["inv 2", "info 3", "reply 3"]));
        assert_eq!(b.take(), text(&["info 2", "reply 2", "inv 3"]));
        assert_eq!(c.take(), text(&["inv 2", "inv 3"]));
    }

    #[test]
    fn a_connection_gets_only_the_commits_after_its_subscription() {
        let hub = Invalidations::new(Tid(5));
        let mut writer = Client::new(&hub, 10);
        hub.publish(writer.commit(5, 6));
        let (outbox, mut frames, _closing) = Outbox::new(10, 10);
        assert_eq!(hub.subscribe(outbox).1, Tid(6));
        hub.publish(writer.commit(6, 7));
        assert_eq!(
            String::from_utf8(frames.try_recv().unwrap().to_vec()).unwrap(),
            "inv 7"
        );
        assert!(frames.try_recv().is_none());

        // A connection that leaves gets nothing more.
        let mut gone = Client::new(&hub, 10);
        hub.unsubscribe(gone.id);
        hub.publish(writer.commit(7, 8));
        assert!(gone.take().is_empty());
        assert_eq!(writer.take().len(), 6);
    }

    #[test]
    fn the_deepest_queue_is_the_longest_hub_queue_of_one_connection() {
        let hub = Invalidations::new(Tid(0));
        assert_eq!(hub.deepest_queue(), 0);
        let mut a = Client::new(&hub, 10);
        let _b = Client::new(&hub, 10);
        hub.publish(a.commit(0, 1));
        hub.send_to_all(b"info".to_vec());
        // The committer holds `info`, its reply and the frame for all. The
        // other connection holds the invalidation and the frame for all.
        assert_eq!(hub.deepest_queue(), 3);
        assert_eq!(a.take().len(), 3);
        assert_eq!(hub.deepest_queue(), 2);
    }

    #[test]
    fn a_full_outbox_closes_its_connection_and_stops_no_one() {
        let hub = Invalidations::new(Tid(0));
        let mut writer = Client::new(&hub, 100);
        let mut slow = Client::new(&hub, 2);
        let mut other = Client::new(&hub, 100);
        for tid in 1..=3 {
            hub.publish(writer.commit(tid - 1, tid));
        }
        // Two frames fit. The third closes the connection.
        assert!(slow.closing.is_closed());
        assert!(!other.closing.is_closed() && !writer.closing.is_closed());
        assert_eq!(slow.take(), text(&["inv 1", "inv 2"]));
        assert_eq!(other.take(), text(&["inv 1", "inv 2", "inv 3"]));

        // The closed connection left the hub.
        hub.publish(writer.commit(3, 4));
        assert!(slow.take().is_empty());
        assert_eq!(other.take(), text(&["inv 4"]));
        assert_eq!(writer.take().len(), 8);
    }

    #[test]
    fn the_own_frames_of_a_full_outbox_close_it() {
        let hub = Invalidations::new(Tid(0));
        let mut writer = Client::new(&hub, 1);
        let mut other = Client::new(&hub, 10);
        hub.publish(writer.commit(0, 1));
        assert!(writer.closing.is_closed());
        assert_eq!(writer.take(), text(&["info 1"]));
        assert_eq!(other.take(), text(&["inv 1"]));
        // The hub publishes the commit all the same, and the closed connection
        // left the hub.
        assert_eq!(hub.published(), Tid(1));
        hub.publish(other.commit(1, 2));
        assert!(writer.take().is_empty());
        assert_eq!(other.take(), text(&["info 2", "reply 2"]));
    }

    #[test]
    fn a_reader_takes_its_place_only_when_no_commit_came_after_its_read() {
        let hub = Invalidations::new(Tid(0));
        let mut writer = Client::new(&hub, 10);
        let mut reader = Client::new(&hub, 10);
        let seen = hub.published();
        let place = hub
            .place_since(seen, reader.outbox.blocking_room())
            .unwrap();
        // A commit after the place goes out after the reply.
        hub.publish(writer.commit(0, 1));
        assert!(reader.take().is_empty());
        assert!(reader.outbox.send_at(place, Some(b"load".to_vec())));
        assert_eq!(reader.take(), text(&["load", "inv 1"]));
        // A commit between the read and the place: the reader reads again.
        hub.publish(writer.commit(1, 2));
        let room = hub
            .place_since(Tid(1), reader.outbox.blocking_room())
            .unwrap_err();
        let place = hub.place_since(Tid(2), room).unwrap();
        assert!(place.fill(None));
        assert_eq!(writer.take().len(), 4);
    }

    #[test]
    fn a_read_under_the_mutex_takes_its_place_before_a_later_commit() {
        let hub = Invalidations::new(Tid(4));
        let mut writer = Client::new(&hub, 10);
        let mut reader = Client::new(&hub, 10);
        let (seen, place) = hub.read_placed(reader.outbox.blocking_room(), |published| published);
        assert_eq!(seen, Tid(4));
        hub.publish(writer.commit(4, 5));
        assert!(reader.outbox.send_at(place, Some(b"load".to_vec())));
        assert_eq!(reader.take(), text(&["load", "inv 5"]));
        assert_eq!(writer.take(), text(&["info 5", "reply 5"]));
    }

    #[test]
    fn a_frame_for_all_takes_its_place_among_the_commits() {
        let hub = Invalidations::new(Tid(0));
        let (mut writer, mut other) = (Client::new(&hub, 10), Client::new(&hub, 10));
        let mut full = Client::new(&hub, 1);
        hub.publish(writer.commit(0, 1));
        // The commit 3 waits for the commit 2. The frame goes out before
        // both.
        hub.publish(writer.commit(2, 3));
        hub.send_to_all(b"all".to_vec());
        hub.publish(other.commit(1, 2));
        assert_eq!(
            writer.take(),
            text(&["info 1", "reply 1", "all", "inv 2", "info 3", "reply 3"])
        );
        assert_eq!(
            other.take(),
            text(&["inv 1", "all", "info 2", "reply 2", "inv 3"])
        );
        // A full queue closes its connection, and the connection leaves
        // the hub.
        assert!(full.closing.is_closed());
        assert_eq!(full.take(), text(&["inv 1"]));
        hub.send_to_all(b"again".to_vec());
        assert!(full.take().is_empty());
        assert_eq!(other.take(), text(&["again"]));
    }

    #[test]
    fn a_commit_with_unknown_oids_closes_the_others() {
        let hub = Invalidations::new(Tid(0));
        let (mut writer, other) = (Client::new(&hub, 10), Client::new(&hub, 10));
        let mut commit = writer.commit(0, 1);
        commit.others = Others::Close;
        hub.publish(commit);
        assert!(other.closing.is_closed());
        assert!(!writer.closing.is_closed());
        assert_eq!(writer.take(), text(&["info 1", "reply 1"]));
        hub.publish(writer.commit(1, 2));
        assert_eq!(writer.take(), text(&["info 2", "reply 2"]));
    }

    #[test]
    fn a_publication_publishes_its_commit_also_on_a_panic() {
        let hub = Invalidations::new(Tid(0));
        let mut writer = Client::new(&hub, 10);
        let mut publication = hub.prepare(writer.commit(0, 1));
        publication.commit().own.pop();
        drop(publication);
        assert_eq!(writer.take(), text(&["info 1"]));

        let panicked = catch_unwind(AssertUnwindSafe(|| {
            let _publication = hub.prepare(writer.commit(1, 2));
            panic!("a read failed");
        }));
        assert!(panicked.is_err());
        assert_eq!(hub.published(), Tid(2));
        hub.publish(writer.commit(2, 3));
        assert_eq!(hub.published(), Tid(3));
    }

    #[test]
    fn concurrent_publications_keep_the_commit_order() {
        const COMMITS: u64 = 400;
        const THREADS: u64 = 8;
        let hub = Invalidations::new(Tid(0));
        let mut observer = Client::new(&hub, COMMITS as usize);
        let writers: Vec<Client> = (0..THREADS).map(|_| Client::new(&hub, 1_000)).collect();
        std::thread::scope(|scope| {
            for (n, writer) in writers.iter().enumerate() {
                let hub = &hub;
                scope.spawn(move || {
                    // Each thread publishes its commits from the last one
                    // down, so most of them wait for another thread.
                    for tid in (1..=COMMITS).rev().filter(|tid| tid % THREADS == n as u64) {
                        hub.publish(writer.commit(tid - 1, tid));
                    }
                });
            }
        });
        let expected: Vec<String> = (1..=COMMITS).map(|tid| format!("inv {tid}")).collect();
        assert_eq!(observer.take(), expected);
        assert_eq!(hub.published(), Tid(COMMITS));
        // Each writer sees the tids in order: its own replies among the
        // invalidations of the others.
        for mut writer in writers {
            let tids: Vec<u64> = writer
                .take()
                .iter()
                .filter(|frame| !frame.starts_with("info"))
                .map(|frame| frame.split(' ').nth(1).unwrap().parse().unwrap())
                .collect();
            assert_eq!(tids, (1..=COMMITS).collect::<Vec<_>>());
        }
    }
}
