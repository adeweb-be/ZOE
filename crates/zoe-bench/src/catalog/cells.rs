//! The cells of `zoe-bench catalog`: calls sent by a number of connections
//! for a time, and their numbers.
//!
//! A reply with `f` counts as a refusal with its reason, a reply with `e`
//! as an error with its kind. Neither gives a latency: a latency counts
//! only for an answered query.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use rmpv::Value;
use serde::Serialize;
use zoe_client::{Client, Error as ClientError};
use zoe_core::{Oid, Tid};

use super::plans::get;
use crate::load::percentile;

/// One call of a cell.
#[derive(Clone, Debug)]
pub enum Call {
    /// `catalog_query(catalog, tid, plan, nil)`.
    Query(Value),
    /// `text_query(catalog, tid, clause, filter)`.
    Text(Value, Value),
}

/// What a reply was.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Answer {
    /// An answer, with the reply.
    Answered,
    /// A reply `f`, with its reason.
    Refused(String),
    /// A reply `e`, with its kind.
    Failed(String),
}

/// The class of a reply map.
pub fn answer(reply: &Value) -> Answer {
    if let Some(reason) = get(reply, "f") {
        return Answer::Refused(reason.as_str().unwrap_or("?").to_owned());
    }
    if let Some(kind) = get(reply, "e") {
        return Answer::Failed(kind.as_str().unwrap_or("?").to_owned());
    }
    Answer::Answered
}

/// Send `call` at the snapshot that ends before `tid`.
pub async fn send(
    client: &Client,
    catalog: Oid,
    tid: Tid,
    call: &Call,
) -> Result<Value, ClientError> {
    match call {
        Call::Query(plan) => {
            client
                .catalog_query(catalog, tid, plan.clone(), Value::Nil)
                .await
        }
        Call::Text(clause, filter) => {
            client
                .text_query(catalog, tid, clause.clone(), filter.clone())
                .await
        }
    }
}

/// The snapshot of the last commit that `client` knows: the tid after it.
pub fn last(client: &Client) -> Tid {
    Tid(client.last_transaction().0 + 1)
}

/// The numbers of one cell.
#[derive(Clone, Debug, Default, Serialize)]
pub struct Cell {
    /// The name of the cell.
    pub name: String,
    /// The connections that sent at once.
    pub concurrency: usize,
    /// The calls sent.
    pub queries: u64,
    /// The calls answered.
    pub answered: u64,
    /// The median latency of an answer, in seconds.
    pub p50_seconds: Option<f64>,
    /// The 99th percentile.
    pub p99_seconds: Option<f64>,
    /// The longest.
    pub max_seconds: Option<f64>,
    /// The answers per second of the cell.
    pub qps: f64,
    /// The replies `f`, by reason.
    pub refusals: BTreeMap<String, u64>,
    /// The replies `e` by kind, and the calls that failed (`call`).
    pub errors: BTreeMap<String, u64>,
    /// The numbers of each query, by name, in the order of the calls: for
    /// a cell of `run_by_query` only.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub by_query: Vec<QuerySpread>,
}

/// The numbers of the answers of one query of a cell.
#[derive(Clone, Debug, Default, Serialize)]
pub struct QuerySpread {
    /// The name of the query.
    pub name: String,
    /// The answers.
    pub count: u64,
    /// The median latency of an answer, in seconds.
    pub p50_seconds: Option<f64>,
    /// The 99th percentile.
    pub p99_seconds: Option<f64>,
    /// The longest.
    pub max_seconds: Option<f64>,
}

/// The tally of the calls of one connection.
#[derive(Default)]
struct Tally {
    latencies: Vec<u64>,
    /// The latencies of the answers of each call, by the index of the call.
    by_call: BTreeMap<usize, Vec<u64>>,
    sent: u64,
    refusals: BTreeMap<String, u64>,
    errors: BTreeMap<String, u64>,
}

impl Tally {
    fn add(&mut self, other: Tally) {
        self.latencies.extend(other.latencies);
        for (k, ns) in other.by_call {
            self.by_call.entry(k).or_default().extend(ns);
        }
        self.sent += other.sent;
        for (k, n) in other.refusals {
            *self.refusals.entry(k).or_default() += n;
        }
        for (k, n) in other.errors {
            *self.errors.entry(k).or_default() += n;
        }
    }

    fn note(&mut self, call: usize, outcome: Result<Value, ClientError>, elapsed: Duration) {
        self.sent += 1;
        match outcome.as_ref().map(answer) {
            Ok(Answer::Answered) => {
                let ns = u64::try_from(elapsed.as_nanos()).unwrap_or(u64::MAX);
                self.latencies.push(ns);
                self.by_call.entry(call).or_default().push(ns);
            }
            Ok(Answer::Refused(reason)) => *self.refusals.entry(reason).or_default() += 1,
            Ok(Answer::Failed(kind)) => *self.errors.entry(kind).or_default() += 1,
            Err(error) => {
                eprintln!("zoe-bench catalog: a call failed: {error}");
                *self.errors.entry("call".into()).or_default() += 1;
            }
        }
    }

    fn cell(mut self, name: &str, concurrency: usize, seconds: f64) -> Cell {
        self.latencies.sort_unstable();
        let answered = self.latencies.len() as u64;
        Cell {
            name: name.to_owned(),
            concurrency,
            queries: self.sent,
            answered,
            p50_seconds: percentile(&self.latencies, 0.50),
            p99_seconds: percentile(&self.latencies, 0.99),
            max_seconds: percentile(&self.latencies, 1.0),
            qps: if seconds > 0.0 {
                answered as f64 / seconds
            } else {
                0.0
            },
            refusals: self.refusals,
            errors: self.errors,
            by_query: Vec::new(),
        }
    }

    /// The numbers of each name of `names`, the name of each call, in the
    /// order of their first call.
    fn by_query(&self, names: &[String]) -> Vec<QuerySpread> {
        let mut order: Vec<&String> = Vec::new();
        let mut latencies: BTreeMap<&String, Vec<u64>> = BTreeMap::new();
        for (k, name) in names.iter().enumerate() {
            if !latencies.contains_key(name) {
                order.push(name);
            }
            let ns = latencies.entry(name).or_default();
            ns.extend(self.by_call.get(&k).into_iter().flatten());
        }
        order
            .into_iter()
            .map(|name| {
                let mut ns = latencies.remove(name).unwrap_or_default();
                ns.sort_unstable();
                QuerySpread {
                    name: name.clone(),
                    count: ns.len() as u64,
                    p50_seconds: percentile(&ns, 0.50),
                    p99_seconds: percentile(&ns, 0.99),
                    max_seconds: percentile(&ns, 1.0),
                }
            })
            .collect()
    }
}

/// How a cell runs.
#[derive(Clone, Copy, Debug)]
pub struct Pace {
    /// The time of the cell, in seconds.
    pub seconds: f64,
    /// The least rounds of the calls of each connection, whatever the time.
    pub rounds: usize,
    /// A fixed snapshot, or `None` for the last commit at each call.
    pub tid: Option<Tid>,
}

/// Run `calls` on the first `concurrency` connections of `clients` as
/// `pace` says. Connection `i` starts at call `i` and sends the calls in
/// turn.
pub async fn run(
    name: &str,
    clients: &[Arc<Client>],
    catalog: Oid,
    calls: &[Call],
    concurrency: usize,
    pace: Pace,
) -> Cell {
    let (tally, seconds) = measure(clients, catalog, calls, concurrency, pace).await;
    tally.cell(name, concurrency, seconds)
}

/// `run`, with the numbers of each query: `names` gives the name of each
/// call.
pub async fn run_by_query(
    name: &str,
    clients: &[Arc<Client>],
    catalog: Oid,
    calls: &[Call],
    names: &[String],
    concurrency: usize,
    pace: Pace,
) -> Cell {
    let (tally, seconds) = measure(clients, catalog, calls, concurrency, pace).await;
    let by_query = tally.by_query(names);
    Cell {
        by_query,
        ..tally.cell(name, concurrency, seconds)
    }
}

async fn measure(
    clients: &[Arc<Client>],
    catalog: Oid,
    calls: &[Call],
    concurrency: usize,
    pace: Pace,
) -> (Tally, f64) {
    if calls.is_empty() {
        return (Tally::default(), 0.0);
    }
    let calls: Arc<[Call]> = calls.to_vec().into();
    let start = Instant::now();
    let deadline = start + Duration::from_secs_f64(pace.seconds);
    let tasks: Vec<_> = clients
        .iter()
        .take(concurrency)
        .enumerate()
        .map(|(i, client)| {
            let client = client.clone();
            let calls = calls.clone();
            tokio::spawn(async move {
                let mut tally = Tally::default();
                let least = pace.rounds * calls.len();
                let mut n = 0;
                while n < least || Instant::now() < deadline {
                    let k = (i + n) % calls.len();
                    let tid = pace.tid.unwrap_or_else(|| last(&client));
                    let sent = Instant::now();
                    let outcome = send(&client, catalog, tid, &calls[k]).await;
                    tally.note(k, outcome, sent.elapsed());
                    n += 1;
                }
                tally
            })
        })
        .collect();
    let mut total = Tally::default();
    for task in tasks {
        total.add(task.await.expect("a task of a cell panicked"));
    }
    (total, start.elapsed().as_secs_f64())
}

/// Send each call once on `client`, as a warm-up. Return the replies.
pub async fn once(
    client: &Client,
    catalog: Oid,
    calls: &[Call],
) -> Vec<Result<Value, ClientError>> {
    let mut out = Vec::with_capacity(calls.len());
    for call in calls {
        out.push(send(client, catalog, last(client), call).await);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_refusal_and_an_error_give_no_latency() {
        let reply = |key: &str, value: &str| {
            Value::Map(vec![
                (Value::from("tid"), Value::Binary(vec![0; 8])),
                (Value::from(key), Value::from(value)),
            ])
        };
        let mut tally = Tally::default();
        let ms = Duration::from_millis(1);
        tally.note(0, Ok(reply("f", "not_ready")), ms);
        tally.note(0, Ok(reply("e", "unavailable")), ms);
        tally.note(1, Ok(reply("n", "")), ms);
        tally.note(1, Err(ClientError::Disconnected), ms);
        let cell = tally.cell("c", 1, 1.0);
        assert_eq!((cell.queries, cell.answered), (4, 1));
        assert_eq!(cell.refusals["not_ready"], 1);
        assert_eq!(cell.errors["unavailable"], 1);
        assert_eq!(cell.errors["call"], 1);
        assert_eq!(cell.p99_seconds, Some(0.001));
        assert_eq!(cell.qps, 1.0);
    }

    #[test]
    fn the_numbers_of_each_query_follow_its_name() {
        let ms = |n: u64| Duration::from_millis(n);
        let mut tally = Tally::default();
        tally.note(0, Ok(Value::Map(vec![])), ms(1));
        tally.note(1, Ok(Value::Map(vec![])), ms(5));
        tally.note(2, Ok(Value::Map(vec![])), ms(3));
        let names = ["b".to_owned(), "a".to_owned(), "b".to_owned()];
        let queries = tally.by_query(&names);
        let got: Vec<(&str, u64, Option<f64>)> = queries
            .iter()
            .map(|q| (q.name.as_str(), q.count, q.max_seconds))
            .collect();
        assert_eq!(got, [("b", 2, Some(0.003)), ("a", 1, Some(0.005))]);
    }
}
