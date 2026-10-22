//! A test hook that reads and rewrites the stored bytes of a row, so that
//! a test can damage a frame and check that every read detects it. It
//! also writes and removes a row of `meta`, so that a test can make a
//! store of an older format.
//!
//! The module also holds the helpers that the tests of this crate share:
//! the unit tests and the integration tests. `tests/support` holds the
//! helpers that only the integration tests use.
//!
//! The feature `testing` builds this module and the test hooks of
//! `Store`. Only dev-dependencies turn the feature on: the one of this
//! crate on itself, and those of zoe-server and zoe-convert. So only test
//! builds have the hook. A release build never has it.

use std::collections::BTreeMap;
use std::io::{self, Read};
use std::ops::Deref;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id, Record};
use tracing::{Event, Level, Metadata, Subscriber};
use zoe_pickle::python::{Heap, Pickler, Value};

use crate::clock::Clock;
use crate::commit::Txn;
use crate::error::{Result, ZoeError};
use crate::kv::Flavour;
use crate::store::{Options, Store};
use crate::types::{Oid, Tid};
use crate::vfs::{DirLock, MemFile, MemVfs, Vfs, WriteFile};

/// A row of a store: its key space and its key.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Key {
    /// The record of the oid at the tid, in `records`.
    Record(Oid, Tid),
    /// The blob record of the oid at the tid, in `blobs`.
    Blob(Oid, Tid),
    /// The transaction row of the tid, in `txns`.
    Txn(Tid),
}

/// Return the bytes that `store` holds under `key`, as stored. A row that
/// does not exist is a `Backend` error.
pub fn read_frame(store: &Store, key: Key) -> Result<Vec<u8>> {
    store
        .backend
        .read_stored(key)?
        .ok_or_else(|| ZoeError::Backend(format!("no row under {key:?}").into()))
}

/// Store `bytes` under `key` in `store`, in place of the row. The hook
/// checks nothing and changes no other row.
pub fn write_frame(store: &Store, key: Key, bytes: &[u8]) -> Result<()> {
    store.backend.write_stored(key, bytes)
}

/// Store `bytes` in the row `key` of `meta`. The hook checks nothing and
/// changes no other row. The open store keeps the state that it read at
/// its open, so the change counts from the next open.
pub fn write_meta(store: &Store, key: &str, bytes: &[u8]) -> Result<()> {
    store.backend.write_meta_stored(key, Some(bytes))
}

/// Remove the row `key` of `meta`, as `write_meta` changes it. A row that
/// does not exist is a `Backend` error.
pub fn remove_meta(store: &Store, key: &str) -> Result<()> {
    store.backend.write_meta_stored(key, None)
}

/// Return the key of the row of `key` in the namespace `name`, as the
/// format writes it.
pub fn kv_row_key(name: &[u8], key: &[u8]) -> Vec<u8> {
    crate::format::kv_key(name, key)
}

/// Return the key of the index of the expiries for the row `row` that
/// expires at `expiry`, in milliseconds since the Unix epoch.
pub fn kv_expiry_key(expiry: u64, row: &[u8]) -> Vec<u8> {
    crate::format::kv_expiry_key(expiry, row)
}

/// Return the bytes that `store` holds for the row `row` of the namespaces
/// of `flavour`, as stored. A row that does not exist is a `Backend` error.
pub fn read_kv_frame(store: &Store, flavour: Flavour, row: &[u8]) -> Result<Vec<u8>> {
    store
        .backend
        .kv_row(flavour, row)?
        .ok_or_else(|| ZoeError::Backend("no namespace row under the key".into()))
}

/// Store `bytes` for the row `row` of the namespaces of `flavour` in
/// `store`, in place of the row, or remove the row for `None`. The hook
/// checks nothing and changes no other row.
pub fn write_kv_frame(
    store: &Store,
    flavour: Flavour,
    row: &[u8],
    bytes: Option<&[u8]>,
) -> Result<()> {
    store.backend.write_kv_stored(Some(flavour), row, bytes)
}

/// Put `key` in the index of the expiries of `store`, or remove it for
/// `false`. The hook checks nothing and changes no other row.
pub fn write_kv_expiry_key(store: &Store, key: &[u8], present: bool) -> Result<()> {
    store
        .backend
        .write_kv_stored(None, key, present.then_some(&[][..]))
}

/// Return the number of zstd compressions of records that this thread
/// made. The finish of a transaction runs on the thread that calls it
/// when no other finish leads a batch, so a test reads what its finish
/// compressed.
pub fn compressions() -> u64 {
    crate::format::compressions()
}

/// The user, the description and the extension of a transaction.
pub type Meta<'m> = (&'m [u8], &'m [u8], &'m [u8]);

/// Vote and finish `txn`. Return the tid of its commit.
pub fn commit_txn<S: Deref<Target = Store>>(mut txn: Txn<S>) -> Tid {
    txn.vote().unwrap();
    txn.finish().unwrap()
}

/// Commit `writes` in one transaction of `meta`. Each write is an oid,
/// the serial that the transaction read and a pickle. Return the tid.
pub fn commit_as<D: AsRef<[u8]>>(store: &Store, meta: Meta<'_>, writes: &[(Oid, Tid, D)]) -> Tid {
    let (user, description, extension) = meta;
    let mut txn = store.tpc_begin(user, description, extension).unwrap();
    for (oid, serial, data) in writes {
        txn.store(*oid, *serial, data.as_ref()).unwrap();
    }
    commit_txn(txn)
}

/// Commit `writes` in one transaction, as `commit_as` does, with an empty
/// user, description and extension.
pub fn commit_all<D: AsRef<[u8]>>(store: &Store, writes: &[(Oid, Tid, D)]) -> Tid {
    commit_as(store, (b"", b"", b""), writes)
}

/// Commit `data` as the record of `oid` over `serial`, in a transaction
/// with an empty user, description and extension. Return the tid.
pub fn commit(store: &Store, oid: Oid, serial: Tid, data: &[u8]) -> Tid {
    commit_all(store, &[(oid, serial, data)])
}

/// Return a new oid of `store` other than the root oid 0.
pub fn new_oid(store: &Store) -> Oid {
    loop {
        let oid = store.new_oid().unwrap();
        if oid != Oid(0) {
            return oid;
        }
    }
}

/// Return the pickle of a record of `class` and `state`.
pub fn record(heap: &Heap, class: Value, state: Value) -> Vec<u8> {
    let mut pickler = Pickler::new(heap);
    pickler.dump(class).unwrap();
    pickler.dump(state).unwrap();
    pickler.into_bytes()
}

/// Return the root of ZODB, a `PersistentMapping` that names the merge
/// policy at `policy`, as ZODB writes it.
pub fn root(policy: Oid) -> Vec<u8> {
    [
        &b"\x80\x03cpersistent.mapping\nPersistentMapping\nq\x00.\x80\x03}q\x01X\x04\x00\x00\x00\
           dataq\x02}q\x03X\x10\x00\x00\x00zoe.merge_policyq\x04C\x08"[..],
        &policy.0.to_be_bytes(),
        b"q\x05czoe.policy\nMergePolicy\nq\x06\x86q\x07Qss.",
    ]
    .concat()
}

/// Wait until `done` is true. Fail the test after 10 s.
pub fn wait_until(done: impl Fn() -> bool) {
    let start = Instant::now();
    while !done() {
        assert!(start.elapsed() < Duration::from_secs(10), "timed out");
        thread::sleep(Duration::from_millis(1));
    }
}

/// A clock that moves only when a test moves it. Its clones share the
/// time.
#[derive(Clone, Debug)]
pub struct ManualClock(Arc<Mutex<SystemTime>>);

impl ManualClock {
    /// Return a clock at a fixed time.
    pub fn new() -> Self {
        let start = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        Self(Arc::new(Mutex::new(start)))
    }

    /// Move the clock forward by `by`.
    pub fn advance(&self, by: Duration) {
        *self.0.lock().unwrap() += by;
    }

    /// Return the time of the clock in milliseconds since the Unix epoch.
    pub fn millis(&self) -> u64 {
        let now = *self.0.lock().unwrap();
        now.duration_since(UNIX_EPOCH).unwrap().as_millis() as u64
    }
}

impl Default for ManualClock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock for ManualClock {
    fn now(&self) -> SystemTime {
        *self.0.lock().unwrap()
    }
}

/// Open a store in memory with `options` and a clock that the test moves.
pub fn open_with_clock(options: Options) -> (Store, ManualClock) {
    let clock = ManualClock::new();
    let store = Store::open_full(MemVfs::new(), clock.clone(), options).unwrap();
    (store, clock)
}

/// One event that a capture of `tracing` kept: its level, and each field
/// as text.
#[derive(Clone, Debug)]
pub struct Captured {
    /// The level of the event.
    pub level: Level,
    /// Each field of the event and its value as text. The message is the
    /// field `message`.
    pub fields: BTreeMap<String, String>,
}

impl Captured {
    /// Return the message of the event, or an empty text.
    pub fn message(&self) -> &str {
        self.fields.get("message").map_or("", String::as_str)
    }

    /// Return the field `field` as a number, or `None` when it has no
    /// number.
    pub fn number(&self, field: &str) -> Option<u64> {
        self.fields.get(field)?.parse().ok()
    }
}

/// The fields of one event, as text.
#[derive(Default)]
struct Fields(BTreeMap<String, String>);

impl Visit for Fields {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.0.insert(field.name().to_owned(), format!("{value:?}"));
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        self.0.insert(field.name().to_owned(), value.to_owned());
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
        self.0.insert(field.name().to_owned(), value.to_string());
    }

    fn record_i64(&mut self, field: &Field, value: i64) {
        self.0.insert(field.name().to_owned(), value.to_string());
    }
}

/// A `tracing` subscriber that keeps each event in a list.
struct Capture {
    events: Arc<Mutex<Vec<Captured>>>,
    next_span: AtomicU64,
}

impl Capture {
    fn new(events: &Arc<Mutex<Vec<Captured>>>) -> Self {
        Capture {
            events: Arc::clone(events),
            next_span: AtomicU64::new(1),
        }
    }
}

impl Subscriber for Capture {
    fn enabled(&self, _: &Metadata<'_>) -> bool {
        true
    }

    fn new_span(&self, _: &Attributes<'_>) -> Id {
        Id::from_u64(self.next_span.fetch_add(1, Ordering::Relaxed))
    }

    fn record(&self, _: &Id, _: &Record<'_>) {}

    fn record_follows_from(&self, _: &Id, _: &Id) {}

    fn event(&self, event: &Event<'_>) {
        let mut fields = Fields::default();
        event.record(&mut fields);
        let captured = Captured {
            level: *event.metadata().level(),
            fields: fields.0,
        };
        crate::lock(&self.events).push(captured);
    }

    fn enter(&self, _: &Id) {}

    fn exit(&self, _: &Id) {}
}

/// Run `f` with a capture of the events of this thread. Return the result
/// of `f` and the events.
pub fn events_of<R>(f: impl FnOnce() -> R) -> (R, Vec<Captured>) {
    let events = Arc::new(Mutex::new(Vec::new()));
    let result = tracing::subscriber::with_default(Capture::new(&events), f);
    let events = crate::lock(&events).clone();
    (result, events)
}

/// The events of every thread of the process, once `capture_events` ran.
static EVENTS: OnceLock<Arc<Mutex<Vec<Captured>>>> = OnceLock::new();

/// Install a capture of the events of every thread of the process, once.
pub fn capture_events() {
    EVENTS.get_or_init(|| {
        let events = Arc::new(Mutex::new(Vec::new()));
        tracing::subscriber::set_global_default(Capture::new(&events)).unwrap();
        events
    });
}

/// Return every event that the capture of `capture_events` kept.
pub fn events() -> Vec<Captured> {
    EVENTS
        .get()
        .map_or_else(Vec::new, |events| crate::lock(events).clone())
}

/// The database file of a store.
const DATABASE: &str = "zoe.redb";

/// A `MemVfs` that counts the syncs of the database file. Its clones
/// share the files and the count.
#[derive(Clone, Default)]
pub struct CountingVfs {
    vfs: MemVfs,
    syncs: Arc<AtomicU64>,
}

/// A file of `CountingVfs`.
#[derive(Debug)]
pub struct CountedFile {
    file: MemFile,
    /// The count of the database file, `None` for another file.
    syncs: Option<Arc<AtomicU64>>,
}

impl CountingVfs {
    /// Return the number of syncs of the database file.
    pub fn syncs(&self) -> u64 {
        self.syncs.load(Ordering::SeqCst)
    }

    fn counted(&self, name: &str, file: MemFile) -> CountedFile {
        let syncs = (name == DATABASE).then(|| Arc::clone(&self.syncs));
        CountedFile { file, syncs }
    }
}

impl redb::StorageBackend for CountedFile {
    fn len(&self) -> io::Result<u64> {
        self.file.len()
    }

    fn read(&self, offset: u64, out: &mut [u8]) -> io::Result<()> {
        self.file.read(offset, out)
    }

    fn set_len(&self, len: u64) -> io::Result<()> {
        self.file.set_len(len)
    }

    fn sync_data(&self) -> io::Result<()> {
        if let Some(syncs) = &self.syncs {
            syncs.fetch_add(1, Ordering::SeqCst);
        }
        self.file.sync_data()
    }

    fn write(&self, offset: u64, data: &[u8]) -> io::Result<()> {
        self.file.write(offset, data)
    }
}

impl Vfs for CountingVfs {
    type File = CountedFile;

    fn exists(&self, name: &str) -> io::Result<bool> {
        self.vfs.exists(name)
    }

    fn open(&self, name: &str) -> io::Result<CountedFile> {
        Ok(self.counted(name, self.vfs.open(name)?))
    }

    fn create_new(&self, name: &str) -> io::Result<CountedFile> {
        Ok(self.counted(name, self.vfs.create_new(name)?))
    }

    fn rename(&self, from: &str, to: &str) -> io::Result<()> {
        self.vfs.rename(from, to)
    }

    fn hard_link(&self, from: &str, to: &str) -> io::Result<()> {
        self.vfs.hard_link(from, to)
    }

    fn remove(&self, name: &str) -> io::Result<()> {
        self.vfs.remove(name)
    }

    fn sync_dir(&self) -> io::Result<()> {
        self.vfs.sync_dir()
    }

    fn sync_dir_at(&self, dir: &str) -> io::Result<()> {
        self.vfs.sync_dir_at(dir)
    }

    fn create_dir(&self, name: &str) -> io::Result<()> {
        self.vfs.create_dir(name)
    }

    fn list_files(&self, dir: &str) -> io::Result<Vec<String>> {
        self.vfs.list_files(dir)
    }

    fn list_entries(&self, dir: &str) -> io::Result<Vec<String>> {
        self.vfs.list_entries(dir)
    }

    fn create_file(&self, name: &str) -> io::Result<Box<dyn WriteFile>> {
        self.vfs.create_file(name)
    }

    fn read_file(&self, name: &str) -> io::Result<Box<dyn Read + Send>> {
        self.vfs.read_file(name)
    }

    fn sync_file(&self, name: &str) -> io::Result<()> {
        self.vfs.sync_file(name)
    }

    fn set_read_only(&self, name: &str) -> io::Result<()> {
        self.vfs.set_read_only(name)
    }

    fn lock_dir(&self, name: &str) -> io::Result<DirLock> {
        self.vfs.lock_dir(name)
    }

    fn path(&self, name: &str) -> PathBuf {
        self.vfs.path(name)
    }
}
