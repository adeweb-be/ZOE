//! The acceptance tests of the metadata table of the catalog mirror (ADR
//! 0059, sections 2, 3 and 8).
//!
//! `tests/data/metadata.json` holds the records of a catalog with the 44
//! metadata columns of a Plone 6.1 site that Products.ZCatalog wrote, the
//! records of two commits, the cells of the brain of each rid as
//! `Catalog.instantiate` makes it at each version, with its path, and the
//! plans and the rids of a set of queries. `tests/data/metadata.py` writes
//! it.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use serde_json::Value as Json;
use zoe_catalog::build::build;
use zoe_catalog::cell::{self, EXT_DATE_TIME, EXT_MESSAGE, EXT_MISSING};
use zoe_catalog::change::{ChangeSet, Slot, read_records};
use zoe_catalog::mirror::{Versions, rebase};
use zoe_catalog::query::View;
use zoe_catalog::read::{Node, Record, node};
use zoe_catalog::testing;
use zoe_catalog::value::Val;
use zoe_catalog::{
    Columns, Config, Mirrors, Opener, Reads, Reply, Wire, decode_plan, decode_request, encode_reply,
};
use zoe_core::Store;
use zoe_core::vfs::MemVfs;

const WAIT: Duration = Duration::from_secs(20);

fn data() -> Json {
    let text = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/data/metadata.json"
    ))
    .expect("the test data");
    serde_json::from_str(&text).expect("JSON")
}

fn hex(text: &str) -> Vec<u8> {
    (0..text.len() / 2)
        .map(|i| u8::from_str_radix(&text[2 * i..2 * i + 2], 16).expect("hex"))
        .collect()
}

fn records(json: &Json) -> Vec<(u64, Vec<u8>)> {
    json.as_object()
        .expect("records")
        .iter()
        .map(|(oid, data)| {
            (
                u64::from_str_radix(oid, 16).expect("an oid"),
                hex(data.as_str().expect("hex")),
            )
        })
        .collect()
}

/// The plan as the client sends it.
fn wire(json: &Json) -> Wire {
    match json {
        Json::Null => Wire::Nil,
        Json::Bool(b) => Wire::Bool(*b),
        Json::Number(n) => match n.as_i64() {
            Some(i) => Wire::Int(i),
            None => Wire::Float(n.as_f64().expect("a number")),
        },
        Json::String(s) => Wire::Str(s.clone()),
        Json::Array(items) => Wire::Array(items.iter().map(wire).collect()),
        Json::Object(map) => Wire::Map(
            map.iter()
                .map(|(k, v)| (Wire::Str(k.clone()), wire(v)))
                .collect(),
        ),
    }
}

/// The msgpack value of a cell of the test data, with the arguments of a
/// `Message` as `opened` gives them.
fn cell_of(json: &Json) -> Wire {
    match json {
        Json::Null => Wire::Nil,
        Json::Bool(b) => Wire::Bool(*b),
        Json::Number(n) => match n.as_i64() {
            Some(i) if !n.is_f64() => Wire::Int(i),
            _ => Wire::Float(n.as_f64().expect("a number")),
        },
        Json::String(s) => Wire::Str(s.clone()),
        Json::Array(items) => Wire::Array(items.iter().map(cell_of).collect()),
        Json::Object(map) => {
            let (tag, value) = map.iter().next().expect("a tagged cell");
            match tag.as_str() {
                "bytes" => Wire::Bin(hex(value.as_str().expect("hex"))),
                "dict" => Wire::Map(
                    value
                        .as_array()
                        .expect("items")
                        .iter()
                        .map(|pair| (cell_of(&pair[0]), cell_of(&pair[1])))
                        .collect(),
                ),
                "missing" => Wire::Ext(EXT_MISSING, Vec::new()),
                "datetime" => {
                    let mut data = value[0].as_i64().expect("micros").to_be_bytes().to_vec();
                    data.push(u8::from(value[1].as_bool().expect("naive")));
                    data.extend(value[2].as_str().expect("a zone").as_bytes());
                    Wire::Ext(EXT_DATE_TIME, data)
                }
                "message" => Wire::Array(vec![
                    Wire::Str("<message>".into()),
                    Wire::Array(
                        value
                            .as_array()
                            .expect("arguments")
                            .iter()
                            .map(cell_of)
                            .collect(),
                    ),
                ]),
                other => panic!("a cell tagged {other}"),
            }
        }
    }
}

/// A decoded cell with the arguments of each `Message` decoded too, as
/// `cell_of` gives them: the ext data of a `Message` is msgpack itself.
fn opened(value: Wire) -> Wire {
    match value {
        Wire::Ext(EXT_MESSAGE, data) => {
            let args = cell::decode(&data).expect("the arguments of a Message");
            Wire::Array(vec![Wire::Str("<message>".into()), opened(args)])
        }
        Wire::Array(items) => Wire::Array(items.into_iter().map(opened).collect()),
        Wire::Map(items) => Wire::Map(
            items
                .into_iter()
                .map(|(k, v)| (opened(k), opened(v)))
                .collect(),
        ),
        other => other,
    }
}

/// The cells of a row of a reply, decoded.
fn decoded(bytes: &[u8]) -> Wire {
    opened(cell::decode(bytes).expect("a msgpack row"))
}

/// The cells of the brain of `rid` at a version, or `None` when ZCatalog
/// makes no brain from the cells alone.
fn expected_row(version: &Json, rid: i32) -> Option<Wire> {
    match &version["rows"][rid.to_string()] {
        Json::Null => None,
        row => Some(cell_of(row)),
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

fn catalog_of(data: &Json) -> u64 {
    u64::from_str_radix(data["catalog"].as_str().unwrap(), 16).unwrap()
}

fn rids(json: &Json) -> Vec<i32> {
    json.as_array()
        .expect("rids")
        .iter()
        .map(|r| r.as_i64().expect("a rid") as i32)
        .collect()
}

/// Checks the rows and the paths of `reply` against the brains of `version`.
fn same_rows(reply: &Reply, expected_rids: &[i32], rn: usize, version: &Json, what: &str) {
    assert_eq!(reply.rids, expected_rids, "{what}: the rids");
    let rows = reply.rows.as_ref().expect("rows");
    let paths = reply.paths.as_ref().expect("paths");
    let n = rn.min(expected_rids.len());
    assert_eq!(rows.len(), n, "{what}: rows for the first rn rids");
    assert_eq!(paths.len(), n, "{what}: paths for the first rn rids");
    for (k, rid) in expected_rids[..n].iter().enumerate() {
        let got = rows[k].as_deref().map(decoded);
        assert_eq!(got, expected_row(version, *rid), "{what}: the row of {rid}");
        assert_eq!(
            paths[k].as_deref(),
            version["paths"][rid.to_string()].as_str(),
            "{what}: the path of {rid}"
        );
    }
}

fn columns(plan: &Json) -> Columns {
    decode_request(&wire(plan)).expect("a plan").1
}

#[test]
fn the_cells_of_each_row_of_a_reply_equal_the_cells_of_the_brains_of_zcatalog() {
    let data = data();
    let store = store();
    let catalog = catalog_of(&data);
    let tid = testing::commit(
        &store,
        &records(&data["base"]),
        &[catalog.to_be_bytes()],
        &[],
    )
    .unwrap();
    let mirrors = mirrors(&store, Config::default());
    assert!(mirrors.wait_for(catalog, WAIT, |s| {
        s.is_some_and(|s| s.state == "ready" && s.applied >= Some(tid))
    }));
    // A plan with `cols` and `rn` reads, and its reply gives rows.
    let batch = wire(&data["plans"]["batch"]);
    assert!(decode_plan(&batch).is_ok());
    let (_, wanted) = decode_request(&batch).expect("a plan with cols");
    assert_eq!(wanted.cols.as_ref().map(Vec::len), Some(44));
    assert_eq!(wanted.rn, 15);
    let reply = mirrors
        .query_wire(catalog, tid + 1, &batch)
        .unwrap()
        .unwrap();
    assert_eq!(reply.rows.as_ref().map(Vec::len), Some(15));
    let version = &data["versions"][0];
    let mut nil = 0;
    for (name, plan) in data["plans"].as_object().unwrap() {
        let reply = mirrors
            .query_wire(catalog, tid + 1, &wire(plan))
            .expect("a plan")
            .expect("a native answer");
        let expected = rids(&data["answers"][name][0]);
        same_rows(&reply, &expected, columns(plan).rn, version, name);
        nil += reply
            .rows
            .as_ref()
            .unwrap()
            .iter()
            .filter(|r| r.is_none())
            .count();
        // The reply on the wire carries the rows and the paths.
        let Wire::Map(items) = encode_reply(tid, &Ok(reply.clone())) else {
            panic!("a map");
        };
        let get = |key: &str| {
            items
                .iter()
                .find(|(k, _)| *k == Wire::Str(key.into()))
                .map(|(_, v)| v.clone())
        };
        let Some(Wire::Array(rows)) = get("rows") else {
            panic!("{name}: rows on the wire");
        };
        for (k, row) in rows.iter().enumerate() {
            let rid = expected[k];
            assert_eq!(
                Some(opened(row.clone())).filter(|r| *r != Wire::Nil),
                expected_row(version, rid),
                "{name}: the row of {rid} on the wire"
            );
        }
        assert!(matches!(get("paths"), Some(Wire::Array(p)) if p.len() == rows.len()));
    }
    // The listing holds each rid: the rows with a cell of another type and
    // the record one value short come as nil.
    assert!(nil > 20, "{nil} nil rows");
    let listing = mirrors
        .query_wire(catalog, tid + 1, &wire(&data["plans"]["listing"]))
        .unwrap()
        .unwrap();
    let short = data["short"].as_i64().unwrap() as i32;
    let at = listing.rids.iter().position(|r| *r == short).unwrap();
    assert_eq!(listing.rows.as_ref().unwrap()[at], None);
    // A plan without columns gives no rows; a column that the schema does
    // not name gives each row as nil, with its path.
    let mut plan = data["plans"]["batch"].clone();
    plan["cols"] = Json::Null;
    let reply = mirrors
        .query_wire(catalog, tid + 1, &wire(&plan))
        .unwrap()
        .unwrap();
    assert_eq!((reply.rows, reply.paths), (None, None));
    plan["cols"] = serde_json::json!(["Title", "no_such_column"]);
    let reply = mirrors
        .query_wire(catalog, tid + 1, &wire(&plan))
        .unwrap()
        .unwrap();
    assert!(reply.rows.unwrap().iter().all(Option::is_none));
    assert!(reply.paths.unwrap().iter().all(Option::is_some));
    // A subset of the columns, in another order.
    plan["cols"] = serde_json::json!(["modified", "Title"]);
    let reply = mirrors
        .query_wire(catalog, tid + 1, &wire(&plan))
        .unwrap()
        .unwrap();
    let names: Vec<&str> = data["columns"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c.as_str().unwrap())
        .collect();
    let (modified, title) = (
        names.iter().position(|c| *c == "modified").unwrap(),
        names.iter().position(|c| *c == "Title").unwrap(),
    );
    for (k, rid) in reply.rids.iter().enumerate() {
        let got = reply.rows.as_ref().unwrap()[k].as_deref().map(decoded);
        let Some(Wire::Array(cells)) = expected_row(version, *rid) else {
            // A cell of another type in another column does not matter.
            // A record one value short still gives no row.
            if *rid == short {
                assert_eq!(got, None);
            }
            continue;
        };
        let expected = Wire::Array(vec![cells[modified].clone(), cells[title].clone()]);
        assert_eq!(
            got,
            Some(expected),
            "the columns modified and Title of {rid}"
        );
    }
    mirrors.stop();
}

#[test]
fn a_commit_that_changes_one_cell_puts_only_that_cell_in_its_change_set() {
    let data = data();
    let store = store();
    let catalog = catalog_of(&data);
    let first = testing::commit(&store, &records(&data["base"]), &[], &[]).unwrap();
    let built = build(&*store, catalog, first, 2, &|_| true).unwrap();
    let edit = &data["commits"][0];
    assert_eq!(edit["name"], "edit");
    let commit: Vec<(u64, Option<Vec<u8>>)> = records(&edit["records"])
        .into_iter()
        .map(|(oid, bytes)| (oid, Some(bytes)))
        .collect();
    let read = read_records(
        &built.base,
        &Default::default(),
        &built.shapes,
        &built.objects,
        &commit,
    );
    assert_eq!(read.stale, None);
    let title = data["columns"]
        .as_array()
        .unwrap()
        .iter()
        .position(|c| c == "Title")
        .unwrap();
    let rid = data["edited"].as_i64().unwrap() as i32;
    assert_eq!(read.changes.len(), 1, "{:?}", read.changes);
    let change = &read.changes[0];
    assert_eq!((change.slot, change.rid), (Slot::Column(title), rid));
    let cell = |v: &Option<Val>| match v {
        Some(Val::Cell(Some(bytes))) => decoded(bytes),
        other => panic!("a cell: {other:?}"),
    };
    let expected = |at: usize| {
        let Some(Wire::Array(cells)) = expected_row(&data["versions"][at], rid) else {
            panic!("a row");
        };
        cells[title].clone()
    };
    assert_eq!(cell(&change.old), expected(0));
    assert_eq!(cell(&change.new), expected(1));
    assert_eq!(cell(&change.new), Wire::Str("edited title".into()));
}

#[test]
fn a_query_at_each_version_gives_the_cells_of_that_version() {
    let data = data();
    let store = store();
    let catalog = catalog_of(&data);
    let first = testing::commit(&store, &records(&data["base"]), &[], &[]).unwrap();
    let commits: Vec<Vec<(u64, Option<Vec<u8>>)>> = data["commits"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| {
            records(&c["records"])
                .into_iter()
                .map(|(oid, bytes)| (oid, Some(bytes)))
                .collect()
        })
        .collect();
    // Bit `k` of `mask`: a rebase after the commit `k`.
    for mask in 0u32..1 << commits.len() {
        let built = build(&*store, catalog, first, 2, &|_| true).unwrap();
        let (mut shapes, objects) = (built.shapes, built.objects);
        let mut versions = Versions::new(built.base);
        for (k, records) in commits.iter().enumerate() {
            let read = read_records(&versions.base, &versions.head, &shapes, &objects, records);
            assert_eq!(read.stale, None);
            shapes.take(read.edits);
            versions.push(ChangeSet {
                tid: first + 1 + k as u64,
                at: std::time::Instant::now(),
                changes: read.changes,
                lengths: read.lengths,
            });
            if mask & (1 << k) != 0 {
                let tid = versions.applied;
                let base = rebase(&versions.base, &versions.head, tid, &|_| true).unwrap();
                versions.rebased(base);
            }
        }
        for at in 0..=commits.len() {
            let (base, over) = versions.snapshot(first + at as u64).unwrap().resolve();
            let view = View::new(&base, &over);
            for (name, plan) in data["plans"].as_object().unwrap() {
                let (typed, columns) = decode_request(&wire(plan)).unwrap();
                let reply = view.query_rows(&typed, &columns).expect("a native answer");
                same_rows(
                    &reply,
                    &rids(&data["answers"][name][at]),
                    columns.rn,
                    &data["versions"][at],
                    &format!("{name} at {at}, rebases {mask:b}"),
                );
            }
        }
    }
    // Through the mirrors: a query at the older tid gives the old cell.
    let store = self::store();
    let base_tid = testing::commit(
        &store,
        &records(&data["base"]),
        &[catalog.to_be_bytes()],
        &[],
    )
    .unwrap();
    let mirrors = mirrors(&store, Config::default());
    let mut tids = vec![base_tid];
    for c in data["commits"].as_array().unwrap() {
        let tid = testing::commit(&store, &records(&c["records"]), &[], &[]).unwrap();
        mirrors.committed(tid);
        tids.push(tid);
    }
    let last = *tids.last().unwrap();
    assert!(mirrors.wait_for(catalog, WAIT, |s| {
        s.is_some_and(|s| s.state == "ready" && s.applied >= Some(last))
    }));
    let rid = data["edited"].as_i64().unwrap() as i32;
    let plan = &data["plans"]["listing"];
    let mut seen = HashMap::new();
    for (at, tid) in tids.iter().enumerate() {
        let reply = mirrors
            .query_wire(catalog, tid + 1, &wire(plan))
            .unwrap()
            .expect("a native answer");
        same_rows(
            &reply,
            &rids(&data["answers"]["listing"][at]),
            columns(plan).rn,
            &data["versions"][at],
            &format!("listing at {at}"),
        );
        let k = reply.rids.iter().position(|r| *r == rid).unwrap();
        seen.insert(at, reply.rows.unwrap()[k].clone());
    }
    assert_ne!(seen[&0], seen[&1], "the old cell and the new cell");
    mirrors.stop();
}

#[test]
fn a_record_of_data_that_the_decoder_refuses_makes_the_catalog_refused() {
    let data = data();
    let store = store();
    let catalog = catalog_of(&data);
    let mut base = records(&data["base"]);
    let catalog_record = &base.iter().find(|(oid, _)| *oid == catalog).unwrap().1;
    let data_root = Record::decode(catalog_record)
        .unwrap()
        .attr_oid("data")
        .unwrap();
    let root = &base.iter().find(|(oid, _)| *oid == data_root).unwrap().1;
    let Node::Inner(children) = node(&Record::decode(root).unwrap()).unwrap() else {
        panic!("data has inner nodes");
    };
    for (oid, bytes) in &mut base {
        if *oid == children[1] {
            bytes.truncate(bytes.len() / 2);
        }
    }
    let tid = testing::commit(&store, &base, &[catalog.to_be_bytes()], &[]).unwrap();
    let mirrors = mirrors(&store, Config::default());
    assert!(mirrors.wait_for(catalog, WAIT, |s| s.is_some_and(|s| s.state == "refused")));
    let reply = mirrors
        .query_wire(catalog, tid + 1, &wire(&data["plans"]["batch"]))
        .unwrap();
    assert_eq!(reply.unwrap_err().reason.name(), "refused");
    // The log names the oid of the record that the decoder refused.
    let oid = format!("{:#x}", children[1]);
    let events = mirrors.take_events();
    assert!(
        events
            .iter()
            .any(|e| e.contains("data") && e.contains(&oid)),
        "{events:?}"
    );
    mirrors.stop();
}
