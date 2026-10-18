//! The clients of a run: `zoe-client` on the runtime of the simulation.
//! They commit records to a small set of shared oids with one hot oid, so
//! that their votes meet on locks and conflicts.
//!
//! A client reads the current serial of each oid with `loadBefore`, then
//! stores, votes and finishes. With blobs, a transaction can also store a
//! blob to an oid of the blob oids, as `ZEO.ClientStorage` uploads it. A
//! conflict aborts and tries again. A lost connection connects again, as
//! `ZEO.ClientStorage` does. A `tpc_finish` whose reply does not arrive has
//! an unknown outcome, and the client does not count it.
//!
//! A client tells the driver when its commit reaches a phase where a
//! planned crash can come: staged before the vote, locked after the vote,
//! and acknowledged after the reply of `tpc_finish`.
//!
//! With a hot `Length`, the hot oid holds a `BTrees.Length.Length`. A
//! transaction that writes it stores the value that it read plus 1, so the
//! writers of the hot oid conflict, and tier 1 of the server merges them.
//!
//! A split writer (`Simulation::split_merges`) declares `server_writes`,
//! and each of its transactions inserts one key into the tree of
//! `tree.rs` instead: it reads the node and the bucket of the key, and
//! stores the bucket with the node as read-current, or, when the bucket is
//! full, the node, the bucket and a new bucket of the split.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use zoe_client::{Client, Error, Options};
use zoe_core::{Oid, Tid};

use super::rng::Rng;
use super::server::SimRuntime;
use super::tree::{self, MAX_KEYS};
use super::world::{Acked, Shared, TreeCommit, phase, sleep};
use crate::protocol::STORAGE_NAME;
use crate::protocol::negotiation::SERVER_WRITES;

/// The oids that the clients share.
const OIDS: u64 = 6;

/// The hot oid: half of the transactions write it.
pub(crate) const HOT: u64 = 1;

/// The class pickle of a `BTrees.Length.Length` and the start of its state
/// pickle, as the pickler of tier 1 writes them.
const LENGTH: &[u8] = b"\x80\x03cBTrees.Length\nLength\nq\x00.\x80\x03";

/// The oids of the blobs, after the other oids. An oid holds blobs or
/// other records, never both, as in ZODB.
const BLOB_OIDS: u64 = 3;

/// The chance that a transaction stores a blob, when the run has blobs.
const BLOB_CHANCE: f64 = 0.3;

/// The largest blob. A blob of more than `BLOB_CHUNK` bytes goes up in two
/// chunks.
const MAX_BLOB: u64 = 70_000;

/// The pickle of a `ZODB.blob.Blob`.
const BLOB_PICKLE: &[u8] = b"\x80\x03cZODB.blob\nBlob\nq\x00.\x80\x03N.";

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
    /// The client also commits blobs.
    pub(crate) blobs: bool,
    /// The hot oid holds a `Length` that the client increments.
    pub(crate) hot_length: bool,
    /// The client declares `server_writes` and inserts into the tree.
    pub(crate) split_merges: bool,
    /// The connection of the client, when it is a split writer.
    pub(crate) conn: Option<u64>,
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
            let connected = if self.split_merges {
                let options = Options {
                    extensions: vec![SERVER_WRITES.to_owned()],
                    ..Options::default()
                };
                Client::open(runtime, ADDR, STORAGE_NAME, &options).await
            } else {
                Client::connect_with(runtime, ADDR, STORAGE_NAME).await
            };
            match connected {
                Ok(client) => {
                    self.trace("registered");
                    if self.split_merges {
                        // The connect made the newest conn of this client.
                        let state = self.shared.lock();
                        let mine = state.conns.iter().filter(|(_, c)| c.client == self.id);
                        self.conn = mine.map(|(id, _)| *id).max();
                    }
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

    /// Record a commit that the server acknowledged. It reached the phase
    /// `acknowledged`.
    fn acknowledge(&self, acked: Acked) {
        let mut state = self.shared.lock();
        state.trace(format!("c{} commits tid {:016x}", self.id, acked.tid));
        for (oid, _) in &acked.records {
            let known = state.known.entry(*oid).or_default();
            *known = (*known).max(acked.tid);
        }
        state.acked.push(acked);
        drop(state);
        self.reach(phase::ACKNOWLEDGED);
    }

    /// Tell the driver that a commit of the client reached `phase`. When a
    /// planned crash comes there, the driver crashes the server after this
    /// step of the client.
    fn reach(&self, phase: &'static str) {
        let mut state = self.shared.lock();
        if state.crash_at(phase) {
            state.trace(format!("c{} reaches the phase {phase}", self.id));
            state.crash_now = Some(phase);
        }
    }

    /// Load the current serial of `oid`. Count a stale read: a serial
    /// older than the newest commit of `oid` that a client saw
    /// acknowledged before the load.
    async fn serial(&self, client: &Client, oid: u64) -> Result<Tid, Outcome> {
        self.load(client, oid).await.map(|(serial, _)| serial)
    }

    /// Load the current serial and the current record of `oid`, as
    /// `serial` does. A new oid gives `Tid::ZERO` and no record.
    async fn load(&self, client: &Client, oid: u64) -> Result<(Tid, Option<Vec<u8>>), Outcome> {
        let known = self.shared.lock().known.get(&oid).copied().unwrap_or(0);
        let (serial, data) = match client.load(Oid(oid)).await {
            Ok((data, serial)) => (serial, Some(data)),
            Err(Error::Server { class, .. }) if class.ends_with("POSKeyError") => (Tid::ZERO, None),
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
        Ok((serial, data))
    }

    /// Try one transaction: read the current serials of one or two oids,
    /// store new records, vote and finish.
    async fn commit(&mut self, client: &Client, t: usize, attempt: usize) -> Outcome {
        if self.split_merges {
            return self.insert(client, t).await;
        }
        // Half of the transactions write oid 1, a hot object as a catalog
        // or a counter is.
        let first = if self.rng.chance(0.5) {
            HOT
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
            let (serial, data) = match self.load(client, oid).await {
                Ok(loaded) => loaded,
                Err(outcome) => return outcome,
            };
            if self.hot_length && oid == HOT {
                let value = data.as_deref().map_or(Some(0), length_value);
                let Some(value) = value else {
                    self.trace("reads a hot oid that is not a Length");
                    return Outcome::Retry;
                };
                records.push((oid, serial, length(value + 1)));
                continue;
            }
            let value = format!("c{} t{t} a{attempt} oid {oid}", self.id);
            records.push((oid, serial, record(value.as_bytes())));
        }
        let mut blob = None;
        if self.blobs && self.rng.chance(BLOB_CHANCE) {
            let oid = OIDS + 1 + self.rng.below(BLOB_OIDS);
            let size = self.rng.below(MAX_BLOB + 1) as usize;
            let serial = match self.serial(client, oid).await {
                Ok(serial) => serial,
                Err(outcome) => return outcome,
            };
            let value = format!("c{} t{t} a{attempt} blob {oid} ", self.id);
            let bytes: Vec<u8> = value.bytes().cycle().take(size).collect();
            blob = Some((oid, serial, bytes));
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
        if let Some((oid, serial, bytes)) = &blob
            && let Err(error) = txn.store_blob(Oid(*oid), *serial, BLOB_PICKLE, bytes).await
        {
            return lost(error);
        }
        self.reach(phase::STAGED);
        if let Err(error) = txn.vote().await {
            if let Error::Conflict { oid, .. } = &error {
                self.trace(&format!("conflicts on oid {}", oid.0));
            }
            return lost(error);
        }
        self.reach(phase::LOCKED);
        if let Some(stall) = self.stall.take() {
            self.trace(&format!("stalls {} s and keeps its locks", stall.as_secs()));
            sleep(&self.shared, stall).await;
        }
        match txn.finish().await {
            Ok(tid) => {
                let mut stored: Vec<(u64, Vec<u8>)> = records
                    .into_iter()
                    .map(|(oid, _, data)| (oid, data))
                    .collect();
                let mut blobs = Vec::new();
                if let Some((oid, _, bytes)) = blob {
                    stored.push((oid, BLOB_PICKLE.to_vec()));
                    blobs.push((oid, bytes));
                }
                Outcome::Acked(Acked {
                    tid: tid.0,
                    records: stored,
                    blobs,
                    tree: None,
                })
            }
            Err(Error::Disconnected) => Outcome::Lost { in_finish: true },
            Err(_) => Outcome::Retry,
        }
    }
}

impl SimClient {
    /// Try one transaction of a split writer: insert the key of `t` into
    /// the tree, vote and finish.
    ///
    /// The client reads the node, then the bucket whose range holds the
    /// key, then the node again: a node that changed between gives a new
    /// try, so the two records are one state of the tree, as a snapshot of
    /// ZODB gives them. A bucket that stays within `MAX_KEYS` keys is the
    /// only record that it stores, with the node as read-current, as
    /// BTrees marks it. A full bucket splits: the client stores the node
    /// with the new bucket as a child, the bucket and the new bucket.
    async fn insert(&mut self, client: &Client, t: usize) -> Outcome {
        let (root, key) = {
            let state = self.shared.lock();
            let root = state.tree_root.expect("a run of split writers has a tree");
            (root, tree::key(t, state.clients, self.id))
        };
        let (serial, data) = match self.load(client, root).await {
            Ok(loaded) => loaded,
            Err(outcome) => return outcome,
        };
        let Some(mut node) = data.as_deref().and_then(tree::node) else {
            self.trace("reads a root of the tree that is not a node");
            return Outcome::Retry;
        };
        let child = node.child_for(key);
        let (child_serial, data) = match self.load(client, child).await {
            Ok(loaded) => loaded,
            Err(outcome) => return outcome,
        };
        let Some(mut bucket) = data.as_deref().and_then(tree::bucket) else {
            self.trace(&format!("reads a child {child} that is not a bucket"));
            return Outcome::Retry;
        };
        match self.serial(client, root).await {
            Ok(again) if again == serial => {}
            Ok(_) => return Outcome::Retry,
            Err(outcome) => return outcome,
        }
        bucket.insert(key, self.id as i128);
        let lost = |error: Error| match error {
            Error::Disconnected => Outcome::Lost { in_finish: false },
            _ => Outcome::Retry,
        };
        let mut writes = Vec::new();
        let mut created = Vec::new();
        let mut current = None;
        if bucket.items.len() <= MAX_KEYS {
            writes.push((child, child_serial, tree::bucket_record(&bucket)));
            current = Some(root);
        } else {
            let new = match client.new_oids(1).await {
                Ok(oids) => oids[0].0,
                Err(error) => return lost(error),
            };
            let (left, right) = bucket.split(new);
            node.insert_after(child, right.items[0].0, new);
            writes.push((root, serial, tree::node_record(&node)));
            writes.push((child, child_serial, tree::bucket_record(&left)));
            writes.push((new, Tid::ZERO, tree::bucket_record(&right)));
            created.push(new);
        }
        let conn = self.conn.expect("a split writer knows its connection");
        // The hand-offs of an earlier finish on the connection do not count.
        self.shared.lock().handed.remove(&conn);
        let user = format!("c{}", self.id);
        let mut txn = match client.begin(user.as_bytes(), b"", b"").await {
            Ok(txn) => txn,
            Err(error) => return lost(error),
        };
        for (oid, serial, data) in &writes {
            if let Err(error) = txn.store(Oid(*oid), *serial, data).await {
                return lost(error);
            }
        }
        if let Some(oid) = current
            && let Err(error) = txn.check_current_serial(Oid(oid), serial).await
        {
            return lost(error);
        }
        self.reach(phase::STAGED);
        let resolved: Vec<u64> = match txn.vote_resolved().await {
            Ok(resolved) => resolved.into_iter().map(|oid| oid.0).collect(),
            Err(error) => {
                if let Error::Conflict { oid, .. } = &error {
                    self.trace(&format!("conflicts on oid {}", oid.0));
                }
                if matches!(error, Error::Disconnected) && !created.is_empty() {
                    self.shared.lock().interrupted_split_votes += 1;
                }
                return lost(error);
            }
        };
        let mut commit = TreeCommit {
            key,
            node: root,
            handed_off: 0,
            stored: writes.iter().map(|(oid, _, _)| *oid).collect(),
            created,
            resolved,
        };
        let unstored = commit.wrote_unstored();
        if unstored {
            self.trace(&format!(
                "votes a split merge: resolves {:?}, stored {:?}",
                commit.resolved, commit.stored
            ));
        }
        self.reach(phase::LOCKED);
        match txn.finish().await {
            Ok(tid) => {
                // The finish notes its hand-offs before it replies.
                commit.handed_off = self.shared.lock().handed.remove(&conn).unwrap_or(0);
                let records = writes
                    .into_iter()
                    .filter(|(oid, _, _)| !commit.resolved.contains(oid))
                    .map(|(oid, _, data)| (oid, data))
                    .collect();
                Outcome::Acked(Acked {
                    tid: tid.0,
                    records,
                    blobs: Vec::new(),
                    tree: Some(Box::new(commit)),
                })
            }
            Err(Error::Disconnected) => {
                if unstored {
                    self.shared.lock().interrupted_split_finishes += 1;
                }
                Outcome::Lost { in_finish: true }
            }
            Err(_) => Outcome::Retry,
        }
    }
}

/// A ZODB record of a `BTrees.Length.Length` of `value`, as the pickler of
/// tier 1 writes it.
pub(crate) fn length(value: u32) -> Vec<u8> {
    let mut pickle = LENGTH.to_vec();
    let le = value.to_le_bytes();
    match value {
        0..=0xff => pickle.extend([b'K', le[0]]),
        0x100..=0xffff => pickle.extend([b'M', le[0], le[1]]),
        _ => {
            pickle.push(b'J');
            pickle.extend(le);
        }
    }
    pickle.push(b'.');
    pickle
}

/// Return the value of a record of [`length`], or `None` for another
/// record.
pub(crate) fn length_value(pickle: &[u8]) -> Option<u32> {
    let state = pickle.strip_prefix(LENGTH)?.strip_suffix(b".")?;
    match *state {
        [b'K', a] => Some(u32::from(a)),
        [b'M', a, b] => Some(u32::from(u16::from_le_bytes([a, b]))),
        [b'J', a, b, c, d] => u32::try_from(i32::from_le_bytes([a, b, c, d])).ok(),
        _ => None,
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
