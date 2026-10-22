//! The reconnect of a pool: a new listener, its verification against the
//! cache and the last tid of the pool, as `verify` of ZEO does, and the
//! upgrade from a read-only server to a writable one.

use std::sync::Arc;
use std::time::Duration;

use zoe_core::Tid;

use super::dispatch::announce;
use super::{Connect, Delivery, Event, FileCheck, Shared, Verification};
use crate::cache::Cache;
use crate::client::{Client, Invalidations};
use crate::lock;

/// The outcome of the verification of a new listener.
enum Verified {
    /// The pool takes the server. The confirmation of `Event::Connected`
    /// moves `last` to the tid.
    Taken(Verification, Option<u64>),
    /// The pool refuses the server: its last tid is below the pool's.
    Refused(String),
    /// The verification failed: the connection is lost, or the server
    /// failed `getInvalidations`.
    Failed,
    /// The pool closed.
    Closed,
}

/// Connect again: open a listener at once, then every `poll`, until one
/// passes the verification. Then take its server, and return its stream of
/// invalidations. Return `None` after the close.
pub(super) async fn reconnect(shared: &Shared) -> Option<Invalidations> {
    let settings = shared.reconnect.clone()?;
    let mut delay = Duration::ZERO;
    loop {
        if !delay.is_zero() && !shared.pause(delay).await {
            return None;
        }
        delay = settings.poll;
        // A pending listener that the server closed since is of no use.
        let pending = shared.lock().pending.take();
        let pending = pending.filter(|(client, _)| client.is_connected());
        let opened = match pending {
            Some(opened) => Ok(opened),
            None => shared.unless_closed((settings.open)(false)).await?,
        };
        let Ok((client, connect)) = opened else {
            continue;
        };
        // Subscribe first, then read the last tid: the reader raises the
        // last tid before it sends an invalidation.
        let invalidations = client.invalidations();
        match verify(shared, &client).await {
            Verified::Taken(verification, tid) => {
                take(shared, client, connect);
                if !announce(shared, Event::Connected(verification), tid).await {
                    return None;
                }
                return Some(invalidations);
            }
            Verified::Refused(text) => {
                client.disconnect();
                delay = settings.refused_poll;
                if !announce(shared, Event::Refused(text), None).await {
                    return None;
                }
            }
            Verified::Failed => client.disconnect(),
            Verified::Closed => return None,
        }
    }
}

/// Verify the cache and the last tid of the pool against the new listener
/// `client`, as `verify` of `ZEO/asyncio/client.py` does.
///
/// The base is `applied` of the cache, or the last tid of the pool without
/// a cache. A server whose last tid is the base keeps the cache. A last tid
/// below the base, or below the last tid of the pool, refuses the server.
/// A last tid above the base asks `getInvalidations(base)`: the cache drops
/// the current entry of each oid that it gives, and the consumer gets them
/// as one invalidation at the tid of the reply. Its confirmation never
/// moves the last tid of the pool back. When the server answers `None`, the
/// cache drops every entry. A pool that never had a listener takes the
/// server as it is.
async fn verify(shared: &Shared, client: &Client) -> Verified {
    let server_tid = client.last_transaction();
    let (last, fresh) = {
        let mut state = shared.lock();
        let fresh = std::mem::take(&mut state.fresh);
        if fresh {
            state.last = state.last.max(server_tid.0);
        }
        (state.last, fresh)
    };
    if fresh {
        if let Some(cache) = &shared.cache {
            if cache.verified() {
                cache.verify(server_tid, Some(&[]));
            } else if !verify_file(shared, cache, client, server_tid).await {
                return Verified::Closed;
            }
        }
        return Verified::Taken(Verification::Kept, None);
    }
    let base = shared
        .cache
        .as_ref()
        .map_or(last, |cache| cache.applied().0);
    if base.max(last) > server_tid.0 {
        return Verified::Refused(format!(
            "the last transaction {:016x} of the server at {} is before the last \
             transaction {last:016x} of the client or {base:016x} of its cache",
            server_tid.0,
            client.address(),
        ));
    }
    if base == server_tid.0 {
        return Verified::Taken(Verification::Kept, None);
    }
    let reply = match shared
        .unless_closed(client.get_invalidations(Tid(base)))
        .await
    {
        None => return Verified::Closed,
        Some(Err(_)) => return Verified::Failed,
        Some(Ok(reply)) => reply,
    };
    match reply {
        Some(invalidation) => {
            if let Some(cache) = &shared.cache {
                cache.verify(invalidation.tid, Some(&invalidation.oids));
            }
            // The consumer always gets the oids of the reply, as ZEO calls
            // `invalidateTransaction` after `getInvalidations`. The tid can
            // be the last tid of the pool: a turn after the loss moved the
            // last tid past the base, and the commits of other clients
            // between the base and that tid never reached the consumer.
            // Then the confirmation keeps the last tid.
            let tid = invalidation.tid.0;
            let event = Event::Delivery(Delivery::Invalidate(invalidation));
            if !announce(shared, event, Some(tid)).await {
                return Verified::Closed;
            }
            Verified::Taken(Verification::Quick, None)
        }
        None => {
            if let Some(cache) = &shared.cache {
                if !before_drop(shared, String::new()).await {
                    return Verified::Closed;
                }
                cache.verify(server_tid, None);
            }
            Verified::Taken(Verification::Dropped, Some(server_tid.0))
        }
    }
}

/// Verify a cache that a file gave against `client`, whose last tid is
/// `server_tid`, before the cache serves. The pool delivers nothing for it:
/// the consumer starts at the last tid of the pool.
///
/// The base is `applied` of the cache. A server tid equal to the base keeps
/// the cache. With a higher one, the server must hold a commit at the base,
/// else its history is another one and the cache drops every entry. Then
/// `getInvalidations(base)`: a reply drops the current entry of each of its
/// oids, and `None` or an error drops every entry. A server tid below the
/// base drops every entry too, and `applied` becomes the server tid. Then
/// the cache serves. Return false at the close.
pub(super) async fn verify_file(shared: &Shared, cache: &Cache, client: &Client, server_tid: Tid) -> bool {
    let base = cache.applied();
    let check = if base == server_tid {
        cache.verify(server_tid, Some(&[]));
        FileCheck::Kept
    } else if base > server_tid {
        cache.verify(server_tid, None);
        FileCheck::Ahead {
            applied: base,
            server: server_tid,
        }
    } else {
        let held = if base == Tid(0) {
            Ok(true)
        } else {
            match shared.unless_closed(client.holds_transaction(base)).await {
                None => return false,
                Some(held) => held,
            }
        };
        match held {
            Ok(true) => {}
            Ok(false) => {
                cache.verify(server_tid, None);
                shared.lock().file_check = Some(FileCheck::Other { applied: base });
                return true;
            }
            Err(error) => {
                cache.verify(server_tid, None);
                shared.lock().file_check = Some(FileCheck::Failed(error.to_string()));
                return true;
            }
        }
        match shared.unless_closed(client.get_invalidations(base)).await {
            None => return false,
            Some(Ok(Some(invalidation))) => {
                cache.verify(invalidation.tid, Some(&invalidation.oids));
                FileCheck::Quick
            }
            Some(Ok(None)) => {
                if !before_drop(shared, String::new()).await {
                    return false;
                }
                cache.verify(server_tid, None);
                FileCheck::Dropped
            }
            Some(Err(error)) => {
                if !before_drop(shared, error.to_string()).await {
                    return false;
                }
                cache.verify(server_tid, None);
                FileCheck::Failed(error.to_string())
            }
        }
    };
    shared.lock().file_check = Some(check);
    true
}

/// Call the `dropping` function of the reconnect, if any, with `reason`,
/// and wait for it. Return false at the close.
async fn before_drop(shared: &Shared, reason: String) -> bool {
    let Some(dropping) = shared
        .reconnect
        .as_ref()
        .and_then(|reconnect| reconnect.dropping.clone())
    else {
        return true;
    };
    shared.unless_closed(dropping(reason)).await.is_some()
}

/// Take the server of the new listener `client`: the pool is connected.
fn take(shared: &Shared, client: Client, connect: Connect) {
    {
        let mut state = shared.lock();
        if state.closed {
            drop(state);
            client.disconnect();
            return;
        }
        state.lost = false;
        state.connect = Some(connect);
        // The `get_info` of the new listener is newer than any size of the
        // server that the pool lost.
        *lock(&shared.pushed) = None;
        shared.server_mut().listener = Some(Arc::new(client));
    }
    shared.bump();
}

/// While the listener is read-only, try a writable server every `poll`.
/// When one opens, keep it for the next connection, and close the
/// listener: the dispatcher then connects again, on that server. Stop at
/// the close.
pub(super) async fn upgrade_forever(shared: Arc<Shared>) {
    let Some(settings) = shared.reconnect.clone() else {
        return;
    };
    loop {
        if !shared.pause(settings.poll).await {
            return;
        }
        let ready = {
            let state = shared.lock();
            !state.lost && state.pending.is_none()
        };
        let listener = shared.server().listener.clone();
        let Some(listener) = listener.filter(|listener| ready && listener.read_only()) else {
            continue;
        };
        let Some(opened) = shared.unless_closed((settings.open)(true)).await else {
            return;
        };
        if let Ok((client, connect)) = opened
            && !client.read_only()
        {
            let kept = {
                let mut state = shared.lock();
                if state.closed {
                    return;
                }
                // The listener can change during the open: the pool lost it,
                // and connected again. Then the writable listener comes too
                // late, and the next attempt opens a new one.
                let same = !state.lost
                    && shared
                        .server()
                        .listener
                        .as_ref()
                        .is_some_and(|current| Arc::ptr_eq(current, &listener));
                if same {
                    state.pending = Some((client, connect));
                    None
                } else {
                    Some(client)
                }
            };
            match kept {
                None => listener.disconnect(),
                Some(client) => client.disconnect(),
            }
        }
    }
}
