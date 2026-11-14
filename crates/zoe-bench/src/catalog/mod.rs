//! `zoe-bench catalog`: the catalog mirror and the text index of `zoe serve`
//! on a fixture, through the protocol, with no Python on the measured path
//! (G2, G3 and G4 of phase 6).
//!
//! A step of the harness makes the plans once in a Zope process and stores
//! them with the fixture (`plans`). `zoe-bench catalog` starts `zoe serve`
//! on a copy of the store, sends each stored plan with `catalog_query` and
//! a nil overlay at the snapshot of the last commit, and reads the reply.
//! Its scenarios run in this order:
//!
//! - `cold`: the pages of the store leave the page cache, then `zoe serve`
//!   opens it. The scenario gives the time to `zoe_catalog_mirror_ready`,
//!   the replies `not_ready` until then, the latency of the first answers,
//!   the time to the first answer of the text index, and the delay of the
//!   invalidations of commits during the build.
//! - `intersect`: the five-index intersections with a sort of the fixture,
//!   warm, at concurrency 1 and 16 (G2); and a full page of brains, with
//!   all the columns, without the date columns and without rows, with the
//!   time that a client takes to decode and encode the reply.
//! - `mix`: the plans of the query log at concurrency 1, 4 and 16, warm,
//!   with the native share of the log; then the path queries before and
//!   after the writes of the plans file, which give many overridden rows,
//!   and the delay of the invalidations while the mirror rebases.
//! - `text`: the text plans in each variant, in each mode of the plans
//!   file, at concurrency 1 and 16 (G4); `text_query` with the filter of a
//!   result set; staged texts with a filter; the same variants after
//!   commits of text deltas during the run; a snapshot behind the index.
//! - `text-rebuild`: the rebuild of the text index in the store with one
//!   indexing thread, from a store without `text/`: its time and the peak
//!   memory of the server.
//!
//! `/metrics` is scraped every second for the whole run. A latency counts
//! only for an answered query.

pub mod cells;
pub mod plans;
pub mod report;
pub mod server;

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rmpv::Value;
use serde_json::{Value as Json, json};
use zoe_client::{Client, Options, Tokio};
use zoe_core::{Oid, Tid};
use zoe_wire::extension::{CATALOG_QUERY, KV, TEXT};

use crate::load::percentile;
use crate::machine::{Machine, Run};
use cells::{Answer, Call, Pace, answer, last};
use plans::{Plans, get};
use server::{Launch, Server};

/// The memory of a catalog of 1,000,000 entries with the indexes of
/// `xlarge`, as ADR 0059 states it (decision 1): 1.5 GB.
pub const ADR_LINE_BYTES: u64 = 1_500_000_000;

/// The version of the form of the result document.
pub const SCHEMA_VERSION: u32 = 1;

/// The connections of a session: the most of a cell.
const CONNECTIONS: usize = 16;

/// The answers that `cold` times after the mirror is ready.
const FIRST_ANSWERS: usize = 50;

/// The interval of the scrape of `/metrics`.
const SCRAPE_INTERVAL: Duration = Duration::from_secs(1);

/// The interval of the commits of the probe of the invalidations.
const PROBE_INTERVAL: Duration = Duration::from_millis(20);

/// The oid of the object of the probe of the invalidations.
const PROBE_OID: Oid = Oid(1 << 62);

/// The staged texts of the cell `staged`.
const STAGED: usize = 10;

/// The text variant whose filter `text_query` gets.
const FILTERED: &str = "listing";

/// A scenario of `zoe-bench catalog`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, clap::ValueEnum)]
pub enum Scenario {
    /// The cold start.
    Cold,
    /// The five-index intersections.
    Intersect,
    /// The query log.
    Mix,
    /// The text queries.
    Text,
    /// The rebuild of the text index in the store.
    #[value(name = "text-rebuild")]
    TextRebuild,
}

impl Scenario {
    /// The name of the scenario in the result.
    pub fn name(self) -> &'static str {
        match self {
            Scenario::Cold => "cold",
            Scenario::Intersect => "intersect",
            Scenario::Mix => "mix",
            Scenario::Text => "text",
            Scenario::TextRebuild => "text-rebuild",
        }
    }
}

/// The settings of a run.
#[derive(Clone, Debug)]
pub struct Settings {
    /// How `zoe serve` starts.
    pub launch: Launch,
    /// The scenarios, in any order: they run in the order of `Scenario`.
    pub scenarios: Vec<Scenario>,
    /// The time of a cell, in seconds.
    pub seconds: f64,
    /// The least rounds of the calls of each connection in a cell.
    pub rounds: usize,
    /// The longest wait for the mirror and for the text index.
    pub ready_timeout: Duration,
    /// The commits of text deltas of the scenario `text`, and their size.
    pub text_commits: usize,
    /// The text deltas of one commit.
    pub text_batch: usize,
    /// The time between two commits of text deltas.
    pub text_pause: Duration,
    /// The rebase threshold of the server, `--catalog-rebase-rows` or its
    /// default: the writes stop under it for the path queries, then pass
    /// it.
    pub rebase_rows: u64,
    /// The time that the probe runs after the last write, for the rebase.
    pub rebase_seconds: f64,
    /// Whether the tree of the run differs from its commit.
    pub dirty: Option<bool>,
}

/// An error of a run: its text.
pub type RunError = String;

fn fail(e: impl std::fmt::Display) -> RunError {
    e.to_string()
}

/// A running `zoe serve` and its connections.
struct Session {
    server: Server,
    clients: Vec<Arc<Client>>,
    last_metrics: Arc<Mutex<String>>,
    stop: Arc<AtomicBool>,
    scraper: tokio::task::JoinHandle<()>,
}

impl Session {
    /// Start `zoe serve` and open the connections.
    async fn open(launch: &Launch, scrapes: Arc<Mutex<Vec<u64>>>) -> Result<Session, RunError> {
        let server = Server::start(launch)?;
        let options = Options {
            extensions: vec![KV.into(), CATALOG_QUERY.into(), TEXT.into()],
            ..Options::default()
        };
        let mut clients = Vec::with_capacity(CONNECTIONS + 2);
        for _ in 0..CONNECTIONS + 2 {
            let client = Client::open(Arc::new(Tokio), server.addr, "1", &options)
                .await
                .map_err(fail)?;
            clients.push(Arc::new(client));
        }
        let stop = Arc::new(AtomicBool::new(false));
        let last_metrics = Arc::new(Mutex::new(String::new()));
        let scraper = {
            let (stop, scrapes, last_metrics) =
                (stop.clone(), scrapes.clone(), last_metrics.clone());
            let admin = server.admin;
            tokio::spawn(async move {
                while !stop.load(Ordering::SeqCst) {
                    let start = Instant::now();
                    if let Ok(text) = server::scrape(admin).await {
                        let ns = u64::try_from(start.elapsed().as_nanos()).unwrap_or(u64::MAX);
                        scrapes.lock().expect("scrapes").push(ns);
                        *last_metrics.lock().expect("metrics") = text;
                    }
                    tokio::time::sleep(SCRAPE_INTERVAL.saturating_sub(start.elapsed())).await;
                }
            })
        };
        Ok(Session {
            server,
            clients,
            last_metrics,
            stop,
            scraper,
        })
    }

    /// The connections of the cells.
    fn cells(&self) -> &[Arc<Client>] {
        &self.clients[..CONNECTIONS]
    }

    /// The connection of the writers, and the one that listens.
    fn writer(&self) -> Arc<Client> {
        self.clients[CONNECTIONS].clone()
    }

    fn listener(&self) -> Arc<Client> {
        self.clients[CONNECTIONS + 1].clone()
    }

    /// The text of `/metrics` now.
    async fn metrics(&self) -> String {
        match server::scrape(self.server.admin).await {
            Ok(text) => text,
            Err(_) => self.last_metrics.lock().expect("metrics").clone(),
        }
    }

    /// The memory of the server and the gauges of the mirror of `catalog`.
    async fn numbers(&self, catalog: Oid) -> Json {
        let text = self.metrics().await;
        let label = server::catalog_label(catalog);
        let memory = self.server.memory();
        json!({
            "rss_bytes": memory.rss_bytes,
            "peak_rss_bytes": memory.peak_rss_bytes,
            "mirror_bytes": server::sample(&text, "zoe_catalog_mirror_bytes", &[&label]),
            "build_seconds": server::sample(&text, "zoe_catalog_mirror_build_seconds", &[&label]),
        })
    }

    /// Wait until the mirror of `catalog` is ready. Return the seconds from
    /// the spawn of the server.
    async fn ready(&self, catalog: Oid, timeout: Duration) -> Result<f64, RunError> {
        let label = server::catalog_label(catalog);
        let deadline = Instant::now() + timeout;
        loop {
            let text = self.metrics().await;
            if server::sample(&text, "zoe_catalog_mirror_ready", &[&label]) == Some(1.0) {
                return Ok(self.server.started.elapsed().as_secs_f64());
            }
            if server::sample(&text, "zoe_catalog_mirror_drops_total", &[]).unwrap_or(0.0) > 0.0 {
                return Err("the memory bound dropped the mirror: raise --catalog-memory".into());
            }
            if Instant::now() > deadline {
                return Err(format!("the mirror is not ready after {timeout:?}"));
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// Stop the scraper, close the connections and stop the server.
    async fn close(self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = self.scraper.await;
        for client in self.clients {
            if let Ok(client) = Arc::try_unwrap(client) {
                client.close().await;
            }
        }
        self.server.stop();
    }
}

/// Wait until the text index answers `clause`. Return the seconds from
/// `since`, or `None` after `timeout`.
async fn text_ready(
    client: &Client,
    catalog: Oid,
    clause: &Value,
    since: Instant,
    timeout: Duration,
) -> Option<f64> {
    let deadline = Instant::now() + timeout;
    loop {
        let reply = client
            .text_query(catalog, last(client), clause.clone(), Value::Nil)
            .await;
        if let Ok(reply) = reply
            && get(&reply, "e").is_none()
        {
            return Some(since.elapsed().as_secs_f64());
        }
        if Instant::now() > deadline {
            return None;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// The text clause of a plan, without `k`: the clause of `text_query`.
fn text_clause(plan: &Value) -> Option<Value> {
    let clauses = get(plan, "c")?.as_array()?;
    let clause = clauses
        .iter()
        .find(|c| get(c, "k").and_then(Value::as_str) == Some("t"))?;
    let items = clause.as_map()?;
    Some(Value::Map(
        items
            .iter()
            .filter(|(k, _)| k.as_str() != Some("k"))
            .cloned()
            .collect(),
    ))
}

/// `plan` with the key `key` set to `value`.
fn with(plan: &Value, key: &str, value: Value) -> Value {
    let mut items = plan.as_map().cloned().unwrap_or_default();
    match items.iter_mut().find(|(k, _)| k.as_str() == Some(key)) {
        Some(item) => item.1 = value,
        None => items.push((Value::from(key), value)),
    }
    Value::Map(items)
}

/// `plan` whose text clause has `staged` in `st`.
fn with_staged(plan: &Value, staged: Value) -> Value {
    let Some(clauses) = get(plan, "c").and_then(Value::as_array) else {
        return plan.clone();
    };
    let clauses = clauses
        .iter()
        .map(|c| {
            if get(c, "k").and_then(Value::as_str) == Some("t") {
                with(c, "st", staged.clone())
            } else {
                c.clone()
            }
        })
        .collect();
    with(plan, "c", Value::Array(clauses))
}

/// The bin of `rids` as signed 32-bit big-endian ints.
fn rid_bin(rids: &[i32]) -> Value {
    Value::Binary(rids.iter().flat_map(|r| r.to_be_bytes()).collect())
}

/// The p50, p99 and max of `ns`, in seconds.
fn spread(mut ns: Vec<u64>) -> Json {
    ns.sort_unstable();
    json!({
        "count": ns.len(),
        "p50_seconds": percentile(&ns, 0.50),
        "p99_seconds": percentile(&ns, 0.99),
        "max_seconds": percentile(&ns, 1.0),
    })
}

fn nanos(d: Duration) -> u64 {
    u64::try_from(d.as_nanos()).unwrap_or(u64::MAX)
}

/// A commit of the probe: its tid, its begin and its end.
type Commit = (Tid, Instant, Instant);

/// The probe of the invalidation hub: one connection commits a small
/// object every `PROBE_INTERVAL`, another one listens. The delay from the
/// begin of a commit to its invalidation at the listener holds the wait of
/// the hub for the mirror.
struct Probe {
    stop: Arc<AtomicBool>,
    writer: tokio::task::JoinHandle<Result<Vec<Commit>, RunError>>,
    listener: tokio::task::JoinHandle<Vec<(Tid, Instant)>>,
}

impl Probe {
    async fn start(writer: Arc<Client>, listener: Arc<Client>) -> Result<Probe, RunError> {
        let stop = Arc::new(AtomicBool::new(false));
        // The probe object takes a fixed oid far above the counter of the
        // store: an oid of `new_oids` could be the oid of a new object of
        // the writes of the plans file.
        let oid = PROBE_OID;
        let serial = match writer.load_before(oid, last(&writer)).await {
            Ok(record) => record.map_or(Tid::ZERO, |r| r.tid),
            Err(zoe_client::Error::Server { class, .. }) if class.ends_with("POSKeyError") => {
                Tid::ZERO
            }
            Err(e) => return Err(fail(e)),
        };
        let mut invalidations = listener.invalidations();
        let listening = {
            let stop = stop.clone();
            tokio::spawn(async move {
                let mut seen = Vec::new();
                while !stop.load(Ordering::SeqCst) {
                    let next =
                        tokio::time::timeout(Duration::from_millis(100), invalidations.recv());
                    match next.await {
                        Ok(Ok(invalidation)) => seen.push((invalidation.tid, Instant::now())),
                        Ok(Err(zoe_client::Error::Lagged(_))) | Err(_) => {}
                        Ok(Err(_)) => break,
                    }
                }
                seen
            })
        };
        let writing = {
            let stop = stop.clone();
            tokio::spawn(async move {
                let mut serial = serial;
                let mut commits = Vec::new();
                let mut n: u64 = 0;
                while !stop.load(Ordering::SeqCst) {
                    let begun = Instant::now();
                    let mut txn = writer
                        .begin(b"zoe-bench", b"probe", b"")
                        .await
                        .map_err(fail)?;
                    txn.store(oid, serial, &crate::load::record(&n.to_be_bytes()))
                        .await
                        .map_err(fail)?;
                    txn.vote().await.map_err(fail)?;
                    serial = txn.finish().await.map_err(fail)?;
                    commits.push((serial, begun, Instant::now()));
                    n += 1;
                    tokio::time::sleep(PROBE_INTERVAL).await;
                }
                Ok(commits)
            })
        };
        Ok(Probe {
            stop,
            writer: writing,
            listener: listening,
        })
    }

    /// Stop the probe. Return the delays of the invalidations and the
    /// times of the commits.
    async fn finish(self) -> Result<Json, RunError> {
        // The last invalidations come after the last commit.
        tokio::time::sleep(Duration::from_millis(200)).await;
        self.stop.store(true, Ordering::SeqCst);
        let commits = self.writer.await.map_err(fail)??;
        let seen: BTreeMap<Tid, Instant> = self.listener.await.map_err(fail)?.into_iter().collect();
        let mut delays = Vec::new();
        let mut times = Vec::new();
        let mut missed = 0;
        for (tid, begun, finished) in &commits {
            times.push(nanos(finished.duration_since(*begun)));
            match seen.get(tid) {
                Some(at) => delays.push(nanos(at.saturating_duration_since(*begun))),
                None => missed += 1,
            }
        }
        Ok(json!({
            "commits": commits.len(),
            "missed": missed,
            "invalidation_delay": spread(delays),
            "commit": spread(times),
        }))
    }
}

/// The state of a run.
struct Runner {
    settings: Settings,
    plans: Plans,
    scrapes: Arc<Mutex<Vec<u64>>>,
    session: Option<Session>,
    /// The mode of the text indexes of the store, when known.
    mode: Option<String>,
    /// The last gauges of the mirror.
    server: Json,
}

/// Run the scenarios of `settings` with the plans `plans`. Return the
/// result document.
pub async fn run(settings: Settings, plans: Plans) -> Result<Json, RunError> {
    let mut scenarios = settings.scenarios.clone();
    scenarios.sort();
    scenarios.dedup();
    let run = Run::now();
    let machine = Machine::describe();
    let mode = server::current_mode(&settings.launch.store, plans.catalog)?;
    let mut runner = Runner {
        settings,
        plans,
        scrapes: Arc::new(Mutex::new(Vec::new())),
        session: None,
        mode,
        server: json!({}),
    };
    let mut results = Vec::new();
    let outcome = runner.scenarios(&scenarios, &mut results).await;
    if let Some(session) = runner.session.take() {
        session.close().await;
    }
    outcome?;
    let scrapes = runner.scrapes.lock().expect("scrapes").clone();
    let mirror = runner.server["mirror_bytes"].as_f64();
    let plans = &runner.plans;
    let settings = &runner.settings;
    Ok(json!({
        "schema_version": SCHEMA_VERSION,
        "machine": machine,
        "run": {"started": run.started, "commit": run.commit, "dirty": runner.settings.dirty},
        "catalog": {"name": plans.name, "preset": plans.preset, "counts": plans.counts},
        "server": {
            "catalog_memory": runner.settings.launch.catalog_memory,
            "catalog_rebase_rows": runner.settings.rebase_rows,
            "mirror_bytes": runner.server["mirror_bytes"],
            "build_seconds": runner.server["build_seconds"],
            "rss_bytes": runner.server["rss_bytes"],
            "peak_rss_bytes": runner.server["peak_rss_bytes"],
            "adr_0059_line_bytes": ADR_LINE_BYTES,
            "over_adr_line": mirror.map(|b| b > ADR_LINE_BYTES as f64),
        },
        "settings": {
            "seconds": settings.seconds,
            "rounds": settings.rounds,
            "ready_timeout_seconds": settings.ready_timeout.as_secs_f64(),
            "text_commits": settings.text_commits,
            "text_batch": settings.text_batch,
            "text_pause_seconds": settings.text_pause.as_secs_f64(),
            "rebase_seconds": settings.rebase_seconds,
        },
        "metrics": {
            "interval_seconds": SCRAPE_INTERVAL.as_secs_f64(),
            "scrapes": spread(scrapes),
        },
        "scenarios": results,
    }))
}

impl Runner {
    async fn scenarios(
        &mut self,
        scenarios: &[Scenario],
        results: &mut Vec<Json>,
    ) -> Result<(), RunError> {
        for scenario in scenarios {
            eprintln!("zoe-bench catalog: {}", scenario.name());
            let mut result = match scenario {
                Scenario::Cold => self.cold().await?,
                Scenario::Intersect => self.intersect().await?,
                Scenario::Mix => self.mix().await?,
                Scenario::Text => self.text().await?,
                Scenario::TextRebuild => self.text_rebuild().await?,
            };
            let session = self.session.as_ref().expect("a session");
            let numbers = session.numbers(self.plans.catalog).await;
            self.server = numbers.clone();
            if let (Json::Object(out), Json::Object(numbers)) = (&mut result, numbers) {
                out.insert("name".into(), json!(scenario.name()));
                out.extend(numbers);
            }
            results.push(result);
        }
        Ok(())
    }

    /// The session of the run, started and ready when none runs.
    async fn session(&mut self) -> Result<&Session, RunError> {
        if self.session.is_none() {
            let session = Session::open(&self.settings.launch, self.scrapes.clone()).await?;
            session
                .ready(self.plans.catalog, self.settings.ready_timeout)
                .await?;
            self.session = Some(session);
        }
        Ok(self.session.as_ref().expect("a session"))
    }

    /// Stop the session of the run.
    async fn restart(&mut self) {
        if let Some(session) = self.session.take() {
            session.close().await;
        }
    }

    fn pace(&self) -> Pace {
        Pace {
            seconds: self.settings.seconds,
            rounds: self.settings.rounds,
            tid: None,
        }
    }

    fn calls(&self, scenario: &str) -> Vec<Call> {
        self.plans
            .of(scenario)
            .into_iter()
            .filter_map(|q| q.plan.clone().map(Call::Query))
            .collect()
    }

    /// The first text clause of the plans, for the wait for the text
    /// index.
    fn probe_clause(&self) -> Option<Value> {
        self.plans
            .of("text")
            .into_iter()
            .find_map(|q| q.plan.as_ref().and_then(text_clause))
    }

    async fn cold(&mut self) -> Result<Json, RunError> {
        self.restart().await;
        let dropped = server::drop_page_cache(&self.settings.launch.store);
        let session = Session::open(&self.settings.launch, self.scrapes.clone()).await?;
        let catalog = self.plans.catalog;
        let started = session.server.started;
        let probe = Probe::start(session.writer(), session.listener()).await?;
        let text = self.probe_clause().map(|clause| {
            let client = session.cells()[1].clone();
            let timeout = self.settings.ready_timeout;
            tokio::spawn(
                async move { text_ready(&client, catalog, &clause, started, timeout).await },
            )
        });
        let ready = Arc::new(Mutex::new(None::<f64>));
        let watcher = {
            let (ready, admin, label) = (
                ready.clone(),
                session.server.admin,
                server::catalog_label(catalog),
            );
            let timeout = self.settings.ready_timeout;
            tokio::spawn(async move {
                let deadline = Instant::now() + timeout;
                while Instant::now() < deadline {
                    if let Ok(text) = server::scrape(admin).await
                        && server::sample(&text, "zoe_catalog_mirror_ready", &[&label]) == Some(1.0)
                    {
                        *ready.lock().expect("ready") = Some(started.elapsed().as_secs_f64());
                        return;
                    }
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            })
        };
        let calls = self.calls("intersect");
        let client = session.cells()[0].clone();
        let mut refusals: BTreeMap<String, u64> = BTreeMap::new();
        let mut errors: BTreeMap<String, u64> = BTreeMap::new();
        let mut first = Vec::new();
        let mut first_answer = None;
        let mut build_delays = None;
        let deadline = Instant::now() + self.settings.ready_timeout;
        let mut n = 0;
        let mut probe = Some(probe);
        while first.len() < FIRST_ANSWERS && !calls.is_empty() {
            if Instant::now() > deadline {
                break;
            }
            let call = &calls[n % calls.len()];
            n += 1;
            let sent = Instant::now();
            let reply = cells::send(&client, catalog, last(&client), call).await;
            let elapsed = sent.elapsed();
            match reply.as_ref().map(answer) {
                Ok(Answer::Answered) => {
                    first_answer.get_or_insert_with(|| started.elapsed().as_secs_f64());
                    first.push(nanos(elapsed));
                    if let Some(probe) = probe.take() {
                        build_delays = Some(probe.finish().await?);
                    }
                }
                Ok(Answer::Refused(reason)) => {
                    *refusals.entry(reason).or_default() += 1;
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                Ok(Answer::Failed(kind)) => *errors.entry(kind).or_default() += 1,
                Err(e) => {
                    eprintln!("zoe-bench catalog: a call failed: {e}");
                    *errors.entry("call".into()).or_default() += 1;
                }
            }
        }
        if let Some(probe) = probe.take() {
            build_delays = Some(probe.finish().await?);
        }
        let _ = watcher.await;
        let mirror_ready = *ready.lock().expect("ready");
        if mirror_ready.is_none() {
            session.close().await;
            return Err(format!(
                "the mirror is not ready after {:?}",
                self.settings.ready_timeout
            ));
        }
        let text_ready = match text {
            Some(task) => task.await.map_err(fail)?,
            None => None,
        };
        let listening = session.server.listening.as_secs_f64();
        self.session = Some(session);
        Ok(json!({
            "page_cache_dropped_bytes": dropped,
            "listening_seconds": listening,
            "mirror_ready_seconds": mirror_ready,
            "first_answer_seconds": first_answer,
            "text_ready_seconds": text_ready,
            "refusals": refusals,
            "errors": errors,
            "first_answers": spread(first),
            "hub_during_build": build_delays,
        }))
    }

    async fn intersect(&mut self) -> Result<Json, RunError> {
        let catalog = self.plans.catalog;
        let calls = self.calls("intersect");
        let pace = self.pace();
        let session = self.session().await?;
        cells::once(&session.cells()[0], catalog, &calls).await;
        let mut out = Vec::new();
        for concurrency in [1, 16] {
            let name = format!("c{concurrency}");
            out.push(cells::run(&name, session.cells(), catalog, &calls, concurrency, pace).await);
        }
        let page = self.page().await?;
        Ok(json!({"cells": out, "page": page}))
    }

    /// A full page of brains: the plan of the intersection `folders`, or
    /// the first one, with `sort_limit` set to its page of rows.
    async fn page(&mut self) -> Result<Json, RunError> {
        let catalog = self.plans.catalog;
        let pace = self.pace();
        let queries = self.plans.of("intersect");
        let Some(base) = queries
            .iter()
            .find(|q| q.name == "folders")
            .or(queries.first())
            .and_then(|q| q.plan.clone())
        else {
            return Ok(Json::Null);
        };
        let rn = get(&base, "rn").and_then(Value::as_i64).unwrap_or(0);
        let Some(cols) = get(&base, "cols").and_then(Value::as_array).cloned() else {
            return Ok(Json::Null);
        };
        let page = with(&with(&base, "l", Value::from(rn)), "bz", Value::Nil);
        let session = self.session().await?;
        let client = &session.cells()[0];
        let reply = client
            .catalog_query(catalog, last(client), page.clone(), Value::Nil)
            .await
            .map_err(fail)?;
        let rows: Vec<Value> = get(&reply, "rows")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        // A date column holds a DateTime: the ext type 2.
        let dated: Vec<bool> = (0..cols.len())
            .map(|i| {
                rows.iter().any(|row| {
                    row.as_array()
                        .and_then(|cells| cells.get(i))
                        .is_some_and(|cell| matches!(cell, Value::Ext(2, _)))
                })
            })
            .collect();
        let undated: Vec<Value> = cols
            .iter()
            .zip(&dated)
            .filter(|(_, d)| !**d)
            .map(|(c, _)| c.clone())
            .collect();
        let date_columns: Vec<Value> = cols
            .iter()
            .zip(&dated)
            .filter(|(_, d)| **d)
            .map(|(c, _)| c.clone())
            .collect();
        let variants = [
            ("all_columns", page.clone()),
            (
                "no_date_columns",
                with(&page, "cols", Value::Array(undated)),
            ),
            (
                "no_rows",
                with(&with(&page, "cols", Value::Nil), "rn", Value::from(0)),
            ),
        ];
        let mut out = Vec::new();
        for (name, plan) in variants {
            out.push(
                cells::run(
                    name,
                    session.cells(),
                    catalog,
                    &[Call::Query(plan)],
                    1,
                    pace,
                )
                .await,
            );
        }
        // The decode of the reply into msgpack values, and its encode again,
        // as the native client does before `CatalogRows`.
        let mut bytes = Vec::new();
        rmpv::encode::write_value(&mut bytes, &reply).map_err(fail)?;
        let (mut decode, mut encode) = (Vec::new(), Vec::new());
        for _ in 0..50 {
            let start = Instant::now();
            let value = rmpv::decode::read_value(&mut bytes.as_slice()).map_err(fail)?;
            decode.push(nanos(start.elapsed()));
            let start = Instant::now();
            let mut again = Vec::with_capacity(bytes.len());
            rmpv::encode::write_value(&mut again, &value).map_err(fail)?;
            encode.push(nanos(start.elapsed()));
        }
        Ok(json!({
            "rows": rows.len(),
            "columns": cols.len(),
            "date_columns": plans::json_of(&Value::Array(date_columns)),
            "reply_bytes": bytes.len(),
            "cells": out,
            "client_decode": spread(decode),
            "client_encode": spread(encode),
        }))
    }

    async fn mix(&mut self) -> Result<Json, RunError> {
        let catalog = self.plans.catalog;
        let pace = self.pace();
        let queries: Vec<_> = self.plans.of("mix").into_iter().cloned().collect();
        let calls = self.calls("mix");
        let paths = self.calls("path");
        let writes = self.plans.writes.clone();
        let rebase_rows = self.settings.rebase_rows;
        let rebase_seconds = self.settings.rebase_seconds;
        let session = self.session().await?;
        // The native share: each line of the log once, by its weight.
        let client = &session.cells()[0];
        let (mut total, mut native) = (0u64, 0u64);
        let mut client_side: BTreeMap<String, u64> = BTreeMap::new();
        let mut server_side: BTreeMap<String, u64> = BTreeMap::new();
        for q in &queries {
            total += q.weight;
            match &q.plan {
                None => {
                    let reason = q.reason.clone().unwrap_or_else(|| "?".into());
                    *client_side.entry(reason).or_default() += q.weight;
                }
                Some(plan) => {
                    let reply = client
                        .catalog_query(catalog, last(client), plan.clone(), Value::Nil)
                        .await;
                    match reply.as_ref().map(answer) {
                        Ok(Answer::Answered) => native += q.weight,
                        Ok(Answer::Refused(reason)) => {
                            *server_side.entry(reason).or_default() += q.weight
                        }
                        Ok(Answer::Failed(kind)) => {
                            *server_side.entry(format!("error:{kind}")).or_default() += q.weight
                        }
                        Err(_) => *server_side.entry("call".into()).or_default() += q.weight,
                    }
                }
            }
        }
        let mut out = Vec::new();
        for concurrency in [1, 4, 16] {
            let name = format!("c{concurrency}");
            out.push(cells::run(&name, session.cells(), catalog, &calls, concurrency, pace).await);
        }
        // The path queries before the writes, then with the overridden rows
        // of the writes under the rebase threshold.
        let mut overrides = Json::Null;
        let mut rebase = Json::Null;
        if !paths.is_empty() && !writes.is_empty() {
            let base = cells::run("path_base", session.cells(), catalog, &paths, 1, pace).await;
            let writer = session.writer();
            let mut rows = 0;
            let mut applied = 0;
            let limit = rebase_rows.saturating_sub(rebase_rows / 10);
            while applied < writes.len() && rows + writes[applied].rows <= limit {
                apply(&writer, &writes[applied]).await?;
                rows += writes[applied].rows;
                applied += 1;
            }
            let over =
                cells::run("path_overrides", session.cells(), catalog, &paths, 1, pace).await;
            overrides = json!({
                "writes": applied,
                "override_rows": rows,
                "cells": [base, over],
            });
            let probe = Probe::start(session.listener(), session.cells()[15].clone()).await?;
            let mut more = 0;
            for write in &writes[applied..] {
                apply(&writer, write).await?;
                rows += write.rows;
                more += 1;
            }
            tokio::time::sleep(Duration::from_secs_f64(rebase_seconds)).await;
            rebase = json!({
                "writes": more,
                "rows": rows,
                "past_threshold": rows > rebase_rows,
                "hub": probe.finish().await?,
            });
        }
        let share = if total > 0 {
            Some(native as f64 / total as f64)
        } else {
            None
        };
        Ok(json!({
            "lines": total,
            "native": native,
            "native_share": share,
            "client_reasons": client_side,
            "server_reasons": server_side,
            "cells": out,
            "overrides": overrides,
            "rebase": rebase,
        }))
    }

    async fn text(&mut self) -> Result<Json, RunError> {
        let modes: Vec<(String, Vec<u8>)> = self
            .plans
            .modes
            .iter()
            .map(|(m, v)| (m.clone(), v.clone()))
            .collect();
        let mut out = Vec::new();
        if modes.is_empty() {
            out.push(self.text_mode(None).await?);
        }
        // The mode of the store runs first: it needs no restart.
        let mut ordered = modes.clone();
        ordered.sort_by_key(|(m, _)| Some(m) != self.mode.as_ref());
        for (mode, value) in ordered {
            if self.mode.as_deref() != Some(mode.as_str()) {
                self.restart().await;
                server::set_mode(
                    &self.settings.launch.store,
                    self.plans.catalog,
                    &mode,
                    &value,
                )?;
                self.mode = Some(mode.clone());
            }
            out.push(self.text_mode(Some(mode)).await?);
        }
        Ok(json!({"modes": out}))
    }

    async fn text_mode(&mut self, mode: Option<String>) -> Result<Json, RunError> {
        let catalog = self.plans.catalog;
        let pace = self.pace();
        let timeout = self.settings.ready_timeout;
        let queries: Vec<_> = self.plans.of("text").into_iter().cloned().collect();
        let filters: Vec<_> = self.plans.of("filter").into_iter().cloned().collect();
        let texts = self.plans.texts.clone();
        let probe = self.probe_clause();
        let (commits, batch, pause) = (
            self.settings.text_commits,
            self.settings.text_batch,
            self.settings.text_pause,
        );
        let session = self.session().await?;
        let client = session.cells()[0].clone();
        let since = Instant::now();
        let ready = match &probe {
            Some(clause) => text_ready(&client, catalog, clause, since, timeout).await,
            None => None,
        };
        let mut variants: Vec<String> = Vec::new();
        for q in &queries {
            if let Some(v) = &q.variant
                && !variants.contains(v)
            {
                variants.push(v.clone());
            }
        }
        let of = |variant: &str| -> Vec<Call> {
            queries
                .iter()
                .filter(|q| q.variant.as_deref() == Some(variant))
                .filter_map(|q| q.plan.clone().map(Call::Query))
                .collect()
        };
        let all: Vec<Call> = queries
            .iter()
            .filter_map(|q| q.plan.clone().map(Call::Query))
            .collect();
        cells::once(&client, catalog, &all).await;
        let mut out = Vec::new();
        for variant in &variants {
            let calls = of(variant);
            for concurrency in [1, 16] {
                out.push(
                    cells::run(variant, session.cells(), catalog, &calls, concurrency, pace).await,
                );
            }
        }
        // `text_query` with the filter of a result set: the rids of the
        // filter of the variant `listing`.
        let filter = match filters
            .iter()
            .find(|q| q.name == FILTERED)
            .and_then(|q| q.plan.clone())
        {
            Some(plan) => {
                let reply = client
                    .catalog_query(catalog, last(&client), plan, Value::Nil)
                    .await
                    .map_err(fail)?;
                plans::rids(get(&reply, "r"))
            }
            None => None,
        };
        let alone: Vec<&plans::Query> = queries
            .iter()
            .filter(|q| q.variant.as_deref() == Some("alone"))
            .collect();
        let filter_rids = filter.as_ref().map_or(0, Vec::len);
        if let Some(rids) = &filter {
            let calls: Vec<Call> = alone
                .iter()
                .filter_map(|q| q.plan.as_ref().and_then(text_clause))
                .map(|clause| Call::Text(clause, rid_bin(rids)))
                .collect();
            out.push(cells::run("text_query", session.cells(), catalog, &calls, 1, pace).await);
        }
        // Staged texts in the clause, with the filter of a listing.
        if !texts.is_empty() {
            let staged = Value::Array(
                texts
                    .iter()
                    .take(STAGED)
                    .enumerate()
                    .map(|(i, t)| {
                        let other = &texts[(i + 1) % texts.len()];
                        Value::Array(vec![
                            Value::from(t.index.as_str()),
                            Value::from(t.rid),
                            Value::from(t.language.as_str()),
                            Value::from(other.text.as_str()),
                        ])
                    })
                    .collect(),
            );
            let calls: Vec<Call> = of(FILTERED)
                .into_iter()
                .map(|c| match c {
                    Call::Query(plan) => Call::Query(with_staged(&plan, staged.clone())),
                    other => other,
                })
                .collect();
            out.push(cells::run("staged", session.cells(), catalog, &calls, 1, pace).await);
        }
        // Commits of text deltas during the run, then the variants again on
        // an index of more segments with deletes, and a snapshot behind it.
        let before = last(&client);
        let mut committed = Json::Null;
        if !texts.is_empty() && commits > 0 {
            let started = Instant::now();
            let deltas =
                commit_texts(&session.writer(), catalog, &texts, commits, batch, pause).await?;
            let lag = wait_text_lag(session, catalog, timeout.min(Duration::from_secs(60))).await;
            committed = json!({
                "transactions": commits,
                "deltas": deltas,
                "seconds": started.elapsed().as_secs_f64(),
                "lag_cleared_seconds": lag,
            });
            for variant in &variants {
                let name = format!("after_commits/{variant}");
                out.push(cells::run(&name, session.cells(), catalog, &of(variant), 1, pace).await);
            }
            let behind = Pace {
                tid: Some(before),
                ..pace
            };
            for variant in ["alone", FILTERED] {
                let name = format!("behind/{variant}");
                out.push(
                    cells::run(&name, session.cells(), catalog, &of(variant), 1, behind).await,
                );
            }
        }
        Ok(json!({
            "mode": mode,
            "text_ready_seconds": ready,
            "filter_rids": filter_rids,
            "commits": committed,
            "cells": out,
        }))
    }

    async fn text_rebuild(&mut self) -> Result<Json, RunError> {
        self.restart().await;
        let store = self.settings.launch.store.clone();
        let dir = server::text_dir(&store, self.plans.catalog);
        let _ = std::fs::remove_dir_all(&dir);
        let session = Session::open(&self.settings.launch, self.scrapes.clone()).await?;
        let started = session.server.started;
        let clause = self.probe_clause();
        let ready = match &clause {
            Some(clause) => {
                text_ready(
                    &session.cells()[0],
                    self.plans.catalog,
                    clause,
                    started,
                    self.settings.ready_timeout,
                )
                .await
            }
            None => None,
        };
        let memory = session.server.memory();
        let mirror = session
            .ready(self.plans.catalog, self.settings.ready_timeout)
            .await
            .ok();
        self.session = Some(session);
        Ok(json!({
            "mode": self.mode,
            "rebuild_seconds": ready,
            "peak_rss_at_rebuild_bytes": memory.peak_rss_bytes,
            "mirror_ready_seconds": mirror,
        }))
    }
}

/// Commit `write` on `client`, each record with the current serial of its
/// oid.
async fn apply(client: &Client, write: &plans::Write) -> Result<(), RunError> {
    let at = last(client);
    let mut serials = Vec::with_capacity(write.records.len());
    for (oid, _) in &write.records {
        let serial = match client.load_before(*oid, at).await {
            Ok(record) => record.map_or(Tid::ZERO, |r| r.tid),
            // A new object of the write.
            Err(zoe_client::Error::Server { class, .. }) if class.ends_with("POSKeyError") => {
                Tid::ZERO
            }
            Err(e) => return Err(fail(e)),
        };
        serials.push(serial);
    }
    let mut txn = client
        .begin(b"zoe-bench", b"catalog write", b"")
        .await
        .map_err(fail)?;
    for ((oid, data), serial) in write.records.iter().zip(serials) {
        txn.store(*oid, serial, data).await.map_err(fail)?;
    }
    txn.vote().await.map_err(fail)?;
    txn.finish().await.map_err(fail)?;
    Ok(())
}

/// Commit `commits` transactions of `batch` text deltas: each delta gives a
/// rid of the sample the text of another rid of the same index. Return the
/// deltas.
async fn commit_texts(
    client: &Client,
    catalog: Oid,
    texts: &[plans::Staged],
    commits: usize,
    batch: usize,
    pause: Duration,
) -> Result<usize, RunError> {
    let mut by_index: BTreeMap<&str, Vec<&plans::Staged>> = BTreeMap::new();
    for t in texts {
        by_index.entry(t.index.as_str()).or_default().push(t);
    }
    let groups: Vec<Vec<&plans::Staged>> = by_index.into_values().filter(|g| g.len() > 1).collect();
    if groups.is_empty() {
        return Ok(0);
    }
    let mut deltas = 0;
    let mut n = 0;
    for k in 0..commits {
        let before = last(client);
        let mut txn = client
            .begin(b"zoe-bench", b"text deltas", b"")
            .await
            .map_err(fail)?;
        for _ in 0..batch {
            let group = &groups[n % groups.len()];
            let target = group[(n / groups.len()) % group.len()];
            let source = group[(n / groups.len() + 1 + k) % group.len()];
            n += 1;
            if source.text == target.text {
                continue;
            }
            txn.text_set(
                catalog,
                &target.index,
                target.rid,
                &target.language,
                &source.text,
                before,
            )
            .await
            .map_err(fail)?;
            deltas += 1;
        }
        txn.vote().await.map_err(fail)?;
        txn.finish().await.map_err(fail)?;
        tokio::time::sleep(pause).await;
    }
    Ok(deltas)
}

/// Wait until the text index holds every committed delta. Return the
/// seconds of the wait, or `None` after `timeout`.
async fn wait_text_lag(session: &Session, catalog: Oid, timeout: Duration) -> Option<f64> {
    let label = server::catalog_label(catalog);
    let start = Instant::now();
    while start.elapsed() < timeout {
        let text = session.metrics().await;
        if server::sample(&text, "zoe_text_commit_lag_seconds", &[&label]) == Some(0.0) {
            return Some(start.elapsed().as_secs_f64());
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    None
}

/// The outcome of `check`: each plan with an answer of the Python path,
/// sent once and compared.
pub async fn check(launch: &Launch, plans: &Plans, timeout: Duration) -> Result<Json, RunError> {
    let session = Session::open(launch, Arc::new(Mutex::new(Vec::new()))).await?;
    let outcome = check_on(&session, plans, timeout).await;
    session.close().await;
    outcome
}

async fn check_on(session: &Session, plans: &Plans, timeout: Duration) -> Result<Json, RunError> {
    let catalog = plans.catalog;
    session.ready(catalog, timeout).await?;
    let client = &session.cells()[0];
    if let Some(clause) = plans
        .of("text")
        .into_iter()
        .find_map(|q| q.plan.as_ref().and_then(text_clause))
    {
        text_ready(client, catalog, &clause, Instant::now(), timeout).await;
    }
    let (mut checked, mut matched) = (0, 0);
    let mut mismatches = Vec::new();
    let mut refusals: BTreeMap<String, u64> = BTreeMap::new();
    let mut errors: BTreeMap<String, u64> = BTreeMap::new();
    let mut scenarios: BTreeMap<String, u64> = BTreeMap::new();
    let mut client_side: BTreeMap<String, u64> = BTreeMap::new();
    for q in &plans.queries {
        if q.plan.is_none() {
            let reason = q.reason.clone().unwrap_or_else(|| "?".into());
            *client_side.entry(reason).or_default() += q.weight;
        }
        let (Some(plan), Some(expect)) = (&q.plan, &q.expect) else {
            continue;
        };
        checked += 1;
        *scenarios.entry(q.scenario.clone()).or_default() += 1;
        let reply = client
            .catalog_query(catalog, last(client), plan.clone(), Value::Nil)
            .await
            .map_err(fail)?;
        match answer(&reply) {
            Answer::Answered => {}
            Answer::Refused(reason) => {
                *refusals.entry(reason).or_default() += 1;
                continue;
            }
            Answer::Failed(kind) => {
                *errors.entry(kind).or_default() += 1;
                continue;
            }
        }
        let rids = plans::rids(get(&reply, "r")).unwrap_or_default();
        let n = get(&reply, "n").and_then(Value::as_i64);
        let a = get(&reply, "a").and_then(Value::as_i64);
        if rids == expect.rids && n == Some(expect.n) && a == Some(expect.a) {
            matched += 1;
        } else {
            mismatches.push(json!({
                "name": q.name,
                "scenario": q.scenario,
                "rids": rids.len(),
                "expected_rids": expect.rids.len(),
                "same_rids": rids == expect.rids,
                "n": n, "expected_n": expect.n,
                "a": a, "expected_a": expect.a,
            }));
        }
    }
    Ok(json!({
        "checked": checked,
        "matched": matched,
        "mismatches": mismatches,
        "refusals": refusals,
        "errors": errors,
        "scenarios": scenarios,
        "client_reasons": client_side,
    }))
}
