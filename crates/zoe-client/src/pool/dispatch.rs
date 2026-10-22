//! The dispatcher of a pool: it delivers each tid of the listener in turn,
//! to the consumer or to the finish that committed it. A lost listener
//! gives the stale mark, then the turn of each finish that has its reply.

use std::sync::Arc;

use zoe_core::Tid;

use super::reconnect::{reconnect, verify_file};
use super::{Delivery, Event, Offer, Shared};
use crate::client::{Client, Invalidation, Invalidations};

/// How the dispatcher delivers one tid.
enum Route {
    /// A commit of another client.
    Foreign,
    /// A commit of the pool: its finish has the turn.
    Own,
}

/// The dispatcher: deliver each tid of the listener in turn. When the
/// listener is lost, give the stale mark, then connect again if the pool
/// does. Stop at the close.
pub(super) async fn dispatch(shared: Arc<Shared>, mut stream: Option<Invalidations>) {
    // A cache that a file gave serves only after its verification against
    // the first listener, at the last tid that the start read.
    if let Some(cache) = &shared.cache
        && stream.is_some()
        && !cache.verified()
    {
        let Ok(listener) = shared.listener() else {
            return;
        };
        let server_tid = Tid(shared.lock().last);
        if !verify_file(&shared, cache, &listener, server_tid).await {
            return;
        }
        shared.bump();
    }
    loop {
        if let Some(mut invalidations) = stream.take() {
            let Ok(listener) = shared.listener() else {
                return;
            };
            if !serve(&shared, &listener, &mut invalidations).await {
                return;
            }
            if shared.lock().closed {
                return;
            }
            lose(&shared, &listener).await;
            if shared.lock().closed {
                return;
            }
        }
        if shared.reconnect.is_none() {
            return;
        }
        match reconnect(&shared).await {
            Some(invalidations) => stream = Some(invalidations),
            None => return,
        }
    }
}

/// Deliver each tid of `listener` until the loss of the listener: then
/// return true. Return false after the close.
async fn serve(shared: &Shared, listener: &Client, invalidations: &mut Invalidations) -> bool {
    loop {
        match invalidations.recv().await {
            Ok(invalidation) => match deliver(shared, listener, invalidation).await {
                Step::Next => {}
                Step::Closed => return false,
                Step::Lost => return true,
            },
            Err(_) => return true,
        }
    }
}

/// What the dispatcher does after a tid.
enum Step {
    /// Deliver the next tid.
    Next,
    /// The pool is closed.
    Closed,
    /// The listener was lost during the hold of the tid.
    Lost,
}

/// Deliver the tid of `invalidation`, which came on `listener`.
async fn deliver(shared: &Shared, listener: &Client, invalidation: Invalidation) -> Step {
    let tid = invalidation.tid.0;
    let horizon = {
        let mut state = shared.lock();
        if state.closed {
            return Step::Closed;
        }
        // A tid at or before the last tid of the pool came before the open,
        // or before the verification of a new listener.
        if tid <= state.last {
            return Step::Next;
        }
        state.head = Some(tid);
        state.next_seq
    };
    shared.bump();
    let mut held = false;
    let hold = shared.until(|state| {
        if state.closed {
            return Some(None);
        }
        if let Some(slot) = state.replied.get_mut(&tid) {
            slot.turn = true;
            if slot.wants_oids {
                slot.oids = Some(invalidation.oids.clone());
            }
            state.started += 1;
            return Some(Some(Route::Own));
        }
        // A finish counted after the tid came commits a later tid.
        if state.in_flight.range(..horizon).next().is_none() {
            state.started += 1;
            return Some(Some(Route::Foreign));
        }
        if !held {
            held = true;
            state.counters.held += 1;
        }
        None
    });
    // A lost listener ends the hold. Then each finish in flight fails.
    let route = tokio::select! {
        biased;
        route = hold => route,
        () = listener.closed() => {
            shared.lock().head = None;
            return Step::Lost;
        }
    };
    let Some(route) = route else {
        return Step::Closed;
    };
    shared.bump();
    let delivered = match route {
        Route::Foreign => offer(shared, invalidation).await,
        Route::Own => match wait_for_turn(shared, tid).await {
            // The finish dropped before it ran: the consumer gets the tid.
            Some(true) => offer(shared, invalidation).await,
            Some(false) => true,
            None => false,
        },
    };
    shared.lock().head = None;
    if delivered { Step::Next } else { Step::Closed }
}

/// Apply the commit of another client to the cache, then offer it to the
/// consumer, and wait until the consumer confirmed it. Return false after
/// the close.
async fn offer(shared: &Shared, invalidation: Invalidation) -> bool {
    if let Some(cache) = &shared.cache {
        cache.apply(invalidation.tid, &invalidation.oids);
    }
    let tid = invalidation.tid.0;
    let event = Event::Delivery(Delivery::Invalidate(invalidation));
    put(shared, event, Some(tid));
    wait_for_consumer(shared).await
}

/// Offer `event` to the consumer. Its confirmation moves `last` to `tid`.
/// The caller counts the delivery in `started`.
fn put(shared: &Shared, event: Event, tid: Option<u64>) {
    shared.lock().offer = Some(Offer {
        event,
        taken: false,
        tid,
    });
    shared.bump();
}

/// Count a delivery, offer `event` to the consumer, and wait until the
/// consumer confirmed it. Return false after the close.
pub(super) async fn announce(shared: &Shared, event: Event, tid: Option<u64>) -> bool {
    shared.lock().started += 1;
    put(shared, event, tid);
    wait_for_consumer(shared).await
}

/// Wait until the consumer confirmed the offer. Return false after the
/// close.
async fn wait_for_consumer(shared: &Shared) -> bool {
    shared
        .until(|state| {
            if state.offer.is_none() {
                Some(true)
            } else if state.closed {
                Some(false)
            } else {
                None
            }
        })
        .await
}

/// Wait until the finish of `tid` ran with its turn, or dropped. Return
/// whether it dropped before it ran, or `None` after the close.
async fn wait_for_turn(shared: &Shared, tid: u64) -> Option<bool> {
    shared
        .until(|state| {
            if state.abandoned.remove(&tid) {
                return Some(Some(true));
            }
            if !state.replied.contains_key(&tid) && state.running != Some(tid) {
                return Some(Some(false));
            }
            if state.closed {
                return Some(None);
            }
            None
        })
        .await
}

/// The listener is lost or lagged. Fail the finishes that wait for their
/// reply. Give the consumer `Delivery::Stale`, then give each finish that
/// has its reply its turn, in tid order.
///
/// A pool that connects again also closes each connection of the server
/// and forgets them: a transaction on one of them fails at its next call.
async fn lose(shared: &Shared, listener: &Client) {
    let reconnects = shared.reconnect.is_some();
    // Without a new connection, the cache no longer gets the commits in
    // order: an empty cache is true at any tid. With one, the verification
    // keeps what stays true.
    if let Some(cache) = &shared.cache
        && !reconnects
    {
        cache.disable();
    }
    let (free, conns) = {
        let mut state = shared.lock();
        state.lost = true;
        let in_flight = std::mem::take(&mut state.in_flight);
        state.doomed.extend(in_flight);
        state.offer = Some(Offer {
            event: Event::Delivery(Delivery::Stale),
            taken: false,
            tid: None,
        });
        state.started += 1;
        if reconnects {
            state.generation += 1;
            state.opened = 0;
            state.connect = None;
            let free = std::mem::take(&mut state.free);
            let mut server = shared.server_mut();
            server.listener = None;
            (free, std::mem::take(&mut server.conns))
        } else {
            (Vec::new(), Vec::new())
        }
    };
    shared.bump();
    listener.disconnect();
    for conn in conns.iter().chain(&free) {
        conn.disconnect();
    }
    if !wait_for_consumer(shared).await {
        return;
    }
    loop {
        let next = {
            let mut state = shared.lock();
            let next = state.replied.iter_mut().next().map(|(tid, slot)| {
                slot.turn = true;
                slot.stale = true;
                *tid
            });
            if next.is_some() {
                state.started += 1;
            }
            next
        };
        let Some(tid) = next else { return };
        shared.bump();
        match wait_for_turn(shared, tid).await {
            // The consumer already treats every object as stale.
            Some(true) => {
                shared.lock().ended += 1;
                shared.bump();
            }
            Some(false) => {}
            None => return,
        }
    }
}
