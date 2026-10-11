//! Acceptance test of Loupe's time travel. Loupe shows the history of an
//! object: the tid, the user and the description of each revision. It
//! shows the diff of the decoded states of two revisions, and the diff
//! compares trees, not bytes. A view "as of" a tid shows each object as it
//! was after that transaction, and never mixes revisions from two tids.
//!
//! The test reads the fixture of `browse.rs`: `fixture/records.txt` and
//! `common/mod.rs`.

mod common;

use std::collections::{BTreeMap, VecDeque};

use serde_json::{Value, json};
use zoe_core::{Oid, Store, Tid};
use zoe_loupe::{Change, Folder, Object, Snapshot};

use common::{Fixture, fresh_dir, write_store};

/// Open the fixture store in a new directory.
fn open(name: &str) -> (Fixture, Folder) {
    let dir = fresh_dir(name);
    let fixture = write_store(&dir);
    (fixture, Folder::open(&dir).unwrap())
}

/// The tid of the transaction of the fixture with the description
/// `description`.
fn tid_of(fixture: &Fixture, description: &str) -> Tid {
    fixture
        .txns
        .iter()
        .find(|txn| txn.description == description.as_bytes())
        .unwrap_or_else(|| panic!("no transaction {description:?}"))
        .tid
}

/// Every object that the references lead to from the root in a view, by
/// oid.
fn reachable(view: &Snapshot<'_>) -> BTreeMap<Oid, Object> {
    let mut seen = BTreeMap::new();
    let mut queue = VecDeque::from([Oid(0)]);
    while let Some(oid) = queue.pop_front() {
        if seen.contains_key(&oid) {
            continue;
        }
        let object = view
            .object(oid)
            .unwrap_or_else(|e| panic!("{oid:?} as of {:?}: {e:?}", view.tid()));
        queue.extend(object.references.iter().map(|r| r.oid));
        seen.insert(oid, object);
    }
    seen
}

/// A diff as (path, before, after), sorted by path.
fn changes(diff: Vec<Change>) -> Vec<(String, Option<Value>, Option<Value>)> {
    let mut changes: Vec<_> = diff
        .into_iter()
        .map(|change| (change.path, change.before, change.after))
        .collect();
    changes.sort_by(|a, b| a.0.cmp(&b.0));
    changes
}

fn changed(path: &str, before: Value, after: Value) -> (String, Option<Value>, Option<Value>) {
    (path.to_owned(), Some(before), Some(after))
}

#[test]
fn history_lists_tid_user_and_description_newest_first() {
    let (fixture, folder) = open("history");

    for path in ["/plone/folder/doc", "/plone/folder", "/plone/big"] {
        let (oid, _) = fixture.at_path(path);
        let history = folder.history(oid).unwrap();
        let mut expected = fixture.revisions(oid);
        expected.reverse();
        assert_eq!(history.len(), expected.len(), "{path}: {history:?}");
        for (revision, (tid, data)) in history.iter().zip(expected) {
            let txn = fixture.txn(tid);
            assert_eq!(revision.tid, tid, "{path}");
            assert_eq!(revision.user.as_bytes(), txn.user, "{path}");
            assert_eq!(revision.description.as_bytes(), txn.description, "{path}");
            assert_eq!(revision.size, data.len() as u64, "{path}");
        }
    }

    // The document has three revisions.
    let (doc, _) = fixture.at_path("/plone/folder/doc");
    let descriptions: Vec<String> = folder
        .history(doc)
        .unwrap()
        .into_iter()
        .map(|revision| format!("{} {}", revision.user, revision.description))
        .collect();
    assert_eq!(
        descriptions,
        [
            "editor annotate the document",
            "editor edit the document",
            "someone create the site",
        ]
    );

    // ZODB wrote the first revision of the root, with no user.
    let root = folder.history(Oid(0)).unwrap();
    let first = root.last().unwrap();
    assert_eq!(first.tid, fixture.txns[0].tid);
    assert_eq!(first.user, "");
    assert_eq!(first.description, "initial database creation");
}

#[test]
fn as_of_a_tid_shows_every_object_as_it_was_after_that_transaction() {
    let (fixture, folder) = open("as-of");
    let created = fixture.created();

    for txn in &fixture.txns {
        let at = txn.tid;
        let view = folder.as_of(at);
        assert_eq!(view.tid(), at);

        // Every object in the view is its newest revision at or before
        // the tid of the view: never a later one, never an older one.
        for (oid, object) in reachable(&view) {
            let (serial, data) = fixture
                .revision(oid, at)
                .unwrap_or_else(|| panic!("{oid:?} as of {at:?} did not exist yet"));
            assert_eq!(object.serial, serial, "{oid:?} as of {at:?}");
            assert_eq!(object.size, data.len() as u64, "{oid:?} as of {at:?}");
        }

        for (path, target) in &txn.paths {
            match target {
                Some((oid, class)) => {
                    let object = view
                        .resolve(path)
                        .unwrap_or_else(|e| panic!("{path} as of {at:?}: {e:?}"));
                    assert_eq!(object.oid, *oid, "{path} as of {at:?}");
                    assert_eq!(object.class.as_deref(), Some(class.as_str()));
                    let (serial, _) = fixture.revision(*oid, at).unwrap();
                    assert_eq!(object.serial, serial, "{path} as of {at:?}");
                }
                None => assert!(view.resolve(path).is_err(), "{path} as of {at:?}"),
            }
        }

        // An object that a later transaction creates is not in the view.
        for (&oid, &first) in &created {
            if first > at {
                assert!(view.object(oid).is_err(), "{oid:?} as of {at:?}");
            }
        }
    }
}

#[test]
fn as_of_a_tid_between_two_transactions_is_as_of_the_earlier_one() {
    let (fixture, folder) = open("between");

    for pair in fixture.txns.windows(2) {
        let (earlier, later) = (pair[0].tid, pair[1].tid);
        let between = Tid(earlier.0 + 1);
        assert!(between < later);
        let (a, b) = (folder.as_of(earlier), folder.as_of(between));
        assert_eq!(
            reachable(&a),
            reachable(&b),
            "between {earlier:?} and {later:?}"
        );
        for (path, _) in &pair[0].paths {
            assert_eq!(
                a.resolve(path).ok(),
                b.resolve(path).ok(),
                "{path} between {earlier:?} and {later:?}"
            );
        }
    }

    // Before the first transaction, the store holds no object.
    let before = folder.as_of(Tid(fixture.txns[0].tid.0 - 1));
    assert!(before.root().is_err());
}

#[test]
fn the_folder_reads_as_of_its_last_tid() {
    let (fixture, folder) = open("latest");
    let latest = folder.as_of(fixture.last_tid());
    assert_eq!(folder.root().unwrap(), latest.root().unwrap());
    for (path, _) in &fixture.txns.last().unwrap().paths {
        assert_eq!(
            folder.resolve(path).ok(),
            latest.resolve(path).ok(),
            "{path}"
        );
    }
    let (doc, _) = fixture.at_path("/plone/folder/doc");
    assert_eq!(folder.object(doc).unwrap(), latest.object(doc).unwrap());
}

#[test]
fn an_older_tid_shows_the_older_values() {
    let (fixture, folder) = open("older");
    let created = tid_of(&fixture, "create the site");
    let edited = tid_of(&fixture, "edit the document");
    let annotated = tid_of(&fixture, "annotate the document");
    let added = tid_of(&fixture, "rename the folder and add a document");
    let removed = tid_of(&fixture, "remove a document");

    let title =
        |at: Tid, path: &str| folder.as_of(at).resolve(path).unwrap().state["title"].clone();
    assert_eq!(title(created, "/plone/folder/doc"), json!("Doc, first"));
    assert_eq!(title(edited, "/plone/folder/doc"), json!("Doc, second"));
    assert_eq!(title(annotated, "/plone/folder"), json!("Folder"));
    assert_eq!(title(added, "/plone/folder"), json!("Folder, renamed"));

    let doc = folder.as_of(edited).resolve("/plone/folder/doc").unwrap();
    assert!(doc.state.get("__annotations__").is_none(), "{doc:?}");
    assert!(doc.references.is_empty(), "{doc:?}");

    // The count of the folder as of each tid comes from the same tid as
    // the folder.
    let count = |at: Tid| {
        let view = folder.as_of(at);
        let parent = view.resolve("/plone/folder").unwrap();
        let reference = parent
            .references
            .iter()
            .find(|r| r.at == "/_count")
            .unwrap();
        view.object(reference.oid).unwrap().state
    };
    assert_eq!(count(created), json!(1));
    assert_eq!(count(added), json!(2));
    assert_eq!(count(removed), json!(1));
}

#[test]
fn the_diff_compares_the_decoded_states_of_two_revisions() {
    let (fixture, folder) = open("diff");
    let created = tid_of(&fixture, "create the site");
    let edited = tid_of(&fixture, "edit the document");
    let annotated = tid_of(&fixture, "annotate the document");
    let added = tid_of(&fixture, "rename the folder and add a document");
    let removed = tid_of(&fixture, "remove a document");
    let (doc, _) = fixture.at_path("/plone/folder/doc");
    let (parent, _) = fixture.at_path("/plone/folder");

    assert_eq!(
        changes(folder.diff(doc, created, edited).unwrap()),
        [changed("/title", json!("Doc, first"), json!("Doc, second"))]
    );
    assert_eq!(
        changes(folder.diff(doc, edited, created).unwrap()),
        [changed("/title", json!("Doc, second"), json!("Doc, first"))]
    );

    // An attribute that the later revision adds has no value before.
    let annotations = changes(folder.diff(doc, edited, annotated).unwrap());
    assert_eq!(annotations.len(), 1, "{annotations:?}");
    let (path, before, after) = &annotations[0];
    assert_eq!(path, "/__annotations__");
    assert!(before.is_none() && after.is_some(), "{annotations:?}");
    let both: Vec<String> = changes(folder.diff(doc, created, annotated).unwrap())
        .into_iter()
        .map(|(path, _, _)| path)
        .collect();
    assert_eq!(both, ["/__annotations__", "/title"]);

    // The same revision, and a tid after the last revision, give no change.
    assert!(folder.diff(doc, edited, edited).unwrap().is_empty());
    assert!(
        folder
            .diff(doc, annotated, fixture.last_tid())
            .unwrap()
            .is_empty()
    );

    // The folder kept the same tree, so only its title changed. The diff
    // compares references by oid, not the objects that they lead to.
    assert_eq!(
        changes(folder.diff(parent, created, added).unwrap()),
        [changed("/title", json!("Folder"), json!("Folder, renamed"))]
    );

    // A state that is not a dict changes at the root of the state.
    let object = folder.resolve("/plone/folder").unwrap();
    let count = object
        .references
        .iter()
        .find(|r| r.at == "/_count")
        .unwrap()
        .oid;
    assert_eq!(
        changes(folder.diff(count, added, removed).unwrap()),
        [changed("", json!(2), json!(1))]
    );
}

/// A record of `app.content.Document` with a dict state that holds a
/// title and a count. The two forms write the same tree in other bytes.
fn document(form: &str, title: &str) -> Vec<u8> {
    fn text(p: &mut Vec<u8>, s: &str) {
        p.push(b'X');
        p.extend_from_slice(&(s.len() as u32).to_le_bytes());
        p.extend_from_slice(s.as_bytes());
    }
    let mut p = b"\x80\x03capp.content\nDocument\nq\x00.\x80\x03}".to_vec();
    match form {
        // Memo puts, one SETITEMS, the count as BININT1.
        "memo" => {
            p.extend_from_slice(b"q\x01(");
            text(&mut p, "title");
            p.extend_from_slice(b"q\x02");
            text(&mut p, title);
            p.extend_from_slice(b"q\x03");
            text(&mut p, "count");
            p.extend_from_slice(b"q\x04K\x07u.");
        }
        // No memo, one SETITEM per item, the keys in the other order, the
        // count as BININT.
        _ => {
            text(&mut p, "count");
            p.extend_from_slice(b"J\x07\x00\x00\x00s");
            text(&mut p, "title");
            text(&mut p, title);
            p.extend_from_slice(b"s.");
        }
    }
    p
}

#[test]
fn the_diff_ignores_how_the_bytes_encode_the_same_tree() {
    let dir = fresh_dir("same-tree");
    write_store(&dir);
    let (oid, tids) = {
        let store = Store::open(&dir).unwrap();
        let oid = store.new_oid().unwrap();
        let mut serial = Tid::ZERO;
        let mut tids = Vec::new();
        for data in [
            document("memo", "one"),
            document("plain", "one"),
            document("plain", "two"),
        ] {
            let mut txn = store.tpc_begin(b"someone", b"same tree", b"").unwrap();
            txn.store(oid, serial, &data).unwrap();
            txn.vote().unwrap();
            serial = txn.finish().unwrap();
            tids.push(serial);
        }
        (oid, tids)
    };
    assert_ne!(document("memo", "one"), document("plain", "one"));

    let folder = Folder::open(&dir).unwrap();
    let first = folder.as_of(tids[0]).object(oid).unwrap();
    let second = folder.as_of(tids[1]).object(oid).unwrap();
    assert_eq!(first.state["title"], json!("one"));
    assert_eq!(first.state["count"], json!(7));
    assert_eq!(second.state["title"], json!("one"));
    assert_eq!(second.state["count"], json!(7));

    assert!(
        folder.diff(oid, tids[0], tids[1]).unwrap().is_empty(),
        "the diff compared bytes"
    );
    assert_eq!(
        changes(folder.diff(oid, tids[1], tids[2]).unwrap()),
        [changed("/title", json!("one"), json!("two"))]
    );
    assert_eq!(
        changes(folder.diff(oid, tids[0], tids[2]).unwrap()),
        [changed("/title", json!("one"), json!("two"))]
    );
}
