//! The driver of the spike on the catalog mirror. It opens a store
//! read-only, builds the mirror of a catalog and measures it.
//!
//!     mirror show STORE OID...
//!     mirror bench STORE CATALOG_OID OUT_JSON [--threads N] [--plans FILE]
//!         [--overlays FILE] [--no-table] [--rounds N] [--commits N]

use std::collections::HashMap;
use std::time::Instant;

use serde_json::{Value as Json, json};
use zoe_catalog::base::Index;
use zoe_catalog::catalog::{self, Base};
use zoe_catalog::query::{Clause, Plan, Reply, View};
use zoe_catalog::read::{Record, Source, summary};
use zoe_catalog::value::Val;
use zoe_catalog::version::{Mirror, Owner, Slot};
use zoe_core::{Oid, Store};
use zoe_pickle::btrees::{self, TreeState};
use zoe_pickle::python::{Heap, Object, Pickler, Value};

struct StoreSource(Store);

/// The records of a file: each an oid (8 bytes), a length (4 bytes) and the
/// pickle.
struct FileSource(HashMap<u64, Vec<u8>>);

impl FileSource {
    fn read(path: &str) -> FileSource {
        FileSource(read_records_file(path).into_iter().collect())
    }
}

impl Source for FileSource {
    fn load(&self, oid: u64) -> Result<Vec<u8>, String> {
        self.0.get(&oid).cloned().ok_or_else(|| format!("no record {oid:#x}"))
    }
}

fn read_records_file(path: &str) -> Vec<(u64, Vec<u8>)> {
    let data = std::fs::read(path).expect("a records file");
    let mut out = Vec::new();
    let mut at = 0;
    while at + 12 <= data.len() {
        let oid = u64::from_be_bytes(data[at..at + 8].try_into().unwrap());
        let len = u32::from_be_bytes(data[at + 8..at + 12].try_into().unwrap()) as usize;
        out.push((oid, data[at + 12..at + 12 + len].to_vec()));
        at += 12 + len;
    }
    out
}

fn overlay_entry(mirror: &Mirror, tid: u64, entry: &Json) -> Json {
    let records: Vec<(u64, Vec<u8>)> = entry["records"]
        .as_array()
        .unwrap_or(&Vec::new())
        .iter()
        .map(|r| {
            let oid = u64::from_str_radix(r[0].as_str().unwrap_or("0"), 16).unwrap_or(0);
            let hex = r[1].as_str().unwrap_or("");
            let bytes = (0..hex.len() / 2)
                .map(|i| u8::from_str_radix(&hex[2 * i..2 * i + 2], 16).unwrap_or(0))
                .collect();
            (oid, bytes)
        })
        .collect();
    let bytes: usize = records.iter().map(|r| r.1.len()).sum();
    let t = Instant::now();
    let patch = mirror.overlay(tid, &records);
    let read_s = t.elapsed().as_secs_f64();
    let mut checks = Vec::new();
    let mut mismatches = 0;
    let mut equal = 0;
    let mut fallbacks = 0;
    match patch {
        Err(e) => checks.push(json!({"error": e})),
        Ok(patch) => {
            let base = mirror.base.clone();
            let t = Instant::now();
            let view = View::new(&base, &patch);
            let view_s = t.elapsed().as_secs_f64();
            for q in entry["plans"].as_array().cloned().unwrap_or_default() {
                if q["expected"].is_null() {
                    continue;
                }
                let plan = plan_of(&q["plan"]);
                let t = Instant::now();
                let r = if q["plan"].is_null() {
                    Err(zoe_catalog::query::Fallback("client".into()))
                } else {
                    view.query(&plan)
                };
                let s = t.elapsed().as_secs_f64();
                let check = match &r {
                    Ok(reply) => compare(reply, &q["expected"]).unwrap_or_else(|| "equal".into()),
                    Err(f) => format!("fallback: {}", f.0),
                };
                if check == "equal" {
                    equal += 1;
                } else if check.starts_with("fallback") {
                    fallbacks += 1;
                } else {
                    mismatches += 1;
                    checks.push(json!({"name": q["name"], "query": q["query"], "check": check}));
                }
                std::hint::black_box(s);
            }
            checks.push(json!({"patch_rows": patch.rows(), "view_ms": view_s * 1e3}));
        }
    }
    json!({
        "name": entry["name"],
        "records": records.len(),
        "bytes": bytes,
        "read_ms": read_s * 1e3,
        "equal": equal,
        "mismatches": mismatches,
        "fallbacks": fallbacks,
        "checks": checks,
    })
}

fn test(args: &[String]) {
    let dir = &args[0];
    let catalog_oid = u64::from_str_radix(args[1].trim_start_matches("0x"), 16).expect("an oid");
    let source = FileSource::read(&format!("{dir}/records.bin"));
    let (base, _) = catalog::build(&source, catalog_oid, 1, 4, true, None).expect("build");
    for e in &base.indexes {
        if let Index::Unavailable(r) = &e.index {
            println!("unavailable {}: {r}", e.name);
        }
    }
    let mut mirror = Mirror::new(base);
    let plans = read_lines(&format!("{dir}/plans.jsonl"));
    let base_arc = mirror.base.clone();
    let result = run_plans(&base_arc, &mirror, &plans, 0);
    println!(
        "base: checked {} mismatches {} fallbacks {}",
        result["checked"], result["mismatches"], result["fallbacks"]
    );
    for p in result["plans"].as_array().unwrap() {
        let check = p["check"].as_str().unwrap_or("");
        if check != "equal" && check != "fallback" {
            let name = &p["name"];
            let q = plans.iter().find(|x| &x["name"] == name).map(|x| x["query"].clone());
            println!("  {name}: {check} {}", q.unwrap_or(Json::Null));
        } else if check == "fallback" {
            println!("  {}: fallback {}", p["name"], p["fallback"]);
        }
    }
    for entry in read_lines(&format!("{dir}/overlays.jsonl")) {
        let r = overlay_entry(&mirror, 1, &entry);
        println!(
            "{}: {} records, equal {} mismatches {} fallbacks {}",
            r["name"], r["records"], r["equal"], r["mismatches"], r["fallbacks"]
        );
        for c in r["checks"].as_array().unwrap() {
            if c.get("check").is_some() || c.get("error").is_some() {
                println!("  {c}");
            }
        }
    }
    let commit = read_records_file(&format!("{dir}/commit.bin"));
    let (n, b, c) = mirror.apply(2, &commit);
    println!("commit: {} records, {n} read, {b} bytes, {c} changes, stale {:?}", commit.len(), mirror.stale);
    let after = read_lines(&format!("{dir}/plans-after.jsonl"));
    let head = mirror.head.clone();
    let view = View::new(&base_arc, &head);
    let (mut equal, mut bad, mut fb) = (0, 0, 0);
    for entry in &after {
        if entry["expected"].is_null() || entry["plan"].is_null() {
            fb += 1;
            continue;
        }
        match view.query(&plan_of(&entry["plan"])) {
            Ok(reply) => match compare(&reply, &entry["expected"]) {
                None => equal += 1,
                Some(c) => {
                    bad += 1;
                    println!("  {}: {c} {}", entry["name"], entry["query"]);
                }
            },
            Err(f) => {
                fb += 1;
                println!("  {}: fallback {}", entry["name"], f.0);
            }
        }
    }
    println!("after commit: equal {equal} mismatches {bad} fallbacks {fb}");
}

impl Source for StoreSource {
    fn load(&self, oid: u64) -> Result<Vec<u8>, String> {
        self.0
            .load(Oid(oid))
            .map(|(data, _)| data)
            .map_err(|e| format!("load {oid:#x}: {e}"))
    }
}

fn status(field: &str) -> u64 {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with(field))
                .and_then(|l| l.split_whitespace().nth(1)?.parse().ok())
        })
        .unwrap_or(0)
        * 1024
}

fn val(json: &Json) -> Val {
    match json {
        Json::Object(m) => {
            if let Some(i) = m.get("i") {
                Val::Int(i.as_i64().unwrap_or(0))
            } else if let Some(s) = m.get("s") {
                Val::Str(s.as_str().unwrap_or("").into())
            } else if let Some(b) = m.get("b") {
                Val::Bool(b.as_bool().unwrap_or(false))
            } else if let Some(f) = m.get("f") {
                Val::Float(f.as_f64().unwrap_or(0.0))
            } else if let Some(t) = m.get("t") {
                Val::Tuple(t.as_array().map(|a| a.iter().map(val).collect()).unwrap_or_default())
            } else {
                Val::None
            }
        }
        _ => Val::None,
    }
}

fn plan_of(json: &Json) -> Plan {
    let mut plan = Plan::default();
    for c in json["clauses"].as_array().cloned().unwrap_or_default() {
        let index = c["index"].as_str().unwrap_or("").to_owned();
        let clause = match c["kind"].as_str().unwrap_or("") {
            "value" => Clause::Value {
                index,
                keys: c["keys"].as_array().map(|a| a.iter().map(val).collect()).unwrap_or_default(),
                and: c["and"].as_bool().unwrap_or(false),
                range: c["range"]
                    .as_array()
                    .map(|a| (a[0].as_bool().unwrap_or(false), a[1].as_bool().unwrap_or(false))),
                not: c["not"].as_array().map(|a| a.iter().map(val).collect()).unwrap_or_default(),
            },
            "boolean" => Clause::Boolean {
                index,
                key: c["key"].as_bool(),
            },
            "daterange" => Clause::DateRange {
                index,
                term: c["term"].as_i64(),
            },
            "path" => Clause::Path {
                index,
                paths: c["paths"]
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .map(|p| (p[0].as_str().unwrap_or("").to_owned(), p[1].as_i64()))
                            .collect()
                    })
                    .unwrap_or_default(),
                level: c["level"].as_i64().unwrap_or(0),
                and: c["and"].as_bool().unwrap_or(false),
                depth: c["depth"].as_i64().unwrap_or(-1),
                navtree: c["navtree"].as_bool().unwrap_or(false),
                navtree_start: c["navtree_start"].as_i64().unwrap_or(0),
                index_path: c["index_path"]
                    .as_array()
                    .map(|a| a.iter().map(|p| p.as_str().unwrap_or("").to_owned()).collect())
                    .unwrap_or_default(),
            },
            other => panic!("a clause kind {other}"),
        };
        plan.clauses.push(clause);
    }
    for s in json["sort"].as_array().cloned().unwrap_or_default() {
        plan.sort.push((s[0].as_str().unwrap_or("").to_owned(), s[1].as_bool()));
    }
    plan.limit = json["limit"].as_i64();
    plan.b_start = json["b_start"].as_i64().unwrap_or(0);
    plan.b_size = json["b_size"].as_i64();
    plan
}

fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let i = ((sorted.len() as f64 - 1.0) * p).round() as usize;
    sorted[i]
}

fn stats(mut times: Vec<f64>) -> Json {
    times.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let mean = times.iter().sum::<f64>() / times.len().max(1) as f64;
    json!({
        "n": times.len(),
        "p50_ms": percentile(&times, 0.5) * 1e3,
        "p99_ms": percentile(&times, 0.99) * 1e3,
        "max_ms": times.last().copied().unwrap_or(0.0) * 1e3,
        "mean_ms": mean * 1e3,
    })
}

fn compare(reply: &Reply, expected: &Json) -> Option<String> {
    let rids: Vec<i64> = expected["rids"]
        .as_array()
        .map(|a| a.iter().filter_map(Json::as_i64).collect())
        .unwrap_or_default();
    let got: Vec<i64> = reply.rids.iter().map(|r| i64::from(*r)).collect();
    let mut problems = Vec::new();
    if got != rids {
        let first = got.iter().zip(&rids).position(|(a, b)| a != b);
        let at = first.unwrap_or(0);
        problems.push(format!(
            "rids differ: {} against {} expected, first difference at {:?}: got {:?} expected {:?}",
            got.len(),
            rids.len(),
            first,
            &got[at.min(got.len())..(at + 4).min(got.len())],
            &rids[at.min(rids.len())..(at + 4).min(rids.len())]
        ));
    }
    if Some(reply.len) != expected["len"].as_i64() {
        problems.push(format!("len {} against {}", reply.len, expected["len"]));
    }
    if Some(reply.actual) != expected["actual"].as_i64() {
        problems.push(format!("actual {} against {}", reply.actual, expected["actual"]));
    }
    (!problems.is_empty()).then(|| problems.join("; "))
}

/// Changes one value as a commit would.
fn mutate(heap: &mut Heap, value: Value, slot: Slot, boolean: bool) -> Value {
    match value {
        Value::Int(i) if boolean => Value::Int(1 - i),
        Value::Int(i) => Value::Int(i + 1),
        Value::Bool(b) => Value::Bool(!b),
        Value::Object(id) => match heap.get(id).clone() {
            Object::Str(s) => heap.new_str(&format!("{s}x")),
            Object::List(items) => {
                let mut items = items;
                let extra = heap.new_str("zzspike");
                items.push(extra);
                heap.new_list(items)
            }
            Object::Tuple(items) if slot == Slot::Data => {
                let mut items = items;
                for item in items.iter_mut() {
                    if let Some(Object::Str(s)) = heap.object(*item).cloned() {
                        *item = heap.new_str(&format!("{s}x"));
                        break;
                    }
                }
                heap.new_tuple(items)
            }
            Object::Tuple(items) if items.len() == 2 => {
                let mut items = items;
                if let Value::Int(i) = items[0] {
                    items[0] = Value::Int(i + 1);
                } else if let Value::Int(i) = items[1] {
                    items[1] = Value::Int(i + 1);
                }
                heap.new_tuple(items)
            }
            _ => value,
        },
        other => other,
    }
}

struct Synth<'a> {
    source: &'a StoreSource,
    /// For each tree, its buckets by first rid: (first, last, oid).
    buckets: HashMap<Slot, Vec<(i64, i64, u64)>>,
    booleans: Vec<bool>,
    /// The bytes of the buckets that the synthetic commits wrote.
    written: HashMap<u64, Vec<u8>>,
}

impl<'a> Synth<'a> {
    fn new(source: &'a StoreSource, mirror: &Mirror) -> Self {
        let mut buckets: HashMap<Slot, Vec<(i64, i64, u64)>> = HashMap::new();
        for (oid, (lo, hi)) in &mirror.shapes.buckets {
            if let Some(Owner::Node(slot)) = mirror.shapes.owners.get(oid) {
                buckets.entry(*slot).or_default().push((*lo, *hi, *oid));
            }
        }
        for v in buckets.values_mut() {
            v.sort_unstable();
        }
        let booleans = mirror
            .base
            .indexes
            .iter()
            .map(|e| matches!(e.index, Index::Boolean(_)))
            .collect();
        Synth {
            source,
            buckets,
            booleans,
            written: HashMap::new(),
        }
    }

    /// The record of the bucket of `slot` that holds `rid`, with the value
    /// of `rid` changed, or with `new` added after it.
    fn change(&mut self, slot: Slot, rid: i32, new: Option<i32>) -> Option<(u64, Vec<u8>)> {
        let list = self.buckets.get(&slot)?;
        let place = list.partition_point(|(lo, _, _)| *lo <= i64::from(rid));
        if place == 0 {
            return None;
        }
        let (_, _, oid) = list[place - 1];
        let data = match self.written.get(&oid) {
            Some(d) => d.clone(),
            None => self.source.load(oid).ok()?,
        };
        let record = Record::decode(&data).ok()?;
        let mut heap = record.heap;
        let mapping = true;
        let (bucket, inline) = match btrees::tree_state(&heap, record.state) {
            Some(TreeState::Inline(b)) if record.name.ends_with("BTree") => (b, true),
            _ => (record.state, false),
        };
        let state = btrees::bucket_state(&heap, bucket, mapping)?;
        let mut items = state.items.clone();
        let at = items
            .chunks(2)
            .position(|p| p[0] == Value::Int(i128::from(rid)))?;
        let boolean = matches!(slot, Slot::Index(i) if self.booleans[i]);
        let changed = mutate(&mut heap, items[2 * at + 1], slot, boolean);
        match new {
            None => items[2 * at + 1] = changed,
            Some(n) => {
                let place = items
                    .chunks(2)
                    .position(|p| matches!(p[0], Value::Int(k) if k > i128::from(n)))
                    .unwrap_or(items.len() / 2);
                items.insert(2 * place, Value::Int(i128::from(n)));
                items.insert(2 * place + 1, changed);
            }
        }
        let mut new_state = btrees::new_bucket_state(&mut heap, items, state.next);
        if inline {
            new_state = btrees::new_tree_state(&mut heap, new_state);
        }
        let mut pickler = Pickler::new(&heap);
        pickler.dump(record.class).ok()?;
        pickler.dump(new_state).ok()?;
        let bytes = pickler.into_bytes();
        Some((oid, bytes))
    }

    /// The records of a commit that changes `rids` in `slots`.
    fn commit(&mut self, slots: &[Slot], rids: &[i32], create: bool, base: &Base) -> Vec<(u64, Vec<u8>)> {
        let mut out: HashMap<u64, Vec<u8>> = HashMap::new();
        for rid in rids {
            let new = if create {
                let mut n = *rid + 1;
                while base.rows.row(n).is_some() {
                    n += 1;
                }
                Some(n)
            } else {
                None
            };
            for slot in slots {
                if let Some((oid, bytes)) = self.change(*slot, *rid, new) {
                    self.written.insert(oid, bytes.clone());
                    out.insert(oid, bytes);
                }
            }
        }
        out.into_iter().collect()
    }
}

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
}

fn run_plans(base: &Base, mirror: &Mirror, plans: &[Json], rounds: usize) -> Json {
    let mut out = Vec::new();
    let empty = mirror.head.clone();
    let view = View::new(base, &empty);
    let mut mismatches = 0;
    let mut checked = 0;
    let mut fallbacks = 0;
    for entry in plans {
        let plan = plan_of(&entry["plan"]);
        let first = if entry["plan"].is_null() {
            Err(zoe_catalog::query::Fallback(format!(
                "client: {}",
                entry["reason"].as_str().unwrap_or("")
            )))
        } else {
            view.query(&plan)
        };
        let (check, reason) = match &first {
            Ok(reply) => match entry.get("expected") {
                Some(expected) if !expected.is_null() => {
                    checked += 1;
                    let c = compare(reply, expected);
                    if c.is_some() {
                        mismatches += 1;
                    }
                    (c.unwrap_or_else(|| "equal".into()), Json::Null)
                }
                _ => ("no expected answer".into(), Json::Null),
            },
            Err(f) => {
                fallbacks += 1;
                ("fallback".into(), json!(f.0))
            }
        };
        let mut times = Vec::new();
        let mut times_rows = Vec::new();
        if first.is_ok() && rounds > 0 {
            for _ in 0..rounds {
                let t = Instant::now();
                let r = view.query(&plan);
                times.push(t.elapsed().as_secs_f64());
                std::hint::black_box(r.ok());
            }
            let mut with_rows = plan.clone();
            with_rows.rows = true;
            for _ in 0..rounds.min(50) {
                let t = Instant::now();
                let r = view.query(&with_rows);
                times_rows.push(t.elapsed().as_secs_f64());
                std::hint::black_box(r.ok());
            }
        }
        let reply = first.ok();
        out.push(json!({
            "name": entry["name"],
            "check": check,
            "fallback": reason,
            "len": reply.as_ref().map(|r| r.len),
            "actual": reply.as_ref().map(|r| r.actual),
            "timing": if times.is_empty() { Json::Null } else { stats(times) },
            "timing_with_rows": if times_rows.is_empty() { Json::Null } else { stats(times_rows) },
        }));
    }
    json!({"checked": checked, "mismatches": mismatches, "fallbacks": fallbacks, "plans": out})
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("show") => {
            let store = Store::open_read_only(&args[2]).expect("open");
            let source = StoreSource(store);
            for oid in &args[3..] {
                let oid = u64::from_str_radix(oid.trim_start_matches("0x"), 16).expect("an oid in hex");
                let data = source.load(oid).expect("load");
                match Record::decode(&data) {
                    Ok(r) => {
                        println!("{oid:#x} {}.{} ({} bytes)", r.module, r.name, data.len());
                        for (k, v) in summary(&r) {
                            println!("  {k}: {v}");
                        }
                    }
                    Err(e) => println!("{oid:#x}: {}", e.0),
                }
            }
        }
        Some("bench") => bench(&args[2..]),
        Some("test") => test(&args[2..]),
        Some("keys") => {
            let source = FileSource::read(&format!("{}/records.bin", args[2]));
            let oid = u64::from_str_radix(args[3].trim_start_matches("0x"), 16).unwrap();
            let (base, _) = catalog::build(&source, oid, 1, 4, false, None).expect("build");
            let names: Vec<&str> = args[4].split(',').collect();
            for rid in &args[5..] {
                let rid: i32 = rid.parse().unwrap();
                let row = base.rows.row(rid).unwrap();
                let vals: Vec<String> = names
                    .iter()
                    .map(|n| {
                        let e = &base.indexes[base.by_name[*n]];
                        format!("{n}={:?}", catalog::base_value(e, row))
                    })
                    .collect();
                println!("{rid}: {}", vals.join(" "));
            }
        }
        _ => eprintln!("usage: mirror show STORE OID... | mirror bench STORE CATALOG OUT [options]"),
    }
}

fn option(args: &[String], name: &str) -> Option<String> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1).cloned())
}

fn read_lines(path: &str) -> Vec<Json> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).expect("a JSON line"))
        .collect()
}

#[allow(clippy::too_many_lines)]
fn bench(args: &[String]) {
    let store_dir = &args[0];
    let catalog_oid = u64::from_str_radix(args[1].trim_start_matches("0x"), 16).expect("an oid");
    let out_path = &args[2];
    let threads: usize = option(args, "--threads")
        .and_then(|t| t.parse().ok())
        .unwrap_or_else(|| std::thread::available_parallelism().map_or(4, |n| n.get()));
    let rounds: usize = option(args, "--rounds").and_then(|t| t.parse().ok()).unwrap_or(200);
    let commits: usize = option(args, "--commits").and_then(|t| t.parse().ok()).unwrap_or(50);
    let with_table = !args.iter().any(|a| a == "--no-table");
    let mut report = serde_json::Map::new();

    let rss0 = status("VmRSS:");
    let t = Instant::now();
    let store = Store::open_read_only(store_dir).expect("open the store");
    let tid = store.last_transaction().0;
    let open_s = t.elapsed().as_secs_f64();
    let source = StoreSource(store);
    let rss_open = status("VmRSS:");
    let only: Option<Vec<String>> =
        option(args, "--indexes").map(|s| s.split(',').map(str::to_owned).collect());
    let (base, build) =
        catalog::build(&source, catalog_oid, tid, threads, with_table, only.as_deref()).expect("build");
    let rss_built = status("VmRSS:");
    eprintln!(
        "built in {:.1} s: {} records, {} MB",
        build.seconds,
        build.records,
        build.bytes / 1_000_000
    );
    report.insert(
        "build".into(),
        json!({
            "threads": threads,
            "open_seconds": open_s,
            "seconds": build.seconds,
            "records": build.records,
            "bytes": build.bytes,
            "rows": base.rows.len(),
            "parts": build.parts.iter().map(|(n, r, b, w, s)| json!({"name": n, "records": r, "bytes": b, "walk_s": w, "build_s": s})).collect::<Vec<_>>(),
            "rss_before": rss0,
            "rss_open": rss_open,
            "rss_built": rss_built,
            "rss_peak": status("VmHWM:"),
        }),
    );
    let memory = base.memory();
    let total: usize = memory.iter().map(|m| m.2).sum();
    let mut by_kind: HashMap<String, usize> = HashMap::new();
    for (_, kind, bytes) in &memory {
        *by_kind.entry(kind.clone()).or_default() += bytes;
    }
    report.insert(
        "memory".into(),
        json!({
            "total": total,
            "per_entry": total as f64 / base.rows.len().max(1) as f64,
            "by_kind": by_kind,
            "parts": memory.iter().map(|(n, k, b)| json!({"name": n, "kind": k, "bytes": b})).collect::<Vec<_>>(),
            "unavailable": base.indexes.iter().filter_map(|e| match &e.index { Index::Unavailable(r) => Some(json!({"name": e.name, "class": e.class, "reason": r})), _ => None }).collect::<Vec<_>>(),
        }),
    );
    let base_tid = base.tid;
    let mut mirror = Mirror::new(base);
    report.insert(
        "shapes_bytes".into(),
        json!(mirror.shapes.heap_bytes()),
    );
    let rss_mirror = status("VmRSS:");
    report.insert("rss_mirror".into(), json!(rss_mirror));

    // Queries at the base.
    let plans = option(args, "--plans").map(|p| read_lines(&p)).unwrap_or_default();
    if !plans.is_empty() {
        let base = mirror.base.clone();
        report.insert("queries".into(), run_plans(&base, &mirror, &plans, rounds));
    }

    // Overlays of the Python check: records of a transaction and its answers.
    if let Some(path) = option(args, "--overlays") {
        let mut results = Vec::new();
        for entry in read_lines(&path) {
            results.push(overlay_entry(&mirror, base_tid, &entry));
        }
        report.insert("overlays".into(), json!(results));
    }

    // Maintenance: synthetic commits of the shapes of B5.
    let all_slots: Vec<Slot> = {
        let mut s: Vec<Slot> = mirror
            .base
            .indexes
            .iter()
            .enumerate()
            .filter(|(_, e)| !matches!(e.index, Index::Unavailable(_)))
            .map(|(i, _)| Slot::Index(i))
            .collect();
        if with_table {
            s.push(Slot::Data);
        }
        s.push(Slot::Paths);
        s
    };
    let named = |names: &[&str]| -> Vec<Slot> {
        let mut s: Vec<Slot> = names
            .iter()
            .filter_map(|n| mirror.base.by_name.get(*n).map(|i| Slot::Index(*i)))
            .collect();
        if with_table {
            s.push(Slot::Data);
        }
        s
    };
    let edit_slots = named(&["modified", "sortable_title", "Date"]);
    let source_ref = &source;
    let base_arc = mirror.base.clone();
    let mut synth = Synth::new(source_ref, &mirror);
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    let rids = base_arc.rows.rids.clone();
    let pick = |n: usize, rng: &mut Rng| -> Vec<i32> {
        (0..n).map(|_| rids[(rng.next() % rids.len() as u64) as usize]).collect()
    };
    let mut maint = Vec::new();
    let mut next_tid = base_tid;
    let listing = plans
        .iter()
        .find(|p| p["name"] == "listing")
        .map(|p| plan_of(&p["plan"]));
    for (shape, slots, per, create) in [
        ("edit", edit_slots.clone(), 1usize, false),
        ("reindex", all_slots.clone(), 1, false),
        ("create", all_slots.clone(), 1, true),
        ("batch", all_slots.clone(), 4, false),
    ] {
        let mut times = Vec::new();
        let mut decoded = Vec::new();
        let mut bytes = Vec::new();
        let mut changes = Vec::new();
        let mut overlay_times = Vec::new();
        let mut overlay_query = Vec::new();
        for _ in 0..commits {
            let chosen = pick(per, &mut rng);
            let records = synth.commit(&slots, &chosen, create, &base_arc);
            // The overlay of the same records, before they commit.
            if let Some(plan) = &listing {
                let t = Instant::now();
                if let Ok(patch) = mirror.overlay(next_tid, &records) {
                    let view = View::new(&base_arc, &patch);
                    let a = t.elapsed().as_secs_f64();
                    let r = view.query(plan);
                    overlay_times.push(a);
                    overlay_query.push(t.elapsed().as_secs_f64());
                    std::hint::black_box(r.ok());
                }
            }
            next_tid += 1;
            let t = Instant::now();
            let (n, b, c) = mirror.apply(next_tid, &records);
            times.push(t.elapsed().as_secs_f64());
            decoded.push(n as f64);
            bytes.push(b as f64);
            changes.push(c as f64);
        }
        let mean = |v: &[f64]| v.iter().sum::<f64>() / v.len().max(1) as f64;
        maint.push(json!({
            "shape": shape,
            "commits": commits,
            "rows_per_commit": per,
            "apply": stats(times),
            "records_mean": mean(&decoded),
            "bytes_mean": mean(&bytes),
            "changes_mean": mean(&changes),
            "overlay_read": if overlay_times.is_empty() { Json::Null } else { stats(overlay_times) },
            "overlay_read_and_listing": if overlay_query.is_empty() { Json::Null } else { stats(overlay_query) },
        }));
    }
    report.insert("maintenance".into(), json!(maint));
    report.insert("stale".into(), json!(mirror.stale));

    // Versions: the deltas, the patch of an older tid, a query at the head.
    let deltas = mirror.deltas.len();
    let mut versions = serde_json::Map::new();
    versions.insert("deltas".into(), json!(deltas));
    versions.insert("deltas_bytes".into(), json!(mirror.deltas_bytes()));
    versions.insert(
        "bytes_per_delta".into(),
        json!(mirror.deltas_bytes() as f64 / deltas.max(1) as f64),
    );
    versions.insert("head_rows".into(), json!(mirror.head.rows()));
    for back in [1u64, 10, 100] {
        let t = Instant::now();
        let p = mirror.patch_at(mirror.tid.saturating_sub(back));
        versions.insert(
            format!("patch_at_minus_{back}_ms"),
            json!(t.elapsed().as_secs_f64() * 1e3),
        );
        std::hint::black_box(p);
    }
    if !plans.is_empty() {
        let base = mirror.base.clone();
        let head = mirror.head.clone();
        let view = View::new(&base, &head);
        let mut per_plan = Vec::new();
        for entry in &plans {
            let plan = plan_of(&entry["plan"]);
            if view.query(&plan).is_err() {
                continue;
            }
            let mut times = Vec::new();
            for _ in 0..rounds.min(100) {
                let t = Instant::now();
                std::hint::black_box(view.query(&plan).ok());
                times.push(t.elapsed().as_secs_f64());
            }
            per_plan.push(json!({"name": entry["name"], "timing": stats(times)}));
        }
        versions.insert("queries_at_head".into(), json!(per_plan));
    }
    report.insert("versions".into(), Json::Object(versions));
    report.insert("rss_end".into(), json!(status("VmRSS:")));
    report.insert("rss_peak".into(), json!(status("VmHWM:")));
    std::fs::write(out_path, serde_json::to_string_pretty(&Json::Object(report)).unwrap()).expect("write");
    eprintln!("wrote {out_path}");
}
