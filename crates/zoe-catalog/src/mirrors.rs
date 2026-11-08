//! The mirrors of one store, and their thread.
//!
//! The store lists its catalogs in the durable namespace `zoe.catalog`: one
//! registration key for each catalog, the 8 bytes of the oid of its
//! `Catalog` object. The thread reads the namespace when it starts and
//! after each commit that writes it. A new key starts a build; a removed
//! key drops the mirror. There is no scan of the store and no build on a
//! first query.
//!
//! A build runs in a thread of its own, one build at a time in the
//! process, as of the last tid that the store published when it starts.
//! When the base is ready, the thread applies the commits after its tid,
//! in tid order, through the transaction index. Then the mirror is ready.
//!
//! The store hands each commit to the thread after its publication
//! (`committed`). The publication never waits for the mirror. A query
//! whose snapshot holds a commit that the mirror did not apply waits for
//! it, at most `wait`; past it, the query takes the Python path (`lag`).
//!
//! The mirrors of a store stay within `memory`: the versions of each
//! mirror, and the memory of a build or a rebase in progress. A build, a
//! rebase or a growth that would pass the bound drops the mirror that
//! answered a query least recently.
//!
//! A query can carry a transaction overlay (`overlay`): the mirror reads it
//! over the version of the query, with the shapes of the trees that the
//! thread keeps, for that query only. The bound counts the overlays of the
//! queries that run: an overlay that does not fit beside the mirrors and
//! the other overlays takes the Python path (`overlay`). An overlay never
//! makes the bound drop a mirror, and a build, a rebase or a growth does
//! not count the overlays.
//!
//! The thread holds a share of the store only while it works, so the store
//! can close between two passes, and a build takes a share for each read.
//! `stop` stops the thread and the builds, and waits for them.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
use std::sync::{
    Arc, Condvar, Mutex, MutexGuard, PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard,
};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::base::Base;
use crate::build::{BuildError, Built, build};
use crate::change::{ChangeSet, Shapes, read_records};
use crate::counts::{Counters, Counts, MirrorCounts};
use crate::mirror::{Versions, rebase};
use crate::overlay;
use crate::plan::{ClauseKind, PlanError, Wire, decode_typed};
use crate::query::{Columns, Plan, Reply, View};
use crate::read::{Commit, Reads};
use crate::reason::{Fallback, Reason};

/// Gives a share of the store while it is open, or `None` after it closed.
pub type Opener = Arc<dyn Fn() -> Option<Arc<dyn Reads>> + Send + Sync>;

/// A hook of the tests, called with an oid or a tid.
pub type Hook = Arc<dyn Fn(u64) + Send + Sync>;

/// One build at a time in the process.
static BUILDS: Mutex<()> = Mutex::new(());

/// The longest sleep of the thread between two passes: the change sets
/// leave the window as time passes.
const TICK: Duration = Duration::from_secs(1);

/// The time before a failed rebase runs again.
const REBASE_RETRY: Duration = Duration::from_secs(60);

/// The settings of the mirrors of a store: the keys `catalog-memory`,
/// `catalog-window-tids`, `catalog-window-seconds`, `catalog-rebase-rows`
/// and `catalog-wait`.
#[derive(Clone)]
pub struct Config {
    /// The bound of the memory of the mirrors, in bytes.
    pub memory: usize,
    /// The newer change sets past which a change set leaves the window.
    pub window_tids: usize,
    /// The age past which a change set leaves the window.
    pub window: Duration,
    /// The rows of the overrides past which the mirror rebases.
    pub rebase_rows: usize,
    /// The longest wait of a query for the mirror to reach its tid.
    pub wait: Duration,
    /// The threads of a build.
    pub threads: usize,
    /// The hooks of the tests.
    pub hooks: Hooks,
}

impl Default for Config {
    fn default() -> Config {
        Config {
            memory: 2 << 30,
            window_tids: 10_000,
            window: Duration::from_secs(600),
            rebase_rows: 20_000,
            wait: Duration::from_secs(1),
            threads: thread::available_parallelism().map_or(4, |n| n.get()),
            hooks: Hooks::default(),
        }
    }
}

/// The hooks of the tests. Each runs in a thread of the mirrors, with no
/// lock held.
#[derive(Clone, Default)]
pub struct Hooks {
    /// Before a build, with the oid of the catalog.
    pub before_build: Option<Hook>,
    /// Before the apply of a commit, with its tid.
    pub before_apply: Option<Hook>,
    /// Before a rebase lays its new base, with the oid of the catalog.
    pub before_swap: Option<Hook>,
    /// Before a query reads its overlay, after it took its version, with
    /// the oid of the catalog.
    pub before_overlay: Option<Hook>,
}

/// The state of one mirror, as `status` gives it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Status {
    /// `pending`, `building`, `ready`, `refused` or `dropped`.
    pub state: &'static str,
    /// Why the catalog has no mirror, when it is refused.
    pub reason: Option<String>,
    /// The tid of the base, when the mirror is ready.
    pub base_tid: Option<u64>,
    /// The last tid that the mirror applied, when it is ready.
    pub applied: Option<u64>,
    /// The first tid whose version the mirror holds, when it is ready.
    pub floor: Option<u64>,
    /// The change sets that the mirror holds.
    pub sets: usize,
    /// The tid of the commit that made the mirror stale, while it builds
    /// again.
    pub stale_at: Option<u64>,
    /// The rebases that the mirror laid.
    pub rebases: u64,
    /// The bytes that the mirror takes.
    pub bytes: usize,
}

enum State {
    Pending,
    Building,
    Ready(Versions),
    Refused(String),
    Dropped,
}

struct Entry {
    state: State,
    /// The shapes and the objects of a ready mirror.
    maint: Option<Arc<RwLock<Maint>>>,
    stale_at: Option<u64>,
    /// The order of the last answer of the mirror; 0 for none.
    last_answer: u64,
    build_seconds: f64,
    /// The bytes of the versions.
    bytes: usize,
    /// The bytes of the versions when the bound dropped the mirror.
    last_bytes: usize,
    rebasing: bool,
    rebase_failed: Option<Instant>,
    rebases: u64,
}

impl Entry {
    fn new() -> Entry {
        Entry {
            state: State::Pending,
            maint: None,
            stale_at: None,
            last_answer: 0,
            build_seconds: 0.0,
            bytes: 0,
            last_bytes: 0,
            rebasing: false,
            rebase_failed: None,
            rebases: 0,
        }
    }
}

struct Inner {
    entries: BTreeMap<u64, Entry>,
    /// The last tid that the store published, as the hand-offs give it.
    target: u64,
    /// The tids of the recent hand-offs.
    handed: VecDeque<u64>,
    /// The generation of the namespace at its last read.
    generation: Option<u64>,
    stop: bool,
    /// The thread has work: a hand-off, or the end of a build or a rebase.
    dirty: bool,
    events: Vec<String>,
    built: Option<(u64, Result<Built, BuildError>)>,
    rebased: Vec<(u64, Result<Base, String>)>,
    building: bool,
    answers: u64,
    /// The bytes of the transaction overlays of the queries that run.
    overlays: usize,
}

struct Shared {
    inner: Mutex<Inner>,
    changed: Condvar,
    config: Config,
    open: Opener,
    counters: Arc<Counters>,
    cancel: AtomicBool,
}

/// The state that the thread keeps for the maintenance of a mirror. A query
/// with an overlay reads it.
struct Maint {
    shapes: Shapes,
    objects: HashMap<u64, Vec<u8>>,
    /// The tid of the last commit that changed the shape of each node,
    /// after the base of the build. A node at or before the floor of the
    /// versions leaves it.
    moved: HashMap<u64, u64>,
    /// The floor of the versions at the last prune of `moved`.
    pruned: u64,
}

impl Maint {
    fn heap_bytes(&self) -> usize {
        self.shapes.heap_bytes() + self.moved.capacity() * 16
    }
}

fn read(maint: &RwLock<Maint>) -> RwLockReadGuard<'_, Maint> {
    maint.read().unwrap_or_else(PoisonError::into_inner)
}

fn write(maint: &RwLock<Maint>) -> RwLockWriteGuard<'_, Maint> {
    maint.write().unwrap_or_else(PoisonError::into_inner)
}

/// The bytes of the maintenance of a mirror.
fn maint_bytes(e: &Entry) -> usize {
    e.maint.as_ref().map_or(0, |m| read(m).heap_bytes())
}

/// The overlay bytes that a query holds in the bound; the drop gives them
/// back.
struct Held<'a> {
    shared: &'a Shared,
    bytes: usize,
}

impl Drop for Held<'_> {
    fn drop(&mut self) {
        if self.bytes > 0 {
            let mut inner = self.shared.lock();
            inner.overlays = inner.overlays.saturating_sub(self.bytes);
        }
    }
}

/// The mirrors of one store.
pub struct Mirrors {
    shared: Arc<Shared>,
    thread: Mutex<Option<JoinHandle<()>>>,
}

impl Mirrors {
    /// Starts the thread of the mirrors of the store that `open` gives. The
    /// store has published each commit up to `published`.
    pub fn start(open: Opener, published: u64, config: Config) -> Mirrors {
        Mirrors::start_counting(open, published, config, Arc::default())
    }

    /// Starts the thread as `start` does, with the counters `counters`. The
    /// host of the mirrors counts on them the queries that it refuses
    /// before the mirrors start.
    pub fn start_counting(
        open: Opener,
        published: u64,
        config: Config,
        counters: Arc<Counters>,
    ) -> Mirrors {
        let shared = Arc::new(Shared {
            inner: Mutex::new(Inner {
                entries: BTreeMap::new(),
                target: published,
                handed: VecDeque::new(),
                generation: None,
                stop: false,
                dirty: true,
                events: Vec::new(),
                built: None,
                rebased: Vec::new(),
                building: false,
                answers: 0,
                overlays: 0,
            }),
            changed: Condvar::new(),
            config,
            open,
            counters,
            cancel: AtomicBool::new(false),
        });
        let worker = Arc::clone(&shared);
        let thread = thread::Builder::new()
            .name("zoe-catalog".to_owned())
            .spawn(move || run(&worker))
            .ok();
        if thread.is_none() {
            shared
                .lock()
                .events
                .push("the thread of the catalog mirrors did not start".into());
        }
        Mirrors {
            shared,
            thread: Mutex::new(thread),
        }
    }

    /// Hands the commit `tid` to the thread, after its publication. It
    /// never waits for the mirror.
    pub fn committed(&self, tid: u64) {
        let mut inner = self.shared.lock();
        inner.target = inner.target.max(tid);
        inner.handed.push_back(tid);
        if inner.handed.len() > 100_000 {
            inner.handed.pop_front();
        }
        inner.dirty = true;
        drop(inner);
        self.shared.changed.notify_all();
    }

    /// Answers `plan` on the catalog `catalog` as of the snapshot that ends
    /// before `tid`, as the tid of `loadBefore`.
    pub fn query(&self, catalog: u64, tid: u64, plan: &Plan) -> Result<Reply, Fallback> {
        self.query_rows(catalog, tid, plan, &Columns::default())
    }

    /// Answers `plan` as `query` does, with the rows of `columns`.
    pub fn query_rows(
        &self,
        catalog: u64,
        tid: u64,
        plan: &Plan,
        columns: &Columns,
    ) -> Result<Reply, Fallback> {
        self.query_typed(catalog, tid, plan, columns, &[], None)
    }

    /// Answers `plan` as `query_rows` does, over the transaction overlay
    /// `records`: the version at `tid` with the records, for this query
    /// only. A record that does not read gives the reason `overlay`.
    pub fn query_overlay(
        &self,
        catalog: u64,
        tid: u64,
        plan: &Plan,
        columns: &Columns,
        records: &[(u64, Vec<u8>)],
    ) -> Result<Reply, Fallback> {
        self.query_typed(catalog, tid, plan, columns, &[], Some(records))
    }

    /// Answers a plan that `decode_typed` read, as `query_overlay` does,
    /// after a check of the kind of each clause (`kinds`) against the class
    /// of its index. A clause whose kind does not read the class of its
    /// index gives the reason `plan`. Without `overlay`, the query reads
    /// the version at `tid` alone.
    pub fn query_typed(
        &self,
        catalog: u64,
        tid: u64,
        plan: &Plan,
        columns: &Columns,
        kinds: &[ClauseKind],
        overlay: Option<&[(u64, Vec<u8>)]>,
    ) -> Result<Reply, Fallback> {
        let started = Instant::now();
        if let Some(records) = overlay {
            self.shared.counters.overlay(records.len());
        }
        let overlay = overlay.filter(|records| !records.is_empty());
        let answer = self.answer(catalog, tid, plan, columns, kinds, overlay);
        match &answer {
            Ok(_) => self.shared.counters.native(started.elapsed()),
            Err(f) => self.shared.counters.python(f.reason, started.elapsed()),
        }
        answer
    }

    /// Answers the plan `plan` in the form of section 8, with its rows, as
    /// `query_rows` does. A plan of another form is an error: its text. A
    /// key or a kind of clause that this version does not know gives the
    /// reason `plan`, and so does a clause whose kind does not read the
    /// class of its index.
    pub fn query_wire(
        &self,
        catalog: u64,
        tid: u64,
        plan: &Wire,
    ) -> Result<Result<Reply, Fallback>, String> {
        self.query_wire_overlay(catalog, tid, plan, None)
    }

    /// Answers the plan `plan` as `query_wire` does, over the transaction
    /// overlay `overlay` when it is given.
    pub fn query_wire_overlay(
        &self,
        catalog: u64,
        tid: u64,
        plan: &Wire,
        overlay: Option<&[(u64, Vec<u8>)]>,
    ) -> Result<Result<Reply, Fallback>, String> {
        match decode_typed(plan) {
            Ok((plan, columns, kinds)) => {
                Ok(self.query_typed(catalog, tid, &plan, &columns, &kinds, overlay))
            }
            Err(PlanError::Malformed(why)) => Err(why),
            Err(PlanError::Unknown(why)) => {
                self.shared.counters.python(Reason::Plan, Duration::ZERO);
                Ok(Err(Fallback::new(Reason::Plan, why)))
            }
        }
    }

    fn answer(
        &self,
        catalog: u64,
        tid: u64,
        plan: &Plan,
        columns: &Columns,
        kinds: &[ClauseKind],
        overlay: Option<&[(u64, Vec<u8>)]>,
    ) -> Result<Reply, Fallback> {
        let shared = &self.shared;
        let v = tid.saturating_sub(1);
        let Some(store) = (shared.open)() else {
            return Err(Fallback::new(Reason::NotReady, "the store is closed"));
        };
        let needed = v.min(store.last_tid());
        drop(store);
        let deadline = Instant::now() + shared.config.wait;
        let mut held = Held { shared, bytes: 0 };
        let (snapshot, maint, at) = {
            let mut inner = shared.lock();
            loop {
                let answers = inner.answers + 1;
                let Some(entry) = inner.entries.get_mut(&catalog) else {
                    return Err(Fallback::new(
                        Reason::NotRegistered,
                        format!("the store has no registration key for {catalog:#x}"),
                    ));
                };
                match &entry.state {
                    State::Pending | State::Building => {
                        return Err(match entry.stale_at {
                            Some(at) if v >= at => Fallback::new(
                                Reason::Stale,
                                format!("the commit {at:#x} changed what the mirror reads"),
                            ),
                            _ => Fallback::new(Reason::NotReady, "the mirror is building"),
                        });
                    }
                    State::Refused(why) => return Err(Fallback::new(Reason::Refused, why.clone())),
                    State::Dropped => {
                        return Err(Fallback::new(
                            Reason::Dropped,
                            "the memory bound dropped the mirror",
                        ));
                    }
                    State::Ready(versions) if versions.applied >= needed => {
                        let Some(snapshot) = versions.snapshot(v) else {
                            return Err(Fallback::new(
                                Reason::OutsideWindow,
                                "a commit after the tid has no change set in the mirror",
                            ));
                        };
                        // The version holds each commit up to `at`.
                        let at = v.min(versions.applied);
                        let maint = entry.maint.clone();
                        entry.last_answer = answers;
                        inner.answers = answers;
                        break (snapshot, maint, at);
                    }
                    State::Ready(_) => {}
                }
                let now = Instant::now();
                if now >= deadline {
                    return Err(Fallback::new(
                        Reason::Lag,
                        "the mirror did not reach the tid in time",
                    ));
                }
                inner = shared
                    .changed
                    .wait_timeout(inner, deadline - now)
                    .unwrap_or_else(PoisonError::into_inner)
                    .0;
            }
        };
        let answer = catch_unwind(AssertUnwindSafe(|| {
            let (base, over) = snapshot.resolve();
            View::new(&base, &over).check_kinds(plan, kinds)?;
            let over = match (overlay, &maint) {
                (None, _) => over,
                (Some(records), Some(maint)) => {
                    // The bound counts the records and the copy of the
                    // overrides that the overlay makes. An overlay that does
                    // not fit drops no mirror.
                    let bytes = overlay::heap_bytes(records) + over.heap_bytes();
                    {
                        let mut inner = shared.lock();
                        if !shared.fits(&inner, bytes) {
                            return Err(Fallback::new(
                                Reason::Overlay,
                                "the overlay passes catalog-memory",
                            ));
                        }
                        inner.overlays += bytes;
                        held.bytes = bytes;
                    }
                    if let Some(hook) = &shared.config.hooks.before_overlay {
                        hook(catalog);
                    }
                    let m = read(maint);
                    // The thread forgot the shape changes at or before the
                    // floor that it pruned at.
                    if at < m.pruned {
                        return Err(Fallback::new(
                            Reason::Overlay,
                            "the mirror forgot the shape changes after the tid",
                        ));
                    }
                    let with =
                        overlay::read(&base, &over, &m.shapes, &m.objects, &m.moved, at, records);
                    drop(m);
                    Arc::new(with?)
                }
                (Some(_), None) => {
                    return Err(Fallback::new(
                        Reason::Overlay,
                        "the mirror keeps no shapes of its trees",
                    ));
                }
            };
            View::new(&base, &over).query_rows(plan, columns)
        }));
        drop(held);
        answer.unwrap_or_else(|_| Err(Fallback::new(Reason::Refused, "the query panicked")))
    }

    /// Counts a query that the host of the mirrors refused before the
    /// mirrors read it, with its reason.
    pub fn count_fallback(&self, reason: Reason) {
        self.shared.counters.python(reason, Duration::ZERO);
    }

    /// The state of the mirror of `catalog`, or `None` when the store has
    /// no registration key for it.
    pub fn status(&self, catalog: u64) -> Option<Status> {
        let inner = self.shared.lock();
        inner.entries.get(&catalog).map(status)
    }

    /// The catalogs that the store lists.
    pub fn catalogs(&self) -> Vec<u64> {
        self.shared.lock().entries.keys().copied().collect()
    }

    /// Waits at most `timeout` until `test` holds for the state of the
    /// mirror of `catalog`. `None` is the state of a catalog that the store
    /// does not list. Tells if it held.
    pub fn wait_for(
        &self,
        catalog: u64,
        timeout: Duration,
        test: impl Fn(Option<&Status>) -> bool,
    ) -> bool {
        let deadline = Instant::now() + timeout;
        let mut inner = self.shared.lock();
        loop {
            let found = inner.entries.get(&catalog).map(status);
            if test(found.as_ref()) {
                return true;
            }
            let now = Instant::now();
            if now >= deadline {
                return false;
            }
            inner = self
                .shared
                .changed
                .wait_timeout(inner, (deadline - now).min(Duration::from_millis(50)))
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
    }

    /// The counts of the mirrors.
    pub fn counts(&self) -> Counts {
        let mut counts = self.shared.counters.snapshot();
        let inner = self.shared.lock();
        counts.mirrors = inner
            .entries
            .iter()
            .map(|(catalog, e)| {
                let applied = match &e.state {
                    State::Ready(v) => Some(v.applied),
                    _ => None,
                };
                MirrorCounts {
                    catalog: *catalog,
                    ready: applied.is_some(),
                    build_seconds: e.build_seconds,
                    lag_tids: applied.map_or(0, |a| {
                        inner.handed.iter().filter(|t| **t > a).count() as u64
                    }),
                    bytes: e.bytes as u64,
                }
            })
            .collect();
        counts
    }

    /// Takes the messages that the mirrors logged since the last call: a
    /// refused catalog, a drop, a stale mirror. The caller logs them.
    pub fn take_events(&self) -> Vec<String> {
        std::mem::take(&mut self.shared.lock().events)
    }

    /// Tells the thread and the builds to stop, and does not wait for them.
    /// `stop` waits.
    pub fn signal_stop(&self) {
        self.shared.signal_stop();
    }

    /// Stops the thread and the builds, and waits for them.
    pub fn stop(&self) {
        self.shared.signal_stop();
        let thread = self
            .thread
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        if let Some(thread) = thread {
            let _ = thread.join();
        }
    }
}

impl Drop for Mirrors {
    fn drop(&mut self) {
        self.shared.signal_stop();
    }
}

fn status(e: &Entry) -> Status {
    let (state, reason) = match &e.state {
        State::Pending => ("pending", None),
        State::Building => ("building", None),
        State::Ready(_) => ("ready", None),
        State::Refused(why) => ("refused", Some(why.clone())),
        State::Dropped => ("dropped", None),
    };
    let ready = match &e.state {
        State::Ready(v) => Some(v),
        _ => None,
    };
    Status {
        state,
        reason,
        base_tid: ready.map(|v| v.base.tid),
        applied: ready.map(|v| v.applied),
        floor: ready.map(|v| v.floor),
        sets: ready.map_or(0, |v| v.sets.len()),
        stale_at: e.stale_at,
        rebases: e.rebases,
        bytes: e.bytes,
    }
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn signal_stop(&self) {
        let mut inner = self.lock();
        inner.stop = true;
        drop(inner);
        self.cancel.store(true, AtomicOrdering::Relaxed);
        self.changed.notify_all();
    }

    fn event(&self, inner: &mut Inner, text: String) {
        if inner.events.len() < 1000 {
            inner.events.push(text);
        }
    }

    /// Tells if the bound has room for an overlay of `bytes` beside the
    /// mirrors and the overlays of the running queries. It never drops a
    /// mirror.
    fn fits(&self, inner: &Inner, bytes: usize) -> bool {
        let used: usize = inner.entries.values().map(|e| e.bytes).sum();
        used + inner.overlays + bytes <= self.config.memory
    }

    /// Tells if the bound has room for `extra` more bytes, after it dropped
    /// the mirrors that answered least recently. `keep` is a mirror that
    /// the bound does not drop.
    fn room(&self, inner: &mut Inner, keep: Option<u64>, extra: usize) -> bool {
        loop {
            let used: usize = inner.entries.values().map(|e| e.bytes).sum::<usize>() + extra;
            if used <= self.config.memory {
                return true;
            }
            let victim = inner
                .entries
                .iter()
                .filter(|(c, e)| Some(**c) != keep && matches!(e.state, State::Ready(_)))
                .min_by_key(|(_, e)| e.last_answer)
                .map(|(c, _)| *c);
            let Some(victim) = victim else {
                return false;
            };
            self.drop_mirror(inner, victim);
        }
    }

    fn drop_mirror(&self, inner: &mut Inner, catalog: u64) {
        if let Some(e) = inner.entries.get_mut(&catalog) {
            e.last_bytes = e.bytes;
            e.bytes = 0;
            e.state = State::Dropped;
            e.maint = None;
            e.rebasing = false;
            self.counters.drop_mirror();
            let text = format!(
                "the catalog mirror of {catalog:#x} is dropped: the mirrors of the store pass catalog-memory"
            );
            self.event(inner, text);
        }
    }
}

/// The reads of a build: each read takes a share of the store and gives it
/// back, so a build never keeps the store open. After the close, each read
/// fails.
struct Weakly<'a>(&'a Opener);

impl Weakly<'_> {
    fn with<T>(&self, f: impl FnOnce(&dyn Reads) -> Result<T, String>) -> Result<T, String> {
        match (self.0)() {
            Some(store) => f(&*store),
            None => Err("the store is closed".into()),
        }
    }
}

impl Reads for Weakly<'_> {
    fn last_tid(&self) -> u64 {
        (self.0)().map_or(0, |store| store.last_tid())
    }

    fn load_before(&self, oid: u64, tid: u64) -> Result<Option<Vec<u8>>, String> {
        self.with(|store| store.load_before(oid, tid))
    }

    fn commits(
        &self,
        from: u64,
        to: u64,
        each: &mut dyn FnMut(Commit) -> bool,
    ) -> Result<(), String> {
        self.with(|store| store.commits(from, to, each))
    }

    fn registrations(&self) -> Result<Vec<Vec<u8>>, String> {
        self.with(|store| store.registrations())
    }

    fn registration_generation(&self) -> Result<u64, String> {
        self.with(|store| store.registration_generation())
    }
}

/// The thread of the mirrors.
fn run(shared: &Arc<Shared>) {
    let mut workers: Vec<JoinHandle<()>> = Vec::new();
    loop {
        {
            let inner = shared.lock();
            let (mut inner, _) = shared
                .changed
                .wait_timeout_while(inner, TICK, |i| !i.stop && !i.dirty)
                .unwrap_or_else(PoisonError::into_inner);
            if inner.stop {
                break;
            }
            inner.dirty = false;
        }
        let Some(store) = (shared.open)() else {
            break;
        };
        let pass = catch_unwind(AssertUnwindSafe(|| {
            pass(shared, &*store, &mut workers);
        }));
        drop(store);
        if pass.is_err() {
            let mut inner = shared.lock();
            for e in inner.entries.values_mut() {
                e.state = State::Refused("the thread of the mirror panicked".into());
                e.maint = None;
                e.bytes = 0;
            }
            let text =
                "the thread of the catalog mirrors panicked: each query takes the Python path"
                    .to_owned();
            shared.event(&mut inner, text);
            drop(inner);
            shared.changed.notify_all();
        }
        workers.retain(|w| !w.is_finished());
    }
    shared.cancel.store(true, AtomicOrdering::Relaxed);
    for worker in workers {
        let _ = worker.join();
    }
}

/// One pass of the thread: the registration, the builds, the commits, the
/// rebases and the window.
fn pass(shared: &Arc<Shared>, store: &dyn Reads, workers: &mut Vec<JoinHandle<()>>) {
    let config = &shared.config;
    read_registration(shared, store);
    install_build(shared);
    start_build(shared, workers);
    apply_commits(shared, store);
    install_rebases(shared);
    // The change sets leave the window as time passes.
    let mut inner = shared.lock();
    let mut due = Vec::new();
    let mut floors = Vec::new();
    for (catalog, e) in inner.entries.iter_mut() {
        if !matches!(e.state, State::Ready(_)) {
            e.maint = None;
        }
        if let State::Ready(versions) = &mut e.state {
            versions.prune(config.window_tids, config.window);
            if let Some(m) = &e.maint {
                floors.push((Arc::clone(m), versions.floor));
            }
            let maint = e.maint.as_ref().map_or(0, |m| read(m).heap_bytes());
            e.bytes = versions.heap_bytes() + maint;
            let retry = e.rebase_failed.is_none_or(|t| t.elapsed() > REBASE_RETRY);
            if !e.rebasing
                && retry
                && versions.rebase_due(config.rebase_rows, config.window_tids, config.window)
            {
                e.rebasing = true;
                due.push((
                    *catalog,
                    Arc::clone(&versions.base),
                    Arc::clone(&versions.head),
                    versions.applied,
                ));
            }
        }
    }
    drop(inner);
    // A node whose shape changed at or before the floor of the versions
    // leaves `moved`: no query reads a version before the floor.
    for (maint, floor) in floors {
        if read(&maint).pruned < floor {
            let mut m = write(&maint);
            m.moved.retain(|_, tid| *tid > floor);
            m.pruned = floor;
        }
    }
    for (catalog, base, head, tid) in due {
        let worker = Arc::clone(shared);
        let spawned = thread::Builder::new()
            .name("zoe-catalog-rebase".to_owned())
            .spawn(move || {
                let shared = worker;
                let room = |bytes: usize| {
                    let mut inner = shared.lock();
                    !shared.cancel.load(AtomicOrdering::Relaxed)
                        && shared.room(&mut inner, Some(catalog), bytes)
                };
                let result = catch_unwind(AssertUnwindSafe(|| rebase(&base, &head, tid, &room)))
                    .unwrap_or_else(|_| Err("the rebase panicked".into()));
                drop((base, head));
                if let Some(hook) = &shared.config.hooks.before_swap {
                    hook(catalog);
                }
                let mut inner = shared.lock();
                inner.rebased.push((catalog, result));
                inner.dirty = true;
                drop(inner);
                shared.changed.notify_all();
            });
        match spawned {
            Ok(worker) => workers.push(worker),
            Err(_) => {
                if let Some(e) = shared.lock().entries.get_mut(&catalog) {
                    e.rebasing = false;
                    e.rebase_failed = Some(Instant::now());
                }
            }
        }
    }
}

/// Reads the namespace when a commit wrote it since the last read.
fn read_registration(shared: &Arc<Shared>, store: &dyn Reads) {
    let generation = match store.registration_generation() {
        Ok(g) => g,
        Err(e) => {
            let mut inner = shared.lock();
            shared.event(&mut inner, e);
            return;
        }
    };
    if shared.lock().generation == Some(generation) {
        return;
    }
    let keys = match store.registrations() {
        Ok(keys) => keys,
        Err(e) => {
            let mut inner = shared.lock();
            shared.event(&mut inner, e);
            return;
        }
    };
    let mut inner = shared.lock();
    inner.generation = Some(generation);
    let mut listed = Vec::new();
    for key in keys {
        match <[u8; 8]>::try_from(key.as_slice()) {
            Ok(bytes) => listed.push(u64::from_be_bytes(bytes)),
            Err(_) => {
                let text =
                    format!("a registration key of zoe.catalog that is not an oid: {key:02x?}");
                shared.event(&mut inner, text);
            }
        }
    }
    let gone: Vec<u64> = inner
        .entries
        .keys()
        .filter(|c| !listed.contains(c))
        .copied()
        .collect();
    let mut freed = false;
    for catalog in gone {
        if let Some(e) = inner.entries.remove(&catalog) {
            freed |= e.bytes > 0;
        }
    }
    for catalog in listed {
        inner.entries.entry(catalog).or_insert_with(Entry::new);
    }
    if freed {
        // An uninstall freed room: a dropped mirror that fits builds again.
        let used: usize = inner.entries.values().map(|e| e.bytes).sum();
        let mut free = shared.config.memory.saturating_sub(used);
        for e in inner.entries.values_mut() {
            if matches!(e.state, State::Dropped) && e.last_bytes <= free {
                free -= e.last_bytes;
                e.state = State::Pending;
            }
        }
    }
    drop(inner);
    shared.changed.notify_all();
}

/// Lays the base of a build that ended.
fn install_build(shared: &Arc<Shared>) {
    let mut inner = shared.lock();
    let Some((catalog, result)) = inner.built.take() else {
        return;
    };
    let Some(e) = inner.entries.get_mut(&catalog) else {
        return;
    };
    if !matches!(e.state, State::Building) {
        return;
    }
    match result {
        Ok(built) => {
            let versions = Versions::new(built.base);
            let bytes = versions.heap_bytes() + built.shapes.heap_bytes();
            e.state = State::Ready(versions);
            e.stale_at = None;
            e.build_seconds = built.seconds;
            e.bytes = bytes;
            e.maint = Some(Arc::new(RwLock::new(Maint {
                shapes: built.shapes,
                objects: built.objects,
                moved: HashMap::new(),
                pruned: 0,
            })));
        }
        Err(BuildError::Refused(why)) => {
            e.state = State::Refused(why.clone());
            let text = format!("the catalog {catalog:#x} gets no mirror: {why}");
            shared.event(&mut inner, text);
        }
        Err(BuildError::Dropped) => {
            e.state = State::Dropped;
            shared.counters.drop_mirror();
            let text = format!(
                "the catalog mirror of {catalog:#x} is dropped: its build passes catalog-memory"
            );
            shared.event(&mut inner, text);
        }
    }
    drop(inner);
    shared.changed.notify_all();
}

/// Starts the build of a pending mirror, when no build of the store runs.
fn start_build(shared: &Arc<Shared>, workers: &mut Vec<JoinHandle<()>>) {
    let mut inner = shared.lock();
    if inner.building || inner.built.is_some() {
        return;
    }
    let Some((&catalog, e)) = inner
        .entries
        .iter_mut()
        .find(|(_, e)| matches!(e.state, State::Pending))
    else {
        return;
    };
    e.state = State::Building;
    e.bytes = 0;
    let tid = inner.target;
    inner.building = true;
    drop(inner);
    let worker = Arc::clone(shared);
    let spawned = thread::Builder::new()
        .name("zoe-catalog-build".to_owned())
        .spawn(move || {
            let shared = worker;
            let result = {
                let _one = BUILDS.lock().unwrap_or_else(PoisonError::into_inner);
                // A build of another store can hold the lock for long: a
                // stop during that wait starts no walk.
                let cancelled = || shared.cancel.load(AtomicOrdering::Relaxed);
                if !cancelled()
                    && let Some(hook) = &shared.config.hooks.before_build
                {
                    hook(catalog);
                }
                let room = |bytes: usize| {
                    let mut inner = shared.lock();
                    !shared.cancel.load(AtomicOrdering::Relaxed)
                        && shared.room(&mut inner, Some(catalog), bytes)
                };
                let reads = Weakly(&shared.open);
                let built = if cancelled() {
                    Err(BuildError::Dropped)
                } else {
                    catch_unwind(AssertUnwindSafe(|| {
                        build(&reads, catalog, tid, shared.config.threads, &room)
                    }))
                    .unwrap_or_else(|_| Err(BuildError::Refused("the build panicked".into())))
                };
                match built {
                    // A build that the close of the store stopped.
                    Err(BuildError::Refused(_)) if (shared.open)().is_none() => {
                        Err(BuildError::Dropped)
                    }
                    other => other,
                }
            };
            let mut inner = shared.lock();
            inner.building = false;
            inner.built = Some((catalog, result));
            inner.dirty = true;
            drop(inner);
            shared.changed.notify_all();
        });
    match spawned {
        Ok(worker) => workers.push(worker),
        Err(_) => {
            let mut inner = shared.lock();
            inner.building = false;
            if let Some(e) = inner.entries.get_mut(&catalog) {
                e.state = State::Pending;
            }
        }
    }
}

/// Applies the commits up to the last hand-off to each ready mirror.
fn apply_commits(shared: &Arc<Shared>, store: &dyn Reads) {
    let (target, ready) = {
        let inner = shared.lock();
        let ready: Vec<(u64, u64)> = inner
            .entries
            .iter()
            .filter_map(|(c, e)| match &e.state {
                State::Ready(v) if v.applied < inner.target => Some((*c, v.applied)),
                _ => None,
            })
            .collect();
        (inner.target, ready)
    };
    let Some(from) = ready.iter().map(|(_, applied)| applied + 1).min() else {
        return;
    };
    let catalogs: Vec<u64> = ready.iter().map(|(c, _)| *c).collect();
    let mut each = |commit: Commit| -> bool {
        if shared.cancel.load(AtomicOrdering::Relaxed) {
            return false;
        }
        if let Some(hook) = &shared.config.hooks.before_apply {
            hook(commit.tid);
        }
        for catalog in &catalogs {
            apply(shared, *catalog, &commit);
        }
        true
    };
    if let Err(e) = store.commits(from, target, &mut each) {
        let mut inner = shared.lock();
        for catalog in &catalogs {
            make_stale(shared, &mut inner, *catalog, from, &e);
        }
        drop(inner);
        shared.changed.notify_all();
        return;
    }
    if shared.cancel.load(AtomicOrdering::Relaxed) {
        return;
    }
    let mut inner = shared.lock();
    for catalog in &catalogs {
        if let Some(e) = inner.entries.get_mut(catalog)
            && let State::Ready(v) = &mut e.state
            && v.applied < target
        {
            v.applied = target;
        }
    }
    // A growth that passes the bound drops the mirror that answered least
    // recently, this one too.
    for catalog in &catalogs {
        let bytes = match inner.entries.get(catalog) {
            Some(
                e @ Entry {
                    state: State::Ready(v),
                    ..
                },
            ) => v.heap_bytes() + maint_bytes(e),
            _ => continue,
        };
        if let Some(e) = inner.entries.get_mut(catalog) {
            e.bytes = bytes;
        }
    }
    shared.room(&mut inner, None, 0);
    let low = inner
        .entries
        .values()
        .filter_map(|e| match &e.state {
            State::Ready(v) => Some(v.applied),
            _ => None,
        })
        .min()
        .unwrap_or(target);
    while inner.handed.front().is_some_and(|t| *t <= low) {
        inner.handed.pop_front();
    }
    drop(inner);
    shared.changed.notify_all();
}

/// Applies one commit to the mirror of `catalog`. The shapes take the
/// edits of the commit before the versions take its change set, so a query
/// never reads a version with older shapes than its own.
fn apply(shared: &Arc<Shared>, catalog: u64, commit: &Commit) {
    let (base, head, maint) = {
        let inner = shared.lock();
        match inner.entries.get(&catalog) {
            Some(Entry {
                state: State::Ready(v),
                maint: Some(maint),
                ..
            }) if v.applied < commit.tid => {
                (Arc::clone(&v.base), Arc::clone(&v.head), Arc::clone(maint))
            }
            _ => return,
        }
    };
    let got = {
        let m = read(&maint);
        read_records(&base, &head, &m.shapes, &m.objects, &commit.records)
    };
    drop((base, head));
    let crate::change::Read {
        changes,
        lengths,
        edits,
        stale,
        ..
    } = got;
    if stale.is_none() {
        let mut m = write(&maint);
        for oid in m.shapes.take(edits) {
            m.moved.insert(oid, commit.tid);
        }
    }
    let mut inner = shared.lock();
    if let Some(why) = stale {
        make_stale(shared, &mut inner, catalog, commit.tid, &why);
    } else if let Some(e) = inner.entries.get_mut(&catalog)
        && let State::Ready(v) = &mut e.state
    {
        v.push(ChangeSet {
            tid: commit.tid,
            at: Instant::now(),
            changes,
            lengths,
        });
    }
    drop(inner);
    shared.changed.notify_all();
}

/// Makes the mirror of `catalog` stale at `tid`: it drops its base and
/// builds again.
fn make_stale(shared: &Shared, inner: &mut Inner, catalog: u64, tid: u64, why: &str) {
    let Some(e) = inner.entries.get_mut(&catalog) else {
        return;
    };
    if !matches!(e.state, State::Ready(_)) {
        return;
    }
    e.state = State::Pending;
    e.maint = None;
    e.stale_at = Some(tid);
    e.bytes = 0;
    e.rebasing = false;
    inner.dirty = true;
    let text =
        format!("the catalog mirror of {catalog:#x} is stale at {tid:#x} and builds again: {why}");
    shared.event(inner, text);
}

/// Lays the bases of the rebases that ended.
fn install_rebases(shared: &Arc<Shared>) {
    let mut inner = shared.lock();
    let rebased = std::mem::take(&mut inner.rebased);
    for (catalog, result) in rebased {
        let Some(e) = inner.entries.get_mut(&catalog) else {
            continue;
        };
        if !e.rebasing {
            continue;
        }
        e.rebasing = false;
        let State::Ready(v) = &mut e.state else {
            continue;
        };
        match result {
            Ok(base) => {
                v.rebased(base);
                v.prune(shared.config.window_tids, shared.config.window);
                e.rebases += 1;
                e.rebase_failed = None;
            }
            Err(why) => {
                e.rebase_failed = Some(Instant::now());
                let text =
                    format!("the rebase of the catalog mirror of {catalog:#x} failed: {why}");
                shared.event(&mut inner, text);
            }
        }
    }
    drop(inner);
    shared.changed.notify_all();
}
