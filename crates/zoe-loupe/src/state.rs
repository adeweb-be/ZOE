//! The state of a record as JSON, and its outbound references.
//!
//! Each node of the state pickle becomes one JSON value:
//!
//! - None, a bool, an int and a float become null, a bool and a number.
//!   An int beyond 2^53 becomes `{"$int": "<decimal or hex>"}`, so the
//!   window does not round it. A float with no fraction becomes
//!   `{"$float": "1.0"}`, so it does not read as an int. A NaN or an
//!   infinity becomes `{"$float": "nan"}`.
//! - A str becomes a string. A Python 2 `str` becomes
//!   `{"$str2": "<text>"}` when it holds UTF-8, else
//!   `{"$bytes": "<escaped>"}`, as bytes do.
//! - A tuple and a list become an array.
//! - A dict with text keys becomes an object. Another dict becomes
//!   `{"$dict": [[key, value], ...]}`. So does a dict with a key that
//!   starts with `$`, so that no key of the store looks like a tag, and a
//!   dict with a key longer than `MAX_KEY` bytes.
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
//! The JSON has limits, so that a large, deep or crafted record cannot
//! exhaust the stack, the memory or the time of the call:
//!
//! - A value past `MAX_DEPTH` levels, or past `VALUE_BUDGET` values,
//!   becomes `{"$elided": "..."}`. The references below it then point to
//!   the elided value.
//! - The text of a str, a bytes value, an opaque value, a large int, a
//!   GLOBAL or a class name takes at most `MAX_TEXT` bytes in the JSON,
//!   escapes and hex digits included. All the text of the state, dict
//!   keys included, takes at most `TEXT_BUDGET` bytes. A value that is
//!   cut shows its start and its size in the record: `{"$str": "<start>",
//!   "length": n}`, `{"$str2": ..., "length": n}`, `{"$bytes": ...,
//!   "length": n}`, `{"$opaque": ..., "pickle": ..., "length": n}`,
//!   `{"$int": ..., "length": n}` or `{"$global": ..., "length": n}`. A
//!   class name that is cut shows its start only.
//! - A pointer of a reference has at most `MAX_POINTER` bytes, and the
//!   pointers of one state at most `POINTER_BUDGET` bytes together. A
//!   longer pointer gives way to the pointer of a value that holds the
//!   reference, at most the root, `""`.
//! - Each get of one value at one depth converts that value once. The
//!   other gets reuse the JSON, and pay for it from the budgets. The
//!   reused JSON holds at most `CACHE_BUDGET` values in all. A JSON with a
//!   cycle marker for a value outside it is not reused, because the
//!   marker is wrong in another place.

use std::collections::HashMap;

use serde_json::{Map, Number, Value, json};
use zoe_core::Oid;
use zoe_pickle::{Float, Global, Node, Resolver, Tree};

use crate::text::{self, IntValue};
use crate::{Reference, oid_repr};

/// The most JSON values in the state of one object. It equals the node
/// budget of zoe-pickle. Memo gets can make more values than the tree
/// has nodes.
const VALUE_BUDGET: usize = zoe_pickle::NODE_BUDGET;

/// The deepest nesting of the state. serde_json and the window walk a
/// value with recursion, so the depth stays small.
const MAX_DEPTH: usize = 100;

/// The most JSON bytes that the text of one value takes.
const MAX_TEXT: usize = 1 << 16;

/// The most JSON bytes that all the text of one state takes.
const TEXT_BUDGET: usize = 1 << 24;

/// The longest key of a dict that becomes the key of a JSON object. Each
/// key is a segment of the pointers below it, so the keys stay short.
const MAX_KEY: usize = 256;

/// The longest pointer of a reference.
pub(crate) const MAX_POINTER: usize = 1 << 10;

/// The most bytes that the pointers of the references of one state take.
pub(crate) const POINTER_BUDGET: usize = 1 << 24;

/// The most JSON values that the reused JSON of the gets holds in all.
/// A chain of gets would else keep a copy of its values at each link.
const CACHE_BUDGET: usize = 1 << 18;

/// The largest int that a JSON number holds without a loss in the window.
const MAX_SAFE_INT: i128 = (1 << 53) - 1;

/// Return the state of `tree` as JSON, and its outbound references.
pub(crate) fn state(tree: &Tree) -> (Value, Vec<Reference>) {
    convert(&tree.resolver(), tree.state())
}

/// Return the state `root` of a tree as JSON, and its outbound references.
/// `resolver` is the resolver of the tree.
pub(crate) fn convert<'t>(resolver: &Resolver<'t>, root: &'t Node) -> (Value, Vec<Reference>) {
    let mut converter = Converter::new(resolver);
    let mut at = String::new();
    let state = converter.value(root, &mut at, 0);
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
pub(crate) fn push_segment(at: &mut String, segment: &str) {
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

/// Return `{tag: text}`, and add `length` when `text` is only the start
/// of a value of `length` bytes in the record.
fn tagged(tag: &str, text: impl Into<Value>, whole: bool, length: usize) -> Value {
    let mut value = Map::new();
    value.insert(tag.to_owned(), text.into());
    if !whole {
        value.insert("length".to_owned(), json!(length));
    }
    Value::Object(value)
}

/// The JSON of a get, kept for the other gets of the same value at the
/// same depth, with what it took from the budgets.
struct Expanded {
    value: Value,
    values: usize,
    text: usize,
}

struct Converter<'r, 't> {
    resolver: &'r Resolver<'t>,
    /// The JSON values that the state can still get.
    budget: usize,
    /// The JSON bytes of text that the state can still take.
    text_budget: usize,
    /// The bytes of reference pointers that the state can still take.
    pointer_budget: usize,
    /// The addresses of the nodes that the converter is in, innermost
    /// last. A get of one of them is a cycle.
    ancestors: Vec<usize>,
    /// The count of the memo gets that the converter is in.
    gets: usize,
    /// The JSON of each value that a get expanded, by the address of the
    /// value and the depth.
    expanded: HashMap<(usize, usize), Expanded>,
    /// The JSON values that `expanded` holds in all.
    cached: usize,
    /// The lowest index in `ancestors` that a cycle marker named since the
    /// start of the innermost expansion. `usize::MAX` for none.
    lowest_cycle: usize,
    references: Vec<Reference>,
}

impl<'r, 't> Converter<'r, 't> {
    fn new(resolver: &'r Resolver<'t>) -> Self {
        Converter {
            resolver,
            budget: VALUE_BUDGET,
            text_budget: TEXT_BUDGET,
            pointer_budget: POINTER_BUDGET,
            ancestors: Vec::new(),
            gets: 0,
            expanded: HashMap::new(),
            cached: 0,
            lowest_cycle: usize::MAX,
            references: Vec::new(),
        }
    }

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
            Node::Int(int) => match text::int(int, self.allowance()) {
                IntValue::Small(value) if (-MAX_SAFE_INT..=MAX_SAFE_INT).contains(&value) => {
                    json!(value as i64)
                }
                IntValue::Small(value) => json!({ "$int": value.to_string() }),
                IntValue::Large {
                    start,
                    length,
                    whole,
                } => {
                    // The start is ASCII and fits the allowance.
                    self.text_budget -= start.len();
                    tagged("$int", start, whole, length)
                }
            },
            Node::Float(float) => {
                let value = match float {
                    Float::Binary(value) => Some(*value),
                    Float::Text(text) => text.parse().ok(),
                };
                match value {
                    Some(value) if value.is_finite() && value.fract() != 0.0 => {
                        Number::from_f64(value).map_or(Value::Null, Value::Number)
                    }
                    Some(value) if value.is_finite() => json!({ "$float": format!("{value:?}") }),
                    Some(value) if value.is_nan() => json!({ "$float": "nan" }),
                    Some(value) if value > 0.0 => json!({ "$float": "inf" }),
                    Some(_) => json!({ "$float": "-inf" }),
                    None => {
                        let (text, _) = self.take_text(match float {
                            Float::Text(text) => text,
                            Float::Binary(_) => "nan",
                        });
                        json!({ "$float": text })
                    }
                }
            }
            Node::Bytes(bytes) => self.bytes(&bytes.data, true, bytes.data.len()),
            Node::ByteString(string) => {
                let length = text::byte_string_len(string);
                let (start, whole) = text::byte_string_start(string, self.allowance());
                let text = match String::from_utf8(start) {
                    Ok(text) => Ok(text),
                    // A start that ends inside a character is text up to it.
                    Err(e) if !whole && e.utf8_error().error_len().is_none() => {
                        let valid = e.utf8_error().valid_up_to();
                        let mut bytes = e.into_bytes();
                        bytes.truncate(valid);
                        Ok(String::from_utf8(bytes).unwrap_or_default())
                    }
                    Err(e) => Err(e.into_bytes()),
                };
                match text {
                    Ok(text) => {
                        let (shown, fits) = self.take_text(&text);
                        tagged("$str2", shown, whole && fits, length)
                    }
                    Err(bytes) => self.bytes(&bytes, whole, length),
                }
            }
            Node::Unicode(value) => {
                let length = text::unicode_len(value);
                let (start, whole) = text::unicode_start(value, self.allowance());
                let (shown, fits) = self.take_text(&start);
                if whole && fits {
                    Value::String(shown.to_owned())
                } else {
                    tagged("$str", shown, false, length)
                }
            }
            Node::Tuple(tuple) => self.array(tuple.items.iter(), at, depth),
            Node::List(_) => self.array(list_items(node).into_iter(), at, depth),
            Node::Dict(_) => self.dict(node, at, depth),
            Node::Global(global) => {
                let (name, whole) = self.qualified(global);
                tagged(
                    "$global",
                    name,
                    whole,
                    global.module.len() + 1 + global.name.len(),
                )
            }
            Node::Persistent(pid) => self.reference(pid, at, depth),
            Node::Reduce(reduce) => {
                let callable = match self.resolver.resolve(&reduce.callable) {
                    Some(Node::Global(global)) => self.qualified(global).0,
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
                Some(value) => self.expand(value, get.index, at, depth + 1),
            },
            Node::Opaque(opaque) => {
                let length = opaque.bytes.len();
                // Each byte takes two hex digits.
                let shown = length.min(self.allowance() / 2);
                self.text_budget -= 2 * shown;
                let mut value = tagged("$opaque", opcode_name(opaque.opcode), true, length);
                value["pickle"] = json!(text::hex(&opaque.bytes[..shown]));
                if shown < length {
                    value["length"] = json!(length);
                }
                value
            }
            // `Node::value` looks through puts.
            Node::Put(_) => Value::Null,
        }
    }

    /// Return the JSON of `value`, which a get with the memo index `index`
    /// stands for, at the depth `depth`. Reuse the JSON of an earlier get
    /// of the same value at the same depth, when the budgets can pay for
    /// it.
    fn expand(&mut self, value: &'t Node, index: u32, at: &mut String, depth: usize) -> Value {
        let address = std::ptr::from_ref(value.value()).addr();
        if let Some(ancestor) = self.ancestors.iter().position(|&a| a == address) {
            self.lowest_cycle = self.lowest_cycle.min(ancestor);
            return json!({ "$cycle": index });
        }
        if let Some(earlier) = self.expanded.get(&(address, depth))
            && earlier.values <= self.budget
            && earlier.text <= self.text_budget
        {
            self.budget -= earlier.values;
            self.text_budget -= earlier.text;
            return earlier.value.clone();
        }
        let (budget, text_budget) = (self.budget, self.text_budget);
        // The value goes into `ancestors` at this index.
        let base = self.ancestors.len();
        let outer = std::mem::replace(&mut self.lowest_cycle, usize::MAX);
        self.gets += 1;
        let json = self.value(value, at, depth);
        self.gets -= 1;
        let lowest = self.lowest_cycle;
        self.lowest_cycle = outer.min(lowest);
        let values = budget - self.budget;
        // A marker for a value outside this one is wrong in another place.
        if lowest >= base && self.cached + values <= CACHE_BUDGET {
            self.cached += values;
            self.expanded.insert(
                (address, depth),
                Expanded {
                    value: json.clone(),
                    values,
                    text: text_budget - self.text_budget,
                },
            );
        }
        json
    }

    /// Return the JSON bytes of text that one value can still take.
    fn allowance(&self) -> usize {
        MAX_TEXT.min(self.text_budget)
    }

    /// Take the longest start of `text` that the allowance pays for. Return
    /// it, and tell if it is the whole text.
    fn take_text<'a>(&mut self, text: &'a str) -> (&'a str, bool) {
        let (end, cost) = text::fit(text, self.allowance());
        self.text_budget -= cost;
        (&text[..end], end == text.len())
    }

    /// Return bytes as `{"$bytes": "<escaped>"}`, as far as the allowance
    /// pays for. `whole` tells if `bytes` is the whole value, and `length`
    /// is the size of the value in the record.
    fn bytes(&mut self, bytes: &[u8], whole: bool, length: usize) -> Value {
        let allowance = self.allowance();
        // Each byte takes one JSON byte at least.
        let start = &bytes[..bytes.len().min(allowance)];
        let (escaped, shown, cost) = text::escape_within(start, allowance);
        self.text_budget -= cost;
        tagged("$bytes", escaped, whole && shown == bytes.len(), length)
    }

    /// Return the name of a GLOBAL, `module.name`, as far as the allowance
    /// pays for, and tell if it is whole.
    fn qualified(&mut self, global: &Global) -> (String, bool) {
        let mut name = String::new();
        for part in [global.module.as_str(), ".", global.name.as_str()] {
            let (shown, whole) = self.take_text(part);
            name.push_str(shown);
            if !whole {
                return (name, false);
            }
        }
        (name, true)
    }

    /// Return the pointer of a reference at `at`, within `MAX_POINTER` and
    /// the pointer budget. A longer pointer gives way to the pointer of the
    /// innermost value that holds the reference and fits.
    fn pointer(&mut self, at: &str) -> String {
        let limit = MAX_POINTER.min(self.pointer_budget);
        let end = if at.len() <= limit {
            at.len()
        } else {
            at.as_bytes()[..=limit]
                .iter()
                .rposition(|&byte| byte == b'/')
                .unwrap_or(0)
        };
        self.pointer_budget -= end;
        at[..end].to_owned()
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

    /// Return the keys of a dict as the keys of a JSON object, or `None`
    /// when the dict must be a list of pairs. Take their JSON size from
    /// the text budget.
    fn keys(&mut self, entries: &[&'t (Node, Node)]) -> Option<Vec<String>> {
        // Check the sizes in the record first, so that no long key is
        // decoded.
        let mut size = 0;
        for (key, _) in entries {
            let len = text::text_len(self.resolver, key)?;
            if len > MAX_KEY {
                return None;
            }
            size += len;
        }
        if size > self.text_budget {
            return None;
        }
        let keys: Vec<String> = entries
            .iter()
            .map(|(key, _)| text::text(self.resolver, key))
            .collect::<Option<_>>()?;
        if keys
            .iter()
            .any(|key| key.len() > MAX_KEY || key.starts_with('$'))
        {
            return None;
        }
        let cost: usize = keys
            .iter()
            .flat_map(|key| key.chars())
            .map(text::json_cost)
            .sum();
        if cost > self.text_budget {
            return None;
        }
        self.text_budget -= cost;
        Some(keys)
    }

    fn dict(&mut self, node: &'t Node, at: &mut String, depth: usize) -> Value {
        let entries = dict_entries(node);
        let len = at.len();
        let Some(keys) = self.keys(&entries) else {
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
        let Some(target) = target(self.resolver, pid) else {
            let len = at.len();
            push_segment(at, "$persistent");
            let pid = self.value(pid, at, depth + 1);
            at.truncate(len);
            return json!({ "$persistent": pid });
        };
        let class = match target.class.and_then(|class| self.resolver.resolve(class)) {
            Some(Node::Global(global)) => Some(self.qualified(global).0),
            _ => None,
        }
        .filter(|class| !class.is_empty());
        if !target.weak && self.gets == 0 {
            let at = self.pointer(at);
            self.references.push(Reference {
                oid: target.oid,
                at,
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
                if let Some(target) = target(self.resolver, pid).filter(|t| !t.weak) {
                    let at = self.pointer(at);
                    self.references.push(Reference {
                        oid: target.oid,
                        at,
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
                {"$str2": "b"},
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
        // Each byte is an escape of four characters, five in JSON.
        let escaped = state["$bytes"].as_str().unwrap();
        assert_eq!(escaped.len(), 4 * (MAX_TEXT / 5));
        assert!(json!(escaped).to_string().len() - 2 <= MAX_TEXT);

        // A quote takes two bytes in JSON.
        let quotes = [
            &b"X"[..],
            &(MAX_TEXT as u32).to_le_bytes(),
            &b"\"".repeat(MAX_TEXT),
        ]
        .concat();
        let (state, _) = decode(&quotes);
        assert_eq!(state["$str"].as_str().unwrap().len(), MAX_TEXT / 2);

        let python2 = [&b"T"[..], &(len as u32).to_le_bytes(), &b"a".repeat(len)].concat();
        let (state, _) = decode(&python2);
        assert_eq!(state["length"], json!(len));
        assert_eq!(state["$str2"].as_str().unwrap().len(), MAX_TEXT);

        let (state, _) = decode(b"X\x02\x00\x00\x00ab");
        assert_eq!(state, json!("ab"));
    }

    #[test]
    fn a_float_without_a_fraction_keeps_its_type() {
        let (state, _) = decode(b"(G?\xf0\x00\x00\x00\x00\x00\x00G?\xf8\x00\x00\x00\x00\x00\x00t");
        assert_eq!(state, json!([{"$float": "1.0"}, 1.5]));
    }

    /// The pickle of a dict that holds `inner` under a key of `size`
    /// bytes.
    fn under_key(size: usize, inner: &[u8]) -> Vec<u8> {
        let key = [&b"X"[..], &(size as u32).to_le_bytes(), &b"k".repeat(size)].concat();
        [&b"}"[..], &key, inner, b"s"].concat()
    }

    #[test]
    fn a_dict_with_a_long_key_is_a_list_of_pairs() {
        let (state, _) = decode(&under_key(MAX_KEY, b"K\x01"));
        assert_eq!(state["k".repeat(MAX_KEY)], json!(1));
        let (state, found) = decode(&under_key(MAX_KEY + 1, &reference(5)));
        assert_eq!(state["$dict"][0][0], json!("k".repeat(MAX_KEY + 1)));
        assert_eq!(references(&found), [(5, "/$dict/0/1")]);
    }

    #[test]
    fn a_long_pointer_gives_way_to_the_pointer_of_a_holder() {
        let mut state = reference(5);
        for _ in 0..10 {
            state = under_key(200, &state);
        }
        let full = format!("/{}", "k".repeat(200)).repeat(10);
        let (state, found) = decode(&state);
        let [(5, at)] = references(&found)[..] else {
            panic!("{found:?}");
        };
        assert!(at.len() <= MAX_POINTER && !at.is_empty(), "{}", at.len());
        assert!(full.starts_with(at) && full.as_bytes()[at.len()] == b'/');
        assert!(state.pointer(at).is_some_and(Value::is_object));
        assert_eq!(state.pointer(&full).unwrap()["$ref"], json!("0x05"));
    }

    #[test]
    fn the_pointers_of_a_state_stop_at_the_pointer_budget() {
        // References in a list under four keys of 250 bytes: each pointer
        // takes about 1,010 bytes.
        let count = POINTER_BUDGET / 1000 + 100;
        let mut list = b"](".to_vec();
        for _ in 0..count {
            list.extend(reference(5));
        }
        list.push(b'e');
        let mut state = list;
        for _ in 0..4 {
            state = under_key(250, &state);
        }
        let (state, found) = decode(&state);
        assert_eq!(found.len(), count);
        let total: usize = found.iter().map(|r| r.at.len()).sum();
        assert!(total <= POINTER_BUDGET, "{total}");
        assert!(found.last().unwrap().at.len() < found[0].at.len());
        for reference in &found {
            assert!(state.pointer(&reference.at).is_some(), "{}", reference.at);
        }
    }

    #[test]
    fn a_cycle_marker_stays_in_its_cycle() {
        // X = [V, W], V = [get X], W = [get V], and P = [[get V]] after X.
        // In W, V holds a cycle marker for X. In P, V holds X itself.
        let x = b"]q\x01(]q\x02(h\x01e](h\x02ee";
        let p = b"](](h\x02ee";
        let (state, _) = decode(&[&b"("[..], x, p, b"t"].concat());
        assert_eq!(state[0][0], json!([{"$cycle": 1}]));
        assert_eq!(state[0][1][0], json!([{"$cycle": 1}]));
        let x_in_p = &state[1][0][0][0];
        assert!(x_in_p.is_array(), "{x_in_p}");
        assert_eq!(x_in_p[0], json!([{"$cycle": 1}]));
    }

    #[test]
    fn the_reused_json_of_the_gets_stays_within_its_budget() {
        // A list of 100,000 ints, then a chain of lists, each with a get of
        // the list before it.
        let mut state = b"(]q\x01(".to_vec();
        state.extend(b"K\x01".repeat(100_000));
        state.push(b'e');
        for index in 2..12_u8 {
            state.extend(b"]q");
            state.push(index);
            state.extend(b"(h");
            state.push(index - 1);
            state.push(b'e');
        }
        state.push(b't');
        let record = [&b"\x80\x03cm\nC\nq\x00.\x80\x03"[..], &state, b"."].concat();
        let tree = zoe_pickle::decode_record(&record).unwrap();
        let resolver = tree.resolver();
        let mut converter = Converter::new(&resolver);
        converter.value(tree.state(), &mut String::new(), 0);
        let held: usize = converter.expanded.values().map(|e| e.values).sum();
        assert_eq!(held, converter.cached);
        assert!(converter.cached > 0);
        assert!(converter.cached <= CACHE_BUDGET, "{}", converter.cached);
    }

    #[test]
    fn many_gets_of_a_large_int_take_little_time() {
        // A LONG4 of 1 MiB whose low bytes are all zero, the slowest case,
        // then 100,000 gets of it.
        const LEN: usize = 1 << 20;
        let gets = 100_000;
        let mut long = vec![0; LEN];
        long[LEN - 1] = 0x80;
        let mut state = [&b"(\x8b"[..], &(LEN as u32).to_le_bytes(), &long, b"q\x01"].concat();
        state.extend(b"h\x01".repeat(gets));
        state.push(b't');
        let started = std::time::Instant::now();
        let (state, _) = decode(&state);
        let elapsed = started.elapsed();
        let items = state.as_array().unwrap();
        assert_eq!(items.len(), gets + 1);
        assert_eq!(items[1], items[0]);
        assert_eq!(items[gets], json!({"$int": "", "length": LEN}));
        // Each get reuses the JSON of the first one, or takes nothing when
        // the text budget is spent. A full conversion per get takes hours.
        assert!(elapsed.as_secs() < 20, "{elapsed:?}");
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
