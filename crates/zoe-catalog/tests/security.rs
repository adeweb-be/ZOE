//! The acceptance tests of the security index in the mirror (ADR 0061,
//! section 6).
//!
//! The tests keep a model of the objects of a site: the permission setting
//! of "Access contents information", the local roles and the block flag of
//! each object, and whether the catalog holds it. The catalog of
//! `zoe_catalog::testing` holds a `ZoeSecurityIndex`, whose inputs the
//! tests write as `zoe.plone.security` writes them: the inputs of the
//! object and the chain of its containers outside the catalog, or the value
//! of the indexer for an opaque entry. The expected rows of each token come
//! from the value of each entry as the indexer of CMFPlone computes it from
//! the objects: `rolesForPermissionOn` up the containers, and the local
//! roles up to a block.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;

use zoe_catalog::build::build;
use zoe_catalog::change::{Overrides, Slot, read_records};
use zoe_catalog::query::View;
use zoe_catalog::testing::{self, CatalogModel, IndexKind};
use zoe_catalog::value::Val;
use zoe_catalog::{Clause, Config, Mirrors, Opener, Plan, Reads, Reason};
use zoe_core::Store;
use zoe_core::vfs::MemVfs;

const WAIT: Duration = Duration::from_secs(20);

/// The rids of each token.
type Postings = BTreeMap<String, BTreeSet<i32>>;
const INDEX: &str = "allowedRolesAndUsers";
const PRIVATE: &[&str] = &[
    "Contributor",
    "Editor",
    "Manager",
    "Owner",
    "Reader",
    "Site Administrator",
];
const PUBLISHED: &[&str] = &["Anonymous"];

/// The permission setting of an object.
#[derive(Clone, Debug, PartialEq)]
enum Perm {
    Missing,
    Tuple(Vec<String>),
    List(Vec<String>),
    Anonymous,
    /// A str: the name of another permission, which the index does not
    /// read.
    Str,
}

fn strs(items: &[&str]) -> Vec<String> {
    items.iter().map(|s| (*s).to_owned()).collect()
}

#[derive(Clone, Debug)]
struct Obj {
    perm: Perm,
    local: BTreeMap<String, BTreeSet<String>>,
    block: bool,
    catalogued: bool,
    /// A local role provider other than the default adapter.
    provider: bool,
}

impl Obj {
    fn new(perm: Perm, catalogued: bool) -> Obj {
        Obj {
            perm,
            local: BTreeMap::new(),
            block: false,
            catalogued,
            provider: false,
        }
    }

    fn readable(&self) -> bool {
        self.perm != Perm::Str && !self.provider
    }
}

fn parent(path: &str) -> Option<&str> {
    if path.is_empty() {
        return None;
    }
    Some(path.rsplit_once('/').map_or("", |(a, _)| a))
}

fn s(text: &str) -> Val {
    Val::Str(text.into())
}

fn tuple(items: Vec<Val>) -> Val {
    Val::Tuple(items.into())
}

/// The model of a site: its objects, the rids of its entries, the chain
/// that each entry recorded, and the catalog.
struct World {
    objs: BTreeMap<String, Obj>,
    rids: BTreeMap<String, i32>,
    next: i32,
    model: CatalogModel,
}

impl World {
    fn new() -> World {
        let mut world = World {
            objs: BTreeMap::new(),
            rids: BTreeMap::new(),
            next: 1,
            model: CatalogModel::new(10, &[(INDEX, IndexKind::Security)], &[]),
        };
        // The Zope root, outside the catalog, with the class default.
        world.objs.insert(
            String::new(),
            Obj::new(Perm::Tuple(strs(&["Anonymous", "Manager"])), false),
        );
        world
    }

    fn add(&mut self, path: &str, perm: Perm, catalogued: bool) {
        self.objs.insert(path.to_owned(), Obj::new(perm, catalogued));
    }

    fn obj(&mut self, path: &str) -> &mut Obj {
        self.objs.get_mut(path).expect("an object of the model")
    }

    fn node_vals(obj: &Obj) -> Vec<Val> {
        let (kind, roles) = match &obj.perm {
            Perm::Missing => ("", Vec::new()),
            Perm::Tuple(r) => ("t", r.clone()),
            Perm::List(r) => ("l", r.clone()),
            Perm::Anonymous => ("a", Vec::new()),
            Perm::Str => unreachable!("an unreadable object has no inputs"),
        };
        let mut roles = roles;
        roles.sort();
        vec![
            s(kind),
            tuple(roles.iter().map(|r| s(r)).collect()),
            tuple(
                obj.local
                    .iter()
                    .filter(|(_, r)| !r.is_empty())
                    .map(|(p, r)| tuple(vec![s(p), tuple(r.iter().map(|x| s(x)).collect())]))
                    .collect(),
            ),
            Val::Bool(obj.block),
        ]
    }

    /// Tells if the object at `path` gives an opaque entry: it, or an
    /// object above it, has an input that the index does not read.
    fn opaque(&self, path: &str) -> bool {
        let mut at = Some(path);
        while let Some(p) = at {
            if !self.objs[p].readable() {
                return true;
            }
            at = parent(p);
        }
        false
    }

    /// The value of the indexer of an opaque entry.
    fn opaque_value(path: &str) -> Vec<String> {
        let last = path.rsplit('/').next().unwrap_or_default();
        vec!["Reader".to_owned(), format!("user:opaque-{last}")]
    }

    /// The inputs of the entry at `path`, as `ZoeSecurityIndex` writes
    /// them.
    fn inputs(&self, path: &str) -> Val {
        if self.opaque(path) {
            let mut value = Self::opaque_value(path);
            value.sort();
            return tuple(vec![
                s("v"),
                tuple(value.iter().map(|r| s(r)).collect()),
                tuple(Vec::new()),
                Val::Bool(false),
                tuple(Vec::new()),
            ]);
        }
        let mut items = Self::node_vals(&self.objs[path]);
        let mut chain = Vec::new();
        let mut at = parent(path);
        while let Some(p) = at {
            if self.rids.contains_key(p) {
                break;
            }
            let mut item = vec![s(p)];
            item.extend(Self::node_vals(&self.objs[p]));
            chain.push(tuple(item));
            at = parent(p);
        }
        items.push(tuple(chain));
        tuple(items)
    }

    /// Catalogs, or catalogs again, the entry at `path`.
    fn index(&mut self, path: &str) {
        let rid = match self.rids.get(path) {
            Some(rid) => *rid,
            None => {
                let rid = self.next * 7 - 3;
                self.next += 1;
                self.rids.insert(path.to_owned(), rid);
                rid
            }
        };
        let inputs = self.inputs(path);
        self.model.catalog(rid, path, &[(INDEX, inputs)]);
    }

    /// Catalogs, or catalogs again, `path` and each entry below it, parents
    /// first.
    fn index_tree(&mut self, path: &str) {
        let paths: Vec<String> = self
            .objs
            .iter()
            .filter(|(p, o)| {
                o.catalogued && (*p == path || p.starts_with(&format!("{path}/")))
            })
            .map(|(p, _)| p.clone())
            .collect();
        for p in paths {
            self.index(&p);
        }
    }

    fn uncatalog(&mut self, path: &str) {
        self.model.uncatalog(path);
        self.rids.remove(path);
    }

    /// Moves the subtree at `from` to `to`: the objects move, each entry
    /// leaves the catalog at its old path and enters it at the new one.
    fn move_tree(&mut self, from: &str, to: &str) {
        let moved: Vec<(String, Obj)> = self
            .objs
            .iter()
            .filter(|(p, _)| *p == from || p.starts_with(&format!("{from}/")))
            .map(|(p, o)| (p.clone(), o.clone()))
            .collect();
        for (path, obj) in &moved {
            if self.rids.contains_key(path) {
                self.uncatalog(path);
            }
            self.objs.remove(path);
            let new = format!("{to}{}", &path[from.len()..]);
            self.objs.insert(new, obj.clone());
        }
        self.index_tree(to);
    }

    /// The value of the indexer of CMFPlone for the object at `path`.
    fn plone_value(&self, path: &str) -> Vec<String> {
        if self.opaque(path) {
            let mut value = Self::opaque_value(path);
            value.sort();
            return value;
        }
        // rolesForPermissionOn.
        let mut r: Option<BTreeSet<String>> = None;
        let mut at = Some(path);
        let roles: BTreeSet<String> = loop {
            let Some(p) = at else {
                break r.unwrap_or_else(|| ["Manager".to_owned()].into());
            };
            match &self.objs[p].perm {
                Perm::Anonymous => break ["Anonymous".to_owned()].into(),
                Perm::Tuple(t) => {
                    let mut all = r.unwrap_or_default();
                    all.extend(t.iter().cloned());
                    break all;
                }
                Perm::List(l) if !l.is_empty() => {
                    r.get_or_insert_with(BTreeSet::new).extend(l.iter().cloned());
                }
                _ => {}
            }
            at = parent(p);
        };
        if roles.contains("Anonymous") {
            return vec!["Anonymous".to_owned()];
        }
        if roles.contains("Authenticated") {
            return vec!["Authenticated".to_owned()];
        }
        // The local roles of borg.localrole, up to a block.
        let mut local: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        let mut at = Some(path);
        while let Some(p) = at {
            let obj = &self.objs[p];
            for (principal, granted) in &obj.local {
                local
                    .entry(principal.clone())
                    .or_default()
                    .extend(granted.iter().cloned());
            }
            if obj.block {
                break;
            }
            at = parent(p);
        }
        let mut allowed = roles.clone();
        for (principal, granted) in &local {
            if granted.iter().any(|g| roles.contains(g)) {
                allowed.insert(format!("user:{principal}"));
            }
        }
        allowed.remove("Owner");
        allowed.into_iter().collect()
    }

    /// The rows of each token, as a stock KeywordIndex holds the value of
    /// the indexer of each entry.
    fn postings(&self) -> Postings {
        let mut out: Postings = BTreeMap::new();
        for (path, rid) in &self.rids {
            for token in self.plone_value(path) {
                out.entry(token).or_default().insert(*rid);
            }
        }
        out
    }

    fn records(&mut self) -> Vec<(u64, Vec<u8>)> {
        self.model.records()
    }
}

fn store() -> Arc<Store> {
    Arc::new(Store::open_vfs(MemVfs::new()).expect("a store"))
}

fn mirrors(store: &Arc<Store>, config: Config) -> Mirrors {
    let weak = Arc::downgrade(store);
    let open: Opener = Arc::new(move || weak.upgrade().map(|s| s as Arc<dyn Reads>));
    Mirrors::start(open, store.last_transaction().0, config)
}

fn commit(store: &Store, mirrors: Option<&Mirrors>, records: &[(u64, Vec<u8>)], keys: &[[u8; 8]]) -> u64 {
    let tid = testing::commit(store, records, keys, &[]).expect("a commit");
    if let Some(mirrors) = mirrors {
        mirrors.committed(tid);
    }
    tid
}

fn ready_at(mirrors: &Mirrors, catalog: u64, tid: u64) -> bool {
    mirrors.wait_for(catalog, WAIT, |s| {
        s.is_some_and(|s| s.state == "ready" && s.applied >= Some(tid))
    })
}

fn plan(keys: &[&str], and: bool) -> Plan {
    Plan {
        clauses: vec![Clause {
            index: INDEX.to_owned(),
            keys: keys.iter().map(|k| s(k)).collect(),
            and,
            range: None,
            not: Vec::new(),
        }],
        ..Plan::default()
    }
}

/// The rids of the result of a stock KeywordIndex for `keys`.
fn expected(postings: &Postings, keys: &[&str], and: bool) -> Vec<i32> {
    let sets: Vec<BTreeSet<i32>> = keys
        .iter()
        .map(|k| postings.get(*k).cloned().unwrap_or_default())
        .collect();
    let found: BTreeSet<i32> = if and {
        let mut it = sets.into_iter();
        let first = it.next().unwrap_or_default();
        it.fold(first, |a, b| a.intersection(&b).copied().collect())
    } else {
        sets.into_iter().flatten().collect()
    };
    found.into_iter().collect()
}

/// The queries of the tests: each token alone, the tokens of an anonymous
/// user, of a member and of a member of a group, and an `and`.
fn queries(postings: &Postings) -> Vec<(Vec<String>, bool)> {
    let mut out: Vec<(Vec<String>, bool)> = postings.keys().map(|k| (vec![k.clone()], false)).collect();
    out.push((strs(&["Anonymous"]), false));
    out.push((strs(&["Anonymous", "Authenticated", "Member", "user:bob"]), false));
    out.push((
        strs(&["Anonymous", "Authenticated", "Member", "user:carol", "user:editors"]),
        false,
    ));
    out.push((strs(&["Reader", "user:bob"]), true));
    out.push((strs(&["user:nobody"]), false));
    out
}

/// Checks each query of the mirror at `tid`, and of a base built at `tid`,
/// against the postings.
fn check(store: &Store, mirrors: &Mirrors, catalog: u64, tid: u64, postings: &Postings, what: &str) {
    let built = build(store, catalog, tid, 2, &|_| true).expect("a build");
    let over = Overrides::default();
    let fresh = View::new(&built.base, &over);
    for (keys, and) in queries(postings) {
        let keys: Vec<&str> = keys.iter().map(String::as_str).collect();
        let want = expected(postings, &keys, and);
        let p = plan(&keys, and);
        let got = mirrors.query(catalog, tid + 1, &p).map_err(|f| f.reason);
        let mut rids = got
            .unwrap_or_else(|r| panic!("{what}: {keys:?} refused: {}", r.name()))
            .rids;
        rids.sort_unstable();
        assert_eq!(rids, want, "{what}: the mirror, {keys:?} and={and}");
        let mut rids = fresh.query(&p).expect("a fresh answer").rids;
        rids.sort_unstable();
        assert_eq!(rids, want, "{what}: a fresh build, {keys:?} and={and}");
    }
}

fn share(world: &mut World, path: &str, principal: &str, role: &str) {
    world
        .obj(path)
        .local
        .entry(principal.to_owned())
        .or_default()
        .insert(role.to_owned());
}

/// The site of the tests: nested folders in three workflow states, users
/// and groups with local roles, an Owner local role, a block, a sequence
/// setting below the site root, a comment below its conversation, a gap,
/// and a folder of 60 entries.
fn site() -> World {
    let mut w = World::new();
    w.add(
        "/site",
        Perm::List(strs(&["Contributor", "Editor", "Reader", "Site Administrator"])),
        true,
    );
    w.add("/site/a", Perm::Tuple(strs(PUBLISHED)), true);
    w.add("/site/a/doc1", Perm::Tuple(strs(PRIVATE)), true);
    w.add("/site/a/doc2", Perm::Tuple(strs(PUBLISHED)), true);
    w.add("/site/a/doc1/++conversation++default", Perm::Missing, false);
    w.add("/site/a/doc1/++conversation++default/1", Perm::Missing, true);
    share(&mut w, "/site/a/doc1/++conversation++default", "carol", "Reader");
    w.add("/site/b", Perm::Tuple(strs(PRIVATE)), true);
    share(&mut w, "/site/b", "editors", "Editor");
    share(&mut w, "/site/b", "alice", "Owner");
    w.add("/site/b/doc", Perm::Missing, true);
    w.add("/site/b/pub", Perm::Tuple(strs(PUBLISHED)), true);
    w.add("/site/b/pending", Perm::Tuple(strs(&["Manager", "Reviewer", "Owner"])), true);
    w.add("/site/c", Perm::Tuple(strs(PRIVATE)), true);
    w.obj("/site/c").block = true;
    share(&mut w, "/site/c", "bob", "Reader");
    w.add("/site/c/d", Perm::List(strs(&["Reviewer"])), true);
    w.add("/site/c/d/x", Perm::Missing, true);
    share(&mut w, "/site/c/d/x", "dave", "Reviewer");
    w.add("/site/c/d/x/y", Perm::Tuple(strs(&["Authenticated"])), true);
    w.add("/site/open", Perm::Anonymous, true);
    w.add("/site/open/doc", Perm::List(strs(&["Reader"])), true);
    w.add("/site/h", Perm::Tuple(strs(PRIVATE)), true);
    w.add("/site/h/k", Perm::Missing, true);
    w.add("/site/big", Perm::Tuple(strs(PUBLISHED)), true);
    for i in 0..60 {
        let perm = if i % 3 == 0 {
            Perm::Tuple(strs(PRIVATE))
        } else {
            Perm::Missing
        };
        w.add(&format!("/site/big/item-{i:02}"), perm, true);
    }
    w.index_tree("/site");
    w
}

#[test]
fn the_inputs_resolve_through_the_paths_of_the_rows() {
    let store = store();
    let mut w = site();
    let first = commit(&store, None, &w.records(), &[w.model.key()]);
    let mirrors = mirrors(&store, Config::default());
    let catalog = w.model.oid();
    assert!(ready_at(&mirrors, catalog, first));
    let postings = w.postings();
    // The site root takes Anonymous from the Zope root, outside the catalog.
    assert_eq!(w.plone_value("/site"), ["Anonymous"]);
    // The comment takes the local roles of its conversation, outside the
    // catalog.
    assert!(w.plone_value("/site/a/doc1/++conversation++default/1").contains(&"user:carol".to_owned()));
    // The block of /site/c keeps the editors out; the list of /site/c/d adds
    // Reviewer.
    assert!(w.plone_value("/site/c/d/x").contains(&"user:dave".to_owned()));
    assert!(!w.plone_value("/site/c/d/x").contains(&"user:editors".to_owned()));
    assert!(w.plone_value("/site/b/doc").contains(&"user:alice".to_owned()));
    check(&store, &mirrors, catalog, first, &postings, "the base");
    mirrors.stop();
}

#[test]
fn a_share_writes_one_row_of_inputs_and_no_resolved_value() {
    let store = store();
    let mut w = site();
    let catalog = w.model.oid();
    let first = commit(&store, None, &w.records(), &[w.model.key()]);
    let built = build(&*store, catalog, first, 2, &|_| true).expect("a build");
    share(&mut w, "/site/big", "bob", "Reader");
    w.index("/site/big");
    let records = w.records();
    let read = read_records(
        &built.base,
        &Overrides::default(),
        &built.shapes,
        &built.objects,
        &records.iter().map(|(o, d)| (*o, Some(d.clone()))).collect::<Vec<_>>(),
    );
    assert!(read.stale.is_none(), "{:?}", read.stale);
    let place = built.base.by_name[INDEX];
    let changes: Vec<_> = read
        .changes
        .iter()
        .filter(|c| c.slot == Slot::Index(place))
        .collect();
    assert_eq!(changes.len(), 1, "one row of inputs");
    assert_eq!(changes[0].rid, w.rids["/site/big"]);
    assert_eq!(changes[0].new, Some(w.inputs("/site/big")));
}

#[test]
fn changes_of_security_resolve_at_each_version_and_older_tids_stay_right() {
    let store = store();
    let mut w = site();
    let catalog = w.model.oid();
    let first = commit(&store, None, &w.records(), &[w.model.key()]);
    let config = Config {
        rebase_rows: 1_000_000,
        ..Config::default()
    };
    let mirrors = mirrors(&store, config);
    assert!(ready_at(&mirrors, catalog, first));
    let mut states = vec![(first, w.postings())];
    let step = |w: &mut World, mirrors: &Mirrors, states: &mut Vec<(u64, Postings)>, what: &str| {
        let tid = commit(&store, Some(mirrors), &w.records(), &[]);
        assert!(ready_at(mirrors, catalog, tid), "{what}");
        states.push((tid, w.postings()));
        let (tid, postings) = states.last().unwrap();
        check(&store, mirrors, catalog, *tid, postings, what);
    };
    // A share on a folder: the folder alone writes.
    share(&mut w, "/site/b", "bob", "Reader");
    w.index("/site/b");
    step(&mut w, &mirrors, &mut states, "a share");
    // A transition of the folder.
    w.obj("/site/b").perm = Perm::Tuple(strs(PUBLISHED));
    w.index("/site/b");
    step(&mut w, &mirrors, &mut states, "a transition");
    w.obj("/site/b").perm = Perm::Tuple(strs(PRIVATE));
    w.index("/site/b");
    step(&mut w, &mirrors, &mut states, "a transition back");
    // A block of the inheritance, and its end.
    w.obj("/site/b").block = true;
    w.index("/site/b");
    step(&mut w, &mirrors, &mut states, "a block");
    w.obj("/site/c").block = false;
    w.index("/site/c");
    step(&mut w, &mirrors, &mut states, "the end of a block");
    // A move of a subtree: each entry leaves and enters again.
    w.move_tree("/site/b", "/site/c/moved");
    step(&mut w, &mirrors, &mut states, "a move of a subtree");
    // A new entry below the folder and one below the conversation.
    w.add("/site/c/moved/new", Perm::Missing, true);
    w.index("/site/c/moved/new");
    w.add("/site/a/doc1/++conversation++default/2", Perm::Missing, true);
    w.index("/site/a/doc1/++conversation++default/2");
    step(&mut w, &mirrors, &mut states, "new entries");
    // The deletion of the local roles of a user on the whole site: each
    // object whose local roles change gets a reindex.
    let held: Vec<String> = w
        .objs
        .iter()
        .filter(|(p, o)| o.catalogued && o.local.contains_key("bob") && w.rids.contains_key(*p))
        .map(|(p, _)| p.clone())
        .collect();
    for path in &held {
        w.obj(path).local.remove("bob");
        w.index(path);
    }
    step(&mut w, &mirrors, &mut states, "the deletion of the local roles of a user");
    // A container that leaves the catalog: the entry below gives no token.
    w.uncatalog("/site/h");
    w.obj("/site/h").catalogued = false;
    let tid = commit(&store, Some(&mirrors), &w.records(), &[]);
    assert!(ready_at(&mirrors, catalog, tid));
    let mut postings = w.postings();
    let lost = w.rids["/site/h/k"];
    for rows in postings.values_mut() {
        rows.remove(&lost);
    }
    check(&store, &mirrors, catalog, tid, &postings, "a gap");
    states.push((tid, postings));
    // Each older tid gives its own result over the overrides.
    for (tid, postings) in &states {
        check(&store, &mirrors, catalog, *tid, postings, "an older tid");
    }
    mirrors.stop();
}

#[test]
fn an_opaque_entry_closes_its_subtree_and_reopens_it() {
    let store = store();
    let mut w = site();
    let catalog = w.model.oid();
    let first = commit(&store, None, &w.records(), &[w.model.key()]);
    let mirrors = mirrors(&store, Config::default());
    assert!(ready_at(&mirrors, catalog, first));
    // A setting that is a str on a folder: the folder and each entry below
    // it are opaque, from the security reindex of the folder.
    w.obj("/site/c").perm = Perm::Str;
    w.index_tree("/site/c");
    let tid = commit(&store, Some(&mirrors), &w.records(), &[]);
    assert!(ready_at(&mirrors, catalog, tid));
    assert!(w.opaque("/site/c/d/x/y"));
    check(&store, &mirrors, catalog, tid, &w.postings(), "a str setting");
    // A local role provider other than the default adapter on another
    // folder.
    w.obj("/site/a").provider = true;
    w.index_tree("/site/a");
    let tid = commit(&store, Some(&mirrors), &w.records(), &[]);
    assert!(ready_at(&mirrors, catalog, tid));
    check(&store, &mirrors, catalog, tid, &w.postings(), "a provider");
    // The setting is a tuple again: the security reindex reaches the
    // opaque entries below.
    w.obj("/site/c").perm = Perm::Tuple(strs(PRIVATE));
    w.index_tree("/site/c");
    let tid = commit(&store, Some(&mirrors), &w.records(), &[]);
    assert!(ready_at(&mirrors, catalog, tid));
    assert!(!w.opaque("/site/c/d/x/y"));
    check(&store, &mirrors, catalog, tid, &w.postings(), "a tuple again");
    mirrors.stop();
}

#[test]
fn a_share_on_a_large_folder_passes_rebase_rows_and_the_rebase_stays_right() {
    let store = store();
    let mut w = site();
    let catalog = w.model.oid();
    let first = commit(&store, None, &w.records(), &[w.model.key()]);
    let config = Config {
        rebase_rows: 40,
        ..Config::default()
    };
    let mirrors = mirrors(&store, config);
    assert!(ready_at(&mirrors, catalog, first));
    let before = w.postings();
    // One row of inputs, and 61 rows below it to resolve.
    share(&mut w, "/site/big", "bob", "Reader");
    w.obj("/site/big").perm = Perm::Tuple(strs(PRIVATE));
    w.index("/site/big");
    let tid = commit(&store, Some(&mirrors), &w.records(), &[]);
    assert!(ready_at(&mirrors, catalog, tid));
    assert!(
        mirrors.wait_for(catalog, WAIT, |s| s.is_some_and(|s| s.rebases >= 1)),
        "a rebase"
    );
    let after = w.postings();
    assert_ne!(before, after);
    check(&store, &mirrors, catalog, tid, &after, "after the rebase");
    check(&store, &mirrors, catalog, first, &before, "before the rebase");
    // A change after the rebase.
    w.obj("/site/big").local.remove("bob");
    w.index("/site/big");
    let tid = commit(&store, Some(&mirrors), &w.records(), &[]);
    assert!(ready_at(&mirrors, catalog, tid));
    check(&store, &mirrors, catalog, tid, &w.postings(), "a change after the rebase");
    mirrors.stop();
}

#[test]
fn a_range_or_not_on_the_security_index_takes_the_python_path() {
    let store = store();
    let mut w = site();
    let catalog = w.model.oid();
    let first = commit(&store, None, &w.records(), &[w.model.key()]);
    let mirrors = mirrors(&store, Config::default());
    assert!(ready_at(&mirrors, catalog, first));
    let mut not = plan(&["Anonymous"], false);
    not.clauses[0].not = vec![s("Reader")];
    let mut range = plan(&["A", "Z"], false);
    range.clauses[0].range = Some((Some(s("A")), Some(s("Z"))));
    let mut other = plan(&[], false);
    other.clauses[0].keys = vec![Val::Int(1)];
    for p in [not, range, other] {
        let got = mirrors.query(catalog, first + 1, &p).map_err(|f| f.reason);
        assert_eq!(got.err(), Some(Reason::Option));
    }
    mirrors.stop();
}
