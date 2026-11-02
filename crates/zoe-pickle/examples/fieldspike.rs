//! The prototype of the spike on field-addressable objects. It is not for
//! merge.
//!
//! `fieldspike fields IN OUT [FIELDS]` reads groups of records and writes,
//! for each record, its class, its bytes, the bytes of the slots of the
//! fields `FIELDS`, and for each field of a dict state the canonical id of
//! its value and the bytes of its pickle. The ids of one group compare: the
//! records of a group share one heap.
//!
//! `fieldspike bench DOCS BUCKETS FIELDS ROUNDS` times the extraction of the
//! fields `FIELDS` (names separated by commas) from the content records of
//! DOCS into typed-field slots, the read of one value from those slots, the
//! decode of each index bucket of BUCKETS, and the state that a pickle and
//! its slots make.
//!
//! `fieldspike decode IN ROUNDS` times the decode of every record of IN
//! into a heap that models `DateTime`, `Message` and `Missing.Value`.
//!
//! An input file holds groups. A group is a `u32` length and a JSON header,
//! a `u32` count, then each record as a `u32` length and its bytes. The
//! numbers are little-endian.

use std::fs::File;
use std::hint::black_box;
use std::io::{BufReader, BufWriter, Read, Write};
use std::time::Instant;

use zoe_pickle::python::{Canon, Heap, Instance, Made, Object, Pickler, Value};
use zoe_pickle::{Limits, btrees, decode_record_with};

struct Group {
    header: String,
    records: Vec<Vec<u8>>,
}

fn read_u32(input: &mut impl Read) -> Option<u32> {
    let mut buf = [0u8; 4];
    match input.read_exact(&mut buf) {
        Ok(()) => Some(u32::from_le_bytes(buf)),
        Err(_) => None,
    }
}

fn read_bytes(input: &mut impl Read, len: u32) -> Vec<u8> {
    let mut buf = vec![0u8; len as usize];
    input.read_exact(&mut buf).expect("a truncated input");
    buf
}

fn read_groups(path: &str) -> Vec<Group> {
    let mut input = BufReader::new(File::open(path).expect("open the input"));
    let mut groups = Vec::new();
    while let Some(len) = read_u32(&mut input) {
        let header = String::from_utf8(read_bytes(&mut input, len)).expect("a JSON header");
        let count = read_u32(&mut input).expect("a count");
        let records = (0..count)
            .map(|_| {
                let len = read_u32(&mut input).expect("a record length");
                read_bytes(&mut input, len)
            })
            .collect();
        groups.push(Group { header, records });
    }
    groups
}

fn json_str(text: &str) -> String {
    let mut out = String::from("\"");
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn limits(record: &[u8]) -> Limits {
    Limits {
        node_budget: record.len().max(64),
    }
}

/// The bytes of the pickle of `value` alone: no PROTO and no STOP.
fn value_bytes(heap: &Heap, value: Value) -> Option<usize> {
    let mut pickler = Pickler::new(heap);
    pickler.dump(value).ok()?;
    Some(pickler.into_bytes().len() - 3)
}

fn class_name(heap: &Heap, class: Value) -> String {
    // The class pickle of a ZODB record is the class, or `(class, None)`.
    let class = match heap.tuple(class) {
        Some([first, ..]) => *first,
        _ => class,
    };
    match heap.class(class) {
        Some((module, name)) => format!("{module}.{name}"),
        None => "?".to_owned(),
    }
}

fn fields(input: &str, output: &str, declared: &[&str]) {
    let mut out = BufWriter::new(File::create(output).expect("create the output"));
    for group in read_groups(input) {
        let mut heap = Heap::with_instances();
        let mut canon = Canon::new(1 << 30);
        let mut parts = Vec::new();
        for record in &group.records {
            if record.is_empty() {
                parts.push("{\"bytes\":0,\"error\":\"empty\"}".to_owned());
                continue;
            }
            let loaded = decode_record_with(record, limits(record))
                .map_err(|_| "decode")
                .and_then(|tree| heap.load(&tree).map_err(|e| e.0));
            let (class, state) = match loaded {
                Ok(pair) => pair,
                Err(reason) => {
                    parts.push(format!(
                        "{{\"bytes\":{},\"error\":{}}}",
                        record.len(),
                        json_str(reason)
                    ));
                    continue;
                }
            };
            let name = class_name(&heap, class);
            let Some(items) = heap.dict(state).map(<[_]>::to_vec) else {
                parts.push(format!(
                    "{{\"bytes\":{},\"class\":{},\"error\":\"state\"}}",
                    record.len(),
                    json_str(&name)
                ));
                continue;
            };
            let mut field_parts = Vec::new();
            for (key, value) in items {
                let Some(key) = heap.str(key).map(str::to_owned) else {
                    continue;
                };
                let id = canon.id(&heap, value).map_or(-1i64, i64::from);
                let size = value_bytes(&heap, value).map_or(-1i64, |n| n as i64);
                field_parts.push(format!("{}:[{id},{size}]", json_str(&key)));
            }
            let slots = if declared.is_empty() {
                0
            } else {
                extract(record, declared).map_or(0, |s| s.len())
            };
            parts.push(format!(
                "{{\"bytes\":{},\"class\":{},\"slots\":{slots},\"fields\":{{{}}}}}",
                record.len(),
                json_str(&name),
                field_parts.join(",")
            ));
        }
        writeln!(
            out,
            "{{\"h\":{},\"records\":[{}]}}",
            group.header,
            parts.join(",")
        )
        .expect("write");
    }
}

// The typed-field slots of the prototype: a `u16` count, then for each
// field its id (`u16`), its type (`u8`), the length of its value (`u32`)
// and the value.

const NONE: u8 = 0;
const BOOL: u8 = 1;
const INT: u8 = 2;
const FLOAT: u8 = 3;
const STR: u8 = 4;
const BYTES: u8 = 5;
const DATETIME: u8 = 6;
const STRS: u8 = 7;
const PICKLE: u8 = 8;

fn put(out: &mut Vec<u8>, id: u16, kind: u8, value: &[u8]) {
    out.extend(id.to_le_bytes());
    out.push(kind);
    out.extend((value.len() as u32).to_le_bytes());
    out.extend(value);
}

fn datetime_state(heap: &Heap, value: Value) -> Option<(i64, bool, &str)> {
    let Instance { class, state, .. } = heap.instance(value)?;
    if heap.class(*class)? != ("DateTime.DateTime", "DateTime") {
        return None;
    }
    match *heap.tuple((*state)?)? {
        [Value::Int(micros), Value::Bool(naive), Value::None] => Some((micros as i64, naive, "")),
        [Value::Int(micros), Value::Bool(naive), tz] => Some((micros as i64, naive, heap.str(tz)?)),
        _ => None,
    }
}

fn encode_value(heap: &Heap, id: u16, value: Value, out: &mut Vec<u8>) {
    match value {
        Value::None => put(out, id, NONE, &[]),
        Value::Bool(b) => put(out, id, BOOL, &[u8::from(b)]),
        Value::Int(i) => put(out, id, INT, &(i as i64).to_le_bytes()),
        Value::Float(f) => put(out, id, FLOAT, &f.to_le_bytes()),
        Value::Object(object) => match heap.get(object) {
            Object::Str(s) => put(out, id, STR, s.as_bytes()),
            Object::Bytes(b) => put(out, id, BYTES, b),
            Object::Tuple(items) | Object::List(items)
                if items.iter().all(|&item| heap.str(item).is_some()) =>
            {
                let mut buf = Vec::new();
                buf.extend((items.len() as u16).to_le_bytes());
                for &item in items {
                    let s = heap.str(item).unwrap();
                    buf.extend((s.len() as u32).to_le_bytes());
                    buf.extend(s.as_bytes());
                }
                put(out, id, STRS, &buf);
            }
            _ => {
                if let Some((micros, naive, tz)) = datetime_state(heap, value) {
                    let mut buf = Vec::with_capacity(9 + tz.len());
                    buf.extend(micros.to_le_bytes());
                    buf.push(u8::from(naive));
                    buf.extend(tz.as_bytes());
                    put(out, id, DATETIME, &buf);
                } else {
                    let mut pickler = Pickler::new(heap);
                    match pickler.dump(value) {
                        Ok(()) => put(out, id, PICKLE, &pickler.into_bytes()),
                        Err(_) => put(out, id, NONE, &[]),
                    }
                }
            }
        },
    }
}

/// Extracts the declared fields of a record into slots: the decode, the
/// load into a heap, and the encode of each declared value.
fn extract(record: &[u8], declared: &[&str]) -> Option<Vec<u8>> {
    let tree = decode_record_with(record, limits(record)).ok()?;
    let mut heap = Heap::with_instances();
    let (_, state) = heap.load(&tree).ok()?;
    let items = heap.dict(state)?;
    let mut out = Vec::with_capacity(256);
    out.extend(0u16.to_le_bytes());
    let mut count = 0u16;
    for &(key, value) in items {
        let Some(name) = heap.str(key) else { continue };
        if let Some(id) = declared.iter().position(|d| *d == name) {
            encode_value(&heap, id as u16, value, &mut out);
            count += 1;
        }
    }
    out[..2].copy_from_slice(&count.to_le_bytes());
    Some(out)
}

#[derive(Debug)]
#[allow(dead_code)]
enum SlotValue {
    None,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(String),
    Bytes(Vec<u8>),
    DateTime(i64, bool, String),
    Strs(Vec<String>),
    Pickle(Vec<u8>),
}

fn u16_at(b: &[u8], at: usize) -> usize {
    usize::from(u16::from_le_bytes([b[at], b[at + 1]]))
}

fn u32_at(b: &[u8], at: usize) -> usize {
    u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]]) as usize
}

/// Reads the value of field `id` from slots, or `None` when the slots do
/// not hold it.
fn read_slot(slots: &[u8], id: u16) -> Option<SlotValue> {
    let count = u16_at(slots, 0);
    let mut at = 2;
    for _ in 0..count {
        let field = u16_at(slots, at) as u16;
        let kind = slots[at + 2];
        let len = u32_at(slots, at + 3);
        let value = &slots[at + 7..at + 7 + len];
        at += 7 + len;
        if field != id {
            continue;
        }
        return Some(match kind {
            NONE => SlotValue::None,
            BOOL => SlotValue::Bool(value[0] != 0),
            INT => SlotValue::Int(i64::from_le_bytes(value.try_into().ok()?)),
            FLOAT => SlotValue::Float(f64::from_le_bytes(value.try_into().ok()?)),
            STR => SlotValue::Str(std::str::from_utf8(value).ok()?.to_owned()),
            BYTES => SlotValue::Bytes(value.to_vec()),
            DATETIME => SlotValue::DateTime(
                i64::from_le_bytes(value[..8].try_into().ok()?),
                value[8] != 0,
                std::str::from_utf8(&value[9..]).ok()?.to_owned(),
            ),
            STRS => {
                let n = u16_at(value, 0);
                let mut items = Vec::with_capacity(n);
                let mut p = 2;
                for _ in 0..n {
                    let l = u32_at(value, p);
                    items.push(
                        std::str::from_utf8(&value[p + 4..p + 4 + l])
                            .ok()?
                            .to_owned(),
                    );
                    p += 4 + l;
                }
                SlotValue::Strs(items)
            }
            _ => SlotValue::Pickle(value.to_vec()),
        });
    }
    None
}

/// The heap value of a slot value, for the state of a load of shape (B).
fn heap_value(heap: &mut Heap, value: &SlotValue) -> Value {
    match value {
        SlotValue::None | SlotValue::Pickle(_) => Value::None,
        SlotValue::Bool(b) => Value::Bool(*b),
        SlotValue::Int(i) => Value::Int(i128::from(*i)),
        SlotValue::Float(f) => Value::Float(*f),
        SlotValue::Str(s) => heap.new_str(s),
        SlotValue::Bytes(b) => heap.new_bytes(b),
        SlotValue::Strs(items) => {
            let items = items.iter().map(|s| heap.new_str(s)).collect();
            heap.new_tuple(items)
        }
        SlotValue::DateTime(micros, naive, tz) => {
            let class = heap.new_class("DateTime.DateTime", "DateTime");
            let args = heap.new_tuple(Vec::new());
            let tz = if tz.is_empty() {
                Value::None
            } else {
                heap.new_str(tz)
            };
            let state = heap.new_tuple(vec![
                Value::Int(i128::from(*micros)),
                Value::Bool(*naive),
                tz,
            ]);
            heap.new_instance(Instance {
                made: Made::New,
                class,
                args,
                state: Some(state),
            })
        }
    }
}

/// Makes the record of a load of shape (B): the pickle with the values of
/// the slots in place of its declared fields.
fn compose(record: &[u8], slots: &[u8], declared: &[&str]) -> Option<Vec<u8>> {
    let tree = decode_record_with(record, limits(record)).ok()?;
    let mut heap = Heap::with_instances();
    let (class, state) = heap.load(&tree).ok()?;
    let items = heap.dict(state)?.to_vec();
    let mut new_items = Vec::with_capacity(items.len());
    for (key, value) in items {
        let id = heap
            .str(key)
            .and_then(|name| declared.iter().position(|d| *d == name));
        let value = match id.and_then(|id| read_slot(slots, id as u16)) {
            Some(SlotValue::Pickle(_)) | None => value,
            Some(slot) => heap_value(&mut heap, &slot),
        };
        new_items.push((key, value));
    }
    let new_state = heap.new_dict(new_items);
    let mut pickler = Pickler::new(&heap);
    pickler.dump(class).ok()?;
    pickler.dump(new_state).ok()?;
    Some(pickler.into_bytes())
}

/// Decodes a record into a heap and pickles it again: the work of
/// `compose` without the slots.
fn round_trip(record: &[u8]) -> Option<Vec<u8>> {
    let tree = decode_record_with(record, limits(record)).ok()?;
    let mut heap = Heap::with_instances();
    let (class, state) = heap.load(&tree).ok()?;
    let mut pickler = Pickler::new(&heap);
    pickler.dump(class).ok()?;
    pickler.dump(state).ok()?;
    Some(pickler.into_bytes())
}

/// Decodes a bucket record and returns its number of items.
fn decode_bucket(record: &[u8]) -> Option<usize> {
    let tree = decode_record_with(record, limits(record)).ok()?;
    let mut heap = Heap::with_values();
    let (_, state) = heap.load(&tree).ok()?;
    let state = btrees::bucket_state(&heap, state, true)?;
    Some(state.items.len() / 2)
}

/// The median of `rounds` timings of `f` over all `items`, in ns per item.
fn time_per_item<T>(rounds: usize, items: &[T], mut f: impl FnMut(&T)) -> f64 {
    let mut samples = Vec::with_capacity(rounds);
    for _ in 0..rounds {
        let start = Instant::now();
        for item in items {
            f(item);
        }
        samples.push(start.elapsed().as_nanos() as f64 / items.len() as f64);
    }
    samples.sort_by(|a, b| a.partial_cmp(b).unwrap());
    samples[samples.len() / 2]
}

fn bench(docs: &str, buckets: &str, declared: &str, rounds: usize) {
    let declared: Vec<&str> = declared.split(',').collect();
    let docs: Vec<Vec<u8>> = read_groups(docs)
        .into_iter()
        .flat_map(|g| g.records)
        .collect();
    let slots: Vec<Vec<u8>> = docs
        .iter()
        .map(|r| extract(r, &declared).expect("a record that extracts"))
        .collect();
    let doc_bytes: usize = docs.iter().map(Vec::len).sum();
    let slot_bytes: usize = slots.iter().map(Vec::len).sum();
    println!(
        "{{\"docs\":{},\"doc_bytes_mean\":{:.1},\"slot_bytes_mean\":{:.1}}}",
        docs.len(),
        doc_bytes as f64 / docs.len() as f64,
        slot_bytes as f64 / docs.len() as f64
    );
    let decode_only = time_per_item(rounds, &docs, |r| {
        black_box(decode_record_with(r, limits(r)).ok());
    });
    let load_only = time_per_item(rounds, &docs, |r| {
        let tree = decode_record_with(r, limits(r)).unwrap();
        let mut heap = Heap::with_instances();
        black_box(heap.load(&tree).ok());
    });
    let extract_ns = time_per_item(rounds, &docs, |r| {
        black_box(extract(r, &declared));
    });
    println!(
        "{{\"op\":\"extract_from_pickle\",\"decode_ns\":{decode_only:.0},\"decode_load_ns\":{load_only:.0},\"extract_ns\":{extract_ns:.0}}}"
    );
    for (id, name) in declared.iter().enumerate() {
        let present: Vec<&Vec<u8>> = slots
            .iter()
            .filter(|s| read_slot(s, id as u16).is_some())
            .collect();
        if present.is_empty() {
            continue;
        }
        let ns = time_per_item(rounds * 10, &present, |s| {
            black_box(read_slot(s, id as u16));
        });
        println!(
            "{{\"op\":\"read_slot\",\"field\":{},\"ns\":{ns:.1}}}",
            json_str(name)
        );
    }
    let groups = read_groups(buckets);
    let mut labels: Vec<String> = groups.iter().map(|g| g.header.clone()).collect();
    labels.sort();
    labels.dedup();
    for label in labels {
        let records: Vec<&Vec<u8>> = groups
            .iter()
            .filter(|g| g.header == label)
            .flat_map(|g| g.records.iter())
            .collect();
        let decoded: Vec<usize> = records.iter().filter_map(|r| decode_bucket(r)).collect();
        if decoded.is_empty() {
            println!("{{\"op\":\"decode_bucket\",\"label\":{label},\"error\":\"none decoded\"}}");
            continue;
        }
        let ok: Vec<&Vec<u8>> = records
            .iter()
            .copied()
            .filter(|r| decode_bucket(r).is_some())
            .collect();
        let items: usize = decoded.iter().sum();
        let bytes: usize = ok.iter().map(|r| r.len()).sum();
        let ns = time_per_item(rounds, &ok, |r| {
            black_box(decode_bucket(r));
        });
        println!(
            "{{\"op\":\"decode_bucket\",\"label\":{label},\"buckets\":{},\"refused\":{},\"items_mean\":{:.1},\"bytes_mean\":{:.0},\"ns_per_bucket\":{ns:.0},\"ns_per_item\":{:.1}}}",
            ok.len(),
            records.len() - ok.len(),
            items as f64 / ok.len() as f64,
            bytes as f64 / ok.len() as f64,
            ns * ok.len() as f64 / items as f64
        );
    }
    let pairs: Vec<(Vec<u8>, Vec<u8>)> = docs.iter().cloned().zip(slots.iter().cloned()).collect();
    let mut same = 0;
    for (r, s) in &pairs {
        if compose(r, s, &declared).as_deref() == round_trip(r).as_deref() {
            same += 1;
        }
    }
    let rt = time_per_item(rounds, &pairs, |(r, _)| {
        black_box(round_trip(r));
    });
    let compose_ns = time_per_item(rounds, &pairs, |(r, s)| {
        black_box(compose(r, s, &declared));
    });
    println!(
        "{{\"op\":\"compose\",\"round_trip_ns\":{rt:.0},\"compose_ns\":{compose_ns:.0},\"same_as_round_trip\":{same},\"of\":{}}}",
        pairs.len()
    );
}

/// Times the decode of every record of IN into a heap of
/// `Heap::with_values`, as the mirror would decode the committed buckets.
fn decode_all(input: &str, rounds: usize) {
    let records: Vec<Vec<u8>> = read_groups(input)
        .into_iter()
        .flat_map(|g| g.records)
        .collect();
    let mut refused = 0;
    for r in &records {
        let ok = decode_record_with(r, limits(r))
            .ok()
            .and_then(|tree| Heap::with_values().load(&tree).ok());
        if ok.is_none() {
            refused += 1;
        }
    }
    let bytes: usize = records.iter().map(Vec::len).sum();
    let ns = time_per_item(rounds, &records, |r| {
        if let Ok(tree) = decode_record_with(r, limits(r)) {
            black_box(Heap::with_values().load(&tree).ok());
        }
    });
    println!(
        "{{\"op\":\"decode_all\",\"records\":{},\"refused\":{refused},\"bytes\":{bytes},\"ns_per_record\":{ns:.0},\"total_ms\":{:.1}}}",
        records.len(),
        ns * records.len() as f64 / 1e6
    );
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("fields") => {
            let declared: Vec<&str> = args
                .get(4)
                .map(|f| f.split(',').collect())
                .unwrap_or_default();
            fields(&args[2], &args[3], &declared)
        }
        Some("decode") => decode_all(&args[2], args[3].parse().expect("rounds")),
        Some("bench") => bench(
            &args[2],
            &args[3],
            &args[4],
            args[5].parse().expect("rounds"),
        ),
        _ => {
            eprintln!(
                "usage: fieldspike fields IN OUT | fieldspike bench DOCS BUCKETS FIELDS ROUNDS"
            );
            std::process::exit(2);
        }
    }
}
