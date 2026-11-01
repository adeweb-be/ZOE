//! The load generator: `zoe-bench load` and `zoe-bench saturate`.
//!
//! A run first creates a store of synthetic objects through one client.
//! Then each client opens its own connection and runs its transactions,
//! one after the other. A transaction reads its objects with `loadBefore`
//! at the last tid that the client knows, with at most `pipeline` loads
//! in flight. A writing transaction then stores each object back with the
//! serial that it read, votes and finishes. The server takes the locks of
//! the oids at the vote, so the time of the vote is the lock wait. A vote
//! that fails with a conflict aborts, and the client runs the same
//! transaction again: a retry. Each retry counts.
//!
//! `saturate` runs the same workload with 1, 2, 4 and more clients, until
//! the p99 of `loadBefore` passes a ceiling. Each step first runs for a
//! warm-up time that does not count, then for a set time.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::stream::{self, StreamExt};
use serde::Serialize;
use zoe_client::{Client, Error as ClientError};
use zoe_core::{Oid, Tid};

use crate::Error;
use crate::machine::{Machine, Run};
use crate::schedule::{Schedule, Zipf};

/// The version of the report. A change that a reader of the previous
/// version cannot read increments it.
pub const SCHEMA_VERSION: u32 = 1;

/// The size of the value in the pickle of each object, in bytes.
const VALUE_SIZE: usize = 1000;

/// The most stores in one transaction of the setup.
const SETUP_BATCH: usize = 1000;

/// The most attempts of one transaction. Past it, the run fails, so a
/// livelock does not hang the run.
pub const MAX_ATTEMPTS: u64 = 1000;

/// The user and the description of each transaction of a run.
const USER: &[u8] = b"zoe-bench";
const DESCRIPTION: &[u8] = b"zoe-bench";

/// The workload of each client: what it reads and writes.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Workload {
    /// The storage name that each client gives to `register`.
    pub storage: String,
    /// The most `loadBefore` calls in flight for one client.
    pub pipeline: usize,
    /// The number of objects that the run creates first.
    pub objects: usize,
    /// The number of objects that each transaction reads.
    pub txn_size: usize,
    /// The probability that a transaction writes back its objects.
    pub write_ratio: f64,
    /// The exponent of the Zipf law that picks the objects.
    pub zipf: f64,
    /// The seed of the schedules of the clients.
    pub seed: u64,
    /// The size of the pickle of each object, in bytes.
    pub object_size: usize,
}

impl Workload {
    /// Return the size of the pickle of each object.
    pub fn record_size() -> usize {
        record(&[0; VALUE_SIZE]).len()
    }
}

/// The settings of a `load` run.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct LoadSettings {
    /// The number of clients, each with its own connection.
    pub clients: usize,
    /// The number of transactions of each client.
    pub transactions: usize,
    /// The workload of each client.
    #[serde(flatten)]
    pub workload: Workload,
}

/// The settings of a `saturate` run.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct SaturateSettings {
    /// The most clients of a step.
    pub max_clients: usize,
    /// The ceiling of the p99 of `loadBefore`, in seconds.
    pub p99_ceiling_seconds: f64,
    /// The time at the start of each step that does not count, in seconds.
    pub warmup_seconds: f64,
    /// The time of each step after the warm-up, in seconds.
    pub step_seconds: f64,
    /// The most transactions of each client in a step after the warm-up,
    /// or `None` for no limit. A step ends at the end of its time or at
    /// this limit, the first of the two.
    pub transactions: Option<usize>,
    /// The workload of each client.
    #[serde(flatten)]
    pub workload: Workload,
}

/// The numbers of one run of the workload by a number of clients.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Measure {
    /// The transactions of all clients: each one commits or reads only.
    pub transactions: u64,
    /// The transactions that only read.
    pub read_only: u64,
    /// The transactions that wrote and committed.
    pub commits: u64,
    /// The votes that failed with a conflict.
    pub conflicts: u64,
    /// The attempts after the first one of each transaction.
    pub retries: u64,
    /// The conflicts for each 1,000 commits, or `None` without a commit.
    pub conflicts_per_1000_commits: Option<f64>,
    /// The `loadBefore` calls, retries included.
    pub loads: u64,
    /// The time from the start of the clients to the end of the last one.
    pub elapsed_seconds: f64,
    /// The `loadBefore` calls for each second of `elapsed_seconds`.
    pub loads_per_second: f64,
    /// The commits for each second of `elapsed_seconds`.
    pub commits_per_second: f64,
    /// The median time of a `loadBefore` call, from the send of the call
    /// to its reply.
    pub load_p50_seconds: f64,
    /// The 99th percentile of the same times.
    pub load_p99_seconds: f64,
    /// The longest of the same times.
    pub load_max_seconds: f64,
    /// The median time of a vote: the server takes the locks and waits
    /// for them there. `None` without a vote.
    pub lock_wait_p50_seconds: Option<f64>,
    /// The 99th percentile of the same times.
    pub lock_wait_p99_seconds: Option<f64>,
}

/// One step of a `saturate` run.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Step {
    /// The number of clients of the step.
    pub clients: usize,
    /// The numbers of the step.
    #[serde(flatten)]
    pub measure: Measure,
}

/// The result of a `saturate` run.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Saturation {
    /// The steps, with more clients at each step. The last step is the
    /// first one over the ceiling, or the step of `max_clients`.
    pub steps: Vec<Step>,
    /// The loads per second of the last step under the ceiling, or `None`
    /// when no step is under it.
    pub loads_per_second_under_ceiling: Option<f64>,
    /// The clients of the same step.
    pub clients_under_ceiling: Option<usize>,
}

/// A report: one JSON document for one run of `load`, `saturate` or
/// `hot`.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Report<S, R> {
    /// `SCHEMA_VERSION`.
    pub schema_version: u32,
    /// `load`, `saturate` or `hot`.
    pub command: &'static str,
    /// The version of `zoe-bench`.
    pub version: &'static str,
    /// The machine of the clients.
    pub machine: Machine,
    /// When the run started, and the commit.
    pub run: Run,
    /// The settings of the run.
    pub settings: S,
    /// The numbers of the run.
    #[serde(flatten)]
    pub result: R,
}

impl<S, R> Report<S, R> {
    /// Return the report of a run of `command` that started at `run`.
    pub(crate) fn new(command: &'static str, run: Run, settings: S, result: R) -> Report<S, R> {
        Report {
            schema_version: SCHEMA_VERSION,
            command,
            version: env!("CARGO_PKG_VERSION"),
            machine: Machine::describe(),
            run,
            settings,
            result,
        }
    }
}

/// Run `zoe-bench load` against the server at `addr`.
pub async fn load(
    addr: SocketAddr,
    settings: LoadSettings,
) -> Result<Report<LoadSettings, Measure>, Error> {
    let run = Run::now();
    let oids = setup(addr, &settings.workload).await?;
    let plan = Plan::Count(settings.transactions);
    let measure = measure(addr, &settings.workload, &oids, settings.clients, plan).await?;
    Ok(Report::new("load", run, settings, measure))
}

/// Run `zoe-bench saturate` against the server at `addr`.
pub async fn saturate(
    addr: SocketAddr,
    settings: SaturateSettings,
) -> Result<Report<SaturateSettings, Saturation>, Error> {
    let run = Run::now();
    let oids = setup(addr, &settings.workload).await?;
    let mut saturation = Saturation {
        steps: Vec::new(),
        loads_per_second_under_ceiling: None,
        clients_under_ceiling: None,
    };
    let plan = Plan::Timed {
        warmup: Duration::from_secs_f64(settings.warmup_seconds),
        duration: Duration::from_secs_f64(settings.step_seconds),
        cap: settings.transactions,
    };
    for clients in steps(settings.max_clients) {
        let measure = measure(addr, &settings.workload, &oids, clients, plan).await?;
        // A step without a load has no p99, so it is not under the ceiling.
        let under = measure.loads > 0 && measure.load_p99_seconds <= settings.p99_ceiling_seconds;
        if under {
            saturation.loads_per_second_under_ceiling = Some(measure.loads_per_second);
            saturation.clients_under_ceiling = Some(clients);
        }
        saturation.steps.push(Step { clients, measure });
        if !under {
            break;
        }
    }
    Ok(Report::new("saturate", run, settings, saturation))
}

/// Return the numbers of clients of the steps of `saturate`: the powers of
/// two under `max`, then `max`.
pub fn steps(max: usize) -> Vec<usize> {
    let mut steps: Vec<usize> = std::iter::successors(Some(1usize), |n| n.checked_mul(2))
        .take_while(|&n| n < max)
        .collect();
    steps.push(max.max(1));
    steps
}

/// Create the objects of the store, and return their oids, the hottest
/// first.
async fn setup(addr: SocketAddr, workload: &Workload) -> Result<Vec<Oid>, Error> {
    let client = Client::connect(addr, &workload.storage).await?;
    let oids = client.new_oids(workload.objects).await?;
    let mut value = Value::new(u64::MAX);
    for batch in oids.chunks(SETUP_BATCH) {
        let mut txn = client.begin(USER, DESCRIPTION, b"").await?;
        for &oid in batch {
            txn.store(oid, Tid::ZERO, &value.next_record()).await?;
        }
        txn.vote().await?;
        txn.finish().await?;
    }
    client.close().await;
    Ok(oids)
}

/// How long the clients of a measure run.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Plan {
    /// Each client runs this number of transactions, and each one counts.
    Count(usize),
    /// Each client runs transactions until `warmup` plus `duration` ends.
    /// Only the transactions that start after `warmup` count. A client
    /// also stops after `cap` transactions that count.
    Timed {
        /// The time that does not count.
        warmup: Duration,
        /// The time that counts.
        duration: Duration,
        /// The most transactions that count, for each client.
        cap: Option<usize>,
    },
}

/// The times of a `Plan::Timed` measure.
#[derive(Clone, Copy, Debug)]
struct Window {
    /// The transactions that start at or after this time count.
    counts_from: Instant,
    /// No transaction starts at or after this time.
    end: Instant,
    cap: Option<usize>,
}

/// Run the workload with `clients` clients, and return its numbers.
async fn measure(
    addr: SocketAddr,
    workload: &Workload,
    oids: &[Oid],
    clients: usize,
    plan: Plan,
) -> Result<Measure, Error> {
    let oids: Arc<[Oid]> = oids.into();
    let zipf = Zipf::new(oids.len(), workload.zipf);
    let mut connections = Vec::with_capacity(clients);
    for _ in 0..clients {
        connections.push(Client::connect(addr, &workload.storage).await?);
    }

    let start = Instant::now();
    let (count, window) = match plan {
        Plan::Count(count) => (count, None),
        Plan::Timed {
            warmup,
            duration,
            cap,
        } => {
            let counts_from = start + warmup;
            let window = Window {
                counts_from,
                end: counts_from + duration,
                cap,
            };
            (0, Some(window))
        }
    };
    let tasks: Vec<_> = connections
        .into_iter()
        .enumerate()
        .map(|(index, client)| {
            let schedule = Schedule::new(
                workload.seed,
                index as u64,
                zipf.clone(),
                workload.txn_size,
                workload.write_ratio,
            );
            let task = ClientTask {
                client,
                oids: oids.clone(),
                schedule,
                count,
                window,
                pipeline: workload.pipeline,
                value: Value::new(index as u64),
            };
            tokio::spawn(task.run())
        })
        .collect();
    let mut tally = Tally::default();
    let mut failure = None;
    let mut done = Vec::with_capacity(clients);
    for task in tasks {
        match task.await.expect("a client task panicked") {
            Ok((one, client)) => {
                tally.add(one);
                done.push(client);
            }
            Err(e) => failure = failure.or(Some(e)),
        }
    }
    // The time counts from the end of the warm-up.
    let elapsed = Instant::now().saturating_duration_since(window.map_or(start, |w| w.counts_from));
    // Close the connections after the measure, so the time of a close
    // does not count.
    for client in done {
        client.close().await;
    }
    match failure {
        Some(e) => Err(e),
        None => Ok(tally.measure(elapsed)),
    }
}

/// The numbers that one client collects.
#[derive(Debug, Default)]
struct Tally {
    transactions: u64,
    read_only: u64,
    commits: u64,
    conflicts: u64,
    retries: u64,
    /// The time of each `loadBefore` call, in nanoseconds.
    loads: Vec<u64>,
    /// The time of each vote, in nanoseconds.
    votes: Vec<u64>,
}

impl Tally {
    fn add(&mut self, other: Tally) {
        self.transactions += other.transactions;
        self.read_only += other.read_only;
        self.commits += other.commits;
        self.conflicts += other.conflicts;
        self.retries += other.retries;
        self.loads.extend(other.loads);
        self.votes.extend(other.votes);
    }

    fn measure(mut self, elapsed: Duration) -> Measure {
        self.loads.sort_unstable();
        self.votes.sort_unstable();
        let seconds = elapsed.as_secs_f64().max(f64::MIN_POSITIVE);
        let loads = self.loads.len() as u64;
        Measure {
            transactions: self.transactions,
            read_only: self.read_only,
            commits: self.commits,
            conflicts: self.conflicts,
            retries: self.retries,
            conflicts_per_1000_commits: (self.commits > 0)
                .then(|| self.conflicts as f64 * 1000.0 / self.commits as f64),
            loads,
            elapsed_seconds: elapsed.as_secs_f64(),
            loads_per_second: loads as f64 / seconds,
            commits_per_second: self.commits as f64 / seconds,
            load_p50_seconds: percentile(&self.loads, 0.50).unwrap_or(0.0),
            load_p99_seconds: percentile(&self.loads, 0.99).unwrap_or(0.0),
            load_max_seconds: percentile(&self.loads, 1.0).unwrap_or(0.0),
            lock_wait_p50_seconds: percentile(&self.votes, 0.50),
            lock_wait_p99_seconds: percentile(&self.votes, 0.99),
        }
    }
}

/// Return the percentile `p` of `sorted`, in seconds, by the nearest
/// rank, or `None` when `sorted` is empty. `sorted` holds nanoseconds.
pub fn percentile(sorted: &[u64], p: f64) -> Option<f64> {
    if sorted.is_empty() {
        return None;
    }
    let rank = (p * sorted.len() as f64).ceil() as usize;
    let index = rank.clamp(1, sorted.len()) - 1;
    Some(sorted[index] as f64 / 1e9)
}

/// One client and its part of the run.
struct ClientTask {
    client: Client,
    oids: Arc<[Oid]>,
    schedule: Schedule,
    /// The number of transactions without a window.
    count: usize,
    window: Option<Window>,
    pipeline: usize,
    value: Value,
}

impl ClientTask {
    /// Run the transactions of the client, and return its numbers and
    /// its connection.
    async fn run(mut self) -> Result<(Tally, Client), Error> {
        let mut tally = Tally::default();
        // The numbers of the warm-up. They do not count.
        let mut warmup = Tally::default();
        loop {
            let now = Instant::now();
            let counts = match self.window {
                None if tally.transactions >= self.count as u64 => break,
                None => true,
                Some(w) if now >= w.end => break,
                Some(w) if w.cap.is_some_and(|cap| tally.transactions >= cap as u64) => break,
                Some(w) => now >= w.counts_from,
            };
            let step = self.schedule.next_step();
            let oids: Vec<Oid> = step.ranks.iter().map(|&rank| self.oids[rank]).collect();
            let into = if counts { &mut tally } else { &mut warmup };
            self.transaction(&oids, step.write, into).await?;
            into.transactions += 1;
        }
        Ok((tally, self.client))
    }

    /// Run one transaction until it commits or only reads.
    async fn transaction(
        &mut self,
        oids: &[Oid],
        write: bool,
        tally: &mut Tally,
    ) -> Result<(), Error> {
        // The tid of the last commit that a conflict named. The next
        // attempt reads after it, also when its invalidation is late.
        let mut floor = Tid::ZERO;
        for attempt in 0..MAX_ATTEMPTS {
            if attempt > 0 {
                tally.retries += 1;
            }
            let last = self.client.last_transaction().max(floor);
            let serials = self
                .read(oids, Tid(last.0.saturating_add(1)), tally)
                .await?;
            if !write {
                tally.read_only += 1;
                return Ok(());
            }
            let mut txn = self.client.begin(USER, DESCRIPTION, b"").await?;
            for (&oid, &serial) in oids.iter().zip(&serials) {
                txn.store(oid, serial, &self.value.next_record()).await?;
            }
            let vote = Instant::now();
            let result = txn.vote().await;
            tally.votes.push(nanos(vote.elapsed()));
            match result {
                Ok(()) => {
                    txn.finish().await?;
                    tally.commits += 1;
                    return Ok(());
                }
                Err(ClientError::Conflict { serials, .. }) => {
                    tally.conflicts += 1;
                    if let Some((committed, _)) = serials {
                        floor = floor.max(committed);
                    }
                }
                Err(e) => return Err(e.into()),
            }
        }
        Err(Error::TooManyRetries)
    }

    /// Load each of `oids` as of just before `before`, with at most
    /// `pipeline` calls in flight, and return their serials.
    async fn read(&self, oids: &[Oid], before: Tid, tally: &mut Tally) -> Result<Vec<Tid>, Error> {
        let client = &self.client;
        let mut replies = stream::iter(oids.iter().copied())
            .map(|oid| async move {
                let start = Instant::now();
                let record = client.load_before(oid, before).await;
                (oid, start.elapsed(), record)
            })
            .buffered(self.pipeline);
        let mut serials = Vec::with_capacity(oids.len());
        while let Some((oid, time, record)) = replies.next().await {
            tally.loads.push(nanos(time));
            serials.push(record?.ok_or(Error::Missing(oid))?.tid);
        }
        Ok(serials)
    }
}

fn nanos(duration: Duration) -> u64 {
    u64::try_from(duration.as_nanos()).unwrap_or(u64::MAX)
}

/// The synthetic values that a client writes: each one is new, and none
/// holds real data.
#[derive(Debug)]
struct Value {
    client: u64,
    count: u64,
}

impl Value {
    fn new(client: u64) -> Value {
        Value { client, count: 0 }
    }

    /// Return the record of the next value.
    fn next_record(&mut self) -> Vec<u8> {
        self.count += 1;
        let word = (self.client.rotate_left(32) ^ self.count).to_le_bytes();
        let value: Vec<u8> = word.iter().copied().cycle().take(VALUE_SIZE).collect();
        record(&value)
    }
}

/// Return a ZODB record of a `PersistentMapping` that maps `v` to the bytes
/// `value`: the class pickle, then the state pickle, in protocol 3.
pub fn record(value: &[u8]) -> Vec<u8> {
    let mut pickle = b"\x80\x03cpersistent.mapping\nPersistentMapping\nq\x00.".to_vec();
    pickle.extend_from_slice(b"\x80\x03}q\x00X\x01\x00\x00\x00vq\x01B");
    let length = u32::try_from(value.len()).expect("a value under 4 GiB");
    pickle.extend_from_slice(&length.to_le_bytes());
    pickle.extend_from_slice(value);
    pickle.extend_from_slice(b"q\x02s.");
    pickle
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;

    #[test]
    fn the_percentile_takes_the_nearest_rank() {
        let sorted: Vec<u64> = (1..=100).map(|n| n * 1_000_000).collect();
        assert_eq!(percentile(&sorted, 0.50), Some(0.050));
        assert_eq!(percentile(&sorted, 0.99), Some(0.099));
        assert_eq!(percentile(&sorted, 1.0), Some(0.100));
        assert_eq!(percentile(&[7_000], 0.99), Some(0.000_007));
        assert_eq!(percentile(&[], 0.5), None);
    }

    #[test]
    fn the_steps_double_up_to_the_most_clients() {
        assert_eq!(steps(1), [1]);
        assert_eq!(steps(16), [1, 2, 4, 8, 16]);
        assert_eq!(steps(10), [1, 2, 4, 8, 10]);
    }

    #[test]
    fn the_measure_counts_conflicts_for_each_1000_commits() {
        let tally = Tally {
            transactions: 10,
            read_only: 6,
            commits: 4,
            conflicts: 2,
            retries: 2,
            loads: vec![3, 1, 2],
            votes: vec![],
        };
        let measure = tally.measure(Duration::from_secs(2));
        assert_eq!(measure.conflicts_per_1000_commits, Some(500.0));
        assert_eq!(measure.loads, 3);
        assert_eq!(measure.loads_per_second, 1.5);
        assert_eq!(measure.commits_per_second, 2.0);
        assert_eq!(measure.load_max_seconds, 3e-9);
        assert_eq!(measure.lock_wait_p99_seconds, None);
    }

    #[test]
    fn a_record_is_a_class_pickle_then_a_state_pickle() {
        let record = record(b"abc");
        let class = b"\x80\x03cpersistent.mapping\nPersistentMapping\nq\x00.";
        assert!(record.starts_with(class));
        assert!(record[class.len()..].starts_with(b"\x80\x03}"));
        assert!(record.ends_with(b"\x03\x00\x00\x00abcq\x02s."));
    }

    fn keys(value: &serde_json::Value) -> BTreeSet<String> {
        value.as_object().unwrap().keys().cloned().collect()
    }

    fn required(schemas: &[&serde_json::Value]) -> BTreeSet<String> {
        schemas
            .iter()
            .flat_map(|schema| schema["required"].as_array().unwrap())
            .map(|key| key.as_str().unwrap().to_owned())
            .collect()
    }

    fn sample_measure() -> Measure {
        Tally {
            transactions: 2,
            read_only: 1,
            commits: 1,
            conflicts: 1,
            retries: 1,
            loads: vec![1, 2],
            votes: vec![3],
        }
        .measure(Duration::from_secs(1))
    }

    fn sample_workload() -> Workload {
        Workload {
            storage: "1".into(),
            pipeline: 1,
            objects: 10,
            txn_size: 1,
            write_ratio: 0.5,
            zipf: 1.0,
            seed: 0,
            object_size: Workload::record_size(),
        }
    }

    #[test]
    fn the_schema_requires_each_key_of_a_report() {
        let schema: serde_json::Value =
            serde_json::from_str(include_str!("../../../bench/schema/zoe-bench.schema.json"))
                .unwrap();
        let defs = &schema["$defs"];
        let (load_schema, saturate_schema) = (&schema["oneOf"][0], &schema["oneOf"][1]);

        let settings = LoadSettings {
            clients: 2,
            transactions: 1,
            workload: sample_workload(),
        };
        let report = Report::new("load", Run::now(), settings, sample_measure());
        let report = serde_json::to_value(&report).unwrap();
        assert_eq!(keys(&report), required(&[load_schema, &defs["measure"]]));
        assert_eq!(
            keys(&report["settings"]),
            required(&[&defs["load_settings"], &defs["workload"]])
        );
        assert_eq!(keys(&report["machine"]), required(&[&defs["machine"]]));
        assert_eq!(keys(&report["run"]), required(&[&defs["run"]]));

        let settings = SaturateSettings {
            max_clients: 2,
            p99_ceiling_seconds: 0.01,
            warmup_seconds: 0.2,
            step_seconds: 1.0,
            transactions: None,
            workload: sample_workload(),
        };
        let saturation = Saturation {
            steps: vec![Step {
                clients: 1,
                measure: sample_measure(),
            }],
            loads_per_second_under_ceiling: Some(2.0),
            clients_under_ceiling: Some(1),
        };
        let report = Report::new("saturate", Run::now(), settings, saturation);
        let report = serde_json::to_value(&report).unwrap();
        assert_eq!(keys(&report), required(&[saturate_schema]));
        assert_eq!(
            keys(&report["settings"]),
            required(&[&defs["saturate_settings"], &defs["workload"]])
        );
        assert_eq!(
            keys(&report["steps"][0]),
            required(&[&defs["step"], &defs["measure"]])
        );
    }

    #[test]
    fn each_value_is_new() {
        let mut one = Value::new(0);
        let mut two = Value::new(1);
        let first = one.next_record();
        assert_ne!(first, one.next_record());
        assert_ne!(first, two.next_record());
        assert_eq!(first.len(), Workload::record_size());
    }
}
