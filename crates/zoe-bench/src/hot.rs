//! `zoe-bench hot`: writers of one hot object (F7, N5).
//!
//! A run creates one object, then `writers` clients each increment it
//! `transactions` times. The object is a `BTrees.Length.Length`, which tier
//! 1 of the server merges, or a `persistent.mapping.PersistentMapping`,
//! which no tier merges. With vote-time locks, a writer loads the current
//! record and votes: a stale serial is a conflict that tier 1 merges, or a
//! `ConflictError`, and the writer tries again. With early locks, a writer
//! takes the intent lock of the object first (`lock_intent`, ADR 0045),
//! then loads the record of the serial that the grant returns, so it never
//! holds a stale state.
//!
//! The report gives the commits, the `ConflictError`s, the commits per
//! second and, from `GET /metrics` of the admin port of the server, the
//! syncs and the commits that they made durable, the lock hand-offs and the
//! intent locks of the run.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Instant;

use serde::Serialize;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use zoe_client::{Client, Error as ClientError, Options, Tokio};
use zoe_core::{Oid, Tid};

use crate::load::{Error, MAX_ATTEMPTS};
use crate::machine::{Machine, Run};

/// The version of the report.
pub const SCHEMA_VERSION: u32 = 1;

/// The class pickle of a `BTrees.Length.Length` and the start of its
/// state pickle, as the pickler of tier 1 writes them.
const LENGTH: &[u8] = b"\x80\x03cBTrees.Length\nLength\nq\x00.\x80\x03";

/// The class pickle of a `persistent.mapping.PersistentMapping` and the
/// start of its state, a dict whose key `n` holds 8 bytes.
const MAPPING: &[u8] = b"\x80\x03cpersistent.mapping\nPersistentMapping\nq\x00.\
\x80\x03}q\x00X\x01\x00\x00\x00nq\x01C\x08";

/// The end of the state pickle of the mapping.
const MAPPING_END: &[u8] = b"q\x02s.";

/// The object that the writers increment.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, clap::ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub enum Object {
    /// A `BTrees.Length.Length`: tier 1 merges it.
    Length,
    /// A `persistent.mapping.PersistentMapping`: no tier merges it.
    Mapping,
}

impl Object {
    /// Return the record of the object that holds `value`.
    pub fn record(self, value: u32) -> Vec<u8> {
        match self {
            Object::Length => {
                let mut pickle = LENGTH.to_vec();
                pickle.push(b'J');
                pickle.extend(value.to_le_bytes());
                pickle.push(b'.');
                pickle
            }
            Object::Mapping => {
                let mut pickle = MAPPING.to_vec();
                pickle.extend(u64::from(value).to_be_bytes());
                pickle.extend(MAPPING_END);
                pickle
            }
        }
    }

    /// Return the value of a record of the object, or `None` for another
    /// record.
    pub fn value(self, pickle: &[u8]) -> Option<u32> {
        match self {
            Object::Length => match *pickle.strip_prefix(LENGTH)?.strip_suffix(b".")? {
                [b'K', a] => Some(u32::from(a)),
                [b'M', a, b] => Some(u32::from(u16::from_le_bytes([a, b]))),
                [b'J', a, b, c, d] => u32::try_from(i32::from_le_bytes([a, b, c, d])).ok(),
                _ => None,
            },
            Object::Mapping => {
                let bytes = pickle.strip_prefix(MAPPING)?.strip_suffix(MAPPING_END)?;
                let value = u64::from_be_bytes(bytes.try_into().ok()?);
                u32::try_from(value).ok()
            }
        }
    }
}

/// The settings of a `hot` run.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct HotSettings {
    /// The storage name that each client gives to `register`.
    pub storage: String,
    /// The number of writers, each with its own connection.
    pub writers: usize,
    /// The increments of each writer.
    pub transactions: usize,
    /// The object of the run.
    pub object: Object,
    /// True when the writers take early write-intent locks.
    pub early_locks: bool,
}

/// The numbers of a `hot` run.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Hot {
    /// The commits of all writers.
    pub commits: u64,
    /// The votes that failed with a conflict.
    pub conflicts: u64,
    /// The value of the object at the end. It equals `commits`.
    pub value: u32,
    /// The time from the start of the writers to the end of the last one.
    pub elapsed_seconds: f64,
    /// The commits for each second of `elapsed_seconds`.
    pub commits_per_second: f64,
    /// The syncs of commit batches during the run, from `/metrics`, or
    /// `None` without an admin address.
    pub syncs: Option<f64>,
    /// The commits that these syncs made durable.
    pub synced_commits: Option<f64>,
    /// The mean commits of a sync: the mean `zoe_fsync_batch_size`.
    pub syncs_per_commit: Option<f64>,
    /// The lock hand-offs of the run (ADR 0044).
    pub lock_handoffs: Option<f64>,
    /// The intent locks of the run (ADR 0045).
    pub intent_locks: Option<f64>,
}

/// A report of a `hot` run.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct HotReport {
    /// `SCHEMA_VERSION`.
    pub schema_version: u32,
    /// `hot`.
    pub command: &'static str,
    /// The version of `zoe-bench`.
    pub version: &'static str,
    /// The machine of the clients.
    pub machine: Machine,
    /// When the run started, and the commit.
    pub run: Run,
    /// The settings of the run.
    pub settings: HotSettings,
    /// The numbers of the run.
    #[serde(flatten)]
    pub result: Hot,
}

/// The counters of `/metrics` that the report reads.
#[derive(Clone, Copy, Debug, Default)]
struct Counters {
    syncs: f64,
    synced: f64,
    handoffs: f64,
    intents: f64,
}

/// Run `zoe-bench hot` against the server at `addr`, with the admin port
/// at `admin` for the metrics.
pub async fn hot(
    addr: SocketAddr,
    admin: Option<SocketAddr>,
    settings: HotSettings,
) -> Result<HotReport, Error> {
    let run = Run::now();
    let options = Options {
        extensions: vec!["lock_intent".to_owned()],
        ..Options::default()
    };
    let setup = Client::open(Arc::new(Tokio), addr, &settings.storage, &options).await?;
    let oid = setup.new_oids(1).await?[0];
    let mut txn = setup.begin(b"zoe-bench", b"hot", b"").await?;
    txn.store(oid, Tid::ZERO, &settings.object.record(0))
        .await?;
    txn.vote().await?;
    txn.finish().await?;
    let before = match admin {
        Some(admin) => Some(counters(admin).await?),
        None => None,
    };
    let mut writers = Vec::with_capacity(settings.writers);
    for _ in 0..settings.writers {
        writers.push(Client::open(Arc::new(Tokio), addr, &settings.storage, &options).await?);
    }
    let start = Instant::now();
    let tasks: Vec<_> = writers
        .into_iter()
        .map(|client| {
            let settings = settings.clone();
            tokio::spawn(async move {
                let tally = write(&client, oid, &settings).await;
                client.close().await;
                tally
            })
        })
        .collect();
    let (mut commits, mut conflicts) = (0, 0);
    let mut failure = None;
    for task in tasks {
        match task.await.expect("a writer task panicked") {
            Ok((c, k)) => {
                commits += c;
                conflicts += k;
            }
            Err(error) => failure = failure.or(Some(error)),
        }
    }
    let elapsed = start.elapsed().as_secs_f64();
    if let Some(error) = failure {
        return Err(error);
    }
    let after = match admin {
        Some(admin) => Some(counters(admin).await?),
        None => None,
    };
    let (data, _) = setup.load(oid).await?;
    setup.close().await;
    let value = settings.object.value(&data).ok_or(Error::Missing(oid))?;
    let delta = before.zip(after).map(|(before, after)| Counters {
        syncs: after.syncs - before.syncs,
        synced: after.synced - before.synced,
        handoffs: after.handoffs - before.handoffs,
        intents: after.intents - before.intents,
    });
    let result = Hot {
        commits,
        conflicts,
        value,
        elapsed_seconds: elapsed,
        commits_per_second: commits as f64 / elapsed,
        syncs: delta.map(|d| d.syncs),
        synced_commits: delta.map(|d| d.synced),
        syncs_per_commit: delta.filter(|d| d.syncs > 0.0).map(|d| d.synced / d.syncs),
        lock_handoffs: delta.map(|d| d.handoffs),
        intent_locks: delta.map(|d| d.intents),
    };
    Ok(HotReport {
        schema_version: SCHEMA_VERSION,
        command: "hot",
        version: env!("CARGO_PKG_VERSION"),
        machine: Machine::describe(),
        run,
        settings,
        result,
    })
}

/// Run the increments of one writer. Return its commits and its conflicts.
async fn write(client: &Client, oid: Oid, settings: &HotSettings) -> Result<(u64, u64), Error> {
    let (mut commits, mut conflicts) = (0, 0);
    for _ in 0..settings.transactions {
        let mut attempts = 0;
        loop {
            attempts += 1;
            if attempts > MAX_ATTEMPTS {
                return Err(Error::TooManyRetries);
            }
            let (data, serial) = if settings.early_locks {
                let serial = client.lock_intent(&[oid]).await?[0];
                (client.load_serial(oid, serial).await?, serial)
            } else {
                client.load(oid).await?
            };
            let value = settings.object.value(&data).ok_or(Error::Missing(oid))?;
            let mut txn = client.begin(b"zoe-bench", b"hot", b"").await?;
            txn.store(oid, serial, &settings.object.record(value + 1))
                .await?;
            match txn.vote().await {
                Ok(()) => {
                    txn.finish().await?;
                    commits += 1;
                    break;
                }
                Err(ClientError::Conflict { .. }) => conflicts += 1,
                Err(error) => return Err(error.into()),
            }
        }
    }
    Ok((commits, conflicts))
}

/// Read the counters of the report from `GET /metrics` of `admin`.
async fn counters(admin: SocketAddr) -> Result<Counters, Error> {
    let mut stream = tokio::net::TcpStream::connect(admin)
        .await
        .map_err(ClientError::Io)?;
    let request = format!("GET /metrics HTTP/1.0\r\nHost: {admin}\r\n\r\n");
    stream
        .write_all(request.as_bytes())
        .await
        .map_err(ClientError::Io)?;
    let mut text = String::new();
    stream
        .read_to_string(&mut text)
        .await
        .map_err(ClientError::Io)?;
    // The first storage of the server is the one of the run.
    let sample = |name: &str| {
        text.lines()
            .filter(|line| line.starts_with(name) && line[name.len()..].starts_with('{'))
            .find_map(|line| line.rsplit_once(' ')?.1.parse::<f64>().ok())
            .unwrap_or(0.0)
    };
    Ok(Counters {
        syncs: sample("zoe_fsync_batch_size_count"),
        synced: sample("zoe_fsync_batch_size_sum"),
        handoffs: sample("zoe_lock_handoffs_total"),
        intents: sample("zoe_intent_locks_total"),
    })
}
