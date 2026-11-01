//! The acceptance tests of the compression pool: a `storea` replies
//! before its encode ends, the vote waits for the encodes of its
//! transaction before it takes a lock, a failed encode fails the vote, and
//! an abort or a lost connection cancels the encodes that wait and leaves
//! no lock.

use std::num::NonZeroUsize;
use std::sync::Condvar;
use std::time::{Duration, Instant};

use zoe_core::{Codec, Encoded};

use super::*;
use crate::protocol::compress::{Compressor, Hook};
use crate::runtime::Tokio;

/// The gate of the jobs of a compression pool. It records the oid
/// of each job that starts. While it is closed, it holds the first
/// `hold` jobs. A job of the oid `fail` fails.
struct Gate {
    state: Mutex<GateState>,
    changed: Condvar,
}

struct GateState {
    started: Vec<Oid>,
    closed: bool,
    hold: usize,
    fail: Option<Oid>,
}

impl Gate {
    fn new(hold: usize, fail: Option<Oid>) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(GateState {
                started: Vec::new(),
                closed: hold > 0,
                hold,
                fail,
            }),
            changed: Condvar::new(),
        })
    }

    fn lock(&self) -> MutexGuard<'_, GateState> {
        self.state.lock().unwrap()
    }

    fn hook(self: &Arc<Self>) -> Hook {
        let gate = Arc::clone(self);
        Arc::new(move |oid| {
            let mut state = gate.lock();
            state.started.push(oid);
            let held = state.started.len() <= state.hold;
            gate.changed.notify_all();
            while held && state.closed {
                state = gate.changed.wait(state).unwrap();
            }
            state.fail != Some(oid)
        })
    }

    /// Let the held jobs go on, and hold no later job.
    fn open(&self) {
        self.lock().closed = false;
        self.changed.notify_all();
    }

    /// Wait until `n` jobs started. Fail after 30 seconds.
    fn wait_started(&self, n: usize) {
        let deadline = Instant::now() + Duration::from_secs(30);
        let mut state = self.lock();
        while state.started.len() < n {
            let left = deadline.saturating_duration_since(Instant::now());
            assert!(!left.is_zero(), "{n} jobs start: {:?}", state.started);
            state = self.changed.wait_timeout(state, left).unwrap().0;
        }
    }

    fn started(&self) -> Vec<Oid> {
        self.lock().started.clone()
    }
}

/// A tokio runtime for the tasks of the jobs.
fn tokio() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap()
}

/// A storage whose votes wait at most 200 ms for a lock, so a test
/// sees a lock that stays. Its store compresses with zstd at level
/// 3, so its transactions encode on the compression pool.
fn storage(name: &str) -> Arc<NamedStorage> {
    let dir = std::env::temp_dir().join(format!(
        "zoe-session-compress-{name}-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    let options = Options {
        lock_timeout: Duration::from_millis(200),
        compression: Compression::Zstd { level: 3 },
        ..Options::default()
    };
    let store = Arc::new(Store::open_with(&dir, options).unwrap());
    Arc::new(NamedStorage::new("1", &dir, store, Queues::DEFAULT))
}

/// A registered connection of `storage` whose transactions encode
/// on `compressor`. Call it inside a tokio runtime.
fn client(storage: &Arc<NamedStorage>, compressor: &Compressor) -> Client {
    let session = Arc::new(Session::new(Arc::clone(storage), outbox(), never_stops()));
    let txn =
        Transaction::new(Arc::clone(&session)).compressing(compressor.clone(), Arc::new(Tokio));
    let mut s = Client {
        session,
        txn,
        sent: None,
    };
    let register = vec![Value::from("1"), Value::Boolean(false)];
    done(call(&mut s, "register", register));
    s
}

/// A pickle of 100 KB that zstd compresses well.
fn large(oid: u64) -> Vec<u8> {
    let mut data = format!("pickle of {oid} ").into_bytes();
    while data.len() < 100 * 1024 {
        data.extend_from_slice(b"X\x05\x00\x00\x00titleq\x02X\x04\x00\x00\x00text");
    }
    data
}

/// Run the transaction `id` of `s` on another thread, inside
/// `tokio`: it begins, stores `oid` with each pickle of `stores`
/// over the serial 0, tells the receiver that each `storea`
/// returned, then votes. The core takes the calls of a transaction
/// only from the thread that began it.
fn transaction_elsewhere(
    tokio: &tokio::runtime::Runtime,
    mut s: Client,
    id: i64,
    stores: Vec<(u64, Vec<u8>)>,
) -> (
    std::sync::mpsc::Receiver<()>,
    std::thread::JoinHandle<(Client, Outcome)>,
) {
    let handle = tokio.handle().clone();
    let (stored, returned) = std::sync::mpsc::channel();
    let thread = std::thread::spawn(move || {
        let _entered = handle.enter();
        begin(&mut s, id, None);
        for (oid, data) in &stores {
            store(&mut s, id, *oid, 0, data);
        }
        stored.send(()).unwrap();
        let outcome = call(&mut s, "vote", vec![Value::from(id)]);
        (s, outcome)
    });
    (returned, thread)
}

#[test]
fn a_storea_returns_before_its_encode_ends_and_the_vote_waits_for_it() {
    let tokio = tokio();
    let _entered = tokio.enter();
    let gate = Gate::new(usize::MAX, None);
    let compressor = Compressor::new(NonZeroUsize::new(2).unwrap()).with_hook(gate.hook());
    let storage = storage("held");
    let s = client(&storage, &compressor);
    let stores = (1..=3).map(|oid| (oid, large(oid))).collect();
    let (stored, voting) = transaction_elsewhere(&tokio, s, 1, stores);
    // Each `storea` returns while the gate holds the encodes.
    stored.recv_timeout(Duration::from_secs(30)).unwrap();
    gate.wait_started(2);
    std::thread::sleep(Duration::from_millis(300));
    assert!(!voting.is_finished(), "the vote waits for the encodes");
    gate.open();
    let (mut s, vote) = voting.join().unwrap();
    assert_eq!(done(vote), Value::Array(vec![]));
    let tid = finish(&mut s, Value::from(1));
    assert_eq!(gate.started().len(), 3);
    for oid in 1..=3 {
        let data = large(oid);
        assert_eq!(s.store.load(Oid(oid)).unwrap(), (data.clone(), Tid(tid)));
        let info = s.store.record_info(Oid(oid), Tid(tid)).unwrap();
        let inline = Encoded::encode(&data, s.store.compression());
        assert_eq!(info.codec, Codec::Zstd);
        assert_eq!(info.stored_len, inline.stored_len() as u64);
    }
}

#[test]
fn a_failed_encode_fails_the_vote_and_leaves_no_lock() {
    let tokio = tokio();
    let _entered = tokio.enter();
    let gate = Gate::new(0, Some(Oid(2)));
    let compressor = Compressor::new(NonZeroUsize::new(2).unwrap()).with_hook(gate.hook());
    let storage = storage("failed");
    let mut s = client(&storage, &compressor);
    begin(&mut s, 1, None);
    for oid in 1..=3 {
        store(&mut s, 1, oid, 0, &large(oid));
    }
    let fault = failed(call(&mut s, "vote", vec![Value::from(1)]));
    assert_eq!(fault.class, "ZODB.POSException.StorageError");
    assert!(s.txn.open.is_none());
    assert!(s.store.load(Oid(1)).is_err());
    assert_eq!(count(&storage.status().to_value(), "active_txns"), 0);
    // No lock stays: each oid commits at once.
    for oid in [1, 3] {
        commit(&mut s, 10 + oid as i64, oid, 0, &large(oid)).unwrap();
    }
}

#[test]
fn a_vote_that_waits_for_its_encodes_holds_no_lock() {
    let tokio = tokio();
    let _entered = tokio.enter();
    let gate = Gate::new(1, None);
    let compressor = Compressor::new(NonZeroUsize::new(2).unwrap()).with_hook(gate.hook());
    let storage = storage("no-lock");
    let waiting = client(&storage, &compressor);
    let (stored, voting) = transaction_elsewhere(&tokio, waiting, 1, vec![(1, large(1))]);
    stored.recv_timeout(Duration::from_secs(30)).unwrap();
    gate.wait_started(1);
    std::thread::sleep(Duration::from_millis(100));
    assert!(!voting.is_finished());

    // Another transaction writes the oid and commits while the vote
    // waits: the waiting vote holds no lock.
    let mut other = client(&storage, &compressor);
    let tid = commit(&mut other, 2, 1, 0, &large(1)).unwrap();
    gate.open();
    let (_, vote) = voting.join().unwrap();
    let fault = failed(vote);
    assert_eq!(fault.class, CONFLICT_ERROR);
    assert_eq!(
        item(&fault, "serials"),
        Value::Array(vec![p64(tid), p64(0)])
    );
}

#[test]
fn an_abort_cancels_the_encodes_that_wait_and_leaves_no_lock() {
    let tokio = tokio();
    let _entered = tokio.enter();
    let gate = Gate::new(1, None);
    let compressor = Compressor::new(NonZeroUsize::MIN).with_hook(gate.hook());
    let storage = storage("abort");
    let mut s = client(&storage, &compressor);
    begin(&mut s, 1, None);
    for oid in 1..=3 {
        store(&mut s, 1, oid, 0, &large(oid));
    }
    gate.wait_started(1);
    done(call(&mut s, "tpc_abort", vec![Value::from(1)]));
    assert!(s.txn.open.is_none());
    gate.open();
    // The one thread of the pool runs the job of the oid 4 after
    // the jobs that waited before it.
    commit(&mut s, 2, 4, 0, &large(4)).unwrap();
    assert_eq!(gate.started(), vec![Oid(1), Oid(4)]);
    for oid in 1..=3 {
        commit(&mut s, 2 + oid as i64, oid, 0, &large(oid)).unwrap();
    }
}

#[test]
fn a_lost_connection_cancels_the_encodes_that_wait_and_leaves_no_lock() {
    let gate = Gate::new(1, None);
    let compressor = Compressor::new(NonZeroUsize::MIN).with_hook(gate.hook());
    let storage = storage("lost");
    let storages = Arc::new(
        Storages::new(vec![Arc::clone(&storage)], Queues::DEFAULT.client)
            .with_compressor(compressor.clone()),
    );
    let (calls, received) = tokio::sync::mpsc::channel(crate::protocol::QUEUE);
    let (replies, _sent, _closing) = Outbox::new(1_000, 1_000);
    let session = std::thread::spawn(move || run(storages, received, replies));
    let send = |method: &str, args: Vec<Value>| {
        let call = Call {
            id: Value::from(0),
            oneway: true,
            method: method.into(),
            args,
            context: None,
        };
        calls.blocking_send(call).unwrap();
    };
    send("register", vec![Value::from("1"), Value::Boolean(false)]);
    let begin = vec![Value::from(7), Value::Nil, Value::Nil, Value::Map(vec![])];
    send("tpc_begin", begin);
    for oid in 1..=3 {
        let data = Value::Binary(large(oid));
        send("storea", vec![p64(oid), p64(0), data, Value::from(7)]);
    }
    gate.wait_started(1);
    // The client goes. The end of the connection aborts the
    // transaction while its first encode is held.
    drop(calls);
    let deadline = Instant::now() + Duration::from_secs(30);
    while count(&storage.status().to_value(), "aborts") == 0 {
        assert!(Instant::now() < deadline, "the transaction aborts");
        std::thread::sleep(Duration::from_millis(10));
    }
    gate.open();
    session.join().unwrap();

    let tokio = tokio();
    let _entered = tokio.enter();
    let mut s = client(&storage, &compressor);
    commit(&mut s, 2, 4, 0, &large(4)).unwrap();
    assert_eq!(gate.started(), vec![Oid(1), Oid(4)]);
    for oid in 1..=3 {
        commit(&mut s, 2 + oid as i64, oid, 0, &large(oid)).unwrap();
    }
}

#[test]
fn the_pool_stores_the_records_of_an_inline_encode() {
    let tokio = tokio();
    let _entered = tokio.enter();
    let compressor = Compressor::new(NonZeroUsize::new(2).unwrap());
    let (pooled, inline) = (storage("pooled"), storage("inline"));
    let mut a = client(&pooled, &compressor);
    let mut b = Client::new(Arc::clone(&inline), outbox());
    done(call(
        &mut b,
        "register",
        vec![Value::from("1"), Value::Boolean(false)],
    ));
    let tid = 0x0300_0000_0000_0001;
    let records: Vec<(u64, Vec<u8>)> = vec![
        (1, large(1)),
        (2, MAPPING.to_vec()),
        (
            3,
            (0..5_000u64)
                .map(|i| ((i * 2_654_435_761) >> 13) as u8)
                .collect(),
        ),
    ];
    for s in [&mut a, &mut b] {
        begin(s, 1, Some(tid));
        for (oid, data) in &records {
            let args = vec![
                p64(*oid),
                p64(0),
                Value::Binary(data.clone()),
                Value::Nil,
                Value::from(1),
            ];
            assert_eq!(done(call(s, "restorea", args)), Value::Nil);
        }
        assert_eq!(vote_and_finish(s, 1).unwrap(), tid);
    }
    for (oid, data) in &records {
        let (oid, tid) = (Oid(*oid), Tid(tid));
        assert_eq!(a.store.load(oid).unwrap(), (data.clone(), tid));
        assert_eq!(
            a.store.record_info(oid, tid).unwrap(),
            b.store.record_info(oid, tid).unwrap()
        );
    }
}
