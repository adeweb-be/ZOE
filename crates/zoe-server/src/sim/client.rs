//! The clients of a run: raw ZEO 6 clients that commit records to a small
//! set of shared oids with one hot oid, so that their votes meet on locks
//! and conflicts.
//!
//! A client calls as `ZEO.ClientStorage` does: `tpc_begin` and `storea`
//! without a reply, then `vote` and `tpc_finish`. A conflict aborts and
//! tries again. A lost connection reconnects. A `tpc_finish` whose reply
//! does not arrive has an unknown outcome, and the client does not count
//! it.

use std::io;
use std::sync::Arc;
use std::time::Duration;

use rmpv::Value;
use tokio::io::AsyncWriteExt;

use super::net::{self, SimStream};
use super::rng::Rng;
use super::world::{Acked, Shared, sleep};
use crate::protocol::{PROTOCOL, STORAGE_NAME, frame};

/// The oids that the clients share.
const OIDS: u64 = 6;

/// The tries of one transaction.
const TRIES: usize = 8;

/// The tid that loads the current record.
const MAX: u64 = 0x7fff_ffff_ffff_ffff;

/// What a call gives back.
enum Failure {
    /// The server raised `class`.
    Server(String),
    /// The connection broke.
    Disconnected,
}

/// How one try of a transaction ends.
enum Outcome {
    Acked(Acked),
    Conflict,
    Disconnected { in_finish: bool },
    Failed,
}

/// A registered connection of a client.
struct Connection {
    stream: SimStream,
    next_id: i64,
}

/// One client: `transactions` transactions, then a clean close.
pub(crate) async fn run(
    shared: Arc<Shared>,
    client: usize,
    transactions: usize,
    mut rng: Rng,
    stall: Option<Duration>,
) {
    let mut connection: Option<Connection> = None;
    let mut stall = stall;
    'transactions: for t in 0..transactions {
        for attempt in 0..TRIES {
            if connection.is_none() {
                connection = connect(&shared, client, &mut rng).await;
                if connection.is_none() {
                    break 'transactions;
                }
            }
            let Some(conn) = connection.as_mut() else {
                break 'transactions;
            };
            let outcome = conn
                .commit(
                    &shared,
                    client,
                    (t * TRIES + attempt) as i64,
                    &mut rng,
                    &mut stall,
                )
                .await;
            match outcome {
                Outcome::Acked(acked) => {
                    shared.lock().acked.push(acked);
                    break;
                }
                Outcome::Conflict | Outcome::Failed => {
                    let backoff = rng.between(Duration::from_micros(100), Duration::from_millis(5));
                    sleep(&shared, backoff).await;
                }
                Outcome::Disconnected { in_finish } => {
                    connection = None;
                    if in_finish {
                        break;
                    }
                }
            }
        }
    }
    if let Some(mut conn) = connection {
        let _ = conn.stream.shutdown().await;
    }
    let mut state = shared.lock();
    state.trace(format!("c{client} done"));
    state.clients_done += 1;
}

/// Connect, answer the handshake and register. Retry while the server is
/// down. Return `None` when the run halted.
async fn connect(shared: &Arc<Shared>, client: usize, rng: &mut Rng) -> Option<Connection> {
    loop {
        if shared.lock().server.halted {
            return None;
        }
        if let Ok(conn) = handshake(shared, client).await {
            return Some(conn);
        }
        let backoff = rng.between(Duration::from_millis(1), Duration::from_millis(50));
        sleep(shared, backoff).await;
    }
}

async fn handshake(shared: &Arc<Shared>, client: usize) -> io::Result<Connection> {
    let mut stream = net::connect(shared, client).await?;
    match frame::read(&mut stream).await? {
        Some(protocol) if protocol == PROTOCOL => {}
        _ => return Err(io::ErrorKind::InvalidData.into()),
    }
    frame::write(&mut stream, PROTOCOL).await?;
    let mut conn = Connection { stream, next_id: 0 };
    let register = vec![Value::from(STORAGE_NAME), Value::Boolean(false)];
    match conn.call(shared, client, "register", register).await {
        Ok(_) => Ok(conn),
        Err(_) => Err(io::ErrorKind::ConnectionReset.into()),
    }
}

impl Connection {
    /// Try one transaction: read the current serials of one or two oids,
    /// store new records, vote and finish.
    async fn commit(
        &mut self,
        shared: &Arc<Shared>,
        client: usize,
        txn: i64,
        rng: &mut Rng,
        stall: &mut Option<Duration>,
    ) -> Outcome {
        // Half of the transactions write oid 1, a hot object as a catalog
        // or a counter is.
        let first = if rng.chance(0.5) {
            1
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
        let mut records = Vec::new();
        for &oid in &oids {
            let args = vec![p64(oid), p64(MAX)];
            let serial = match self.call(shared, client, "loadBefore", args).await {
                Ok(Value::Array(parts)) => parts.get(1).and_then(u64_of).unwrap_or(0),
                Ok(_) => 0,
                Err(Failure::Server(_)) => 0,
                Err(Failure::Disconnected) => return Outcome::Disconnected { in_finish: false },
            };
            let value = format!("c{client} txn {txn} oid {oid}");
            records.push((oid, serial, record(value.as_bytes())));
        }
        let id = Value::from(txn);
        let begin = vec![
            id.clone(),
            Value::Binary(format!("c{client}").into_bytes()),
            Value::Binary(Vec::new()),
            Value::Map(Vec::new()),
        ];
        if self
            .oneway(shared, client, "tpc_begin", begin)
            .await
            .is_err()
        {
            return Outcome::Disconnected { in_finish: false };
        }
        for (oid, serial, data) in &records {
            let args = vec![
                p64(*oid),
                p64(*serial),
                Value::Binary(data.clone()),
                id.clone(),
            ];
            if self.oneway(shared, client, "storea", args).await.is_err() {
                return Outcome::Disconnected { in_finish: false };
            }
        }
        match self.call(shared, client, "vote", vec![id.clone()]).await {
            Ok(_) => {}
            Err(Failure::Disconnected) => return Outcome::Disconnected { in_finish: false },
            Err(Failure::Server(class)) => {
                if self
                    .call(shared, client, "tpc_abort", vec![id])
                    .await
                    .is_err()
                {
                    return Outcome::Disconnected { in_finish: false };
                }
                return if class.ends_with("ConflictError") {
                    Outcome::Conflict
                } else {
                    Outcome::Failed
                };
            }
        }
        if let Some(stall) = stall.take() {
            shared.lock().trace(format!(
                "c{client} stalls {} s and keeps its locks",
                stall.as_secs()
            ));
            sleep(shared, stall).await;
        }
        match self.call(shared, client, "tpc_finish", vec![id]).await {
            Ok(tid) => match u64_of(&tid) {
                Some(tid) => Outcome::Acked(Acked {
                    tid,
                    records: records
                        .into_iter()
                        .map(|(oid, _, data)| (oid, data))
                        .collect(),
                }),
                None => Outcome::Failed,
            },
            Err(Failure::Disconnected) => Outcome::Disconnected { in_finish: true },
            Err(Failure::Server(_)) => Outcome::Failed,
        }
    }

    /// Call `method` and wait for its reply.
    async fn call(
        &mut self,
        shared: &Arc<Shared>,
        client: usize,
        method: &str,
        args: Vec<Value>,
    ) -> Result<Value, Failure> {
        self.next_id += 1;
        let id = self.next_id;
        shared.lock().trace(format!("c{client} > {method}"));
        let message = encode(Value::from(id), false, method, args);
        if frame::write(&mut self.stream, &message).await.is_err() {
            return self.lost(shared, client);
        }
        loop {
            let bytes = match frame::read(&mut self.stream).await {
                Ok(Some(bytes)) => bytes,
                _ => return self.lost(shared, client),
            };
            let Ok(Value::Array(parts)) = rmpv::decode::read_value(&mut bytes.as_slice()) else {
                return self.lost(shared, client);
            };
            let [reply_id, flags, kind, result] = <[Value; 4]>::try_from(parts).unwrap_or([
                Value::Nil,
                Value::Nil,
                Value::Nil,
                Value::Nil,
            ]);
            if kind.as_str() != Some(".reply") || reply_id.as_i64() != Some(id) {
                continue;
            }
            let mut state = shared.lock();
            if flags.as_u64() == Some(2) {
                let class = match &result {
                    Value::Array(parts) => parts
                        .first()
                        .and_then(Value::as_str)
                        .unwrap_or("?")
                        .to_owned(),
                    _ => "?".to_owned(),
                };
                state.trace(format!("c{client} < {method} {class}"));
                return Err(Failure::Server(class));
            }
            let shown = match (method, u64_of(&result)) {
                ("tpc_finish", Some(tid)) => format!(" tid {tid:016x}"),
                _ => String::new(),
            };
            state.trace(format!("c{client} < {method}{shown}"));
            return Ok(result);
        }
    }

    /// Send `method` without a reply.
    async fn oneway(
        &mut self,
        shared: &Arc<Shared>,
        client: usize,
        method: &str,
        args: Vec<Value>,
    ) -> Result<(), Failure> {
        shared.lock().trace(format!("c{client} > {method}"));
        let message = encode(Value::from(0), true, method, args);
        match frame::write(&mut self.stream, &message).await {
            Ok(()) => Ok(()),
            Err(_) => self.lost(shared, client).map(|_| ()),
        }
    }

    fn lost(&self, shared: &Arc<Shared>, client: usize) -> Result<Value, Failure> {
        shared
            .lock()
            .trace(format!("c{client} lost its connection"));
        Err(Failure::Disconnected)
    }
}

fn encode(id: Value, oneway: bool, method: &str, args: Vec<Value>) -> Vec<u8> {
    let message = Value::Array(vec![
        id,
        Value::Boolean(oneway),
        Value::from(method),
        Value::Array(args),
    ]);
    let mut bytes = Vec::new();
    rmpv::encode::write_value(&mut bytes, &message).expect("a message encodes");
    bytes
}

fn p64(n: u64) -> Value {
    Value::Binary(n.to_be_bytes().to_vec())
}

fn u64_of(value: &Value) -> Option<u64> {
    match value {
        Value::Binary(bytes) => <[u8; 8]>::try_from(bytes.as_slice())
            .ok()
            .map(u64::from_be_bytes),
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
