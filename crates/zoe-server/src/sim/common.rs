//! What the clients of `client.rs` and the pools of `pool.rs` share: the
//! oids, the tries and the address of the server, the end of one try of a
//! transaction, the choice of the oids of a transaction, the load with its
//! check of a stale read, and the report of the phase of a commit.

use std::future::Future;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};

use zoe_client::Error;
use zoe_core::Tid;

use super::rng::Rng;
use super::world::Shared;

/// The oids that the clients share.
pub(crate) const OIDS: u64 = 6;

/// The hot oid: half of the transactions write it.
pub(crate) const HOT: u64 = 1;

/// The tries of one transaction.
pub(crate) const TRIES: usize = 8;

/// The address of the server. The network of the simulation has one
/// server, so the address is only a name.
pub(crate) const ADDR: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 8100);

/// The end of one try of a transaction.
pub(crate) enum Outcome<T> {
    /// The server acknowledged the commit.
    Acked(T),
    /// A conflict, or another error of the server: try again.
    Retry,
    /// The connection is lost. In the finish, the outcome is unknown.
    Lost { in_finish: bool },
}

/// Return the oids that a transaction writes, in ascending order: the hot
/// oid for half of the transactions, or else a shared oid, and a second
/// shared oid for half of them.
pub(crate) fn oids(rng: &mut Rng) -> Vec<u64> {
    let first = if rng.chance(0.5) {
        HOT
    } else {
        1 + rng.below(OIDS)
    };
    let mut oids = vec![first];
    if rng.chance(0.5) {
        let second = 1 + rng.below(OIDS);
        if second != first {
            oids.push(second);
        }
    }
    oids.sort_unstable();
    oids
}

/// Load the current serial and the current record of `oid` with `load`, a
/// load of the client `client`. A new oid gives `Tid::ZERO` and no record.
/// Count a stale read: a serial older than the newest commit of `oid` that
/// a client saw acknowledged before the load.
pub(crate) async fn load<T>(
    shared: &Shared,
    client: usize,
    oid: u64,
    load: impl Future<Output = Result<(Vec<u8>, Tid), Error>>,
) -> Result<(Tid, Option<Vec<u8>>), Outcome<T>> {
    let known = shared.lock().known.get(&oid).copied().unwrap_or(0);
    let (serial, data) = match load.await {
        Ok((data, serial)) => (serial, Some(data)),
        Err(Error::Server { class, .. }) if class.ends_with("POSKeyError") => (Tid::ZERO, None),
        Err(Error::Disconnected) => return Err(Outcome::Lost { in_finish: false }),
        Err(_) => return Err(Outcome::Retry),
    };
    if serial.0 < known {
        let mut state = shared.lock();
        state.stale_reads += 1;
        state.trace(format!(
            "c{client} reads a stale serial {:016x} of oid {oid}, after {known:016x}",
            serial.0
        ));
    }
    Ok((serial, data))
}

/// Tell the driver that a commit of the client `client` reached `phase`.
/// When a planned crash comes there, the driver crashes the server after
/// this step of the client.
pub(crate) fn reach(shared: &Shared, client: usize, phase: &'static str) {
    let mut state = shared.lock();
    if state.crash_at(phase) {
        state.trace(format!("c{client} reaches the phase {phase}"));
        state.crash_now = Some(phase);
    }
}
