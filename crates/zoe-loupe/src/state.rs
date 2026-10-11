//! The state of a record as JSON, and its outbound references.
//!
//! Each node of the state pickle becomes one JSON value:
//!
//! - None, a bool, an int and a float become null, a bool and a number.
//!   An int beyond 2^53 becomes `{"$int": "<decimal or hex>"}`, so the
//!   window does not round it. A NaN or an infinity becomes
//!   `{"$float": "nan"}`.
//! - A str becomes a string. A Python 2 `str` becomes a string when it
//!   holds UTF-8, else `{"$bytes": "<escaped>"}`, as bytes do.
//! - A tuple and a list become an array.
//! - A dict with text keys becomes an object. Another dict becomes
//!   `{"$dict": [[key, value], ...]}`. So does a dict with a key that
//!   starts with `$`, so that no key of the store looks like a tag.
//! - A persistent reference becomes `{"$ref": "<oid>", "class": ...}`. A
//!   weak reference also has `"weak": true`. Another persistent id, such
//!   as a reference to another database, becomes `{"$persistent": <id>}`.
//! - A GLOBAL becomes `{"$global": "module.name"}`. A REDUCE of `set` or
//!   `frozenset` becomes `{"$call": "builtins.set", "args": [...]}`.
//! - An opaque node becomes `{"$opaque": "<opcode>", "pickle": "<hex>"}`.
//! - A memo get becomes the value that it stands for. A get inside the
//!   value that it stands for becomes `{"$cycle": <memo index>}`. A get
//!   whose value is in an opaque node becomes `{"$memo": <memo index>}`.
//!
//! The references are the persistent references of the state, strong
//! ones only, as ZODB's `referencesf` gives them. Each has a JSON pointer
//! to its place in the state. A get never adds a reference: the place of
//! the value that it stands for has it already. The references in an
//! opaque node are not known.
//!
//! The JSON has limits, so that a large or deep record cannot exhaust
//! the stack or the memory. A value past `MAX_DEPTH` levels, or past
//! `VALUE_BUDGET` values, becomes `{"$elided": "..."}`. The references
//! below it then point to the elided value. A str, bytes, opaque bytes or
//! the text of a large int show at most `MAX_TEXT` bytes, and all of
//! them together at most `TEXT_BUDGET` bytes. A value that is cut shows
//! its start and its size in the record: `{"$str": "<start>", "length":
//! n}`, `{"$bytes": "<start>", "length": n}`, `{"$opaque": ..., "pickle":
//! "<start>", "length": n}` or `{"$int": "<start>", "length": n}`.

use serde_json::{Map, Number, Value, json};
use zoe_core::Oid;
use zoe_pickle::{Float, Node, Resolver, Tree};

use crate::text::{self, IntValue};
use crate::{Reference, oid_repr};

/// The most JSON values in the state of one object. It equals the node
/// budget of zoe-pickle. Memo gets can make more values than the tree
/// has nodes.
const VALUE_BUDGET: usize = zoe_pickle::NODE_BUDGET;

/// The deepest nesting of the state. serde_json and the window walk a
/// value with recursion, so the depth stays small.
const MAX_DEPTH: usize = 100;

/// The most bytes that one str, bytes or opaque value shows.
const MAX_TEXT: usize = 1 << 16;

/// The most bytes that the str, bytes and opaque values of one state show
/// together. The keys of the dicts count too.
const TEXT_BUDGET: usize = 1 << 24;

/// The largest int that a JSON number holds without a loss in the window.
const MAX_SAFE_INT: i128 = (1 << 53) - 1;

/// Return the state of `tree` as JSON, and its outbound references.
pub(crate) fn state(tree: &Tree) -> (Value, Vec<Reference>) {
    let mut converter = Converter {
        resolver: tree.resolver(),
        budget: VALUE_BUDGET,
        text_budget: TEXT_BUDGET,
        ancestors: Vec::new(),
        gets: 0,
        references: Vec::new(),
    };
    let mut at = String::new();
    let state = converter.value(tree.state(), &mut at, 0);
    (state, converter.references)
}

/// What a persistent id leads to.
pub(crate) struct Target<'t> {
    /// The oid.
    pub(crate) oid: Oid,
    /// The class that the id names, if any.
    pub(crate) class: Option<&'t Node>,
    /// True for a weak reference.
    pub(crate) weak: bool,
}

/// Read a persistent id of ZODB: `oid`, `(oid, class)`, `[oid]`, or
/// `['w', (oid,)]`. Other forms, such as the references to another
/// database, give `None`.
pub(crate) fn target<'t>(resolver: &Resolver<'t>, pid: &'t Node) -> Option<Target<'t>> {
    let pid = resolver.resolve(pid)?;
    match pid {
        Node::Tuple(tuple) => match &tuple.items[..] {
            [oid, class] => Some(Target {
                oid: Oid(text::oid_bytes(resolver, oid)?),
                class: Some(class),
                weak: false,
            }),
            _ => None,
        },
        Node::List(_) => {
            let items = list_items(pid);
            match &items[..] {
                [oid] => Some(Target {
                    oid: Oid(text::oid_bytes(resolver, oid)?),
                    class: None,
                    weak: true,
                }),
                [kind, args] if text::text(resolver, kind).as_deref() == Some("w") => {
                    // `(oid, database)` is a reference to another database.
                    let Node::Tuple(args) = resolver.resolve(args)? else {
                        return None;
                    };
                    let [oid] = &args.items[..] else {
                        return None;
                    };
                    Some(Target {
                        oid: Oid(text::oid_bytes(resolver, oid)?),
                        class: None,
                        weak: true,
                    })
                }
                _ => None,
            }
        }
        _ => Some(Target {
            oid: Oid(text::oid_bytes(resolver, pid)?),
            class: None,
            weak: false,
        }),
    }
}

/// Return the items of a list node, in order.
pub(crate) fn list_items(node: &Node) -> Vec<&Node> {
    match node {
        Node::List(list) => list
            .initial
            .iter()
            .flatten()
            .chain(list.batches.iter().flat_map(|batch| &batch.items))
            .collect(),
        _ => Vec::new(),
    }
}

/// Return the entries of a dict node, in order.
pub(crate) fn dict_entries(node: &Node) -> Vec<&(Node, Node)> {
    match node {
        Node::Dict(dict) => dict
            .initial
            .iter()
            .flatten()
            .chain(dict.batches.iter().flat_map(|batch| &batch.items))
            .collect(),
        _ => Vec::new(),
    }
}

/// Add the segment `segment` to the JSON pointer `at`.
fn push_segment(at: &mut String, segment: &str) {
    at.push('/');
    for c in segment.chars() {
        match c {
            '~' => at.push_str("~0"),
            '/' => at.push_str("~1"),
            c => at.push(c),
        }
    }
}

/// Return the name of the opcode that labels an opaque node.
fn opcode_name(opcode: u8) -> String {
    let name = match opcode {
        b'R' => "REDUCE",
        b'b' => "BUILD",
        b'i' => "INST",
        b'o' => "OBJ",
        0x81 => "NEWOBJ",
        0x82 => "EXT1",
        0x83 => "EXT2",
        0x84 => "EXT4",
        b'0' => "POP",
        b'1' => "POP_MARK",
        b'2' => "DUP",
        b'a' => "APPEND",
        b'e' => "APPENDS",
        b's' => "SETITEM",
        b'u' => "SETITEMS",
        b'P' => "PERSID",
        b'Q' => "BINPERSID",
        _ => return format!("0x{opcode:02x}"),
    };
    name.to_owned()
}

/// Return bytes as `{"$bytes": "<escaped>"}`. `len` is the size of the
/// value in the record. When `shown` is only its start, add `length`.
fn bytes_value(shown: &[u8], len: usize) -> Value {
    let mut value = json!({ "$bytes": text::escape_bytes(shown) });
    if shown.len() < len {
        value["length"] = json!(len);
    }
    value
}

struct Converter<'t> {
    resolver: Resolver<'t>,
    /// The JSON values that the state can still get.
    budget: usize,
    /// The bytes of text that the state can still show.
    text_budget: usize,
    /// The addresses of the nodes that the converter is in, innermost
    /// last. A get of one of them is a cycle.
    ancestors: Vec<usize>,
    /// The count of the memo gets that the converter is in.
    gets: usize,
    references: Vec<Reference>,
}

impl<'t> Converter<'t> {
    /// Return `node` as JSON. `at` is the JSON pointer of the value.
    fn value(&mut self, node: &'t Node, at: &mut String, depth: usize) -> Value {
        let node = node.value();
        if depth > MAX_DEPTH {
            return self.elided(node, at, "too deep");
        }
        if self.budget == 0 {
            return self.elided(node, at, "too many values");
        }
        self.budget -= 1;
        self.ancestors.push(std::ptr::from_ref(node).addr());
        let value = self.convert(node, at, depth);
        self.ancestors.pop();
        value
    }

    /// Return `node` as JSON, for a node that is not a put.
    fn convert(&mut self, node: &'t Node, at: &mut String, depth: usize) -> Value {
        match node {
            Node::None => Value::Null,
            Node::Bool(value) => Value::Bool(value.value()),
            Node::Int(int) => match text::int(int) {
                IntValue::Small(value) if (-MAX_SAFE_INT..=MAX_SAFE_INT).contains(&value) => {
                    json!(value as i64)
                }
                IntValue::Small(value) => json!({ "$int": value.to_string() }),
                IntValue::Large(text) => {
                    let shown = self.cut(text.len());
                    if shown < text.len() {
                        json!({ "$int": &text[..shown], "length": text.len() })
                    } else {
                        json!({ "$int": text })
                    }
                }
            },
            Node::Float(float) => {
                let value = match float {
                    Float::Binary(value) => Some(*value),
                    Float::Text(text) => text.parse().ok(),
                };
                match value.and_then(Number::from_f64) {
                    Some(number) => Value::Number(number),
                    None => {
                        let text = match (value, float) {
                            (Some(value), _) if value.is_nan() => "nan".to_owned(),
                            (Some(value), _) if value > 0.0 => "inf".to_owned(),
                            (Some(_), _) => "-inf".to_owned(),
                            (None, Float::Text(text)) => text.clone(),
                            (None, Float::Binary(_)) => "nan".to_owned(),
                        };
                        json!({ "$float": text })
                    }
                }
            }
            Node::Bytes(bytes) => {
                let len = bytes.data.len();
                let shown = self.cut(len);
                bytes_value(&bytes.data[..shown], len)
            }
            Node::ByteString(string) => {
                let len = text::byte_string_len(string);
                let shown = self.cut(len);
                let start = text::byte_string_start(string, shown);
                if shown < len {
                    return bytes_value(&start, len);
                }
                match String::from_utf8(start) {
                    Ok(text) => Value::String(text),
                    Err(e) => bytes_value(e.as_bytes(), e.as_bytes().len()),
                }
            }
            Node::Unicode(value) => {
                let len = text::unicode_len(value);
                let shown = self.cut(len);
                let start = text::unicode_start(value, shown);
                if shown < len {
                    json!({ "$str": start, "length": len })
                } else {
                    Value::String(start)
                }
            }
            Node::Tuple(tuple) => self.array(tuple.items.iter(), at, depth),
            Node::List(_) => self.array(list_items(node).into_iter(), at, depth),
            Node::Dict(_) => self.dict(node, at, depth),
            Node::Global(global) => {
                json!({ "$global": format!("{}.{}", global.module, global.name) })
            }
            Node::Persistent(pid) => self.reference(pid, at, depth),
            Node::Reduce(reduce) => {
                let callable = match self.resolver.resolve(&reduce.callable) {
                    Some(Node::Global(global)) => format!("{}.{}", global.module, global.name),
                    _ => String::new(),
                };
                let len = at.len();
                push_segment(at, "args");
                let args = self.value(&reduce.args, at, depth + 1);
                at.truncate(len);
                json!({ "$call": callable, "args": args })
            }
            Node::Get(get) => match self.resolver.resolve(node) {
                None => json!({ "$memo": get.index }),
                Some(value) => {
                    let address = std::ptr::from_ref(value.value()).addr();
                    if self.ancestors.contains(&address) {
                        return json!({ "$cycle": get.index });
                    }
                    self.gets += 1;
                    let value = self.value(value, at, depth + 1);
                    self.gets -= 1;
                    value
                }
            },
            Node::Opaque(opaque) => {
                let len = opaque.bytes.len();
                let shown = self.cut(len);
                let mut value = json!({
                    "$opaque": opcode_name(opaque.opcode),
                    "pickle": text::hex(&opaque.bytes[..shown]),
                });
                if shown < len {
                    value["length"] = json!(len);
                }
                value
            }
            // `Node::value` looks through puts.
            Node::Put(_) => Value::Null,
        }
    }

    /// Take up to `len` bytes from the text budget, and at most
    /// `MAX_TEXT`. Return the count of bytes to show.
    fn cut(&mut self, len: usize) -> usize {
        let shown = len.min(MAX_TEXT).min(self.text_budget);
        self.text_budget -= shown;
        shown
    }

    fn array(
        &mut self,
        items: impl Iterator<Item = &'t Node>,
        at: &mut String,
        depth: usize,
    ) -> Value {
        let mut array = Vec::new();
        for (index, item) in items.enumerate() {
            let len = at.len();
            push_segment(at, &index.to_string());
            array.push(self.value(item, at, depth + 1));
            at.truncate(len);
        }
        Value::Array(array)
    }

    fn dict(&mut self, node: &'t Node, at: &mut String, depth: usize) -> Value {
        let entries = dict_entries(node);
        // Text keys whose size fits the text budget become the keys of an
        // object, unless one of them looks like a tag.
        let size: Option<usize> = entries
            .iter()
            .map(|(key, _)| text::text_len(&self.resolver, key))
            .sum();
        let keys: Option<Vec<String>> = match size {
            Some(size) if size <= self.text_budget => {
                self.text_budget -= size;
                entries
                    .iter()
                    .map(|(key, _)| text::text(&self.resolver, key))
                    .collect()
            }
            _ => None,
        };
        let keys = keys.filter(|keys| !keys.iter().any(|key| key.starts_with('$')));
        let len = at.len();
        let Some(keys) = keys else {
            // A list of pairs.
            push_segment(at, "$dict");
            let pairs = entries
                .iter()
                .enumerate()
                .map(|(index, (key, value))| (index, [key, value]));
            let mut array = Vec::new();
            for (index, pair) in pairs {
                let len = at.len();
                push_segment(at, &index.to_string());
                let pair = self.array(pair.into_iter(), at, depth + 1);
                at.truncate(len);
                array.push(pair);
            }
            at.truncate(len);
            return json!({ "$dict": array });
        };
        let mut map = Map::new();
        for ((_, value), key) in entries.into_iter().zip(keys) {
            push_segment(at, &key);
            let value = self.value(value, at, depth + 1);
            at.truncate(len);
            map.insert(key, value);
        }
        Value::Object(map)
    }

    fn reference(&mut self, pid: &'t Node, at: &mut String, depth: usize) -> Value {
        let Some(target) = target(&self.resolver, pid) else {
            let len = at.len();
            push_segment(at, "$persistent");
            let pid = self.value(pid, at, depth + 1);
            at.truncate(len);
            return json!({ "$persistent": pid });
        };
        let class = target
            .class
            .and_then(|class| self.resolver.resolve(class))
            .and_then(|class| match class {
                Node::Global(global) => Some(format!("{}.{}", global.module, global.name)),
                _ => None,
            });
        if !target.weak && self.gets == 0 {
            self.references.push(Reference {
                oid: target.oid,
                at: at.clone(),
            });
        }
        let mut value = json!({ "$ref": oid_repr(target.oid), "class": class });
        if target.weak {
            value["weak"] = Value::Bool(true);
        }
        value
    }

    /// Return the placeholder of a value that the state leaves out. Keep
    /// the references below it, with the pointer of the placeholder.
    fn elided(&mut self, node: &'t Node, at: &str, why: &str) -> Value {
        if self.gets == 0 {
            for pid in node.walk().filter_map(|node| match node {
                Node::Persistent(pid) => Some(pid),
                _ => None,
            }) {
                if let Some(target) = target(&self.resolver, pid).filter(|t| !t.weak) {
                    self.references.push(Reference {
                        oid: target.oid,
                        at: at.to_owned(),
                    });
                }
            }
        }
        json!({ "$elided": why })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Decode a record whose class pickle is GLOBAL m C, BINPUT 0, and
    /// return its state as JSON and its references.
    fn decode(state: &[u8]) -> (Value, Vec<Reference>) {
        let record = [&b"\x80\x03cm\nC\nq\x00.\x80\x03"[..], state, b"."].concat();
        super::state(&zoe_pickle::decode_record(&record).unwrap())
    }

    /// The pickle of a persistent reference to `oid` with class m.C.
    fn reference(oid: u8) -> Vec<u8> {
        [&b"C\x08\0\0\0\0\0\0\0"[..], &[oid], b"h\x00\x86Q"].concat()
    }

    fn references(found: &[Reference]) -> Vec<(u64, &str)> {
        found.iter().map(|r| (r.oid.0, r.at.as_str())).collect()
    }

    #[test]
    fn values_have_their_json_form() {
        let (state, _) = decode(
            b"(NI01\nK\x07J\xff\xff\xff\xffG?\xf8\x00\x00\x00\x00\x00\x00\
              X\x01\x00\x00\x00aC\x02\x00\xffU\x01b]K\x01a\x8a\x09\x00\x00\x00\x00\x00\x00\x00\x00\x01t",
        );
        assert_eq!(
            state,
            json!([
                null,
                true,
                7,
                -1,
                1.5,
                "a",
                {"$bytes": "\\x00\\xff"},
                "b",
                [1],
                {"$int": "18446744073709551616"},
            ])
        );
    }

    #[test]
    fn the_smallest_long_of_16_bytes_is_a_tagged_int() {
        let (state, _) = decode(&[&b"\x8a\x10"[..], &[0; 15], b"\x80"].concat());
        assert_eq!(state, json!({"$int": i128::MIN.to_string()}));
        let (state, _) = decode(b"\x8a\x07\x01\x00\x00\x00\x00\x00\xe0");
        assert_eq!(state, json!(1 - (1_i64 << 53)));
        let (state, _) = decode(b"\x8a\x07\x00\x00\x00\x00\x00\x00\xe0");
        assert_eq!(state, json!({"$int": (-(1_i64 << 53)).to_string()}));
    }

    #[test]
    fn a_dict_with_a_key_like_a_tag_is_a_list_of_pairs() {
        let state = [
            &b"}(X\x04\x00\x00\x00$refX\x04\x00\x00\x000x01X\x01\x00\x00\x00a"[..],
            &reference(5),
            b"u",
        ]
        .concat();
        let (state, found) = decode(&state);
        assert_eq!(state["$dict"][0], json!(["$ref", "0x01"]));
        assert!(state.get("$ref").is_none());
        assert_eq!(references(&found), [(5, "/$dict/1/1")]);
        let (state, _) = decode(b"}X\x04\x00\x00\x00$intK\x01s");
        assert_eq!(state, json!({"$dict": [["$int", 1]]}));
    }

    #[test]
    fn a_long_value_shows_its_start_and_its_length() {
        let len = MAX_TEXT + 10;
        let text = [&b"X"[..], &(len as u32).to_le_bytes(), &b"a".repeat(len)].concat();
        let (state, _) = decode(&text);
        assert_eq!(state["length"], json!(len));
        assert_eq!(state["$str"].as_str().unwrap().len(), MAX_TEXT);

        let bytes = [&b"B"[..], &(len as u32).to_le_bytes(), &vec![0xff; len]].concat();
        let (state, _) = decode(&bytes);
        assert_eq!(state["length"], json!(len));
        assert_eq!(state["$bytes"].as_str().unwrap().len(), 4 * MAX_TEXT);

        let (state, _) = decode(b"X\x02\x00\x00\x00ab");
        assert_eq!(state, json!("ab"));
    }

    #[test]
    fn many_gets_of_a_long_value_stop_at_the_text_budget() {
        // One str of MAX_TEXT bytes, then gets of it: past the budget, the
        // gets show an empty start.
        let gets = TEXT_BUDGET / MAX_TEXT + 2;
        let mut state = [&b"(X"[..], &(MAX_TEXT as u32).to_le_bytes()].concat();
        state.extend(b"a".repeat(MAX_TEXT));
        state.extend(b"q\x01");
        state.extend(b"h\x01".repeat(gets));
        state.push(b't');
        let (state, _) = decode(&state);
        let items = state.as_array().unwrap();
        assert_eq!(items.len(), gets + 1);
        let full = items.iter().filter(|item| item.is_string()).count();
        assert_eq!(full, TEXT_BUDGET / MAX_TEXT);
        assert_eq!(items[gets], json!({"$str": "", "length": MAX_TEXT}));
    }

    #[test]
    fn a_weak_reference_to_another_database_is_no_reference() {
        let (state, found) =
            decode(b"]q\x01(X\x01\x00\x00\x00wC\x08\0\0\0\0\0\0\0\x05X\x02\x00\x00\x00db\x86eQ");
        assert!(state.get("$persistent").is_some(), "{state}");
        assert!(found.is_empty());
    }

    #[test]
    fn a_dict_with_other_keys_is_a_list_of_pairs() {
        let (state, found) = decode(&[&b"}(K\x01"[..], &reference(5), b"u"].concat());
        assert_eq!(state["$dict"][0][0], json!(1));
        assert_eq!(state["$dict"][0][1]["$ref"], json!("0x05"));
        assert_eq!(references(&found), [(5, "/$dict/0/1")]);
    }

    #[test]
    fn references_point_into_the_state() {
        let state = [
            &b"}(X\x03\x00\x00\x00a/b"[..],
            &reference(5),
            b"X\x01\x00\x00\x00c]",
            &reference(6),
            b"au",
        ]
        .concat();
        let (state, found) = decode(&state);
        assert_eq!(references(&found), [(5, "/a~1b"), (6, "/c/0")]);
        for reference in &found {
            assert_eq!(state.pointer(&reference.at).unwrap()["class"], json!("m.C"));
        }
    }

    #[test]
    fn a_weak_reference_is_in_the_state_only() {
        let (state, found) = decode(b"]q\x01(X\x01\x00\x00\x00wC\x08\0\0\0\0\0\0\0\x05\x85eQ");
        assert_eq!(state, json!({"$ref": "0x05", "class": null, "weak": true}));
        assert!(found.is_empty());
    }

    #[test]
    fn a_get_shows_its_value_and_adds_no_reference() {
        let state = [&b"("[..], &reference(5), b"q\x01h\x01t"].concat();
        let (state, found) = decode(&state);
        assert_eq!(state[0], state[1]);
        assert_eq!(references(&found), [(5, "/0")]);
    }

    #[test]
    fn a_get_inside_its_own_value_is_a_cycle() {
        let (state, _) = decode(b"]q\x01(h\x01h\x01e");
        assert_eq!(state, json!([{"$cycle": 1}, {"$cycle": 1}]));
    }

    #[test]
    fn an_opaque_node_keeps_its_bytes() {
        let (state, _) = decode(b"cdecimal\nDecimal\nX\x03\x00\x00\x001.5\x85R");
        assert_eq!(state["$opaque"], json!("REDUCE"));
        assert!(state["pickle"].as_str().unwrap().starts_with("63646563"));
    }

    #[test]
    fn a_deep_value_is_elided_with_its_references() {
        let depth = MAX_DEPTH + 10;
        let mut state = b"(".repeat(depth);
        state.extend(reference(5));
        state.extend(b"t".repeat(depth));
        let (state, found) = decode(&state);
        let [(5, at)] = references(&found)[..] else {
            panic!("{found:?}");
        };
        assert_eq!(state.pointer(at).unwrap(), &json!({"$elided": "too deep"}));
    }

    #[test]
    fn many_gets_stop_at_the_value_budget() {
        // A list of 2^11 items, then 2^10 gets of it: 2^21 values.
        let mut state = b"]q\x01(".to_vec();
        state.extend(b"K\x01".repeat(1 << 11));
        state.extend(b"e(");
        state.extend(b"h\x01".repeat(1 << 10));
        state.push(b't');
        let (state, _) = decode(&[&b"("[..], &state, b"t"].concat());
        let gets = state[1].as_array().unwrap();
        assert_eq!(gets.len(), 1 << 10);
        assert_eq!(gets.last().unwrap(), &json!({"$elided": "too many values"}));
    }
}
