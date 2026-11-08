//! The text indexes of a store, and their thread (ADR 0060, section 10).
//!
//! One tantivy index serves each registered catalog whose settings in
//! `zoe.text` name a text index in the enhanced mode, in `text/<oid>/` of
//! the store directory. Only the process that holds the lock of the store
//! opens a writer: a read-only open writes nothing in `text/`.
//!
//! The thread reads the registration keys and the settings at its start,
//! and after each commit that writes `zoe.catalog` or `zoe.text`. It opens
//! the index of each catalog, or rebuilds it from the live texts, then
//! follows the commits after their publication in tid order through
//! `Store::text_changed`. A rebuild starts for a missing directory, one
//! that tantivy cannot open, a file that fails its checksum, a tid before
//! the pack watermark or after the last tid, a change of the layout, and a
//! clear delta of the catalog. The writer commits every 500 ms or every
//! 10,000 deltas, with one indexing thread and a budget of 256 MB.
//!
//! The index holds a cap of pack at its tid. A pack that ran while the cap
//! was taken is not held: after each catch-up, the thread reads the pack
//! watermark again and rebuilds the index when it is above the tid of the
//! cap.
//!
//! A delta that fails its checksum stops the index of its catalog: each
//! text query of the catalog then fails with `corrupt`.

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use tantivy::{Index, IndexReader, IndexWriter, Searcher, Term};
use zoe_core::{Flavour, Oid, PackCap, Store, TextChange, Tid, ZoeError};

use crate::counts::TextCounts;
use crate::mirrors::{Hook, Opener};
use crate::read::NAMESPACE as CATALOGS;

use super::clause::{TextError, TextErrorKind};
use super::index::{self, Analyzers, Fields, Maps, SegmentRids};
use super::rebuild::Step;
use super::settings::{CatalogSettings, Layout, Mode, NAMESPACE};

/// The longest time that the writer holds a delta before a commit.
const COMMIT_EVERY: Duration = Duration::from_millis(500);

/// The deltas after which the writer commits.
const COMMIT_DELTAS: usize = 10_000;

/// The memory budget of the writer.
const WRITER_BUDGET: usize = 256 << 20;

/// The longest sleep of the thread between two passes.
const TICK: Duration = Duration::from_secs(1);

/// The keys that one scan of the deltas of a rebuild reads.
const SCAN: usize = 1024;

/// Where the text indexes of a store live.
#[derive(Clone, Debug, Default)]
pub struct TextConfig {
    /// The store directory: the indexes are in its `text/`. `None` for a
    /// store without a directory, whose catalogs get no index.
    pub dir: Option<PathBuf>,
    /// True for a read-only open: no writer, no rebuild.
    pub read_only: bool,
}

/// The state of the text index of one catalog.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TextState {
    /// The thread opens it.
    Opening,
    /// The thread rebuilds it from the live texts.
    Building,
    /// It answers queries.
    Ready,
    /// It cannot answer: the reason.
    Unavailable(String),
    /// A delta fails its checksum: the delta.
    Corrupt(String),
}

/// What a query of the text index sees of it: a searcher and its maps, the
/// tid up to which it holds the deltas, and the settings.
pub struct Snap {
    /// The searcher.
    pub searcher: Searcher,
    /// The maps of its documents.
    pub maps: Arc<Maps>,
    /// The fields of its schema.
    pub fields: Fields,
    /// The settings of the text indexes of the catalog.
    pub settings: CatalogSettings,
    /// The tid up to which the index holds the deltas.
    pub tid: u64,
    /// The tid of the newest clear delta that the index holds, by index id.
    pub cleared: HashMap<String, u64>,
}

struct Entry {
    state: TextState,
    snap: Option<Arc<Snap>>,
    settings: CatalogSettings,
    /// The tid up to which the index held the deltas before its open or
    /// its rebuild began, for the lag; `None` for none.
    held: Option<u64>,
    /// When the open or the rebuild began.
    since: Instant,
}

struct Inner {
    entries: BTreeMap<u64, Entry>,
    target: u64,
    stop: bool,
    dirty: bool,
    /// True after the first read of the registration and the settings.
    loaded: bool,
    /// The tids of the recent hand-offs, for the lag.
    handed: VecDeque<u64>,
    generations: Option<(u64, u64)>,
}

struct Shared {
    inner: Mutex<Inner>,
    changed: Condvar,
    open: Opener,
    config: TextConfig,
    cancel: AtomicBool,
    /// The hook of the tests before a rebuild.
    before_build: Option<Hook>,
    /// The hook of the tests between the check of an open and the cap of
    /// pack that the open takes.
    before_cap: Option<Hook>,
    /// The hook of the tests before the check of the cap of a commit or of
    /// an advance of the tid.
    before_check: Option<Hook>,
}

/// The hooks of the tests of the text indexes, each called with the oid of
/// the catalog in the thread of the text indexes.
#[derive(Clone, Default)]
pub struct TextHooks {
    /// Before a rebuild reads the deltas.
    pub before_build: Option<Hook>,
    /// Between the checks of an open and the cap of pack that it takes.
    pub before_cap: Option<Hook>,
    /// Before the check of the cap of a commit of the writer or of an
    /// advance of the tid.
    pub before_check: Option<Hook>,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// The text indexes of one store.
pub struct Texts {
    shared: Arc<Shared>,
    wait: Duration,
    thread: Mutex<Option<JoinHandle<()>>>,
}

impl Texts {
    /// Start the thread of the text indexes of the store that `open`
    /// gives. The store published each commit up to `published`. A query
    /// waits at most `wait` for an index that opens or builds.
    /// `hooks` are the hooks of the tests.
    pub fn start(
        open: Opener,
        published: u64,
        config: TextConfig,
        wait: Duration,
        hooks: TextHooks,
    ) -> Texts {
        let shared = Arc::new(Shared {
            inner: Mutex::new(Inner {
                entries: BTreeMap::new(),
                target: published,
                stop: false,
                dirty: true,
                loaded: false,
                handed: VecDeque::new(),
                generations: None,
            }),
            changed: Condvar::new(),
            open,
            config,
            cancel: AtomicBool::new(false),
            before_build: hooks.before_build,
            before_cap: hooks.before_cap,
            before_check: hooks.before_check,
        });
        let worker = Arc::clone(&shared);
        let thread = thread::Builder::new()
            .name("zoe-text".to_owned())
            .spawn(move || run(&worker))
            .ok();
        if thread.is_none() {
            let mut inner = shared.lock();
            inner.loaded = true;
        }
        Texts {
            shared,
            wait,
            thread: Mutex::new(thread),
        }
    }

    /// Hand the commit `tid` to the thread, after its publication. It
    /// never waits for the index.
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

    /// The longest wait of a query for an index that opens or builds.
    pub fn wait(&self) -> Duration {
        self.wait
    }

    /// The index of `catalog` as a query sees it, after a wait of at most
    /// `wait` for an index that opens or builds, and for one that holds
    /// the deltas up to `at` when `at` is given.
    pub fn snap(&self, catalog: u64, at: Option<u64>) -> Result<Arc<Snap>, Failure> {
        self.snap_by(catalog, at, Instant::now() + self.wait)
    }

    /// The index of `catalog` as `snap` gives it, with a wait until
    /// `deadline` at most.
    pub fn snap_by(
        &self,
        catalog: u64,
        at: Option<u64>,
        deadline: Instant,
    ) -> Result<Arc<Snap>, Failure> {
        let mut inner = self.shared.lock();
        loop {
            if inner.loaded {
                let Some(entry) = inner.entries.get(&catalog) else {
                    return Err(Failure::unavailable(format!(
                        "the catalog {catalog:#x} has no text index"
                    )));
                };
                match &entry.state {
                    TextState::Ready => {
                        let snap = entry.snap.clone().expect("a ready index has a snap");
                        if at.is_none_or(|at| snap.tid >= at) {
                            return Ok(snap);
                        }
                    }
                    TextState::Opening | TextState::Building => {}
                    TextState::Unavailable(why) => return Err(Failure::unavailable(why.clone())),
                    TextState::Corrupt(why) => {
                        return Err(Failure::Text(TextError::new(
                            TextErrorKind::Corrupt,
                            why.clone(),
                        )));
                    }
                }
            }
            let now = Instant::now();
            if now >= deadline {
                return Err(Failure::unavailable(format!(
                    "the text index of {catalog:#x} opens or builds past catalog-wait"
                )));
            }
            inner = self
                .shared
                .changed
                .wait_timeout(inner, deadline - now)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
    }

    /// The state of the index of `catalog`, or `None` when the store has no
    /// text index for it. This is a hook for the tests.
    pub fn state(&self, catalog: u64) -> Option<TextState> {
        self.shared
            .lock()
            .entries
            .get(&catalog)
            .map(|e| e.state.clone())
    }

    /// Wait at most `timeout` until `test` holds for the counts of
    /// `catalog`. Tell if it held.
    pub fn wait_for(
        &self,
        catalog: u64,
        timeout: Duration,
        test: impl Fn(Option<&TextCounts>) -> bool,
    ) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            let counts = self.counts();
            if test(counts.iter().find(|c| c.catalog == catalog)) {
                return true;
            }
            let now = Instant::now();
            if now >= deadline {
                return false;
            }
            let inner = self.shared.lock();
            let _ = self
                .shared
                .changed
                .wait_timeout(inner, (deadline - now).min(Duration::from_millis(20)));
        }
    }

    /// The counts of the text indexes. The lag of an index is the age of
    /// the oldest commit after its tid that it did not apply yet, also while
    /// it opens or rebuilds; an index that holds no tid yet counts from the
    /// start of its open or its rebuild.
    pub fn counts(&self) -> Vec<TextCounts> {
        let inner = self.shared.lock();
        let now = unix_now();
        let lag = |e: &Entry| -> f64 {
            let held = e.snap.as_ref().map(|s| s.tid).or(e.held);
            match held {
                Some(held) => inner
                    .handed
                    .iter()
                    .find(|t| **t > held)
                    .map_or(0.0, |t| (now - tid_seconds(*t)).max(0.0)),
                None if e.state == TextState::Ready => 0.0,
                None => e.since.elapsed().as_secs_f64(),
            }
        };
        inner
            .entries
            .iter()
            .map(|(catalog, e)| TextCounts {
                catalog: *catalog,
                ready: e.state == TextState::Ready,
                lag_seconds: lag(e),
                docs: e.snap.as_ref().map_or(0, |s| s.maps.num_docs()),
                tid: e.snap.as_ref().map_or(0, |s| s.tid),
            })
            .collect()
    }

    /// Tell the thread to stop, and do not wait for it.
    pub fn signal_stop(&self) {
        let mut inner = self.shared.lock();
        inner.stop = true;
        drop(inner);
        self.shared.cancel.store(true, Ordering::Relaxed);
        self.shared.changed.notify_all();
    }

    /// Stop the thread and wait for it.
    pub fn stop(&self) {
        self.signal_stop();
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

impl Drop for Texts {
    fn drop(&mut self) {
        self.signal_stop();
    }
}

/// Why a text query fails.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Failure {
    /// An error of the reply (`e`).
    Text(TextError),
    /// The mode of the index is not served: `Unsupported`.
    Unsupported(String),
}

impl Failure {
    /// The error `unavailable` with `why`.
    pub fn unavailable(why: impl Into<String>) -> Failure {
        Failure::Text(TextError::new(TextErrorKind::Unavailable, why))
    }

    /// The failure of the read error `error` of the store.
    pub fn of(error: &ZoeError) -> Failure {
        match error {
            ZoeError::CorruptText { .. } => {
                Failure::Text(TextError::new(TextErrorKind::Corrupt, error.to_string()))
            }
            _ => Failure::unavailable(error.to_string()),
        }
    }
}

/// Now, in seconds since the epoch.
fn unix_now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0.0, |d| d.as_secs_f64())
}

/// The days of the date `y`-`m`-`d` since 1970-01-01.
fn days_from_civil(y: i64, m: u64, d: u64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400) as u64;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe as i64 - 719_468
}

/// The time of `tid`, in seconds since the epoch.
pub fn tid_seconds(tid: u64) -> f64 {
    let (y, m, d, h, min) = zoe_core::civil_from_tid(Tid(tid));
    let days = days_from_civil(y as i64, m, d);
    let secs = (tid & 0xffff_ffff) as f64 * 60.0 / 4_294_967_296.0;
    (days * 86_400 + (h * 3_600 + min * 60) as i64) as f64 + secs
}

/// The state of the index of one catalog that the thread keeps.
struct Live {
    /// The index: the writer and the reader read it.
    _index: Index,
    writer: Option<IndexWriter>,
    reader: IndexReader,
    fields: Fields,
    layout: Layout,
    segments: SegmentRids,
    analyzers: Analyzers,
    /// The tid of the last delta in the writer.
    applied: u64,
    /// The tid of the last commit of the writer.
    committed: u64,
    cleared: HashMap<String, u64>,
    pending: usize,
    first_pending: Option<Instant>,
    cap: Option<PackCap>,
}

/// The thread of the text indexes.
fn run(shared: &Arc<Shared>) {
    let mut lives: HashMap<u64, Live> = HashMap::new();
    loop {
        let timeout = lives
            .values()
            .filter_map(|l| l.first_pending)
            .map(|t| COMMIT_EVERY.saturating_sub(t.elapsed()))
            .min()
            .unwrap_or(TICK)
            .min(TICK);
        {
            let inner = shared.lock();
            let (mut inner, _) = shared
                .changed
                .wait_timeout_while(inner, timeout, |i| !i.stop && !i.dirty)
                .unwrap_or_else(PoisonError::into_inner);
            if inner.stop {
                break;
            }
            inner.dirty = false;
        }
        let Some(reads) = (shared.open)() else {
            break;
        };
        let Some(store) = reads.store() else {
            let mut inner = shared.lock();
            inner.loaded = true;
            drop(inner);
            shared.changed.notify_all();
            break;
        };
        let pass = catch_unwind(AssertUnwindSafe(|| pass(shared, store, &mut lives)));
        if pass.is_err() {
            lives.clear();
            let mut inner = shared.lock();
            for e in inner.entries.values_mut() {
                e.state = TextState::Unavailable("the thread of the text index panicked".into());
                e.snap = None;
            }
            inner.loaded = true;
            drop(inner);
            tracing::error!("the thread of the text indexes panicked");
            shared.changed.notify_all();
        }
        drop(reads);
    }
    // The writers stop their threads at the drop.
    drop(lives);
}

/// One pass of the thread: the settings, the opens and the rebuilds, the
/// commits and the batches of the writers.
fn pass(shared: &Arc<Shared>, store: &Store, lives: &mut HashMap<u64, Live>) {
    read_settings(shared, store, lives);
    let opening: Vec<u64> = {
        let inner = shared.lock();
        inner
            .entries
            .iter()
            .filter(|(_, e)| e.state == TextState::Opening)
            .map(|(c, _)| *c)
            .collect()
    };
    for catalog in opening {
        if shared.cancel.load(Ordering::Relaxed) {
            return;
        }
        open_index(shared, store, lives, catalog);
    }
    follow(shared, store, lives);
}

/// Read the registration keys and the settings, when a commit wrote them
/// since the last read.
fn read_settings(shared: &Arc<Shared>, store: &Store, lives: &mut HashMap<u64, Live>) {
    let generations = (
        store.kv_generation(CATALOGS).map_or(0, |t| t.0),
        store.kv_generation(NAMESPACE).map_or(0, |t| t.0),
    );
    if shared.lock().generations == Some(generations) {
        return;
    }
    let wanted = match registered(store) {
        Ok(wanted) => wanted,
        Err(e) => {
            // The entries stay, and the next pass reads again.
            tracing::warn!("the read of the registration keys fails: {e}");
            return;
        }
    };
    let mut inner = shared.lock();
    inner.generations = Some(generations);
    let gone: Vec<u64> = inner
        .entries
        .keys()
        .filter(|c| !wanted.contains_key(c))
        .copied()
        .collect();
    for catalog in gone {
        inner.entries.remove(&catalog);
        lives.remove(&catalog);
    }
    for (catalog, settings) in wanted {
        let settings = match settings {
            Ok(s) if layout_of(&s).is_empty() => {
                // No text index in the enhanced mode: the queries see the
                // settings, and no index serves them.
                let entry = Entry {
                    state: TextState::Unavailable(
                        "the catalog has no text index in the enhanced mode".into(),
                    ),
                    snap: None,
                    settings: s,
                    held: None,
                    since: Instant::now(),
                };
                inner.entries.insert(catalog, entry);
                lives.remove(&catalog);
                continue;
            }
            Ok(s) => s,
            Err(why) => {
                let entry = Entry {
                    state: TextState::Unavailable(format!("the settings of zoe.text: {why}")),
                    snap: None,
                    settings: CatalogSettings::default(),
                    held: None,
                    since: Instant::now(),
                };
                inner.entries.insert(catalog, entry);
                lives.remove(&catalog);
                continue;
            }
        };
        match inner.entries.get_mut(&catalog) {
            Some(e) if e.settings == settings => {}
            Some(e)
                if layout_of(&e.settings) == layout_of(&settings)
                    && e.state == TextState::Ready =>
            {
                // The boosts or the attributes changed: the next query
                // reads them.
                e.settings = settings.clone();
                if let Some(old) = &e.snap {
                    e.snap = Some(Arc::new(Snap {
                        searcher: old.searcher.clone(),
                        maps: Arc::clone(&old.maps),
                        fields: old.fields.clone(),
                        settings,
                        tid: old.tid,
                        cleared: old.cleared.clone(),
                    }));
                }
            }
            _ => {
                lives.remove(&catalog);
                inner.entries.insert(
                    catalog,
                    Entry {
                        state: TextState::Opening,
                        snap: None,
                        settings,
                        held: None,
                        since: Instant::now(),
                    },
                );
            }
        }
    }
    inner.loaded = true;
    drop(inner);
    shared.changed.notify_all();
}

/// The settings in `zoe.text` of each registered catalog that has them,
/// or why they do not read.
pub(super) fn registered(
    store: &Store,
) -> Result<BTreeMap<u64, Result<CatalogSettings, String>>, ZoeError> {
    let mut wanted = BTreeMap::new();
    let mut start: Option<Vec<u8>> = None;
    loop {
        let page = store.kv_scan(Flavour::Durable, CATALOGS, start.as_deref(), None, 1000)?;
        let Some((last, _)) = page.last() else {
            break;
        };
        let mut next = last.clone();
        next.push(0);
        for (key, _) in &page {
            let Ok(bytes) = <[u8; 8]>::try_from(key.as_slice()) else {
                continue;
            };
            let catalog = u64::from_be_bytes(bytes);
            match store.kv_get(Flavour::Durable, NAMESPACE, key) {
                Ok(Some(value)) => {
                    wanted.insert(catalog, CatalogSettings::parse(&value));
                }
                Ok(None) => {}
                Err(e) => {
                    wanted.insert(catalog, Err(e.to_string()));
                }
            }
        }
        start = Some(next);
    }
    Ok(wanted)
}

/// The layout of the index of `settings`: its text indexes in the enhanced
/// mode.
pub(super) fn layout_of(settings: &CatalogSettings) -> Layout {
    settings
        .layout()
        .into_iter()
        .filter(|(_, mode, _)| *mode == Mode::Enhanced)
        .collect()
}

fn set_state(shared: &Shared, catalog: u64, state: TextState) {
    let mut inner = shared.lock();
    if let Some(e) = inner.entries.get_mut(&catalog) {
        if state != TextState::Ready {
            if let Some(snap) = e.snap.take() {
                e.held = Some(snap.tid);
            }
            if e.state == TextState::Ready {
                e.since = Instant::now();
            }
        }
        e.state = state;
    }
    drop(inner);
    shared.changed.notify_all();
}

/// Open the index of `catalog`, or rebuild it.
fn open_index(shared: &Arc<Shared>, store: &Store, lives: &mut HashMap<u64, Live>, catalog: u64) {
    let Some(settings) = shared
        .lock()
        .entries
        .get(&catalog)
        .map(|e| e.settings.clone())
    else {
        return;
    };
    let layout = layout_of(&settings);
    let Some(dir) = &shared.config.dir else {
        set_state(
            shared,
            catalog,
            TextState::Unavailable("the store has no directory for text/".into()),
        );
        return;
    };
    let path = index::directory(dir, catalog);
    let read_only = shared.config.read_only;
    if !read_only && let Err(e) = super::rebuild::clean(&path) {
        tracing::warn!("the text index of {catalog:#x} keeps a directory of a rebuild: {e}");
    }
    let found = open_existing(store, &path, &layout, read_only);
    match found {
        Ok((index, tid)) => {
            let writer = if read_only {
                None
            } else {
                // The process holds the lock of the store: a writer lock
                // that a crash left is stale.
                let _ = std::fs::remove_file(path.join(".tantivy-writer.lock"));
                match index.writer_with_num_threads(1, WRITER_BUDGET) {
                    Ok(w) => Some(w),
                    Err(e) => {
                        tracing::warn!(
                            "the text index of {catalog:#x} opens no writer and builds again: {e}"
                        );
                        return rebuild(shared, store, lives, catalog, &settings);
                    }
                }
            };
            let cleared = match clears(store, catalog, &layout, tid) {
                Ok(c) => c,
                Err(e) => return fail(shared, catalog, &e),
            };
            let reader = match index::reader(&index) {
                Ok(r) => r,
                Err(e) => {
                    set_state(shared, catalog, TextState::Unavailable(e.to_string()));
                    return;
                }
            };
            let fields = index::schema(&layout).1;
            let cap = if read_only {
                None
            } else {
                if let Some(hook) = &shared.before_cap {
                    hook(catalog);
                }
                let cap = store.pack_cap(Tid(tid));
                // A pack that started before the cap is not held by it: its
                // watermark passes the tid of the index.
                if store.pack_watermark().is_ok_and(|w| w.0 > tid) {
                    drop(cap);
                    tracing::warn!(
                        "a pack passed the tid of the text index of {catalog:#x}: it builds again"
                    );
                    return rebuild(shared, store, lives, catalog, &settings);
                }
                Some(cap)
            };
            let mut live = Live {
                _index: index,
                writer,
                reader,
                fields,
                layout,
                segments: SegmentRids::new(),
                analyzers: Analyzers::default(),
                applied: tid,
                committed: tid,
                cleared,
                pending: 0,
                first_pending: None,
                cap,
            };
            if publish(shared, catalog, &mut live, &settings).is_err() {
                return;
            }
            lives.insert(catalog, live);
        }
        Err(why) if read_only => {
            set_state(
                shared,
                catalog,
                TextState::Unavailable(format!("a read-only store does not rebuild: {why}")),
            );
        }
        Err(why) => {
            tracing::warn!("the text index of {catalog:#x} builds again: {why}");
            rebuild(shared, store, lives, catalog, &settings);
        }
    }
}

/// Open the index in `path` when the open would not rebuild it: return it
/// and the tid of its payload, or why it needs a rebuild.
fn open_existing(
    store: &Store,
    path: &Path,
    layout: &Layout,
    read_only: bool,
) -> Result<(Index, u64), String> {
    if !path.is_dir() {
        return Err("the directory is missing".into());
    }
    let index = index::open(path, read_only).map_err(|e| format!("tantivy cannot open it: {e}"))?;
    match index.validate_checksum() {
        Ok(bad) if bad.is_empty() => {}
        Ok(bad) => return Err(format!("{} files fail their checksum", bad.len())),
        Err(e) => return Err(format!("the check of its files fails: {e}")),
    }
    let metas = index
        .load_metas()
        .map_err(|e| format!("its meta does not read: {e}"))?;
    let (tid, found) = metas
        .payload
        .as_deref()
        .and_then(index::read_payload)
        .ok_or("its payload does not read")?;
    if &found != layout {
        return Err("the mode, the language or the indexes changed".into());
    }
    if tid > store.last_transaction().0 {
        return Err("its tid is after the last tid of the store".into());
    }
    let watermark = store.pack_watermark().map_err(|e| e.to_string())?;
    if Tid(tid) < watermark {
        return Err("its tid is before the pack watermark".into());
    }
    Ok((index, tid))
}

/// The tid of the newest clear delta at or before `tid` of each index of
/// `layout` of `catalog`.
fn clears(
    store: &Store,
    catalog: u64,
    layout: &Layout,
    tid: u64,
) -> Result<HashMap<String, u64>, ZoeError> {
    let mut out = HashMap::new();
    let mut after = Some(first_key(catalog));
    'scan: loop {
        let keys = store.text_keys(after.as_ref(), SCAN)?;
        for k in &keys {
            if k.catalog.0 != catalog || k.rid.is_some() {
                break 'scan;
            }
            if k.tid.0 <= tid && layout.iter().any(|(name, _, _)| *name == k.index) {
                let e = out.entry(k.index.clone()).or_insert(0);
                *e = (*e).max(k.tid.0);
            }
        }
        match keys.last() {
            Some(last) if keys.len() == SCAN => after = Some(last.clone()),
            _ => break,
        }
    }
    Ok(out)
}

/// A key of `text_deltas` just before each key of `catalog`: a clear delta
/// of the index id of one byte 0 at the tid 0, which no commit writes.
fn first_key(catalog: u64) -> TextChange {
    TextChange {
        tid: Tid::ZERO,
        catalog: Oid(catalog),
        index: "\0".into(),
        rid: None,
    }
}

/// Stop the index of `catalog` at a read that failed: a damaged delta
/// makes it corrupt, another error unavailable.
fn fail(shared: &Shared, catalog: u64, error: &ZoeError) {
    match error {
        ZoeError::CorruptText {
            catalog: c,
            index,
            rid,
            tid,
        } => {
            tracing::error!(
                catalog = format!("{:#018x}", c.0),
                index = %index,
                rid = ?rid,
                tid = format!("{:#018x}", tid.0),
                "a text delta fails its checksum: the text index of the catalog stops"
            );
            set_state(shared, catalog, TextState::Corrupt(error.to_string()));
        }
        other => {
            tracing::warn!("the text index of {catalog:#x} stops: {other}");
            set_state(shared, catalog, TextState::Unavailable(other.to_string()));
        }
    }
}

/// The live texts of `rid` before `before`, for each index of `layout`.
fn texts_of(
    store: &Store,
    catalog: u64,
    layout: &Layout,
    rid: i32,
    before: u64,
) -> Result<Vec<(String, String, String)>, ZoeError> {
    let mut out = Vec::new();
    for (name, _, _) in layout {
        if let Some(t) = store.text_before(Oid(catalog), name, rid, Tid(before))? {
            out.push((name.clone(), t.language, t.text));
        }
    }
    Ok(out)
}

/// An index of a catalog, built from the live texts at the tid `at` and
/// committed with the payload of that tid.
pub(super) struct Built {
    pub(super) index: Index,
    pub(super) writer: IndexWriter,
    pub(super) fields: Fields,
    pub(super) analyzers: Analyzers,
    /// The tid of the newest clear delta at or before `at`, by index id.
    pub(super) cleared: HashMap<String, u64>,
    pub(super) at: u64,
    /// The cap of pack at `at`.
    pub(super) cap: PackCap,
    /// The documents of the index.
    pub(super) docs: u64,
}

/// Why a build stops before its commit.
pub(super) enum Halt {
    /// `cancel` is set.
    Cancelled,
    /// A pack that started before the cap of the build passed it: the
    /// caller builds again.
    Packed,
    /// A read of the store fails.
    Store(ZoeError),
    /// tantivy fails: why.
    Index(String),
}

/// Build in `path` the index of `catalog` with `layout`, from the live
/// texts at the last tid, after the removal of what `path` holds. The
/// writer has `threads` indexing threads. `step` gets `Step::Created`
/// when the empty index is in `path`, and `Step::Added` when each document
/// is in the writer, before the commit.
///
/// The build checks the pack watermark before it writes its commit: a
/// commit on disk with a tid past a lost cap would survive a restart.
pub(super) fn build(
    store: &Store,
    catalog: u64,
    layout: &Layout,
    path: &Path,
    threads: usize,
    cancel: &AtomicBool,
    step: &dyn Fn(Step),
) -> Result<Built, Halt> {
    let at = store.last_transaction().0;
    let cap = store.pack_cap(Tid(at));
    let (index, fields) = index::create(path, layout)
        .map_err(|e| Halt::Index(format!("the rebuild of the text index fails: {e}")))?;
    step(Step::Created);
    let mut writer: IndexWriter = index
        .writer_with_num_threads(threads, WRITER_BUDGET)
        .map_err(|e| Halt::Index(format!("the writer of the text index fails: {e}")))?;
    let mut rids = BTreeSet::new();
    let mut cleared: HashMap<String, u64> = HashMap::new();
    let mut after = Some(first_key(catalog));
    'scan: loop {
        if cancel.load(Ordering::Relaxed) {
            return Err(Halt::Cancelled);
        }
        let keys = store.text_keys(after.as_ref(), SCAN).map_err(Halt::Store)?;
        for k in &keys {
            if k.catalog.0 != catalog {
                break 'scan;
            }
            if k.tid.0 > at || !layout.iter().any(|(name, _, _)| *name == k.index) {
                continue;
            }
            match k.rid {
                Some(rid) => {
                    rids.insert(rid);
                }
                None => {
                    let e = cleared.entry(k.index.clone()).or_insert(0);
                    *e = (*e).max(k.tid.0);
                }
            }
        }
        match keys.last() {
            Some(last) if keys.len() == SCAN => after = Some(last.clone()),
            _ => break,
        }
    }
    let mut analyzers = Analyzers::default();
    let mut docs = 0;
    for (n, rid) in rids.into_iter().enumerate() {
        if n % 1000 == 0 && cancel.load(Ordering::Relaxed) {
            return Err(Halt::Cancelled);
        }
        let texts = texts_of(store, catalog, layout, rid, at + 1).map_err(Halt::Store)?;
        if !texts.is_empty() {
            writer
                .add_document(index::document(&fields, &mut analyzers, rid, &texts))
                .map_err(|e| Halt::Index(e.to_string()))?;
            docs += 1;
        }
    }
    step(Step::Added);
    // A pack that started before the cap of the build can pass it.
    if store.pack_watermark().is_ok_and(|w| w.0 > at) {
        return Err(Halt::Packed);
    }
    writer
        .prepare_commit()
        .and_then(|mut p| {
            p.set_payload(&index::payload(at, layout));
            p.commit()
        })
        .map_err(|e| Halt::Index(e.to_string()))?;
    Ok(Built {
        index,
        writer,
        fields,
        analyzers,
        cleared,
        at,
        cap,
        docs,
    })
}

/// Rebuild the index of `catalog` from the live texts at the last tid.
fn rebuild(
    shared: &Arc<Shared>,
    store: &Store,
    lives: &mut HashMap<u64, Live>,
    catalog: u64,
    settings: &CatalogSettings,
) {
    lives.remove(&catalog);
    set_state(shared, catalog, TextState::Building);
    if let Some(hook) = &shared.before_build {
        hook(catalog);
    }
    let layout = layout_of(settings);
    let Some(dir) = &shared.config.dir else {
        return;
    };
    let path = index::directory(dir, catalog);
    let built = match build(store, catalog, &layout, &path, 1, &shared.cancel, &|_| {}) {
        Ok(built) => built,
        Err(Halt::Cancelled) => return,
        Err(Halt::Packed) => return rebuild(shared, store, lives, catalog, settings),
        Err(Halt::Store(e)) => return fail(shared, catalog, &e),
        Err(Halt::Index(why)) => {
            set_state(shared, catalog, TextState::Unavailable(why));
            return;
        }
    };
    let reader = match index::reader(&built.index) {
        Ok(r) => r,
        Err(e) => {
            set_state(shared, catalog, TextState::Unavailable(e.to_string()));
            return;
        }
    };
    let mut live = Live {
        _index: built.index,
        writer: Some(built.writer),
        reader,
        fields: built.fields,
        layout,
        segments: SegmentRids::new(),
        analyzers: built.analyzers,
        applied: built.at,
        committed: built.at,
        cleared: built.cleared,
        pending: 0,
        first_pending: None,
        cap: Some(built.cap),
    };
    if publish(shared, catalog, &mut live, settings).is_ok() {
        lives.insert(catalog, live);
    }
}

/// Reload the reader of `live` and give queries its new snap.
fn publish(
    shared: &Shared,
    catalog: u64,
    live: &mut Live,
    settings: &CatalogSettings,
) -> Result<(), ()> {
    if let Err(e) = live.reader.reload() {
        set_state(shared, catalog, TextState::Unavailable(e.to_string()));
        return Err(());
    }
    let searcher = live.reader.searcher();
    let maps = match Maps::build(&searcher, &mut live.segments) {
        Ok(m) => m,
        Err(e) => {
            set_state(shared, catalog, TextState::Unavailable(e.to_string()));
            return Err(());
        }
    };
    let snap = Snap {
        searcher,
        maps: Arc::new(maps),
        fields: live.fields.clone(),
        settings: settings.clone(),
        tid: live.committed,
        cleared: live.cleared.clone(),
    };
    let mut inner = shared.lock();
    if let Some(e) = inner.entries.get_mut(&catalog) {
        e.snap = Some(Arc::new(snap));
        e.state = TextState::Ready;
    }
    prune_handed(&mut inner);
    drop(inner);
    shared.changed.notify_all();
    Ok(())
}

/// Drop the hand-offs that each index holds.
fn prune_handed(inner: &mut Inner) {
    let low = inner
        .entries
        .values()
        .map(|e| e.snap.as_ref().map(|s| s.tid).or(e.held).unwrap_or(0))
        .min()
        .unwrap_or(u64::MAX);
    while inner.handed.front().is_some_and(|t| *t <= low) {
        inner.handed.pop_front();
    }
}

/// Give queries the tid `tid` of `live` with no new document: the index
/// holds every delta up to it.
fn advance(shared: &Shared, catalog: u64, tid: u64) {
    let mut inner = shared.lock();
    if let Some(e) = inner.entries.get_mut(&catalog)
        && let Some(old) = &e.snap
        && old.tid < tid
    {
        e.snap = Some(Arc::new(Snap {
            searcher: old.searcher.clone(),
            maps: Arc::clone(&old.maps),
            fields: old.fields.clone(),
            settings: old.settings.clone(),
            tid,
            cleared: old.cleared.clone(),
        }));
    }
    prune_handed(&mut inner);
    drop(inner);
    shared.changed.notify_all();
}

/// The settings of the text indexes of `catalog`.
fn settings_of(shared: &Shared, catalog: u64) -> Option<CatalogSettings> {
    shared
        .lock()
        .entries
        .get(&catalog)
        .map(|e| e.settings.clone())
}

/// Rebuild the index of `catalog`, after a pack passed its cap.
fn rebuild_after_pack(
    shared: &Arc<Shared>,
    store: &Store,
    lives: &mut HashMap<u64, Live>,
    catalog: u64,
) {
    lives.remove(&catalog);
    if let Some(settings) = settings_of(shared, catalog) {
        tracing::warn!("a pack passed the cap of the text index of {catalog:#x}: it builds again");
        rebuild(shared, store, lives, catalog, &settings);
    }
}

/// Apply the commits up to the last hand-off to each index, and commit the
/// writers that are due. An index whose cap a pack passed rebuilds first.
fn follow(shared: &Arc<Shared>, store: &Store, lives: &mut HashMap<u64, Live>) {
    let passed: Vec<u64> = lives
        .iter()
        .filter(|(_, l)| cap_lost(store, l))
        .map(|(c, _)| *c)
        .collect();
    for catalog in passed {
        rebuild_after_pack(shared, store, lives, catalog);
    }
    let target = shared.lock().target.min(store.last_transaction().0);
    let Some(from) = lives
        .values()
        .filter(|l| l.writer.is_some() && l.applied < target)
        .map(|l| l.applied)
        .min()
    else {
        commit_due(shared, store, lives);
        return;
    };
    let changes = match store.text_changed(Tid(from), Tid(target)) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!("the scan of text_txn fails: {e}");
            return;
        }
    };
    let catalogs: Vec<u64> = lives.keys().copied().collect();
    for catalog in catalogs {
        if shared.cancel.load(Ordering::Relaxed) {
            return;
        }
        let Some(live) = lives.get_mut(&catalog) else {
            continue;
        };
        if live.writer.is_none() || live.applied >= target {
            continue;
        }
        let mine: Vec<&TextChange> = changes
            .iter()
            .filter(|c| {
                c.catalog.0 == catalog
                    && c.tid.0 > live.applied
                    && live.layout.iter().any(|(name, _, _)| *name == c.index)
            })
            .collect();
        if mine.iter().any(|c| c.rid.is_none()) {
            if let Some(settings) = settings_of(shared, catalog) {
                tracing::info!("a clear delta of {catalog:#x}: its text index builds again");
                rebuild(shared, store, lives, catalog, &settings);
            }
            continue;
        }
        match apply(shared, store, live, catalog, &mine, target) {
            Ok(false) => {}
            Ok(true) => rebuild_after_pack(shared, store, lives, catalog),
            Err(e) => {
                lives.remove(&catalog);
                fail(shared, catalog, &e);
            }
        }
    }
    commit_due(shared, store, lives);
}

/// Apply `changes`, the deltas of `catalog` after the tid of `live`, up to
/// `target`, in batches of whole commits of at most 10,000 deltas. Tell if
/// a pack passed the cap of the index: then the caller rebuilds it, and
/// nothing is published.
fn apply(
    shared: &Shared,
    store: &Store,
    live: &mut Live,
    catalog: u64,
    changes: &[&TextChange],
    target: u64,
) -> Result<bool, ZoeError> {
    let mut start = 0;
    while start < changes.len() {
        // A batch ends at the end of a commit.
        let mut end = (start + COMMIT_DELTAS).min(changes.len());
        while end < changes.len() && changes[end].tid == changes[end - 1].tid {
            end += 1;
        }
        let at = changes[end - 1].tid.0;
        let rids: BTreeSet<i32> = changes[start..end].iter().filter_map(|c| c.rid).collect();
        let writer = live.writer.as_ref().expect("a writer");
        for (n, rid) in rids.into_iter().enumerate() {
            if n % 1000 == 999 && shared.cancel.load(Ordering::Relaxed) {
                return Ok(false);
            }
            let texts = texts_of(store, catalog, &live.layout, rid, at + 1)?;
            writer.delete_term(Term::from_field_i64(live.fields.rid, i64::from(rid)));
            if !texts.is_empty() {
                let doc = index::document(&live.fields, &mut live.analyzers, rid, &texts);
                writer
                    .add_document(doc)
                    .map_err(|e| ZoeError::Backend(e.to_string().into()))?;
            }
        }
        live.pending += end - start;
        live.first_pending.get_or_insert_with(Instant::now);
        live.applied = at;
        if live.pending >= COMMIT_DELTAS && commit(shared, store, live, catalog)? {
            return Ok(true);
        }
        start = end;
    }
    live.applied = target;
    if live.pending == 0 {
        // The check comes before the queries see the new tid.
        if let Some(hook) = &shared.before_check {
            hook(catalog);
        }
        if cap_lost(store, live) {
            return Ok(true);
        }
        live.committed = target;
        if let Some(cap) = &live.cap {
            cap.advance(Tid(target));
        }
        advance(shared, catalog, target);
    }
    Ok(false)
}

/// Commit the writers whose oldest delta waits 500 ms or more.
fn commit_due(shared: &Arc<Shared>, store: &Store, lives: &mut HashMap<u64, Live>) {
    let due: Vec<u64> = lives
        .iter()
        .filter(|(_, l)| l.first_pending.is_some_and(|t| t.elapsed() >= COMMIT_EVERY))
        .map(|(c, _)| *c)
        .collect();
    for catalog in due {
        let Some(live) = lives.get_mut(&catalog) else {
            continue;
        };
        match commit(shared, store, live, catalog) {
            Ok(false) => {}
            Ok(true) => rebuild_after_pack(shared, store, lives, catalog),
            Err(e) => {
                lives.remove(&catalog);
                fail(shared, catalog, &e);
            }
        }
    }
}

/// Commit the writer of `live` with the tid of its last delta, then give
/// queries the new snap. Tell if a pack passed the cap of the index: the
/// check runs before the publication, and a lost cap publishes nothing.
fn commit(shared: &Shared, store: &Store, live: &mut Live, catalog: u64) -> Result<bool, ZoeError> {
    if live.writer.is_none() {
        return Ok(false);
    }
    // The check comes before the write of the commit: a commit on disk
    // with a tid past a lost cap would survive a restart.
    if let Some(hook) = &shared.before_check {
        hook(catalog);
    }
    if cap_lost(store, live) {
        return Ok(true);
    }
    let Some(writer) = live.writer.as_mut() else {
        return Ok(false);
    };
    let payload = index::payload(live.applied, &live.layout);
    writer
        .prepare_commit()
        .and_then(|mut p| {
            p.set_payload(&payload);
            p.commit()
        })
        .map_err(|e| ZoeError::Backend(e.to_string().into()))?;
    live.committed = live.applied;
    live.pending = 0;
    live.first_pending = None;
    if let Some(cap) = &live.cap {
        cap.advance(Tid(live.committed));
    }
    let settings = settings_of(shared, catalog).unwrap_or_default();
    let _ = publish(shared, catalog, live, &settings);
    Ok(false)
}

/// Tell if a pack passed the cap of `live`: the pack watermark is above the
/// tid of its cap. Such a pack started before the cap, so it may have
/// removed an unindex that the index did not read.
fn cap_lost(store: &Store, live: &Live) -> bool {
    live.cap
        .as_ref()
        .is_some_and(|cap| store.pack_watermark().is_ok_and(|w| w > cap.tid()))
}
