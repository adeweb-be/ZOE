//! The frames that wait for one connection.
//!
//! A connection has two queues. The session puts its replies in the
//! session queue, and it waits while that queue is full. The hub of the
//! invalidations puts its frames in the hub queue: the invalidations, and
//! the `info` and the reply of each `tpc_finish`. The hub never waits: a
//! hub queue that is full closes the connection. So a client that downloads
//! a blob, and keeps the session queue full, does not stop a commit.
//!
//! Each frame takes a place in the order of the connection when it goes
//! into its queue: one counter gives the places of both queues. The writer
//! task sends the frames strictly in the order of their places. So the
//! client gets the frames in the order in which the server queued them, as
//! from one queue.
//!
//! A frame can take its place before it exists: `loadBefore` takes its
//! place before an invalidation of a commit that its read did not see. A
//! place that gets no frame holds an empty frame, and the writer skips it.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use tokio::sync::mpsc::{self, error::TrySendError};
use tokio::sync::{Semaphore, watch};

/// The bytes of one message. All the connections that get the message
/// share them.
pub(crate) type Frame = Arc<Vec<u8>>;

/// A frame and its place in the order of its connection. A place without a
/// frame has none.
struct Placed {
    place: u64,
    frame: Option<Frame>,
}

/// A place in the order of a connection, for a frame of the session.
#[derive(Debug, PartialEq, Eq)]
#[must_use]
pub(crate) struct Place(u64);

/// The two queues of one connection, and the signal that closes it.
#[derive(Clone)]
pub(crate) struct Outbox {
    /// The replies of the session. The session waits for room.
    session: mpsc::Sender<Placed>,
    /// The frames of the hub. The hub never waits.
    hub: mpsc::Sender<Placed>,
    /// The place of the next frame.
    next: Arc<AtomicU64>,
    closing: Arc<watch::Sender<bool>>,
}

/// What a push into the hub queue did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Pushed {
    /// The frame waits in the queue.
    Queued,
    /// The queue was full. The push closed the connection.
    Overflow,
    /// The connection is closed.
    Gone,
}

impl Outbox {
    /// Return the queues of a connection: a session queue of at most
    /// `session` frames and a hub queue of at most `hub` frames. Also
    /// return the receiving side for the writer task, and the signal of the
    /// close. A size of 0 holds 1 frame. A size above the bound of tokio
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
            closing: Arc::new(closing),
        };
        let inbox = Inbox {
            session: from_session,
            hub: from_hub,
            next: 0,
            from_session: None,
            from_hub: None,
            session_open: true,
            hub_open: true,
        };
        (outbox, inbox, Closing(signal))
    }

    /// Take the next place in the order of the connection.
    pub(crate) fn place(&self) -> Place {
        Place(self.next.fetch_add(1, Ordering::Relaxed))
    }

    /// Put a frame of the session at `place`, or leave the place empty.
    /// Wait while the session queue is full. Return false when the
    /// connection is closed.
    ///
    /// The session takes its places and puts its frames in order. Call
    /// this only on a thread that is not a thread of the async runtime.
    pub(crate) fn send_at(&self, place: Place, frame: Option<Vec<u8>>) -> bool {
        let placed = Placed {
            place: place.0,
            frame: frame.map(Arc::new),
        };
        self.session.blocking_send(placed).is_ok()
    }

    /// Put a frame of the session at the next place.
    pub(crate) fn send(&self, frame: Vec<u8>) -> bool {
        self.send_at(self.place(), Some(frame))
    }

    /// Put a frame of the hub at the next place, and do not wait. When the
    /// hub queue is full, close the connection.
    ///
    /// The hub calls this under its mutex, so its frames take their places
    /// in the order of the queue.
    pub(crate) fn push(&self, frame: Frame) -> Pushed {
        let placed = Placed {
            place: self.place().0,
            frame: Some(frame),
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

    /// Return the number of frames that wait in the hub queue.
    pub(crate) fn hub_len(&self) -> usize {
        self.hub.max_capacity() - self.hub.capacity()
    }

    /// Close the connection. Its writer task sends no more frames, and its
    /// reader stops.
    pub(crate) fn close(&self) {
        self.closing.send_replace(true);
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
    /// The queue still has a sender.
    session_open: bool,
    hub_open: bool,
}

/// What the inbox needs to give its next frame.
enum Step {
    /// This frame is next.
    Frame(Frame),
    /// The next frame is not read yet: read ahead in these queues.
    Read { session: bool, hub: bool },
    /// Both queues are closed and empty.
    End,
}

impl Inbox {
    /// Return the next frame in the order of the connection. Return `None`
    /// when both queues are closed and empty. The call is cancel safe: a
    /// frame that it read ahead stays in the inbox.
    pub(crate) async fn recv(&mut self) -> Option<Frame> {
        loop {
            match self.step() {
                Step::Frame(frame) => return Some(frame),
                Step::End => return None,
                Step::Read { session, hub } => tokio::select! {
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
    pub(crate) fn try_recv(&mut self) -> Option<Frame> {
        use mpsc::error::TryRecvError;

        loop {
            let (session, hub) = match self.step() {
                Step::Frame(frame) => return Some(frame),
                Step::End => return None,
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
    /// what to read.
    fn step(&mut self) -> Step {
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
                match placed.frame {
                    Some(frame) => return Step::Frame(frame),
                    // An empty place.
                    None => continue,
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
    fn a_place_whose_sender_is_gone_does_not_stop_the_others() {
        let (outbox, mut inbox, _closing) = Outbox::new(10, 10);
        let _lost = outbox.place();
        assert_eq!(outbox.push(frame("inv")), Pushed::Queued);
        // The first place waits for the session.
        assert!(take(&mut inbox).is_empty());
        // The session queue closes, so the first place gets no frame.
        let Outbox { session, hub, .. } = outbox;
        drop(session);
        assert_eq!(take(&mut inbox), ["inv"]);
        drop(hub);
        assert!(inbox.try_recv().is_none());
        assert!(matches!(inbox.step(), Step::End));
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
