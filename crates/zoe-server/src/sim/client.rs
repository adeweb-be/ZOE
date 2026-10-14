//! The clients of a run: `zoe-client` on the runtime of the simulation.
//! They commit records to a small set of shared oids with one hot oid, so
//! that their votes meet on locks and conflicts.
//!
//! A client reads the current serial of each oid with `loadBefore`, then
//! stores, votes and finishes. A conflict aborts and tries again. A lost
//! connection connects again, as `ZEO.ClientStorage` does. A `tpc_finish`
//! whose reply does not arrive has an unknown outcome, and the client does
//! not count it.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use zoe_client::{Client, Error};
use zoe_core::{Oid, Tid};

use super::rng::Rng;
use super::server::SimRuntime;
use super::world::{Acked, Shared, sleep};
use crate::protocol::STORAGE_NAME;

/// The oids that the clients share.
const OIDS: u64 = 6;

/// The tries of one transaction.
const TRIES: usize = 8;

/// The address of the server. The network of the simulation has one
/// server, so the address is only a name.
const ADDR: SocketAddr = SocketAddr::new(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST), 8100);

/// How one try of a transaction ends.
enum Outcome {
    Acked(Acked),
    /// A conflict, or another error of the server: try again.
    Retry,
    /// The connection is lost. In `tpc_finish`, the outcome is unknown.
    Lost {
        in_finish: bool,
    },
}

/// One client of a run.
pub(crate) struct SimClient {
    pub(crate) shared: Arc<Shared>,
    pub(crate) id: usize,
    /// The transactions that the client runs.
    pub(crate) transactions: usize,
    pub(crate) rng: Rng,
    /// The time that the client stalls after its first vote, with its
    /// locks.
    pub(crate) stall: Option<Duration>,
}

impl SimClient {
    /// Run the transactions of the client, then close its connection.
    pub(crate) async fn run(mut self) {
        let runtime = Arc::new(SimRuntime::client(&self.shared, self.id));
        let mut connection: Option<Client> = None;
        'transactions: for t in 0..self.transactions {
            for attempt in 0..TRIES {
                let client = match connection.take() {
                    Some(client) => client,
                    None => match self.connect(&runtime).await {
                        Some(client) => client,
                        None => break 'transactions,
                    },
                };
                match self.commit(&client, t, attempt).await {
                    Outcome::Acked(acked) => {
                        connection = Some(client);
                        self.acknowledge(acked);
                        break;
                    }
                    Outcome::Retry => {
                        connection = Some(client);
                        let backoff = self
                            .rng
                            .between(Duration::from_micros(100), Duration::from_millis(5));
                        sleep(&self.shared, backoff).await;
                    }
                    Outcome::Lost { in_finish } => {
                        self.trace("lost its connection");
                        if in_finish {
                            break;
                        }
                    }
                }
            }
        }
        if let Some(client) = connection {
            client.close().await;
        }
        let mut state = self.shared.lock();
        state.trace(format!("c{} done", self.id));
        state.clients_done += 1;
    }

    fn trace(&self, text: &str) {
        self.shared.lock().trace(format!("c{} {text}", self.id));
    }

    /// Connect and register. Try again while the server is down. Return
    /// `None` when the store did not open again.
    async fn connect(&mut self, runtime: &Arc<SimRuntime>) -> Option<Client> {
        loop {
            if self.shared.lock().server.halted {
                return None;
            }
            let runtime: Arc<dyn zoe_client::Runtime> = runtime.clone();
            match Client::connect_with(runtime, ADDR, STORAGE_NAME).await {
                Ok(client) => {
                    self.trace("registered");
                    return Some(client);
                }
                Err(error) => self.trace(&format!("does not connect: {error}")),
            }
            let backoff = self
                .rng
                .between(Duration::from_millis(1), Duration::from_millis(50));
            sleep(&self.shared, backoff).await;
        }
    }

    /// Record a commit that the server acknowledged.
    fn acknowledge(&self, acked: Acked) {
        let mut state = self.shared.lock();
        state.trace(format!("c{} commits tid {:016x}", self.id, acked.tid));
        for (oid, _) in &acked.records {
            let known = state.known.entry(*oid).or_default();
            *known = (*known).max(acked.tid);
        }
        state.acked.push(acked);
    }

    /// Load the current serial of `oid`. Count a stale read: a serial
    /// older than the newest commit of `oid` that a client saw
    /// acknowledged before the load.
    async fn serial(&self, client: &Client, oid: u64) -> Result<Tid, Outcome> {
        let known = self.shared.lock().known.get(&oid).copied().unwrap_or(0);
        let serial = match client.load(Oid(oid)).await {
            Ok((_, serial)) => serial,
            Err(Error::Server { class, .. }) if class.ends_with("POSKeyError") => Tid::ZERO,
            Err(Error::Disconnected) => return Err(Outcome::Lost { in_finish: false }),
            Err(_) => return Err(Outcome::Retry),
        };
        if serial.0 < known {
            let mut state = self.shared.lock();
            state.stale_reads += 1;
            state.trace(format!(
                "c{} reads a stale serial {:016x} of oid {oid}, after {known:016x}",
                self.id, serial.0
            ));
        }
        Ok(serial)
    }

    /// Try one transaction: read the current serials of one or two oids,
    /// store new records, vote and finish.
    async fn commit(&mut self, client: &Client, t: usize, attempt: usize) -> Outcome {
        // Half of the transactions write oid 1, a hot object as a catalog
        // or a counter is.
        let first = if self.rng.chance(0.5) {
            1
        } else {
            1 + self.rng.below(OIDS)
        };
        let mut oids = vec![first];
        if self.rng.chance(0.5) {
            let second = 1 + self.rng.below(OIDS);
            if second != first {
                oids.push(second);
            }
        }
        oids.sort_unstable();
        let mut records = Vec::new();
        for &oid in &oids {
            let serial = match self.serial(client, oid).await {
                Ok(serial) => serial,
                Err(outcome) => return outcome,
            };
            let value = format!("c{} t{t} a{attempt} oid {oid}", self.id);
            records.push((oid, serial, record(value.as_bytes())));
        }
        let lost = |error: Error| match error {
            Error::Disconnected => Outcome::Lost { in_finish: false },
            _ => Outcome::Retry,
        };
        let user = format!("c{}", self.id);
        let mut txn = match client.begin(user.as_bytes(), b"", b"").await {
            Ok(txn) => txn,
            Err(error) => return lost(error),
        };
        for (oid, serial, data) in &records {
            if let Err(error) = txn.store(Oid(*oid), *serial, data).await {
                return lost(error);
            }
        }
        if let Err(error) = txn.vote().await {
            if let Error::Conflict { oid, .. } = &error {
                self.trace(&format!("conflicts on oid {}", oid.0));
            }
            return lost(error);
        }
        if let Some(stall) = self.stall.take() {
            self.trace(&format!("stalls {} s and keeps its locks", stall.as_secs()));
            sleep(&self.shared, stall).await;
        }
        match txn.finish().await {
            Ok(tid) => Outcome::Acked(Acked {
                tid: tid.0,
                records: records
                    .into_iter()
                    .map(|(oid, _, data)| (oid, data))
                    .collect(),
            }),
            Err(Error::Disconnected) => Outcome::Lost { in_finish: true },
            Err(_) => Outcome::Retry,
        }
    }
}

/// A ZODB record of a `PersistentMapping` that holds `value`.
pub(crate) fn record(value: &[u8]) -> Vec<u8> {
    let mut pickle = b"\x80\x03cpersistent.mapping\nPersistentMapping\nq\x00.".to_vec();
    pickle.extend_from_slice(b"\x80\x03}q\x00X\x01\x00\x00\x00vq\x01C");
    pickle.push(value.len() as u8);
    pickle.extend_from_slice(value);
    pickle.extend_from_slice(b"q\x02s.");
    pickle
}
