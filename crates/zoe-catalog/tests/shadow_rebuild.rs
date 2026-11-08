//! Acceptance test of the shadow rebuild of a mirror (ADR 0059, sections 5
//! and 6): the way back from `stale`.
//!
//! A commit of each case of section 5 (`clearIndex`, `addIndex`,
//! `delIndex`, `addColumn`) makes the mirror stale and starts a shadow
//! rebuild. A hook holds the shadow rebuild while later commits apply. Each query at each tid then gets the answer of a mirror built
//! from scratch at that tid, or `stale` at the stale commit and later when
//! it reads a changed structure. After the swap, each query at the stale
//! commit and later gets the answer of a mirror built from scratch; a query
//! before it that reads a changed structure gets `outside_window`, as the
//! new base holds that structure from the stale commit on.
//!
//! `manage_catalogClear` changes the rows: a second whole base does not fit
//! the bound of ADR 0059, section 6, so the mirror drops its base and
//! builds again from nothing. So does a shadow rebuild that fails, or whose
//! copy does not fit under the bound.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use zoe_catalog::build::build;
use zoe_catalog::query::View;
use zoe_catalog::testing::{self, CatalogModel, IndexKind};
use zoe_catalog::value::Val;
use zoe_catalog::{Clause, Columns, Config, Hooks, Mirrors, Opener, Plan, Reads, Reason, Reply};
use zoe_core::Store;
use zoe_core::vfs::MemVfs;

const WAIT: Duration = Duration::from_secs(20);

/// One test at a time: a hook that holds a build or a shadow rebuild holds
/// the one-build lock of the process, so it holds the builds of the other
/// tests too.
static SERIAL: Mutex<()> = Mutex::new(());

fn serial() -> std::sync::MutexGuard<'static, ()> {
    SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

const KINDS: [(&str, IndexKind); 4] = [
    ("portal_type", IndexKind::Field),
    ("review_state", IndexKind::Field),
    ("Subject", IndexKind::Keyword),
    ("created", IndexKind::Date),
];

const COLUMNS: [&str; 2] = ["getId", "Title"];

const TYPES: [&str; 3] = ["Document", "Folder", "News Item"];

/// The entries of a new catalog.
const ENTRIES: usize = 200;

fn store() -> Arc<Store> {
    Arc::new(Store::open_vfs(MemVfs::new()).expect("a store"))
}

fn mirrors(store: &Arc<Store>, config: Config) -> Mirrors {
    let weak = Arc::downgrade(store);
    let open: Opener = Arc::new(move || weak.upgrade().map(|s| s as Arc<dyn Reads>));
    Mirrors::start(open, store.last_transaction().0, config)
}

fn commit(
    store: &Store,
    mirrors: Option<&Mirrors>,
    records: &[(u64, Vec<u8>)],
    keys: &[[u8; 8]],
) -> u64 {
    let tid = testing::commit(store, records, keys, &[]).expect("a commit");
    if let Some(mirrors) = mirrors {
        mirrors.committed(tid);
    }
    tid
}

/// Catalogs the entry `i` as of the turn `turn`: its state and its title
/// follow the turn.
fn add(m: &mut CatalogModel, i: usize, turn: usize) {
    let state = if (i + turn).is_multiple_of(2) {
        "published"
    } else {
        "private"
    };
    let subjects: Box<[Val]> = vec![
        Val::Str(format!("tag{}", i % 4).into()),
        Val::Str(format!("tag{}", 4 + i % 3).into()),
    ]
    .into();
    m.catalog(
        1000 + i as i32,
        &format!("/site/item-{i}"),
        &[
            ("portal_type", Val::Str(TYPES[i % 3].into())),
            ("review_state", Val::Str(state.into())),
            ("Subject", Val::List(subjects)),
            ("created", Val::Int(100_000 + (i as i64 * 37) % 1000)),
            ("added", Val::Str(format!("a{}", i % 2).into())),
            ("getId", Val::Str(format!("item-{i}").into())),
            ("Title", Val::Str(format!("Item {i} of turn {turn}").into())),
            ("Description", Val::Str(format!("About {i}").into())),
        ],
    );
}

/// The commit of a turn: new entries, and other values for some entries.
fn edit(m: &mut CatalogModel, turn: usize) {
    for i in 0..5 {
        add(m, ENTRIES + turn * 10 + i, turn);
    }
    for i in (turn..ENTRIES).step_by(17) {
        add(m, i, turn);
    }
    m.uncatalog(&format!("/site/item-{}", turn * 3));
}

fn clause(index: &str, key: &str) -> Clause {
    Clause {
        index: index.into(),
        keys: vec![Val::Str(key.into())],
        and: false,
        range: None,
        not: Vec::new(),
    }
}

fn plan(clauses: Vec<Clause>, sort: Option<&str>) -> Plan {
    Plan {
        clauses,
        sort: sort
            .into_iter()
            .map(|s| (s.to_owned(), Some(false)))
            .collect(),
        ..Plan::default()
    }
}

fn rows(cols: &[&str]) -> Columns {
    Columns {
        cols: Some(cols.iter().map(|c| (*c).to_owned()).collect()),
        rn: 40,
    }
}

/// The plans of the test, by name, with the rows of their replies.
fn plans() -> Vec<(&'static str, Plan, Columns)> {
    let none = Columns::default();
    vec![
        (
            "type",
            plan(vec![clause("portal_type", "Document")], None),
            none.clone(),
        ),
        (
            "state",
            plan(vec![clause("review_state", "published")], None),
            none.clone(),
        ),
        (
            "type-by-state",
            plan(vec![clause("portal_type", "Folder")], Some("review_state")),
            none.clone(),
        ),
        (
            "subject",
            plan(vec![clause("Subject", "tag1")], None),
            none.clone(),
        ),
        (
            "type-by-created",
            plan(vec![clause("portal_type", "News Item")], Some("created")),
            none.clone(),
        ),
        (
            "added",
            plan(vec![clause("added", "a1")], None),
            none.clone(),
        ),
        (
            "type-rows",
            plan(vec![clause("portal_type", "Document")], Some("created")),
            rows(&["getId", "Title"]),
        ),
        (
            "type-description",
            plan(vec![clause("portal_type", "Folder")], None),
            rows(&["getId", "Description"]),
        ),
    ]
}

fn strip(found: Result<Reply, Reason>) -> Result<Reply, Reason> {
    found.map(|mut reply| {
        reply.trace = Default::default();
        reply
    })
}

/// The answer of a mirror built from scratch at `tid`.
fn fresh(
    store: &Store,
    catalog: u64,
    tid: u64,
    plan: &Plan,
    columns: &Columns,
) -> Result<Reply, Reason> {
    let built = build(store, catalog, tid, 2, &|_| true).expect("a build");
    let over = Default::default();
    strip(
        View::new(&built.base, &over)
            .query_rows(plan, columns)
            .map_err(|f| f.reason),
    )
}

fn answer(
    mirrors: &Mirrors,
    catalog: u64,
    tid: u64,
    plan: &Plan,
    columns: &Columns,
) -> Result<Reply, Reason> {
    strip(
        mirrors
            .query_rows(catalog, tid + 1, plan, columns)
            .map_err(|f| f.reason),
    )
}

/// A hook that holds its caller while it is on, and tells each call.
#[derive(Clone)]
struct Gate {
    on: Arc<(Mutex<bool>, Condvar)>,
    reached: Arc<Mutex<mpsc::Sender<u64>>>,
}

impl Gate {
    fn new() -> (Gate, mpsc::Receiver<u64>) {
        let (tx, rx) = mpsc::channel();
        let gate = Gate {
            on: Arc::new((Mutex::new(false), Condvar::new())),
            reached: Arc::new(Mutex::new(tx)),
        };
        (gate, rx)
    }

    fn set(&self, on: bool) {
        *self.on.0.lock().unwrap() = on;
        self.on.1.notify_all();
    }

    fn hook(&self) -> zoe_catalog::mirrors::Hook {
        let gate = self.clone();
        Arc::new(move |n| {
            let mut on = gate.on.0.lock().unwrap();
            if !*on {
                return;
            }
            let _ = gate.reached.lock().unwrap().send(n);
            while *on {
                on = gate.on.1.wait(on).unwrap();
            }
        })
    }
}

fn applied(mirrors: &Mirrors, catalog: u64, tid: u64) -> bool {
    mirrors.wait_for(catalog, WAIT, |s| s.is_some_and(|s| s.applied >= Some(tid)))
}

fn swapped(mirrors: &Mirrors, catalog: u64, swaps: u64) -> bool {
    mirrors.wait_for(catalog, WAIT, |s| {
        s.is_some_and(|s| s.state == "ready" && s.swaps >= swaps && s.stale_at.is_none())
    })
}

/// A catalog of `ENTRIES` entries in a new store, with its mirror ready.
fn prepared(config: Config) -> (Arc<Store>, Mirrors, CatalogModel, u64) {
    let store = store();
    let mut m = CatalogModel::new(10, &KINDS, &COLUMNS);
    for i in 0..ENTRIES {
        add(&mut m, i, 0);
    }
    let first = commit(&store, None, &m.records(), &[m.key()]);
    let mirrors = mirrors(&store, config);
    assert!(applied(&mirrors, m.oid(), first));
    assert!(mirrors.wait_for(m.oid(), WAIT, |s| s.is_some_and(|s| s.state == "ready")));
    (store, mirrors, m, first)
}

/// One case of section 5: the change of the stale commit, and the plans
/// that read a structure that it changes.
struct Case {
    name: &'static str,
    stale: fn(&mut CatalogModel),
    changed: &'static [&'static str],
}

/// Each query at each tid of `tids` against a mirror built from scratch.
/// `stale` is the tid of the stale commit; `during` tells if the shadow
/// rebuild runs.
fn check(
    store: &Store,
    mirrors: &Mirrors,
    m: &CatalogModel,
    tids: &[u64],
    stale: u64,
    case: &Case,
    during: bool,
) {
    for tid in tids {
        for (name, plan, columns) in plans() {
            let got = answer(mirrors, m.oid(), *tid, &plan, &columns);
            let what = format!("{}: {name} at {tid:#x}, stale at {stale:#x}", case.name);
            let changed = case.changed.contains(&name);
            if changed && during && *tid >= stale {
                assert_eq!(got.err(), Some(Reason::Stale), "{what}");
            } else if changed && !during && *tid < stale {
                assert_eq!(got.err(), Some(Reason::OutsideWindow), "{what}");
            } else {
                assert_eq!(got, fresh(store, m.oid(), *tid, &plan, &columns), "{what}");
            }
        }
    }
}

fn run(case: &Case) {
    let (gate, reached) = Gate::new();
    let config = Config {
        hooks: Hooks {
            before_build: Some(gate.hook()),
            ..Hooks::default()
        },
        ..Config::default()
    };
    let (store, mirrors, mut m, first) = prepared(config);
    let catalog = m.oid();
    let mut tids = vec![first];
    edit(&mut m, 1);
    tids.push(commit(&store, Some(&mirrors), &m.records(), &[]));
    assert!(applied(&mirrors, catalog, tids[1]));
    gate.set(true);
    (case.stale)(&mut m);
    let stale = commit(&store, Some(&mirrors), &m.records(), &[]);
    tids.push(stale);
    assert_eq!(reached.recv_timeout(WAIT), Ok(catalog), "{}", case.name);
    // Later commits apply while the shadow rebuild waits.
    for turn in 2..4 {
        edit(&mut m, turn);
        tids.push(commit(&store, Some(&mirrors), &m.records(), &[]));
    }
    assert!(applied(&mirrors, catalog, *tids.last().unwrap()));
    let status = mirrors.status(catalog).unwrap();
    assert_eq!(status.state, "building", "{}", case.name);
    assert_eq!(status.stale_at, Some(stale), "{}", case.name);
    check(&store, &mirrors, &m, &tids, stale, case, true);
    gate.set(false);
    // A commit while the shadow rebuild runs.
    edit(&mut m, 4);
    tids.push(commit(&store, Some(&mirrors), &m.records(), &[]));
    assert!(
        swapped(&mirrors, catalog, 1),
        "{}: {:?}",
        case.name,
        mirrors.status(catalog)
    );
    edit(&mut m, 5);
    tids.push(commit(&store, Some(&mirrors), &m.records(), &[]));
    assert!(applied(&mirrors, catalog, *tids.last().unwrap()));
    check(&store, &mirrors, &m, &tids, stale, case, false);
    let counts = mirrors.counts();
    if !case.changed.is_empty() {
        assert!(
            counts
                .fallbacks
                .iter()
                .any(|(r, n)| *r == Reason::Stale && *n > 0),
            "{}: {:?}",
            case.name,
            counts.fallbacks
        );
    }
    let events = mirrors.take_events();
    assert!(events.iter().any(|e| e.contains("stale")), "{events:?}");
    assert!(!events.iter().any(|e| e.contains("failed")), "{events:?}");
    mirrors.stop();
}

#[test]
fn clear_index_rebuilds_the_index_in_the_shadow_and_swaps() {
    let _serial = serial();
    run(&Case {
        name: "clearIndex",
        stale: |m| {
            m.clear_index("review_state");
            for i in 0..ENTRIES / 2 {
                add(m, i, 7);
            }
        },
        changed: &["state", "type-by-state"],
    });
}

#[test]
fn add_index_builds_the_new_index_in_the_shadow_and_swaps() {
    let _serial = serial();
    run(&Case {
        name: "addIndex",
        stale: |m| {
            m.add_index("added", IndexKind::Field);
            for i in 0..ENTRIES / 2 {
                add(m, i, 0);
            }
        },
        changed: &["added"],
    });
}

#[test]
fn del_index_drops_the_index_in_the_shadow_and_swaps() {
    let _serial = serial();
    run(&Case {
        name: "delIndex",
        stale: |m| m.del_index("Subject"),
        changed: &["subject"],
    });
}

#[test]
fn add_column_rebuilds_the_table_in_the_shadow_and_swaps() {
    let _serial = serial();
    run(&Case {
        name: "addColumn",
        stale: |m| m.add_column("Description"),
        changed: &["type-rows", "type-description"],
    });
}

#[test]
fn a_new_record_of_a_refused_index_rebuilds_it_in_the_shadow() {
    let _serial = serial();
    let store = store();
    let mut m = CatalogModel::new(10, &KINDS, &COLUMNS);
    for i in 0..ENTRIES {
        add(&mut m, i, 0);
    }
    let mut records = m.records();
    let root = m.unindex_root("review_state");
    let place = records.iter().position(|(oid, _)| *oid == root).unwrap();
    let good = std::mem::replace(&mut records[place].1, b"not a pickle".to_vec());
    let first = commit(&store, None, &records, &[m.key()]);
    let mirrors = mirrors(&store, Config::default());
    assert!(applied(&mirrors, m.oid(), first));
    let (_, state, none) = plans().into_iter().find(|(n, ..)| *n == "state").unwrap();
    assert_eq!(
        answer(&mirrors, m.oid(), first, &state, &none).err(),
        Some(Reason::IndexUnavailable)
    );
    // A commit that replaces the refused record.
    let fixed = commit(&store, Some(&mirrors), &[(root, good)], &[]);
    assert!(
        swapped(&mirrors, m.oid(), 1),
        "{:?}",
        mirrors.status(m.oid())
    );
    assert!(applied(&mirrors, m.oid(), fixed));
    let got = answer(&mirrors, m.oid(), fixed, &state, &none);
    assert!(got.is_ok(), "{got:?}");
    assert_eq!(got, fresh(&store, m.oid(), fixed, &state, &none));
    mirrors.stop();
}

/// The bytes of the structures of the index `name` of a mirror built from
/// scratch at `tid`.
fn index_bytes(store: &Store, catalog: u64, tid: u64, name: &str) -> usize {
    let built = build(store, catalog, tid, 1, &|_| true).expect("a build");
    let i = built.base.by_name[name];
    built.base.indexes[i].index.heap_bytes()
}

#[test]
fn the_second_copy_stays_within_the_bound_and_drops_the_mirror_that_answered_least_recently() {
    let _serial = serial();
    let store = store();
    let distinct = |m: &mut CatalogModel, turn: usize| {
        for i in 0..ENTRIES * 2 {
            m.catalog(
                1000 + i as i32,
                &format!("/site/item-{i}"),
                &[
                    ("portal_type", Val::Str(TYPES[i % 3].into())),
                    ("review_state", Val::Str(format!("state-{i}-{turn}").into())),
                    ("getId", Val::Str(format!("item-{i}").into())),
                ],
            );
        }
    };
    let mut a = CatalogModel::new(10, &KINDS, &COLUMNS);
    let mut b = CatalogModel::new(1 << 20, &KINDS, &COLUMNS);
    distinct(&mut a, 0);
    distinct(&mut b, 0);
    let mut records = a.records();
    records.extend(b.records());
    let first = commit(&store, None, &records, &[a.key(), b.key()]);
    // The bytes of the two mirrors, from a first start.
    let config = || Config {
        threads: 1,
        ..Config::default()
    };
    let measure = mirrors(&store, config());
    assert!(applied(&measure, a.oid(), first) && applied(&measure, b.oid(), first));
    let bytes = |m: &Mirrors, c: u64| m.status(c).unwrap().bytes;
    let (bytes_a, bytes_b) = (bytes(&measure, a.oid()), bytes(&measure, b.oid()));
    measure.stop();
    let index = index_bytes(&store, a.oid(), first, "review_state");
    // Room for both mirrors, and not for the copy of the index beside them.
    let memory = bytes_a + bytes_b + index / 2;
    let (gate, reached) = Gate::new();
    let config = Config {
        memory,
        hooks: Hooks {
            before_swap: Some(gate.hook()),
            ..Hooks::default()
        },
        ..config()
    };
    let mirrors = mirrors(&store, config);
    assert!(applied(&mirrors, a.oid(), first) && applied(&mirrors, b.oid(), first));
    let (_, type_plan, none) = plans().into_iter().next().unwrap();
    assert!(answer(&mirrors, b.oid(), first, &type_plan, &none).is_ok());
    assert!(answer(&mirrors, a.oid(), first, &type_plan, &none).is_ok());
    gate.set(true);
    a.clear_index("review_state");
    distinct(&mut a, 0);
    let stale = commit(&store, Some(&mirrors), &a.records(), &[]);
    assert_eq!(reached.recv_timeout(WAIT), Ok(a.oid()));
    // The shadow holds its copy: the bound dropped the other mirror.
    let held = bytes(&mirrors, a.oid()) + bytes(&mirrors, b.oid());
    assert!(held <= memory, "{held} > {memory}");
    assert_eq!(mirrors.status(b.oid()).unwrap().state, "dropped");
    assert_eq!(mirrors.counts().drops, 1);
    gate.set(false);
    assert!(
        swapped(&mirrors, a.oid(), 1),
        "{:?}",
        mirrors.status(a.oid())
    );
    let held = bytes(&mirrors, a.oid()) + bytes(&mirrors, b.oid());
    assert!(held <= memory, "{held} > {memory}");
    for (name, plan, columns) in plans() {
        assert_eq!(
            answer(&mirrors, a.oid(), stale, &plan, &columns),
            fresh(&store, a.oid(), stale, &plan, &columns),
            "{name}"
        );
    }
    mirrors.stop();
}

/// Each query at each tid of `tids`: the reason that `expect` gives for the
/// plan and the tid, or else the answer of a mirror built from scratch.
fn check_with(
    store: &Store,
    mirrors: &Mirrors,
    m: &CatalogModel,
    tids: &[u64],
    what: &str,
    expect: impl Fn(&str, u64) -> Option<Reason>,
) {
    for tid in tids {
        for (name, plan, columns) in plans() {
            let got = answer(mirrors, m.oid(), *tid, &plan, &columns);
            let label = format!("{what}: {name} at {tid:#x}");
            match expect(name, *tid) {
                Some(reason) => assert_eq!(got.err(), Some(reason), "{label}"),
                None => assert_eq!(got, fresh(store, m.oid(), *tid, &plan, &columns), "{label}"),
            }
        }
    }
}

/// Waits until the mirror is ready again, with no stale commit, and
/// applied `tid`.
fn ready_again(mirrors: &Mirrors, catalog: u64, tid: u64) -> bool {
    mirrors.wait_for(catalog, WAIT, |s| {
        s.is_some_and(|s| s.state == "ready" && s.stale_at.is_none() && s.applied >= Some(tid))
    })
}

/// The bytes of the mirror of a catalog of `store`, from a first start.
fn mirror_bytes(store: &Arc<Store>, catalog: u64, tid: u64) -> usize {
    let measure = mirrors(
        store,
        Config {
            threads: 1,
            ..Config::default()
        },
    );
    assert!(applied(&measure, catalog, tid));
    let bytes = measure.status(catalog).unwrap().bytes;
    measure.stop();
    bytes
}

/// A catalog whose `review_state` holds a distinct value for each entry, in
/// a new store.
fn distinct_catalog() -> (Arc<Store>, CatalogModel, u64) {
    let store = store();
    let mut m = CatalogModel::new(10, &KINDS, &COLUMNS);
    refill_distinct(&mut m);
    let first = commit(&store, None, &m.records(), &[m.key()]);
    (store, m, first)
}

fn refill_distinct(m: &mut CatalogModel) {
    for i in 0..ENTRIES * 2 {
        m.catalog(
            1000 + i as i32,
            &format!("/site/item-{i}"),
            &[
                ("portal_type", Val::Str(TYPES[i % 3].into())),
                ("review_state", Val::Str(format!("state-{i}").into())),
                ("getId", Val::Str(format!("item-{i}").into())),
            ],
        );
    }
}

#[test]
fn manage_catalog_clear_under_a_tight_bound_drops_the_base_and_builds_again() {
    let _serial = serial();
    let store = store();
    let mut m = CatalogModel::new(10, &KINDS, &COLUMNS);
    // A catalog large enough that one commit grows its mirror by little.
    for i in 0..ENTRIES * 10 {
        add(&mut m, i, 0);
    }
    let first = commit(&store, None, &m.records(), &[m.key()]);
    let catalog = m.oid();
    // Room for one base and the growth of its maps, not for a second
    // whole base.
    let memory = mirror_bytes(&store, catalog, first) * 7 / 4;
    let (gate, reached) = Gate::new();
    let config = Config {
        memory,
        threads: 1,
        hooks: Hooks {
            before_build: Some(gate.hook()),
            ..Hooks::default()
        },
        ..Config::default()
    };
    let mirrors = mirrors(&store, config);
    assert!(ready_again(&mirrors, catalog, first));
    let mut tids = vec![first];
    edit(&mut m, 1);
    tids.push(commit(&store, Some(&mirrors), &m.records(), &[]));
    assert!(
        applied(&mirrors, catalog, tids[1]),
        "{:?}",
        mirrors.status(catalog)
    );
    gate.set(true);
    let entries = m.entries();
    m.clear();
    for (_, path) in entries.iter().step_by(2) {
        let i: usize = path.trim_start_matches("/site/item-").parse().unwrap();
        add(&mut m, i, 7);
    }
    let stale = commit(&store, Some(&mirrors), &m.records(), &[]);
    tids.push(stale);
    // The build from nothing waits on the hook.
    assert_eq!(
        reached.recv_timeout(WAIT),
        Ok(catalog),
        "{:?} {:?}",
        mirrors.status(catalog),
        mirrors.take_events()
    );
    edit(&mut m, 2);
    tids.push(commit(&store, Some(&mirrors), &m.records(), &[]));
    let status = mirrors.status(catalog).unwrap();
    assert_eq!(
        (status.state, status.stale_at, status.base_tid),
        ("building", Some(stale), None)
    );
    check_with(&store, &mirrors, &m, &tids, "during the build", |_, tid| {
        Some(if tid < stale {
            Reason::NotReady
        } else {
            Reason::Stale
        })
    });
    gate.set(false);
    let last = *tids.last().unwrap();
    assert!(
        ready_again(&mirrors, catalog, last),
        "{:?}",
        mirrors.status(catalog)
    );
    let base = mirrors.status(catalog).unwrap().base_tid.unwrap();
    edit(&mut m, 3);
    tids.push(commit(&store, Some(&mirrors), &m.records(), &[]));
    assert!(
        applied(&mirrors, catalog, *tids.last().unwrap()),
        "{:?} {:?}",
        mirrors.status(catalog),
        mirrors.take_events()
    );
    check_with(&store, &mirrors, &m, &tids, "after the build", |_, tid| {
        (tid < base).then_some(Reason::OutsideWindow)
    });
    let status = mirrors.status(catalog).unwrap();
    assert!(status.bytes <= memory, "{} > {memory}", status.bytes);
    assert_eq!(status.swaps, 0);
    assert_eq!(mirrors.counts().drops, 0);
    let events = mirrors.take_events();
    assert!(
        events.iter().any(|e| e.contains("builds again")),
        "{events:?}"
    );
    mirrors.stop();
}

#[test]
fn a_failed_shadow_rebuild_drops_the_base_and_builds_again() {
    let _serial = serial();
    let (gate, reached) = Gate::new();
    let fail = Arc::new(AtomicBool::new(false));
    let hold = gate.hook();
    let armed = Arc::clone(&fail);
    let config = Config {
        hooks: Hooks {
            before_build: Some(Arc::new(move |n| {
                hold(n);
                if armed.swap(false, Ordering::SeqCst) {
                    panic!("the test fails the shadow rebuild");
                }
            })),
            ..Hooks::default()
        },
        ..Config::default()
    };
    let (store, mirrors, mut m, first) = prepared(config);
    let catalog = m.oid();
    fail.store(true, Ordering::SeqCst);
    gate.set(true);
    m.clear_index("review_state");
    let stale = commit(&store, Some(&mirrors), &m.records(), &[]);
    assert_eq!(reached.recv_timeout(WAIT), Ok(catalog));
    edit(&mut m, 1);
    let later = commit(&store, Some(&mirrors), &m.records(), &[]);
    assert!(applied(&mirrors, catalog, later));
    // The old base serves while the shadow rebuild runs.
    let changed = ["state", "type-by-state"];
    check_with(
        &store,
        &mirrors,
        &m,
        &[first, stale, later],
        "during the shadow rebuild",
        |name, tid| (changed.contains(&name) && tid >= stale).then_some(Reason::Stale),
    );
    gate.set(false);
    assert!(
        ready_again(&mirrors, catalog, later),
        "{:?}",
        mirrors.status(catalog)
    );
    let status = mirrors.status(catalog).unwrap();
    assert_eq!(status.swaps, 0);
    let base = status.base_tid.unwrap();
    edit(&mut m, 2);
    let after = commit(&store, Some(&mirrors), &m.records(), &[]);
    assert!(applied(&mirrors, catalog, after));
    check_with(
        &store,
        &mirrors,
        &m,
        &[first, stale, later, after],
        "after the build",
        |_, tid| (tid < base).then_some(Reason::OutsideWindow),
    );
    let failed: Vec<String> = mirrors
        .take_events()
        .into_iter()
        .filter(|e| e.contains("failed"))
        .collect();
    assert_eq!(failed.len(), 1, "{failed:?}");
    mirrors.stop();
}

#[test]
fn a_shadow_copy_that_does_not_fit_drops_the_base_and_builds_again() {
    let _serial = serial();
    let (store, mut m, first) = distinct_catalog();
    let catalog = m.oid();
    let bytes = mirror_bytes(&store, catalog, first);
    let index = index_bytes(&store, catalog, first, "review_state");
    // Room for the base, and not for the copy of the index beside it.
    let config = Config {
        memory: bytes + index / 2,
        threads: 1,
        ..Config::default()
    };
    let mirrors = mirrors(&store, config);
    assert!(ready_again(&mirrors, catalog, first));
    m.clear_index("review_state");
    refill_distinct(&mut m);
    let stale = commit(&store, Some(&mirrors), &m.records(), &[]);
    assert!(
        ready_again(&mirrors, catalog, stale),
        "{:?}",
        mirrors.status(catalog)
    );
    assert_eq!(mirrors.status(catalog).unwrap().swaps, 0);
    check_with(&store, &mirrors, &m, &[stale], "after the build", |_, _| {
        None
    });
    let events = mirrors.take_events();
    assert!(
        events
            .iter()
            .any(|e| e.contains("failed") && e.contains("no room")),
        "{events:?}"
    );
    mirrors.stop();
}

#[test]
fn a_second_stale_commit_during_a_shadow_rebuild_starts_it_again_for_both() {
    let _serial = serial();
    let (gate, reached) = Gate::new();
    let config = Config {
        hooks: Hooks {
            before_build: Some(gate.hook()),
            ..Hooks::default()
        },
        ..Config::default()
    };
    let (store, mirrors, mut m, first) = prepared(config);
    let catalog = m.oid();
    gate.set(true);
    m.clear_index("review_state");
    for i in 0..ENTRIES / 2 {
        add(&mut m, i, 7);
    }
    let one = commit(&store, Some(&mirrors), &m.records(), &[]);
    assert_eq!(reached.recv_timeout(WAIT), Ok(catalog));
    m.add_column("Description");
    let two = commit(&store, Some(&mirrors), &m.records(), &[]);
    assert!(applied(&mirrors, catalog, two));
    let state = ["state", "type-by-state"];
    let rows = ["type-rows", "type-description"];
    let tids = [first, one, two];
    check_with(&store, &mirrors, &m, &tids, "during", |name, tid| {
        let stale = (state.contains(&name) && tid >= one) || (rows.contains(&name) && tid >= two);
        stale.then_some(Reason::Stale)
    });
    // The first shadow rebuild goes; the next one builds both.
    gate.set(false);
    assert!(
        swapped(&mirrors, catalog, 1),
        "{:?}",
        mirrors.status(catalog)
    );
    edit(&mut m, 2);
    let after = commit(&store, Some(&mirrors), &m.records(), &[]);
    assert!(applied(&mirrors, catalog, after));
    assert_eq!(mirrors.status(catalog).unwrap().swaps, 1);
    check_with(
        &store,
        &mirrors,
        &m,
        &[first, one, two, after],
        "after the swap",
        |name, tid| {
            ((state.contains(&name) || rows.contains(&name)) && tid < two)
                .then_some(Reason::OutsideWindow)
        },
    );
    mirrors.stop();
}

#[test]
fn a_later_commit_that_the_shadow_cannot_follow_starts_it_again_there() {
    let _serial = serial();
    let (gate, reached) = Gate::new();
    let builds = Arc::new(AtomicUsize::new(0));
    let hold = gate.hook();
    let count = Arc::clone(&builds);
    let config = Config {
        hooks: Hooks {
            before_build: Some(Arc::new(move |n| {
                count.fetch_add(1, Ordering::SeqCst);
                hold(n);
            })),
            ..Hooks::default()
        },
        ..Config::default()
    };
    let (store, mirrors, mut m, first) = prepared(config);
    let catalog = m.oid();
    gate.set(true);
    m.add_index("added", IndexKind::Field);
    for i in 0..ENTRIES / 2 {
        add(&mut m, i, 0);
    }
    let one = commit(&store, Some(&mirrors), &m.records(), &[]);
    assert_eq!(reached.recv_timeout(WAIT), Ok(catalog));
    // A new root of the new index: the shadow cannot follow it.
    m.clear_index("added");
    for i in ENTRIES / 2..ENTRIES {
        add(&mut m, i, 0);
    }
    let two = commit(&store, Some(&mirrors), &m.records(), &[]);
    assert!(applied(&mirrors, catalog, two));
    gate.set(false);
    assert!(
        swapped(&mirrors, catalog, 1),
        "{:?}",
        mirrors.status(catalog)
    );
    // The first build, the shadow rebuild, and the shadow rebuild again.
    assert_eq!(builds.load(Ordering::SeqCst), 3);
    check_with(
        &store,
        &mirrors,
        &m,
        &[first, one, two],
        "after the swap",
        |name, tid| (name == "added" && tid < two).then_some(Reason::OutsideWindow),
    );
    mirrors.stop();
}

#[test]
fn the_thread_of_the_mirrors_applies_the_last_commits_to_the_shadow() {
    let _serial = serial();
    let (gate, reached) = Gate::new();
    let config = Config {
        hooks: Hooks {
            before_swap: Some(gate.hook()),
            ..Hooks::default()
        },
        ..Config::default()
    };
    let (store, mirrors, mut m, _) = prepared(config);
    let catalog = m.oid();
    gate.set(true);
    m.clear_index("review_state");
    for i in 0..ENTRIES / 2 {
        add(&mut m, i, 7);
    }
    let stale = commit(&store, Some(&mirrors), &m.records(), &[]);
    // The shadow rebuild ends its own catch-up and waits.
    assert_eq!(reached.recv_timeout(WAIT), Ok(catalog));
    for i in (0..ENTRIES).step_by(3) {
        add(&mut m, i, 9);
    }
    let last = commit(&store, Some(&mirrors), &m.records(), &[]);
    assert!(applied(&mirrors, catalog, last));
    gate.set(false);
    assert!(
        swapped(&mirrors, catalog, 1),
        "{:?}",
        mirrors.status(catalog)
    );
    check_with(
        &store,
        &mirrors,
        &m,
        &[stale, last],
        "after the swap",
        |_, _| None,
    );
    mirrors.stop();
}

#[test]
fn a_refused_record_that_each_commit_rewrites_starts_one_shadow_rebuild() {
    let _serial = serial();
    let builds = Arc::new(AtomicUsize::new(0));
    let count = Arc::clone(&builds);
    let config = Config {
        hooks: Hooks {
            before_build: Some(Arc::new(move |_| {
                count.fetch_add(1, Ordering::SeqCst);
            })),
            ..Hooks::default()
        },
        ..Config::default()
    };
    let store = store();
    let mut m = CatalogModel::new(10, &KINDS, &COLUMNS);
    for i in 0..ENTRIES {
        add(&mut m, i, 0);
    }
    let mut records = m.records();
    let root = m.unindex_root("review_state");
    let place = records.iter().position(|(oid, _)| *oid == root).unwrap();
    records[place].1 = b"not a pickle".to_vec();
    let first = commit(&store, None, &records, &[m.key()]);
    let mirrors = mirrors(&store, config);
    assert!(ready_again(&mirrors, m.oid(), first));
    // The first new record of the refused root rebuilds the index, which
    // refuses it again.
    let again = commit(
        &store,
        Some(&mirrors),
        &[(root, b"still not".to_vec())],
        &[],
    );
    assert!(
        swapped(&mirrors, m.oid(), 1),
        "{:?}",
        mirrors.status(m.oid())
    );
    assert!(ready_again(&mirrors, m.oid(), again));
    let mut last = again;
    for turn in 0..3 {
        let bytes = format!("not a pickle {turn}").into_bytes();
        last = commit(&store, Some(&mirrors), &[(root, bytes)], &[]);
    }
    assert!(ready_again(&mirrors, m.oid(), last));
    let status = mirrors.status(m.oid()).unwrap();
    assert_eq!((status.swaps, builds.load(Ordering::SeqCst)), (1, 2));
    let (_, state, none) = plans().into_iter().find(|(n, ..)| *n == "state").unwrap();
    assert_eq!(
        answer(&mirrors, m.oid(), last, &state, &none).err(),
        Some(Reason::IndexUnavailable)
    );
    mirrors.stop();
}
