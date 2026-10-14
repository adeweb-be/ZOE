//! The frames that wait for one connection.
//!
//! A connection has two queues. The calls of the connection put their
//! replies in the session queue. A call takes room in that queue before it
//! runs, so it never waits to put its reply. The hub of the invalidations
//! puts its messages in the hub queue: each invalidation, the `info` of a
//! pack, and the frames of each commit for its own client. A message is
//! one frame, except for a commit: its `info` and the reply of its
//! `tpc_finish` are one message of two frames. So a hub queue of one
//! message holds them. The hub never waits: a hub queue that is full closes
//! the connection. So a client that downloads a blob, and keeps the session
//! queue full, does not stop a commit.
//!
//! Each frame takes a place in the order of the connection when it goes
//! into its queue: one counter gives the places of both queues. The frames
//! of one message share one place. The writer task sends the frames
//! strictly in the order of their places, and the frames of one place in
//! their order. So the client gets the frames in the order in which the
//! server queued them, as from one queue. The calls of a connection run on many threads. A frame
//! of the session takes its place and goes into the session queue under one
//! lock, so that queue holds its frames in the order of their places.
//!
//! A frame can take its place before it exists: `loadBefore` takes its
//! place before an invalidation of a commit that its read did not see. The
//! place goes into the session queue at once, and the call fills it later.
//! The writer waits for that frame. A place that gets no frame holds no
//! frame, and the writer skips it.
//!
//! A frame of the session counts from the time it goes into the queue
//! until the writer has written it. A call starts only while at most
//! `WAITING` bytes of these frames wait, and a download sends its next
//! frame only then. A frame larger than `WAITING` goes out whole. The size
//! of a reply is known only after its call ran, so a window limits the
//! calls of the read pool that run at once (`Turn`). The window of a new
//! connection holds one call. Each frame of the session that the writer
//! has written, while at most `WAITING` bytes of other frames wait, opens
//! the window by one call, up to `QUEUE`. A frame that goes into the queue
//! while more than `WAITING` bytes wait closes it to one call.
//!
//! So the frames of the session that wait for one client, the frame that
//! the writer sends included, hold at most `WAITING` bytes beyond one
//! frame for each call of its window. The writer has written a frame when
//! the socket took it, whether the client reads or not. A large frame
//! fills the buffers of the socket, so large replies open the window only
//! for a client that reads. A small frame fits in the buffers, so small
//! replies open it also for a client that does not read. Limit: a client
//! that got small replies and then stops reading can hold one large reply
//! for each call of its window, up to `QUEUE`, once, until the window
//! closes.
//!
//! The waits on the count end when the writer is gone. A frame that a call
//! puts after that can stay counted, so only the end of the writer ends
//! these waits.

use std::collections::VecDeque;
use std::ops::Deref;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use tokio::sync::mpsc::{self, OwnedPermit, error::TrySendError};
use tokio::sync::{Semaphore, oneshot, watch};

use super::QUEUE;
use crate::runtime::block_on;

/// The most bytes of the frames of the session that wait for one client
/// when a call starts or a download sends its next frame.
pub(crate) const WAITING: usize = 1 << 20;

/// The bytes of one message. All the connections that get the message
/// share them.
pub(crate) type Frame = Arc<Vec<u8>>;

/// A frame that goes out. A frame of the session holds its bytes in the
/// count of its connection until it drops.
pub(crate) struct Outgoing {
    frame: Frame,
    charge: Option<Charge>,
}

impl Outgoing {
    /// A frame of the hub. It does not count.
    fn hub(frame: Frame) -> Self {
        Self {
            frame,
            charge: None,
        }
    }

    /// Tell that the writer wrote the frame. Its bytes no longer count.
    /// When at most `WAITING` bytes of other frames wait, the window of
    /// the read pool opens by one call.
    pub(crate) fn sent(mut self) {
        if let Some(charge) = self.charge.as_mut() {
            charge.sent = true;
        }
    }
}

impl Deref for Outgoing {
    type Target = Vec<u8>;

    fn deref(&self) -> &Vec<u8> {
        &self.frame
    }
}

impl std::fmt::Debug for Outgoing {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.frame.fmt(f)
    }
}

/// The count of the frames of the session that wait for one connection,
/// and the window of its calls of the read pool.
#[derive(Debug, Clone, Copy)]
struct Count {
    /// The bytes of the frames of the session that wait, the frame that
    /// the writer sends included.
    waiting: usize,
    /// The most calls of the read pool that run at once: 1 to `QUEUE`.
    window: usize,
    /// The calls of the read pool that hold a turn.
    reading: usize,
}

/// The count of one connection. All its outboxes share it.
#[derive(Debug)]
struct Flow(watch::Sender<Count>);

impl Flow {
    fn new() -> Arc<Self> {
        Arc::new(Self(watch::Sender::new(Count {
            waiting: 0,
            window: 1,
            reading: 0,
        })))
    }

    /// Count a frame of `bytes` that goes into the session queue. When
    /// more than `WAITING` bytes wait, close the window to one call.
    fn charge(self: &Arc<Self>, bytes: usize) -> Charge {
        self.0.send_modify(|count| {
            if count.waiting > WAITING {
                count.window = 1;
            }
            count.waiting += bytes;
        });
        Charge {
            flow: Arc::clone(self),
            bytes,
            sent: false,
        }
    }

    /// Return `frame` as a frame of the session, counted.
    fn charged(self: &Arc<Self>, frame: Vec<u8>) -> Outgoing {
        Outgoing {
            charge: Some(self.charge(frame.len())),
            frame: Arc::new(frame),
        }
    }

    /// Wait while more than `WAITING` bytes wait.
    async fn below(&self) {
        let mut count = self.0.subscribe();
        // `self` holds the sender, so the wait cannot fail.
        let _ = count.wait_for(|count| count.waiting <= WAITING).await;
    }

    /// Wait for a turn of the read pool: while more than `WAITING` bytes
    /// wait, and while the calls of the window hold their turns.
    async fn turn(self: &Arc<Self>) -> Turn {
        let mut count = self.0.subscribe();
        let free = |count: &Count| count.waiting <= WAITING && count.reading < count.window;
        let _ = count.wait_for(free).await;
        // The check and the count are two steps. That is correct because
        // only the reader of the connection takes turns, one at a time.
        self.0.send_modify(|count| count.reading += 1);
        Turn(Arc::clone(self))
    }
}

/// The bytes of one frame of the session in the count of its connection.
/// Dropping it takes them out.
struct Charge {
    flow: Arc<Flow>,
    bytes: usize,
    /// The writer wrote the frame.
    sent: bool,
}

impl Drop for Charge {
    fn drop(&mut self) {
        self.flow.0.send_modify(|count| {
            count.waiting -= self.bytes;
            if self.sent && count.waiting <= WAITING {
                count.window = (count.window + 1).min(QUEUE);
            }
        });
    }
}

/// The turn of one call of the read pool in the window of its connection.
/// Dropping it gives the turn back.
pub(crate) struct Turn(Arc<Flow>);

impl Drop for Turn {
    fn drop(&mut self) {
        self.0.0.send_modify(|count| count.reading -= 1);
    }
}

/// What a place holds.
enum Slot {
    /// The frame, or none for an empty place.
    Ready(Option<Outgoing>),
    /// The frames of one message of the hub, in their order.
    Message(Vec<Frame>),
    /// A frame that the call gives later. A closed channel is an empty
    /// place.
    Pending(oneshot::Receiver<Outgoing>),
}

/// A place in the order of its connection and what it holds.
struct Placed {
    place: u64,
    slot: Slot,
}

/// The two queues of one connection, and the signal that closes it.
#[derive(Clone)]
pub(crate) struct Outbox {
    /// The replies of the calls. A call takes room before it runs.
    session: mpsc::Sender<Placed>,
    /// The messages of the hub. The hub never waits.
    hub: mpsc::Sender<Placed>,
    /// The place of the next frame.
    next: Arc<AtomicU64>,
    /// A frame of the session takes its place and goes into the session
    /// queue under this lock.
    order: Arc<Mutex<()>>,
    /// The count of the frames of the session that wait.
    flow: Arc<Flow>,
    closing: Arc<watch::Sender<bool>>,
}

/// What a push into the hub queue did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Pushed {
    /// The message waits in the queue.
    Queued,
    /// The queue was full. The push closed the connection.
    Overflow,
    /// The connection is closed.
    Gone,
}

impl Outbox {
    /// Return the queues of a connection: a session queue of at most
    /// `session` frames and a hub queue of at most `hub` messages. Also
    /// return the receiving side for the writer task, and the signal of the
    /// close. A size of 0 holds 1 entry. A size above the bound of tokio
    /// holds that bound.
    pub(crate) fn new(session: usize, hub: usize) -> (Self, Inbox, Closing) {
        let size = |n: usize| n.clamp(1, Semaphore::MAX_PERMITS);
        let (session, from_session) = mpsc::channel(size(session));
        let (hub, from_hub) = mpsc::channel(size(hub));
        let (closing, signal) = watch::channel(false);
        let outbox = Self {
            session,
            hub,
            next: Arc::new(AtomicU64::new(0)),
            order: Arc::new(Mutex::new(())),
            flow: Flow::new(),
            closing: Arc::new(closing),
        };
        let inbox = Inbox {
            session: from_session,
            hub: from_hub,
            next: 0,
            from_session: None,
            from_hub: None,
            pending: None,
            rest: VecDeque::new(),
            session_open: true,
            hub_open: true,
        };
        (outbox, inbox, Closing(signal))
    }

    /// Take the next place in the order of the connection.
    fn next_place(&self) -> u64 {
        self.next.fetch_add(1, Ordering::Relaxed)
    }

    /// Wait for room for one frame in the session queue. When the
    /// connection is closed, return a room that holds nothing.
    pub(crate) async fn room(&self) -> Room {
        Room {
            permit: self.session.clone().reserve_owned().await.ok(),
            outbox: self.clone(),
        }
    }

    /// Wait while more than `WAITING` bytes of frames of the session wait.
    /// Stop when the writer is gone: then a room holds nothing.
    pub(crate) async fn below(&self) {
        tokio::select! {
            biased;
            () = self.session.closed() => {}
            () = self.flow.below() => {}
        }
    }

    /// Wait for a turn in the window of the calls of the read pool, while
    /// at most `WAITING` bytes of frames of the session wait. Return `None`
    /// when the writer is gone.
    pub(crate) async fn turn(&self) -> Option<Turn> {
        tokio::select! {
            biased;
            () = self.session.closed() => None,
            turn = self.flow.turn() => Some(turn),
        }
    }

    /// Wait for room for one frame in the session queue, on a thread that
    /// is not a thread of the async runtime.
    pub(crate) fn blocking_room(&self) -> Room {
        block_on(self.room())
    }

    /// Take room and put a placeholder in the session queue at the next
    /// place. Wait while the session queue is full.
    #[cfg(test)]
    pub(crate) fn place(&self) -> Place {
        self.blocking_room().place()
    }

    /// Put a frame at `place`, or leave the place empty. Return false when
    /// the connection is closed.
    #[cfg(test)]
    pub(crate) fn send_at(&self, place: Place, frame: Option<Vec<u8>>) -> bool {
        place.fill(frame)
    }

    /// Put a frame of the session at the next place. Wait while more than
    /// `WAITING` bytes of frames of the session wait, then while the
    /// session queue is full. Return false when the connection is closed.
    ///
    /// Call this only on a thread that is not a thread of the async runtime.
    pub(crate) fn send(&self, frame: Vec<u8>) -> bool {
        block_on(self.below());
        self.blocking_room().put(Some(frame))
    }

    /// Put a frame of the hub at the next place, and do not wait. When the
    /// hub queue is full, close the connection.
    ///
    /// The hub calls this under its mutex, so its frames take their places
    /// in the order of the queue.
    pub(crate) fn push(&self, frame: Frame) -> Pushed {
        self.push_slot(Slot::Ready(Some(Outgoing::hub(frame))))
    }

    /// Put the frames of one message of the hub at the next place, and do
    /// not wait. The frames count as one message: they go in together, or
    /// the queue is full and the push closes the connection.
    ///
    /// The hub calls this under its mutex, as `push`.
    pub(crate) fn push_message(&self, frames: Vec<Frame>) -> Pushed {
        self.push_slot(Slot::Message(frames))
    }

    /// Put `slot` in the hub queue at the next place, and do not wait.
    /// When the hub queue is full, close the connection.
    fn push_slot(&self, slot: Slot) -> Pushed {
        let placed = Placed {
            place: self.next_place(),
            slot,
        };
        match self.hub.try_send(placed) {
            Ok(()) => Pushed::Queued,
            Err(TrySendError::Full(_)) => {
                self.close();
                Pushed::Overflow
            }
            Err(TrySendError::Closed(_)) => Pushed::Gone,
        }
    }

    /// Return the number of messages that wait in the hub queue.
    pub(crate) fn hub_len(&self) -> usize {
        self.hub.max_capacity() - self.hub.capacity()
    }

    /// Close the connection. Its writer task sends no more frames, and its
    /// reader stops.
    pub(crate) fn close(&self) {
        self.closing.send_replace(true);
    }

    /// Put `slot` at the next place, with the room `permit`. Return false
    /// when the writer is gone.
    fn put_slot(&self, permit: OwnedPermit<Placed>, slot: Slot) -> bool {
        let _order = self.order.lock().unwrap_or_else(PoisonError::into_inner);
        let placed = Placed {
            place: self.next_place(),
            slot,
        };
        !permit.send(placed).is_closed()
    }
}

/// Room for one frame in the session queue of a connection. A call takes
/// it before it runs, so it puts its reply without a wait. Dropping it
/// gives the room back.
#[must_use]
pub(crate) struct Room {
    /// `None` when the connection is closed.
    permit: Option<OwnedPermit<Placed>>,
    outbox: Outbox,
}

impl std::fmt::Debug for Room {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Room")
            .field("closed", &self.is_closed())
            .finish_non_exhaustive()
    }
}

impl Room {
    /// Tell if the connection was closed when the room was taken.
    pub(crate) fn is_closed(&self) -> bool {
        self.permit.is_none()
    }

    /// Put `frame` at the next place, or leave the place empty. Return
    /// false when the connection is closed.
    pub(crate) fn put(self, frame: Option<Vec<u8>>) -> bool {
        match self.permit {
            Some(permit) => {
                let frame = frame.map(|frame| self.outbox.flow.charged(frame));
                self.outbox.put_slot(permit, Slot::Ready(frame))
            }
            None => false,
        }
    }

    /// Take the next place in the order of the connection. The frame comes
    /// later, with `Place::fill`. The writer waits for it.
    pub(crate) fn place(self) -> Place {
        let Some(permit) = self.permit else {
            return Place(None);
        };
        let (frame, slot) = oneshot::channel();
        if self.outbox.put_slot(permit, Slot::Pending(slot)) {
            Place(Some((frame, Arc::clone(&self.outbox.flow))))
        } else {
            Place(None)
        }
    }
}

/// A place in the order of a connection, for a frame that a call gives
/// later. Dropping it leaves the place empty.
#[derive(Debug)]
#[must_use]
pub(crate) struct Place(Option<(oneshot::Sender<Outgoing>, Arc<Flow>)>);

impl Place {
    /// Put `frame` at the place, or leave the place empty. Return false
    /// when the connection is closed.
    pub(crate) fn fill(self, frame: Option<Vec<u8>>) -> bool {
        match (self.0, frame) {
            (Some((place, flow)), Some(frame)) => place.send(flow.charged(frame)).is_ok(),
            (Some((place, _)), None) => !place.is_closed(),
            (None, _) => false,
        }
    }
}

/// The receiving side of the queues of a connection. The writer task takes
/// the frames of both queues in the order of their places.
pub(crate) struct Inbox {
    session: mpsc::Receiver<Placed>,
    hub: mpsc::Receiver<Placed>,
    /// The place of the next frame to send.
    next: u64,
    /// The first frame of each queue, read ahead.
    from_session: Option<Placed>,
    from_hub: Option<Placed>,
    /// The frame of the last place that went out, when its call did not
    /// give it yet.
    pending: Option<oneshot::Receiver<Outgoing>>,
    /// The frames of the last place that went out, after its first frame.
    rest: VecDeque<Frame>,
    /// The queue still has a sender.
    session_open: bool,
    hub_open: bool,
}

/// What the inbox needs to give its next frame.
enum Step {
    /// This frame is next.
    Frame(Outgoing),
    /// The next frame comes from its call: wait for it.
    Pending,
    /// The next frame is not read yet: read ahead in these queues.
    Read { session: bool, hub: bool },
    /// Both queues are closed and empty.
    End,
}

impl Inbox {
    /// Return the next frame in the order of the connection. Return `None`
    /// when both queues are closed and empty. The call is cancel safe: a
    /// frame that it read ahead, or waits for, stays in the inbox.
    pub(crate) async fn recv(&mut self) -> Option<Outgoing> {
        loop {
            match self.step() {
                Step::Frame(frame) => return Some(frame),
                Step::End => return None,
                Step::Pending => {
                    let pending = self.pending.as_mut().expect("a pending frame");
                    let frame = pending.await;
                    self.pending = None;
                    if let Ok(frame) = frame {
                        return Some(frame);
                    }
                }
                Step::Read { session, hub } => tokio::select! {
                    biased;
                    placed = self.session.recv(), if session => match placed {
                        Some(placed) => self.from_session = Some(placed),
                        None => self.session_open = false,
                    },
                    placed = self.hub.recv(), if hub => match placed {
                        Some(placed) => self.from_hub = Some(placed),
                        None => self.hub_open = false,
                    },
                },
            }
        }
    }

    /// Return the next frame if it waits now, and do not wait.
    #[cfg(test)]
    pub(crate) fn try_recv(&mut self) -> Option<Outgoing> {
        use mpsc::error::TryRecvError;

        loop {
            let (session, hub) = match self.step() {
                Step::Frame(frame) => return Some(frame),
                Step::End => return None,
                Step::Pending => {
                    let pending = self.pending.as_mut().expect("a pending frame");
                    match pending.try_recv() {
                        Ok(frame) => {
                            self.pending = None;
                            return Some(frame);
                        }
                        Err(oneshot::error::TryRecvError::Empty) => return None,
                        Err(oneshot::error::TryRecvError::Closed) => {
                            self.pending = None;
                            continue;
                        }
                    }
                }
                Step::Read { session, hub } => (session, hub),
            };
            let mut read = false;
            if session {
                match self.session.try_recv() {
                    Ok(placed) => (self.from_session, read) = (Some(placed), true),
                    Err(TryRecvError::Disconnected) => (self.session_open, read) = (false, true),
                    Err(TryRecvError::Empty) => {}
                }
            }
            if hub {
                match self.hub.try_recv() {
                    Ok(placed) => (self.from_hub, read) = (Some(placed), true),
                    Err(TryRecvError::Disconnected) => (self.hub_open, read) = (false, true),
                    Err(TryRecvError::Empty) => {}
                }
            }
            if !read {
                return None;
            }
        }
    }

    /// Give the frame of the next place when it is read ahead, or tell
    /// what to read or to wait for.
    fn step(&mut self) -> Step {
        if let Some(frame) = self.rest.pop_front() {
            return Step::Frame(Outgoing::hub(frame));
        }
        if self.pending.is_some() {
            return Step::Pending;
        }
        loop {
            let at_next = |slot: &Option<Placed>| slot.as_ref().map(|placed| placed.place);
            let placed = if at_next(&self.from_session) == Some(self.next) {
                self.from_session.take()
            } else if at_next(&self.from_hub) == Some(self.next) {
                self.from_hub.take()
            } else {
                None
            };
            if let Some(placed) = placed {
                self.next += 1;
                match placed.slot {
                    Slot::Ready(Some(frame)) => return Step::Frame(frame),
                    // An empty place.
                    Slot::Ready(None) => continue,
                    Slot::Message(frames) => {
                        self.rest = frames.into();
                        match self.rest.pop_front() {
                            Some(frame) => return Step::Frame(Outgoing::hub(frame)),
                            None => continue,
                        }
                    }
                    Slot::Pending(frame) => {
                        self.pending = Some(frame);
                        return Step::Pending;
                    }
                }
            }
            let session = self.from_session.is_none() && self.session_open;
            let hub = self.from_hub.is_none() && self.hub_open;
            if session || hub {
                return Step::Read { session, hub };
            }
            // Each queue is closed or holds a later place first. So the
            // next place has no frame: its sender is gone. Go on from the
            // first place that waits.
            let first = [at_next(&self.from_session), at_next(&self.from_hub)]
                .into_iter()
                .flatten()
                .min();
            match first {
                Some(place) => self.next = place,
                None => return Step::End,
            }
        }
    }
}

/// The signal that a connection closes.
#[derive(Clone)]
pub(crate) struct Closing(watch::Receiver<bool>);

impl Closing {
    /// Wait until the connection closes. When every outbox of the
    /// connection drops and none closed it, wait forever.
    pub(crate) async fn wait(&mut self) {
        if self.0.wait_for(|closing| *closing).await.is_err() {
            std::future::pending::<()>().await;
        }
    }

    /// Tell if the connection is closed.
    #[cfg(test)]
    pub(crate) fn is_closed(&self) -> bool {
        *self.0.borrow()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(text: &str) -> Frame {
        Arc::new(text.as_bytes().to_vec())
    }

    /// Take the frames that wait now, as text.
    fn take(inbox: &mut Inbox) -> Vec<String> {
        let mut taken = Vec::new();
        while let Some(frame) = inbox.try_recv() {
            taken.push(String::from_utf8(frame.to_vec()).unwrap());
        }
        taken
    }

    #[test]
    fn the_writer_sends_the_frames_of_both_queues_in_the_order_of_their_places() {
        let (outbox, mut inbox, _closing) = Outbox::new(10, 10);
        let early = outbox.place();
        assert_eq!(outbox.push(frame("inv 1")), Pushed::Queued);
        assert_eq!(outbox.push(frame("inv 2")), Pushed::Queued);
        // The frame of the first place is not there yet: nothing goes out.
        assert!(take(&mut inbox).is_empty());
        assert!(outbox.send_at(early, Some(b"load".to_vec())));
        assert!(outbox.send(b"reply".to_vec()));
        assert_eq!(outbox.push(frame("inv 3")), Pushed::Queued);
        assert_eq!(
            take(&mut inbox),
            ["load", "inv 1", "inv 2", "reply", "inv 3"]
        );
    }

    #[test]
    fn an_empty_place_holds_no_frame() {
        let (outbox, mut inbox, _closing) = Outbox::new(10, 10);
        let empty = outbox.place();
        assert_eq!(outbox.push(frame("inv 1")), Pushed::Queued);
        assert!(take(&mut inbox).is_empty());
        assert!(outbox.send_at(empty, None));
        assert_eq!(take(&mut inbox), ["inv 1"]);
    }

    #[test]
    fn a_place_whose_call_is_gone_does_not_stop_the_others() {
        let (outbox, mut inbox, _closing) = Outbox::new(10, 10);
        let lost = outbox.place();
        assert_eq!(outbox.push(frame("inv")), Pushed::Queued);
        // The first place waits for its call.
        assert!(take(&mut inbox).is_empty());
        // The call drops its place, so the place gets no frame.
        drop(lost);
        assert_eq!(take(&mut inbox), ["inv"]);
        drop(outbox);
        assert!(inbox.try_recv().is_none());
        assert!(matches!(inbox.step(), Step::End));
    }

    #[test]
    fn a_frame_takes_its_place_when_it_goes_into_the_queue() {
        let (outbox, mut inbox, _closing) = Outbox::new(2, 10);
        let first = outbox.blocking_room();
        let second = outbox.blocking_room();
        // The queue has no more room.
        let third = outbox.clone();
        let waiting = std::thread::spawn(move || third.send(b"third".to_vec()));
        assert!(second.put(Some(b"second".to_vec())));
        assert_eq!(outbox.push(frame("inv")), Pushed::Queued);
        assert!(first.put(Some(b"first".to_vec())));
        let mut taken = take(&mut inbox);
        assert!(waiting.join().unwrap());
        taken.extend(take(&mut inbox));
        assert_eq!(taken, ["second", "inv", "first", "third"]);
    }

    #[test]
    fn a_room_of_a_closed_connection_holds_nothing() {
        let (outbox, inbox, _closing) = Outbox::new(1, 1);
        drop(inbox);
        let room = outbox.blocking_room();
        assert!(room.is_closed());
        assert!(!room.put(Some(b"reply".to_vec())));
        assert!(!outbox.blocking_room().place().fill(None));
    }

    #[test]
    fn a_full_hub_queue_closes_the_connection() {
        let (outbox, mut inbox, closing) = Outbox::new(10, 2);
        assert_eq!(outbox.push(frame("inv 1")), Pushed::Queued);
        assert_eq!(outbox.push(frame("inv 2")), Pushed::Queued);
        assert!(!closing.is_closed());
        assert_eq!(outbox.push(frame("inv 3")), Pushed::Overflow);
        assert!(closing.is_closed());
        assert_eq!(take(&mut inbox), ["inv 1", "inv 2"]);
    }

    #[test]
    fn the_frames_of_one_message_count_as_one_and_keep_their_order() {
        let (outbox, mut inbox, closing) = Outbox::new(10, 1);
        assert!(outbox.send(b"reply 1".to_vec()));
        let message = vec![frame("info"), frame("finish")];
        assert_eq!(outbox.push_message(message), Pushed::Queued);
        assert!(outbox.send(b"reply 2".to_vec()));
        assert!(!closing.is_closed());
        assert_eq!(take(&mut inbox), ["reply 1", "info", "finish", "reply 2"]);
        // The queue holds one message again.
        assert_eq!(outbox.push(frame("inv")), Pushed::Queued);
        assert_eq!(take(&mut inbox), ["inv"]);
    }

    #[test]
    fn a_message_that_does_not_fit_puts_none_of_its_frames() {
        let (outbox, mut inbox, closing) = Outbox::new(10, 1);
        assert_eq!(outbox.push(frame("inv")), Pushed::Queued);
        let message = vec![frame("info"), frame("finish")];
        assert_eq!(outbox.push_message(message), Pushed::Overflow);
        assert!(closing.is_closed());
        assert_eq!(take(&mut inbox), ["inv"]);
        // An empty message is an empty place.
        let (outbox, mut inbox, _closing) = Outbox::new(10, 10);
        assert_eq!(outbox.push_message(Vec::new()), Pushed::Queued);
        assert_eq!(outbox.push(frame("inv")), Pushed::Queued);
        assert_eq!(take(&mut inbox), ["inv"]);
    }

    #[test]
    fn a_full_session_queue_makes_the_session_wait() {
        let (outbox, mut inbox, closing) = Outbox::new(1, 10);
        assert!(outbox.send(b"reply 1".to_vec()));
        let session = outbox.clone();
        let sent = std::thread::spawn(move || session.send(b"reply 2".to_vec()));
        std::thread::sleep(std::time::Duration::from_millis(50));
        assert!(!sent.is_finished());
        let mut taken = take(&mut inbox);
        assert!(sent.join().unwrap());
        taken.extend(take(&mut inbox));
        assert_eq!(taken, ["reply 1", "reply 2"]);
        assert!(!closing.is_closed());
    }

    #[test]
    fn a_queue_holds_at_least_one_frame_and_at_most_the_bound_of_tokio() {
        let (outbox, inbox, _closing) = Outbox::new(0, usize::MAX);
        assert_eq!(outbox.push(frame("one")), Pushed::Queued);
        assert_eq!(outbox.push(frame("two")), Pushed::Queued);
        drop(inbox);
        assert_eq!(outbox.push(frame("three")), Pushed::Gone);
        assert!(!outbox.send(b"four".to_vec()));
    }

    /// The bytes of the frames of the session that wait, and the window.
    fn count(outbox: &Outbox) -> (usize, usize) {
        let count = *outbox.flow.0.borrow();
        (count.waiting, count.window)
    }

    /// Tell if a turn is free now. Give it back at once.
    fn turn_is_free(outbox: &Outbox) -> bool {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        runtime.block_on(async {
            tokio::select! {
                biased;
                _turn = outbox.turn() => true,
                () = std::future::ready(()) => false,
            }
        })
    }

    #[test]
    fn a_frame_of_the_session_counts_until_the_writer_drops_it() {
        let (outbox, mut inbox, _closing) = Outbox::new(10, 10);
        assert!(outbox.send(vec![1; 100]));
        assert_eq!(outbox.push(frame("inv")), Pushed::Queued);
        let place = outbox.place();
        assert!(place.fill(Some(vec![2; 50])));
        // A frame of the hub does not count.
        assert_eq!(count(&outbox).0, 150);
        let first = inbox.try_recv().unwrap();
        // The writer sends the frame: it still counts.
        assert_eq!(count(&outbox).0, 150);
        drop(first);
        assert_eq!(count(&outbox).0, 50);
        assert_eq!(take(&mut inbox).len(), 2);
        assert_eq!(count(&outbox).0, 0);
        // The frames of a closed connection drop with its queue.
        assert!(outbox.send(vec![3; 10]));
        drop(inbox);
        assert_eq!(count(&outbox).0, 0);
    }

    #[test]
    fn the_window_opens_when_the_writer_sends_and_closes_when_much_waits() {
        let (outbox, mut inbox, _closing) = Outbox::new(100, 10);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        // A new connection runs one call of the read pool at a time.
        assert_eq!(count(&outbox), (0, 1));
        let first = runtime.block_on(outbox.turn()).unwrap();
        assert!(!turn_is_free(&outbox));
        // A frame that the writer does not send opens nothing.
        assert!(outbox.send(b"reply".to_vec()));
        assert_eq!(count(&outbox).1, 1);
        // Each frame that the writer sends while little waits opens the
        // window by one call.
        inbox.try_recv().unwrap().sent();
        assert_eq!(count(&outbox), (0, 2));
        for window in 3..=QUEUE + 1 {
            assert!(outbox.send(b"reply".to_vec()));
            inbox.try_recv().unwrap().sent();
            assert_eq!(count(&outbox), (0, window.min(QUEUE)));
        }
        let second = runtime.block_on(outbox.turn()).unwrap();
        // A large frame goes in whole. Then no call starts.
        assert!(outbox.send(vec![0; 2 * WAITING]));
        assert!(!turn_is_free(&outbox));
        // A frame that goes in while much waits closes the window.
        assert!(outbox.send(b"reply".to_vec()));
        assert_eq!(count(&outbox).1, 1);
        // The frames drop unsent, as at a close. The two calls still hold
        // their turns.
        take(&mut inbox);
        assert_eq!(count(&outbox), (0, 1));
        assert!(!turn_is_free(&outbox));
        drop(first);
        assert!(!turn_is_free(&outbox));
        drop(second);
        assert!(turn_is_free(&outbox));
    }

    #[test]
    fn a_large_frame_that_the_writer_sends_opens_the_window_when_little_else_waits() {
        let (outbox, mut inbox, _closing) = Outbox::new(100, 10);
        assert!(outbox.send(vec![0; 2 * WAITING]));
        assert!(outbox.send(vec![0; 2 * WAITING]));
        // Another large frame waits: the window stays closed.
        inbox.try_recv().unwrap().sent();
        assert_eq!(count(&outbox), (2 * WAITING, 1));
        inbox.try_recv().unwrap().sent();
        assert_eq!(count(&outbox), (0, 2));
        // A frame of the hub opens nothing.
        assert_eq!(outbox.push(frame("inv")), Pushed::Queued);
        inbox.try_recv().unwrap().sent();
        assert_eq!(count(&outbox), (0, 2));
    }

    #[test]
    fn the_waits_on_the_count_end_when_the_writer_is_gone() {
        let (outbox, inbox, _closing) = Outbox::new(10, 10);
        let room = outbox.blocking_room();
        // The writer stops, as at a reset of the client. A call then puts
        // a large frame, and the frame stays counted.
        drop(inbox);
        let _ = room.put(Some(vec![0; 2 * WAITING]));
        // A download stops.
        let session = outbox.clone();
        let sent = std::thread::spawn(move || session.send(b"chunk".to_vec()));
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        while !sent.is_finished() {
            assert!(std::time::Instant::now() < deadline, "the download waits");
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(!sent.join().unwrap());
        // The next call takes a room that holds nothing.
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();
        runtime.block_on(async {
            let limit = std::time::Duration::from_secs(30);
            let turn = tokio::time::timeout(limit, outbox.turn()).await.unwrap();
            assert!(turn.is_none());
            tokio::time::timeout(limit, outbox.below()).await.unwrap();
            assert!(outbox.room().await.is_closed());
        });
    }

    #[test]
    fn a_download_stops_when_the_hub_closes_the_connection() {
        let (outbox, inbox, closing) = Outbox::new(10, 1);
        let room = outbox.blocking_room();
        // The hub queue overflows, and the writer stops.
        assert_eq!(outbox.push(frame("inv 1")), Pushed::Queued);
        assert_eq!(outbox.push(frame("inv 2")), Pushed::Overflow);
        assert!(closing.is_closed());
        drop(inbox);
        // A load puts its large reply after that.
        let _ = room.put(Some(vec![0; 2 * WAITING]));
        let session = outbox.clone();
        let sent = std::thread::spawn(move || session.send(b"chunk".to_vec()));
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        while !sent.is_finished() {
            assert!(std::time::Instant::now() < deadline, "the download waits");
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(!sent.join().unwrap());
    }

    #[test]
    fn a_download_waits_while_much_waits() {
        let (outbox, mut inbox, _closing) = Outbox::new(10, 10);
        assert!(outbox.send(vec![0; WAITING + 1]));
        let session = outbox.clone();
        let sent = std::thread::spawn(move || session.send(b"chunk".to_vec()));
        std::thread::sleep(std::time::Duration::from_millis(50));
        assert!(!sent.is_finished());
        // The writer sends the large frame. Then the chunk goes in.
        drop(inbox.try_recv().unwrap());
        assert!(sent.join().unwrap());
        assert_eq!(take(&mut inbox), ["chunk"]);
    }

    #[test]
    fn recv_waits_for_the_next_place_and_keeps_what_it_read_ahead() {
        let (outbox, mut inbox, _closing) = Outbox::new(10, 10);
        let early = outbox.place();
        assert_eq!(outbox.push(frame("inv")), Pushed::Queued);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        runtime.block_on(async {
            // The call reads the frame of the hub ahead, then waits for the
            // first place. The ready branch cancels it.
            tokio::select! {
                biased;
                frame = inbox.recv() => panic!("a frame went out of order: {frame:?}"),
                () = std::future::ready(()) => {}
            }
            let session = outbox.clone();
            let send = move || session.send_at(early, Some(b"load".to_vec()));
            assert!(tokio::task::spawn_blocking(send).await.unwrap());
            assert_eq!(inbox.recv().await.unwrap().as_slice(), b"load");
            assert_eq!(inbox.recv().await.unwrap().as_slice(), b"inv");
            drop(outbox);
            assert!(inbox.recv().await.is_none());
        });
    }
}
