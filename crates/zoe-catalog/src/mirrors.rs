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
//! A commit that changes what the mirror reads makes the mirror stale and
//! starts a shadow rebuild (`shadow`) in a thread of its own, one build at
//! a time in the process. The base keeps serving meanwhile: a query takes
//! `stale` only at the tid of that commit or later, and only when it reads
//! a changed structure. The thread lays the new base in at one step. A
//! commit that changes the rows, and a shadow rebuild that fails, drop the
//! base: the mirror builds again from nothing.
//!
//! A query with an order clause reads the order of its container as of its
//! tid, or from its transaction overlay, at each query (`order`).
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
use crate::change::{ChangeSet, Shapes, Unfollowed, read_records};
use crate::counts::{Counters, Counts, MirrorCounts};
use crate::mirror::{Versions, rebase};
use crate::order::{self as order_clause, Order};
use crate::overlay;
use crate::plan::{ClauseKind, PlanError, Wire, decode_ordered};
use crate::query::{Columns, Plan, Reply, View};
use crate::read::{Commit, Reads};
use crate::reason::{Fallback, Reason};
use crate::shadow::{self, Changed, Job, Shadow, ShadowError};

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
    /// Where the text indexes of the store live (ADR 0060). A store with
    /// no directory has no text index.
    #[cfg(feature = "text")]
    pub text: crate::text::TextConfig,
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
            #[cfg(feature = "text")]
            text: crate::text::TextConfig::default(),
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
    /// Before a rebase or a shadow rebuild lays its new base, with the oid
    /// of the catalog. A build and a shadow rebuild call `before_build`
    /// before they read.
    pub before_swap: Option<Hook>,
    /// Before a query reads its overlay, after it took its version, with
    /// the oid of the catalog.
    pub before_overlay: Option<Hook>,
    /// Before a rebuild of a text index reads the deltas, with the oid of
    /// the catalog.
    #[cfg(feature = "text")]
    pub before_text_build: Option<Hook>,
    /// Before the open of a text index takes its cap of pack, after it
    /// checked the index, with the oid of the catalog.
    #[cfg(feature = "text")]
    pub before_text_cap: Option<Hook>,
    /// Before a text index checks its cap of pack, at a commit of its
    /// writer or at an advance of its tid, with the oid of the catalog.
    #[cfg(feature = "text")]
    pub before_text_check: Option<Hook>,
}

/// The state of one mirror, as `status` gives it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Status {
    /// `pending`, `building`, `ready`, `refused` or `dropped`. A mirror
    /// that waits for its shadow rebuild is `building`, and answers.
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
    /// again, from nothing or in a shadow rebuild.
    pub stale_at: Option<u64>,
    /// The rebases that the mirror laid.
    pub rebases: u64,
    /// The shadow rebuilds that the mirror laid.
    pub swaps: u64,
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

/// The shadow rebuild of a mirror.
enum Shadowing {
    /// None is due.
    Idle,
    /// A shadow rebuild is due: it starts when no rebase runs.
    Due,
    /// The shadow rebuild `id` runs. `again` tells that a later stale
    /// commit cancelled it: its result goes, and a new one is due.
    Running {
        id: u64,
        cancel: Arc<AtomicBool>,
        again: bool,
    },
}

struct Entry {
    state: State,
    /// The shapes and the objects of a ready mirror.
    maint: Option<Arc<RwLock<Maint>>>,
    /// The stale commit of a mirror that builds again from nothing.
    stale_at: Option<u64>,
    /// The stale commits since the base of a ready mirror, each with the
    /// structures that it changed.
    marks: Vec<(u64, Changed)>,
    /// The tids before which the base does not hold the structures that a
    /// shadow rebuild laid.
    fresh: Vec<(u64, Changed)>,
    shadow: Shadowing,
    /// The bytes that a running shadow rebuild holds.
    shadow_bytes: usize,
    swaps: u64,
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
    /// The structures that the stale commits changed.
    fn changed(&self) -> Changed {
        let mut out = Changed::default();
        for (_, c) in &self.marks {
            out.add(c);
        }
        out
    }

    /// Forgets the shadow rebuild, and cancels it when it runs.
    fn forget_shadow(&mut self) {
        if let Shadowing::Running { cancel, .. } = &self.shadow {
            cancel.store(true, AtomicOrdering::Relaxed);
        }
        self.shadow = Shadowing::Idle;
        self.shadow_bytes = 0;
        self.marks.clear();
        self.fresh.clear();
    }

    /// The bytes that the bound counts for the mirror.
    fn held(&self) -> usize {
        self.bytes + self.shadow_bytes
    }

    fn new() -> Entry {
        Entry {
            state: State::Pending,
            maint: None,
            stale_at: None,
            marks: Vec::new(),
            fresh: Vec::new(),
            shadow: Shadowing::Idle,
            shadow_bytes: 0,
            swaps: 0,
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
    /// The shadow rebuilds that ended: catalog, id, result.
    shadowed: Vec<(u64, u64, Result<Shadow, ShadowError>)>,
    /// The id of the last shadow rebuild.
    jobs: u64,
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

/// The text clause of a plan (ADR 0060, section 12), with the feature
/// `text`.
#[cfg(feature = "text")]
type TextPart = crate::text::TextClause;
/// No text clause without the feature `text`.
#[cfg(not(feature = "text"))]
type TextPart = ();

/// The mirrors of one store, and the text indexes of its catalogs.
pub struct Mirrors {
    shared: Arc<Shared>,
    thread: Mutex<Option<JoinHandle<()>>>,
    /// The text indexes, when the store has a directory (ADR 0060).
    #[cfg(feature = "text")]
    texts: Option<crate::text::Texts>,
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
        #[cfg(feature = "text")]
        let texts = config.text.dir.is_some().then(|| {
            crate::text::Texts::start(
                Arc::clone(&open),
                published,
                config.text.clone(),
                config.wait,
                crate::text::TextHooks {
                    before_build: config.hooks.before_text_build.clone(),
                    before_cap: config.hooks.before_text_cap.clone(),
                    before_check: config.hooks.before_text_check.clone(),
                },
            )
        });
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
                shadowed: Vec::new(),
                jobs: 0,
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
            #[cfg(feature = "text")]
            texts,
        }
    }

    /// The text indexes of the store, when it has a directory.
    #[cfg(feature = "text")]
    pub fn texts(&self) -> Option<&crate::text::Texts> {
        self.texts.as_ref()
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
        #[cfg(feature = "text")]
        if let Some(texts) = &self.texts {
            texts.committed(tid);
        }
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
        self.query_full(catalog, tid, plan, columns, kinds, overlay, None, None)
    }

    /// Answers a plan as `query_typed` does, with the order clause `order`
    /// that `decode_ordered` read.
    #[allow(clippy::too_many_arguments)]
    pub fn query_ordered(
        &self,
        catalog: u64,
        tid: u64,
        plan: &Plan,
        columns: &Columns,
        kinds: &[ClauseKind],
        overlay: Option<&[(u64, Vec<u8>)]>,
        order: Option<&Order>,
    ) -> Result<Reply, Fallback> {
        self.query_full(catalog, tid, plan, columns, kinds, overlay, None, order)
    }

    /// Answers a plan as `query_ordered` does, with the text clause `text`
    /// of ADR 0060, section 12, which `split_text` took out of the plan.
    #[cfg(feature = "text")]
    #[allow(clippy::too_many_arguments)]
    pub fn query_with_text(
        &self,
        catalog: u64,
        tid: u64,
        plan: &Plan,
        columns: &Columns,
        kinds: &[ClauseKind],
        overlay: Option<&[(u64, Vec<u8>)]>,
        text: Option<&crate::text::TextClause>,
        order: Option<&Order>,
    ) -> Result<Reply, Fallback> {
        self.query_full(catalog, tid, plan, columns, kinds, overlay, text, order)
    }

    /// Answers a plan as `query_typed` does, with its text clause `text`
    /// and its order clause `order`.
    #[allow(clippy::too_many_arguments)]
    fn query_full(
        &self,
        catalog: u64,
        tid: u64,
        plan: &Plan,
        columns: &Columns,
        kinds: &[ClauseKind],
        overlay: Option<&[(u64, Vec<u8>)]>,
        text: Option<&TextPart>,
        order: Option<&Order>,
    ) -> Result<Reply, Fallback> {
        let started = Instant::now();
        if let Some(records) = overlay {
            self.shared.counters.overlay(records.len());
        }
        let overlay = overlay.filter(|records| !records.is_empty());
        let answer = self.answer(catalog, tid, plan, columns, kinds, overlay, text, order);
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
        #[cfg(feature = "text")]
        let (rest, text) = match crate::plan::split_text(plan) {
            Ok(split) => split,
            Err(PlanError::Malformed(why) | PlanError::Unknown(why)) => return Err(why),
        };
        #[cfg(feature = "text")]
        let plan = &rest;
        #[cfg(not(feature = "text"))]
        let text: Option<TextPart> = None;
        match decode_ordered(plan) {
            Ok((plan, columns, kinds, order)) => Ok(self.query_full(
                catalog,
                tid,
                &plan,
                &columns,
                &kinds,
                overlay,
                text.as_ref(),
                order.as_ref(),
            )),
            Err(PlanError::Malformed(why)) => Err(why),
            Err(PlanError::Unknown(why)) => {
                self.shared.counters.python(Reason::Plan, Duration::ZERO);
                Ok(Err(Fallback::new(Reason::Plan, why)))
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn answer(
        &self,
        catalog: u64,
        tid: u64,
        plan: &Plan,
        columns: &Columns,
        kinds: &[ClauseKind],
        overlay: Option<&[(u64, Vec<u8>)]>,
        text: Option<&TextPart>,
        order: Option<&Order>,
    ) -> Result<Reply, Fallback> {
        #[cfg(not(feature = "text"))]
        let _ = text;
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
                        // A query never reads a structure of the base that a
                        // commit up to its tid changed.
                        if let Some((at, _)) = entry
                            .marks
                            .iter()
                            .find(|(at, c)| v >= *at && c.needed_by(plan, columns))
                        {
                            return Err(Fallback::new(
                                Reason::Stale,
                                format!("the commit {at:#x} changed what the query reads"),
                            ));
                        }
                        if entry
                            .fresh
                            .iter()
                            .any(|(at, c)| v < *at && c.needed_by(plan, columns))
                        {
                            return Err(Fallback::new(
                                Reason::OutsideWindow,
                                "the mirror rebuilt a structure that the query reads after the tid",
                            ));
                        }
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
            // The order of an order clause, read at each query.
            let list = match order {
                Some(order) => {
                    let Some(reads) = (shared.open)() else {
                        return Err(Fallback::new(Reason::NotReady, "the store is closed"));
                    };
                    Some(order_clause::read(&*reads, order, tid, overlay)?)
                }
                None => None,
            };
            let mut view = View::new(&base, &over);
            if let Some(list) = &list {
                view = view.ordered(list);
            }
            #[cfg(feature = "text")]
            if let Some(clause) = text {
                return self.answer_text(&view, catalog, tid, plan, clause, columns);
            }
            view.query_rows(plan, columns)
        }));
        drop(held);
        answer.unwrap_or_else(|_| Err(Fallback::new(Reason::Refused, "the query panicked")))
    }

    /// Answers `plan` with its text clause `clause` on `view`.
    #[cfg(feature = "text")]
    fn answer_text(
        &self,
        view: &View<'_>,
        catalog: u64,
        tid: u64,
        plan: &Plan,
        clause: &crate::text::TextClause,
        columns: &Columns,
    ) -> Result<Reply, Fallback> {
        let Some(reads) = (self.shared.open)() else {
            return Err(Fallback::new(Reason::NotReady, "the store is closed"));
        };
        let Some(store) = reads.store() else {
            return Err(Fallback::new(Reason::NotReady, "the store gives no deltas"));
        };
        view.query_text(plan, columns, &mut |filter, want| {
            let query = crate::text::Query {
                catalog,
                tid,
                clause,
                filter: None,
                want,
                forced: None,
            };
            crate::text::engine::run_in(self.texts.as_ref(), store, &query, Some(filter))
                .map(|a| a.found)
        })
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
                    bytes: e.held() as u64,
                }
            })
            .collect();
        drop(inner);
        #[cfg(feature = "text")]
        if let Some(texts) = &self.texts {
            counts.texts = texts.counts();
        }
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
        #[cfg(feature = "text")]
        if let Some(texts) = &self.texts {
            texts.signal_stop();
        }
    }

    /// Stops the thread and the builds, and waits for them.
    pub fn stop(&self) {
        self.shared.signal_stop();
        #[cfg(feature = "text")]
        if let Some(texts) = &self.texts {
            texts.stop();
        }
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
        State::Ready(_) if matches!(e.shadow, Shadowing::Due | Shadowing::Running { .. }) => {
            ("building", None)
        }
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
        stale_at: e.stale_at.or(e.marks.first().map(|(at, _)| *at)),
        rebases: e.rebases,
        swaps: e.swaps,
        bytes: e.held(),
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
        let used: usize = inner.entries.values().map(Entry::held).sum();
        used + inner.overlays + bytes <= self.config.memory
    }

    /// Tells if the bound has room for `extra` more bytes, after it dropped
    /// the mirrors that answered least recently. `keep` is a mirror that
    /// the bound does not drop.
    fn room(&self, inner: &mut Inner, keep: Option<u64>, extra: usize) -> bool {
        loop {
            let used: usize = inner.entries.values().map(Entry::held).sum::<usize>() + extra;
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
            e.forget_shadow();
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
                e.forget_shadow();
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
    install_shadows(shared, store);
    start_shadows(shared, workers);
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
            let floor = versions.floor;
            e.fresh.retain(|(at, _)| *at > floor);
            if let Some(m) = &e.maint {
                floors.push((Arc::clone(m), versions.floor));
            }
            let maint = e.maint.as_ref().map_or(0, |m| read(m).heap_bytes());
            e.bytes = versions.heap_bytes() + maint;
            let retry = e.rebase_failed.is_none_or(|t| t.elapsed() > REBASE_RETRY);
            // A shadow rebuild reads the change sets after its tid: no
            // rebase drops them before it lays its base.
            let shadowing = matches!(e.shadow, Shadowing::Due | Shadowing::Running { .. });
            if !e.rebasing
                && !shadowing
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
        if let Some(mut e) = inner.entries.remove(&catalog) {
            freed |= e.held() > 0;
            // A shadow rebuild of a removed catalog drops no other mirror.
            e.forget_shadow();
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
            e.forget_shadow();
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
    let (base, head, maint, rebuilding) = {
        let inner = shared.lock();
        match inner.entries.get(&catalog) {
            Some(
                e @ Entry {
                    state: State::Ready(v),
                    maint: Some(maint),
                    ..
                },
            ) if v.applied < commit.tid => (
                Arc::clone(&v.base),
                Arc::clone(&v.head),
                Arc::clone(maint),
                e.changed(),
            ),
            _ => return,
        }
    };
    let got = {
        let m = read(&maint);
        read_records(&base, &head, &m.shapes, &m.objects, &commit.records)
    };
    drop(head);
    let crate::change::Read {
        changes,
        lengths,
        edits,
        stale,
        mut unfollowed,
        ..
    } = got;
    // A new record of a refused index that a shadow rebuild already builds
    // starts no other one.
    unfollowed.retain(|what| match what {
        Unfollowed::Refused(i) => !rebuilding.has_index(&base.indexes, *i),
        _ => true,
    });
    let changed = {
        let mut m = write(&maint);
        let mut changed = shadow::changed_by(&base, &m.objects, &unfollowed);
        if stale.is_some() && unfollowed.is_empty() {
            changed.rows = true;
        }
        // The next commit compares its pickles with the last ones.
        for what in unfollowed {
            match what {
                Unfollowed::Catalog(Some(bytes)) => {
                    m.objects.insert(catalog, bytes);
                }
                Unfollowed::Index(i, Some(bytes)) => {
                    if let Some(e) = base.indexes.get(i) {
                        m.objects.insert(e.oid, bytes);
                    }
                }
                _ => {}
            }
        }
        for oid in m.shapes.take(edits) {
            m.moved.insert(oid, commit.tid);
        }
        changed
    };
    let mut inner = shared.lock();
    if changed.rows {
        // New rows need a second whole base beside the old one: the mirror
        // drops its base and builds again from nothing.
        let first = rebuilding_since(&inner, catalog).unwrap_or(commit.tid);
        let why = stale.unwrap_or_else(|| "the commit changes the rows".into());
        make_stale(shared, &mut inner, catalog, first.min(commit.tid), &why);
    } else if let Some(e) = inner.entries.get_mut(&catalog)
        && matches!(e.state, State::Ready(_))
    {
        let marked = !changed.is_empty();
        if marked {
            e.marks.push((commit.tid, changed));
            match &mut e.shadow {
                Shadowing::Running { cancel, again, .. } => {
                    cancel.store(true, AtomicOrdering::Relaxed);
                    *again = true;
                }
                other => *other = Shadowing::Due,
            }
        }
        let all = e.changed();
        if let State::Ready(v) = &mut e.state {
            let set = ChangeSet {
                tid: commit.tid,
                at: Instant::now(),
                changes,
                lengths,
            };
            v.push(shadow::strip(set, &v.base.indexes, &all));
        }
        if marked {
            let why = stale.unwrap_or_else(|| "a new record of an unavailable index".into());
            let text = format!(
                "the catalog mirror of {catalog:#x} is stale at {:#x} and starts a shadow rebuild: {why}",
                commit.tid
            );
            shared.event(&mut inner, text);
        }
    }
    drop(inner);
    drop(base);
    shared.changed.notify_all();
}

/// The tid of the first stale commit of the mirror of `catalog`.
fn rebuilding_since(inner: &Inner, catalog: u64) -> Option<u64> {
    inner
        .entries
        .get(&catalog)
        .and_then(|e| e.marks.first().map(|(at, _)| *at))
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
    e.forget_shadow();
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

/// Starts the shadow rebuild of each mirror that has one due, when no
/// rebase of it runs. It reads as of the last stale commit.
fn start_shadows(shared: &Arc<Shared>, workers: &mut Vec<JoinHandle<()>>) {
    let mut jobs = Vec::new();
    {
        let mut inner = shared.lock();
        let mut failed = Vec::new();
        let mut next = inner.jobs;
        for (catalog, e) in inner.entries.iter_mut() {
            if !matches!(e.shadow, Shadowing::Due) || e.rebasing {
                continue;
            }
            let State::Ready(versions) = &e.state else {
                continue;
            };
            let Some(&(tid, _)) = e.marks.last() else {
                e.shadow = Shadowing::Idle;
                continue;
            };
            let Some(snapshot) = versions.snapshot(tid) else {
                failed.push(*catalog);
                continue;
            };
            next += 1;
            let cancel = Arc::new(AtomicBool::new(false));
            e.shadow = Shadowing::Running {
                id: next,
                cancel: Arc::clone(&cancel),
                again: false,
            };
            let job = Job {
                catalog: *catalog,
                tid,
                changed: e.changed(),
                snapshot,
            };
            jobs.push((next, cancel, job));
        }
        inner.jobs = next;
        for catalog in failed {
            fall_back(
                shared,
                &mut inner,
                catalog,
                "the mirror holds no version at the stale commit",
            );
        }
    }
    for (id, cancel, job) in jobs {
        let catalog = job.catalog;
        let worker = Arc::clone(shared);
        let stop = Arc::clone(&cancel);
        let spawned = thread::Builder::new()
            .name("zoe-catalog-shadow".to_owned())
            .spawn(move || {
                let shared = worker;
                let result = {
                    let _one = BUILDS.lock().unwrap_or_else(PoisonError::into_inner);
                    catch_unwind(AssertUnwindSafe(|| run_shadow(&shared, &stop, job)))
                        .unwrap_or_else(|_| {
                            Err(ShadowError::Failed("the shadow rebuild panicked".into()))
                        })
                };
                let mut inner = shared.lock();
                inner.shadowed.push((catalog, id, result));
                inner.dirty = true;
                drop(inner);
                shared.changed.notify_all();
            });
        match spawned {
            Ok(worker) => workers.push(worker),
            Err(_) => {
                if let Some(e) = shared.lock().entries.get_mut(&catalog) {
                    e.shadow = Shadowing::Due;
                }
            }
        }
    }
}

/// Runs the shadow rebuild `job` in its thread: the build, then the
/// commits that the mirror applied meanwhile.
fn run_shadow(shared: &Arc<Shared>, cancel: &AtomicBool, job: Job) -> Result<Shadow, ShadowError> {
    let catalog = job.catalog;
    let cancelled =
        || cancel.load(AtomicOrdering::Relaxed) || shared.cancel.load(AtomicOrdering::Relaxed);
    let stopped = || ShadowError::Failed("the shadow rebuild stopped".into());
    if cancelled() {
        return Err(stopped());
    }
    if let Some(hook) = &shared.config.hooks.before_build {
        hook(catalog);
    }
    // The bound counts the bytes that the shadow holds as the mirror's own.
    let room = |bytes: usize| {
        let mut inner = shared.lock();
        if cancelled() {
            return false;
        }
        if let Some(e) = inner.entries.get_mut(&catalog) {
            e.shadow_bytes = 0;
        }
        let fits = shared.room(&mut inner, Some(catalog), bytes);
        if fits && let Some(e) = inner.entries.get_mut(&catalog) {
            e.shadow_bytes = bytes;
        }
        fits
    };
    let reads = Weakly(&shared.open);
    let mut shadow = shadow::build(&reads, job, shared.config.threads, &room)?;
    // The commits that the mirror applied while the shadow built, in tid
    // order. The thread of the mirrors applies the last ones.
    for _ in 0..3 {
        let applied = match shared.lock().entries.get(&catalog) {
            Some(Entry {
                state: State::Ready(v),
                ..
            }) => v.applied,
            _ => return Err(stopped()),
        };
        if applied <= shadow.applied || cancelled() {
            break;
        }
        catch_up(&reads, &mut shadow, applied, &cancelled)?;
        if !room(shadow.heap_bytes()) {
            return Err(ShadowError::Failed(
                "the memory bound has no room for the shadow rebuild".into(),
            ));
        }
    }
    if cancelled() {
        return Err(stopped());
    }
    if let Some(hook) = &shared.config.hooks.before_swap {
        hook(catalog);
    }
    Ok(shadow)
}

/// Applies the commits after the tid of `shadow` up to `to`.
fn catch_up(
    reads: &dyn Reads,
    shadow: &mut Shadow,
    to: u64,
    cancelled: &dyn Fn() -> bool,
) -> Result<(), ShadowError> {
    let from = shadow.applied + 1;
    let mut again = None;
    let mut each = |commit: Commit| -> bool {
        if cancelled() {
            return false;
        }
        match shadow.catch_up(&commit) {
            Ok(()) => true,
            Err(tid) => {
                again = Some(tid);
                false
            }
        }
    };
    reads
        .commits(from, to, &mut each)
        .map_err(ShadowError::Failed)?;
    match again {
        Some(tid) => Err(ShadowError::Again(tid)),
        None if cancelled() => Err(ShadowError::Failed("the shadow rebuild stopped".into())),
        None => {
            // A tid without a commit changes nothing.
            shadow.applied = shadow.applied.max(to);
            Ok(())
        }
    }
}

/// Lays the shadow rebuilds that ended. The thread first applies to each
/// shadow the commits that the mirror applied after it, then lays its base,
/// its change sets and its shapes in at one step. A shadow rebuild that
/// failed, or that panics here, drops the base: the mirror builds again.
fn install_shadows(shared: &Arc<Shared>, store: &dyn Reads) {
    let results = std::mem::take(&mut shared.lock().shadowed);
    for (catalog, id, result) in results {
        let (old, maint) = {
            let mut inner = shared.lock();
            let Some(e) = inner.entries.get_mut(&catalog) else {
                continue;
            };
            let again = match &e.shadow {
                Shadowing::Running { id: j, again, .. } if *j == id => *again,
                _ => continue,
            };
            e.shadow_bytes = 0;
            if again {
                e.shadow = Shadowing::Due;
                continue;
            }
            let (State::Ready(v), Some(maint)) = (&e.state, &e.maint) else {
                e.shadow = Shadowing::Idle;
                continue;
            };
            let old = v.clone();
            let maint = Arc::clone(maint);
            match &result {
                Ok(_) => {}
                Err(ShadowError::Again(tid)) => {
                    let changed = e.changed();
                    e.marks.push((*tid, changed));
                    e.shadow = Shadowing::Due;
                    continue;
                }
                Err(ShadowError::Failed(why)) => {
                    fall_back(shared, &mut inner, catalog, why);
                    continue;
                }
            }
            (old, maint)
        };
        let Ok(built) = result else {
            continue;
        };
        let tid = built.tid;
        let changed = built.changed.clone();
        let seconds = built.seconds;
        let laid = catch_unwind(AssertUnwindSafe(|| {
            let mut built = built;
            // The commits that the mirror applied after the shadow.
            let cancelled = || shared.cancel.load(AtomicOrdering::Relaxed);
            if built.applied < old.applied {
                catch_up(store, &mut built, old.applied, &cancelled)?;
            }
            let m = read(&maint);
            let swapped = shadow::swap(&old, &m.shapes, &m.objects, &m.moved, built);
            let new_maint = Maint {
                shapes: swapped.shapes,
                objects: swapped.objects,
                moved: swapped.moved,
                pruned: m.pruned,
            };
            Ok((swapped.versions, new_maint))
        }))
        .unwrap_or_else(|_| Err(ShadowError::Failed("the swap panicked".into())));
        drop(old);
        let mut inner = shared.lock();
        let current = inner.entries.get(&catalog).is_some_and(|e| {
            matches!(&e.shadow, Shadowing::Running { id: j, again: false, .. } if *j == id)
                && matches!(e.state, State::Ready(_))
        });
        if !current {
            continue;
        }
        match laid {
            Ok((versions, new_maint)) => {
                if let Some(e) = inner.entries.get_mut(&catalog) {
                    let bytes = versions.heap_bytes() + new_maint.heap_bytes();
                    e.state = State::Ready(versions);
                    e.maint = Some(Arc::new(RwLock::new(new_maint)));
                    e.shadow = Shadowing::Idle;
                    e.marks.clear();
                    e.fresh.push((tid, changed));
                    e.swaps += 1;
                    e.build_seconds = seconds;
                    e.bytes = bytes;
                }
            }
            Err(ShadowError::Again(tid)) => {
                if let Some(e) = inner.entries.get_mut(&catalog) {
                    let changed = e.changed();
                    e.marks.push((tid, changed));
                    e.shadow = Shadowing::Due;
                }
            }
            Err(ShadowError::Failed(why)) => fall_back(shared, &mut inner, catalog, &why),
        }
        drop(inner);
        shared.changed.notify_all();
    }
}

/// Drops the base of a mirror whose shadow rebuild failed: the mirror
/// builds again from nothing, as of the first of its stale commits.
fn fall_back(shared: &Shared, inner: &mut Inner, catalog: u64, why: &str) {
    if let Some(tid) = rebuilding_since(inner, catalog) {
        let why = format!("its shadow rebuild failed: {why}");
        make_stale(shared, inner, catalog, tid, &why);
    }
}
