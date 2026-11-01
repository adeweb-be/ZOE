//! Acceptance test of Loupe's folder mode and object browser. Loupe opens
//! a store folder read-only and holds the lock of the store, so a running
//! instance and Loupe never share a store. It shows the root and follows
//! references. It resolves the paths of a Plone site through the `_tree`
//! of a folder and through `__annotations__`. For each object it shows the
//! class, the oid, the serial, the size, the decoded state and the
//! outbound references. It never writes to the folder.
//!
//! The end-to-end test of the window, `apps/loupe/tests/e2e/browser.spec.ts`,
//! runs `the_fixture_store_opens` with `ZOE_LOUPE_FIXTURE` set to get the
//! fixture store in a directory of its own.

mod common;

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::PathBuf;

use serde_json::json;
use zoe_core::{Oid, Store, ZoeError};
use zoe_loupe::{Folder, Object};

use common::{E2E_STORE, class_name, files, fresh_dir, open, write_store};

/// Every object that the references lead to from the root, by oid.
fn reachable(folder: &Folder) -> BTreeMap<Oid, Object> {
    let mut seen = BTreeMap::new();
    let mut queue = VecDeque::from([Oid(0)]);
    while let Some(oid) = queue.pop_front() {
        if seen.contains_key(&oid) {
            continue;
        }
        let object = folder
            .object(oid)
            .unwrap_or_else(|e| panic!("{oid:?}: {e:?}"));
        queue.extend(object.references.iter().map(|r| r.oid));
        seen.insert(oid, object);
    }
    seen
}

/// The oid and the place in the state of each outbound reference.
fn references(object: &Object) -> BTreeSet<(Oid, &str)> {
    object
        .references
        .iter()
        .map(|r| (r.oid, r.at.as_str()))
        .collect()
}

/// The object that the reference at `at` leads to.
fn follow(folder: &Folder, object: &Object, at: &str) -> Object {
    let reference = object
        .references
        .iter()
        .find(|r| r.at == at)
        .unwrap_or_else(|| panic!("no reference at {at}: {object:?}"));
    folder.object(reference.oid).unwrap()
}

#[test]
fn the_fixture_store_opens() {
    // The end-to-end test of the window keeps its store in `E2E_STORE`.
    // Another run gets a directory that the test removes at its end.
    let temp;
    let dir = match std::env::var_os(E2E_STORE) {
        Some(dir) => PathBuf::from(dir),
        None => {
            temp = fresh_dir("fixture");
            temp.to_path_buf()
        }
    };
    let fixture = write_store(&dir);
    let folder = Folder::open(&dir).unwrap();
    assert_eq!(folder.last_tid(), fixture.last_tid());
}

#[test]
fn a_folder_opens_read_only_and_holds_the_store_lock() {
    let dir = fresh_dir("lock");
    write_store(&dir);

    let folder = Folder::open(&dir).unwrap();
    // A running instance cannot open the store while Loupe has it.
    let store = Store::open(&dir);
    assert!(matches!(store, Err(ZoeError::Locked)), "{:?}", store.err());
    assert!(
        Folder::open(&dir).is_err(),
        "a second Loupe opened the folder"
    );
    drop(folder);

    // Loupe cannot open a store that a running instance has.
    let store = Store::open(&dir).unwrap();
    assert!(Folder::open(&dir).is_err(), "Loupe opened a store in use");
    drop(store);
    Folder::open(&dir).unwrap();
}

#[test]
fn a_folder_without_a_store_does_not_open_and_stays_as_it_was() {
    let dir = fresh_dir("no-store");

    let missing = dir.join("missing");
    assert!(Folder::open(&missing).is_err());
    assert!(!missing.exists(), "Loupe created a directory");

    let empty = dir.join("empty");
    std::fs::create_dir(&empty).unwrap();
    assert!(Folder::open(&empty).is_err());
    assert_eq!(std::fs::read_dir(&empty).unwrap().count(), 0);

    // Loupe never opens a Data.fs. It converts one.
    let data_fs = dir.join("Data.fs");
    std::fs::write(&data_fs, b"FS30").unwrap();
    assert!(Folder::open(&data_fs).is_err());
    assert_eq!(std::fs::read(&data_fs).unwrap(), b"FS30");
}

#[test]
fn the_root_shows_its_class_oid_serial_size_state_and_references() {
    let (_dir, fixture, folder) = open("root");
    let root = folder.root().unwrap();
    let (serial, data) = fixture.revision(Oid(0), fixture.last_tid()).unwrap();

    assert_eq!(root.oid, Oid(0));
    assert_eq!(
        root.class.as_deref(),
        Some("persistent.mapping.PersistentMapping")
    );
    assert_eq!(root.serial, serial);
    assert_eq!(root.size, data.len() as u64);
    assert_eq!(root, folder.object(Oid(0)).unwrap());

    // The root mapping holds the Zope application, and the application
    // holds the site as an attribute.
    assert_eq!(references(&root).len(), 1, "{root:?}");
    assert!(
        root.state.pointer("/data/Application").is_some(),
        "{root:?}"
    );
    let app = follow(&folder, &root, "/data/Application");
    assert_eq!(app.class.as_deref(), Some("app.content.Application"));
    let (site, _) = fixture.at_path("/plone");
    assert!(references(&app).contains(&(site, "/plone")), "{app:?}");
}

#[test]
fn references_lead_from_the_root_to_every_live_object() {
    let (_dir, fixture, folder) = open("references");
    let reached = reachable(&folder);

    // The last transaction unlinked one document. Nothing else is garbage.
    let unlinked: BTreeSet<Oid> = fixture
        .txns
        .iter()
        .filter_map(|txn| fixture.path_at("/plone/folder/doc-2", txn.tid))
        .map(|(oid, _)| oid)
        .collect();
    assert_eq!(unlinked.len(), 1);
    let live: BTreeSet<Oid> = fixture
        .created()
        .into_keys()
        .filter(|oid| !unlinked.contains(oid))
        .collect();
    assert_eq!(reached.keys().copied().collect::<BTreeSet<_>>(), live);

    for (oid, object) in &reached {
        let (serial, data) = fixture.revision(*oid, fixture.last_tid()).unwrap();
        assert_eq!(object.oid, *oid);
        assert_eq!(object.serial, serial, "{oid:?}");
        assert_eq!(object.size, data.len() as u64, "{oid:?}");
        assert_eq!(object.class, Some(class_name(data)), "{oid:?}");
        for reference in &object.references {
            assert!(
                object.state.pointer(&reference.at).is_some(),
                "{oid:?}: the reference at {} is not in the state",
                reference.at
            );
        }
    }
}

#[test]
fn plone_paths_resolve_through_tree_and_annotations() {
    let (_dir, fixture, folder) = open("paths");
    // The big folder has more items than one bucket holds, so its tree has
    // buckets of its own.
    let buckets = fixture
        .txns
        .iter()
        .flat_map(|txn| &txn.records)
        .filter(|(_, data)| class_name(data) == "BTrees.OOBTree.OOBucket")
        .count();
    assert!(buckets > 1, "{buckets} buckets");

    let paths = &fixture.txns.last().unwrap().paths;
    assert!(paths.iter().any(|(path, target)| {
        path.ends_with("/__annotations__/app.settings") && target.is_some()
    }));
    for (path, target) in paths {
        match target {
            Some((oid, class)) => {
                let object = folder
                    .resolve(path)
                    .unwrap_or_else(|e| panic!("{path}: {e:?}"));
                assert_eq!(object.oid, *oid, "{path}");
                assert_eq!(object.class.as_deref(), Some(class.as_str()), "{path}");
                assert_eq!(object, folder.object(*oid).unwrap(), "{path}");
            }
            None => assert!(folder.resolve(path).is_err(), "{path} resolved"),
        }
    }
}

#[test]
fn an_object_shows_its_decoded_state() {
    let (_dir, fixture, folder) = open("state");

    let doc = folder.resolve("/plone/folder/doc").unwrap();
    assert_eq!(doc.state["id"], json!("doc"));
    assert_eq!(doc.state["title"], json!("Doc, second"));
    assert_eq!(doc.state["text"], json!("first text"));
    // A Decimal is an opaque node in zoe-pickle. The browser still shows it.
    assert!(
        doc.state
            .get("weight")
            .is_some_and(|value| !value.is_null()),
        "{doc:?}"
    );
    let (annotations, _) = fixture.at_path("/plone/folder/doc/__annotations__");
    assert_eq!(
        references(&doc),
        BTreeSet::from([(annotations, "/__annotations__")])
    );

    let settings = folder
        .resolve("/plone/folder/doc/__annotations__/app.settings")
        .unwrap();
    assert_eq!(settings.state["data"]["flag"], json!(true));

    // A folder keeps its items in `_tree` and counts them in `_count`.
    let parent = folder.resolve("/plone/folder").unwrap();
    assert_eq!(parent.state["title"], json!("Folder, renamed"));
    let mut places: Vec<&str> = parent.references.iter().map(|r| r.at.as_str()).collect();
    places.sort();
    assert_eq!(places, ["/_count", "/_tree"]);
    let tree = follow(&folder, &parent, "/_tree");
    assert_eq!(tree.class.as_deref(), Some("BTrees.OOBTree.OOBTree"));
    let count = follow(&folder, &parent, "/_count");
    assert_eq!(count.class.as_deref(), Some("BTrees.Length.Length"));
    assert_eq!(count.state, json!(1));

    let big = folder.resolve("/plone/big").unwrap();
    assert_eq!(follow(&folder, &big, "/_count").state, json!(100));
}

#[test]
fn an_unknown_oid_is_an_error() {
    let (_dir, _, folder) = open("unknown");
    assert!(folder.object(Oid(0xdead)).is_err());
}

#[test]
fn browsing_writes_nothing() {
    let dir = fresh_dir("writes-nothing");
    let fixture = write_store(&dir);
    let before = files(&dir);

    let folder = Folder::open(&dir).unwrap();
    reachable(&folder);
    for (path, _) in &fixture.txns.last().unwrap().paths {
        let _ = folder.resolve(path);
    }
    let _ = folder.object(Oid(0xdead));
    drop(folder);

    assert!(
        files(&dir) == before,
        "browsing changed a file of the folder"
    );
}
