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
//! The references are the persistent references of the record, strong
//! ones only, as ZODB's `referencesf` gives them. `zoe_pickle::persistent_ids`
//! gives the list, so it also holds the references that the state does not
//! show. A pointer holds its reference, except a reference from the class
//! pickle:
//!
//! - A reference that the state shows points to its place. Each place
//!   serves one reference of its oid. A get never adds a reference: the
//!   place of the value that it stands for has it already.
//! - Each other reference points to the deepest value that holds every
//!   place where such a reference can be: an opaque node, a persistent id
//!   that the converter does not read, and a value that the state leaves
//!   out. When there is only one such place, the reference points to it.
//! - A persistent id in the class pickle, as an instance of a persistent
//!   class has, is not in the state. Then each reference that the state
//!   does not show points to the root, `""`.
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
//!   `{"$int": ..., "length": n}`, `{"$global": ..., "length": n}` or
//!   `{"$call": ..., "length": n, "args": ...}`. A class name that is cut
//!   shows its start only.
//! - A pointer of a reference has at most `MAX_POINTER` bytes, and the
//!   pointers of one state at most `POINTER_BUDGET` bytes together. A
//!   longer pointer gives way to the pointer of a value that holds the
//!   reference, at most the root, `""`.
//! - Each get of one value at one depth converts that value once. The
//!   other gets reuse the JSON, and pay for it from the budgets. The
//!   reused JSON holds at most `CACHE_BUDGET` values in all. A JSON with a
//!   cycle marker for a value outside it is not reused, because the
//!   marker is wrong in another place.

use std::collections::{HashMap, VecDeque};

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

/// Return the state of `tree` as JSON, and the references that the state
/// shows.
pub(crate) fn state(tree: &Tree) -> (Value, Found) {
    let (state, mut found) = convert(&tree.resolver(), tree.state());
    // The state does not hold the class pickle, so the root stands for it.
    if holds_hidden_reference(tree.class()) {
        found.hidden = Some(String::new());
    }
    (state, found)
}

/// Return the state `root` of a tree as JSON, and the references that the
/// state shows. `resolver` is the resolver of the tree.
pub(crate) fn convert<'t>(resolver: &Resolver<'t>, root: &'t Node) -> (Value, Found) {
    let mut converter = Converter::new(resolver);
    let mut at = String::new();
    let state = converter.value(root, &mut at, 0);
    let found = Found {
        references: converter.references,
        hidden: converter.hidden,
    };
    (state, found)
}

/// What the converter finds out about the references of a state.
#[derive(Debug)]
pub(crate) struct Found {
    /// The strong references that the state shows, with their pointers.
    pub(crate) references: Vec<Reference>,
    /// The pointer of the deepest value that holds every place where a
    /// reference that the state does not show can be. `None` when there
    /// is no such place.
    hidden: Option<String>,
}

impl Found {
    /// Return one reference for each oid of `oids`, in their order.
    /// `oids` are the strong references of the record, in pickle order,
    /// as `zoe_pickle::persistent_ids` gives them.
    ///
    /// Each reference that the state shows serves one oid of `oids`, the
    /// same oid. Each other oid is in a place that the state does not
    /// show, so it points to the value that holds all those places.
    pub(crate) fn place(self, oids: &[Oid]) -> Vec<Reference> {
        let mut shown: HashMap<Oid, VecDeque<Reference>> = HashMap::new();
        for reference in self.references {
            shown.entry(reference.oid).or_default().push_back(reference);
        }
        let hidden = self.hidden.unwrap_or_default();
        oids.iter()
            .map(|&oid| {
                shown
                    .get_mut(&oid)
                    .and_then(VecDeque::pop_front)
                    .unwrap_or_else(|| Reference {
                        oid,
                        at: hidden.clone(),
                    })
            })
            .collect()
    }
}

/// Narrow `holder` to the deepest value that holds both its value and the
/// value at the pointer `at`.
fn narrow(holder: &mut Option<String>, at: &str) {
    match holder {
        None => *holder = Some(at.to_owned()),
        Some(holder) => {
            let mut common = 0;
            // Each pointer starts with `/`, or is the root, `""`.
            for (a, b) in holder.split('/').zip(at.split('/')).skip(1) {
                if a != b {
                    break;
                }
                common += 1 + a.len();
            }
            holder.truncate(common);
        }
    }
}

/// Return the longest start of the pointer `at` that has at most
/// `MAX_POINTER` bytes and ends at a segment. It points to a value that
/// holds the value at `at`.
fn cap(at: &str) -> &str {
    if at.len() <= MAX_POINTER {
        return at;
    }
    let end = at.as_bytes()[..=MAX_POINTER]
        .iter()
        .rposition(|&byte| byte == b'/')
        .unwrap_or(0);
    &at[..end]
}

/// Tell if `node`, or a node below it, can hold a persistent id that the
/// state does not show as a reference: a persistent id, or an opaque node
/// that can hold one.
fn holds_hidden_reference(node: &Node) -> bool {
    node.walk().any(|node| match node {
        Node::Persistent(_) => true,
        Node::Opaque(opaque) => can_hold_reference(&opaque.bytes),
        _ => false,
    })
}

/// Tell if the bytes of an opaque node can hold a persistent id: they hold
/// the byte of BINPERSID or of PERSID. The byte can also be part of an
/// argument, so the test can say yes for a node without one.
fn can_hold_reference(bytes: &[u8]) -> bool {
    bytes.iter().any(|&byte| byte == b'Q' || byte == b'P')
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
        Node::List(list) => list.items().collect(),
        _ => Vec::new(),
    }
}

/// Return the entries of a dict node, in order.
pub(crate) fn dict_entries(node: &Node) -> Vec<&(Node, Node)> {
    match node {
        Node::Dict(dict) => dict.entries().collect(),
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
    /// The holder of the places where a reference that the state does not
    /// show can be.
    hidden: Option<String>,
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
            hidden: None,
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
                let mut call = match self.resolver.resolve(&reduce.callable) {
                    Some(Node::Global(global)) => {
                        let (name, whole) = self.qualified(global);
                        tagged(
                            "$call",
                            name,
                            whole,
                            global.module.len() + 1 + global.name.len(),
                        )
                    }
                    _ => json!({ "$call": "" }),
                };
                let len = at.len();
                push_segment(at, "args");
                call["args"] = self.value(&reduce.args, at, depth + 1);
                at.truncate(len);
                call
            }
            Node::Get(get) => match self.resolver.resolve(node) {
                None => json!({ "$memo": get.index }),
                Some(value) => self.expand(value, get.index, at, depth + 1),
            },
            Node::Opaque(opaque) => {
                if can_hold_reference(&opaque.bytes) {
                    self.hide(at);
                }
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
            // The converter does not read this persistent id, so it can be
            // a reference that the state does not show.
            self.hide(at);
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
        // The state does not show the parts of the id, and a part can hold
        // another persistent id.
        if holds_hidden_reference(pid) {
            self.hide(at);
        }
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
    /// the references below it with the pointer of the placeholder. When
    /// the value can hold a reference that it does not show, the
    /// placeholder is a place of such references.
    fn elided(&mut self, node: &'t Node, at: &str, why: &str) -> Value {
        if self.gets == 0 {
            for node in node.walk() {
                match node {
                    Node::Persistent(pid) => match target(self.resolver, pid) {
                        Some(target) if !target.weak => {
                            let at = self.pointer(at);
                            self.references.push(Reference {
                                oid: target.oid,
                                at,
                            });
                        }
                        // The walk goes on into the parts of the id.
                        Some(_) => {}
                        None => self.hide(at),
                    },
                    Node::Opaque(opaque) if can_hold_reference(&opaque.bytes) => self.hide(at),
                    _ => {}
                }
            }
        }
        json!({ "$elided": why })
    }

    /// Make the value at `at` a place of the references that the state
    /// does not show. A get adds no place: the place of the value that it
    /// stands for is one already.
    fn hide(&mut self, at: &str) {
        if self.gets == 0 {
            narrow(&mut self.hidden, cap(at));
        }
    }
}

#[cfg(test)]
mod tests {
    use zoe_pickle::testing::{record_with_put, reference_to_put as reference};

    use super::*;

    /// Decode a record whose class pickle is GLOBAL m C, BINPUT 0, and
    /// return its state as JSON and its references.
    fn decode(state: &[u8]) -> (Value, Vec<Reference>) {
        let (state, found) = decode_found(state);
        (state, found.references)
    }

    /// Like `decode`, with all that the converter finds out.
    fn decode_found(state: &[u8]) -> (Value, Found) {
        super::state(&zoe_pickle::decode_record(&record(state)).unwrap())
    }

    /// A record whose class pickle is GLOBAL m C, BINPUT 0, with the state
    /// pickle `state` without its STOP.
    fn record(state: &[u8]) -> Vec<u8> {
        record_with_put("m", "C", state)
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
    fn a_call_name_past_the_text_budget_shows_its_length() {
        // Gets of one str of MAX_TEXT bytes spend the text budget. Then a
        // set has no room for the name of its constructor.
        let mut state = [&b"(X"[..], &(MAX_TEXT as u32).to_le_bytes()].concat();
        state.extend(b"a".repeat(MAX_TEXT));
        state.extend(b"q\x01");
        state.extend(b"h\x01".repeat(TEXT_BUDGET / MAX_TEXT));
        state.extend(b"cbuiltins\nset\n)\x85R");
        state.push(b't');
        let (state, _) = decode(&state);
        let call = state.as_array().unwrap().last().unwrap();
        assert_eq!(call, &json!({"$call": "", "length": 12, "args": [[]]}));
        let (whole, _) = decode(b"cbuiltins\nset\n)\x85R");
        assert_eq!(whole, json!({"$call": "builtins.set", "args": [[]]}));
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

    /// The pickle of a plain object of the class m.Plain, as NEWOBJ and
    /// BUILD write it, whose attribute `x` holds the pickle `value`.
    fn plain(value: &[u8]) -> Vec<u8> {
        [&b"cm\nPlain\n)\x81}X\x01\x00\x00\x00x"[..], value, b"sb"].concat()
    }

    /// The oids of the strong references of a record, as Loupe reads them.
    fn oids(record: &[u8]) -> Vec<Oid> {
        crate::strong_references(record).unwrap()
    }

    #[test]
    fn a_reference_inside_an_opaque_node_points_to_the_node() {
        // A plain object holds the persistent object 6. Pickle keeps the
        // object as an opaque node, so the state does not show 6.
        let state = [
            &b"}(X\x01\x00\x00\x00a"[..],
            &reference(5),
            b"X\x01\x00\x00\x00b",
            &plain(&reference(6)),
            b"X\x01\x00\x00\x00c",
            &reference(7),
            b"u",
        ]
        .concat();
        let (json, found) = decode_found(&state);
        assert_eq!(json["b"]["$opaque"], json!("NEWOBJ"), "{json}");
        assert_eq!(references(&found.references), [(5, "/a"), (7, "/c")]);
        let oids = oids(&record(&state));
        assert_eq!(oids, [Oid(5), Oid(6), Oid(7)]);
        let placed = found.place(&oids);
        assert_eq!(references(&placed), [(5, "/a"), (6, "/b"), (7, "/c")]);
    }

    #[test]
    fn a_reference_in_one_of_several_opaque_nodes_points_to_a_holder() {
        // Two plain objects, side by side in a list, each hold a reference.
        // No reference that the state shows lies between them, so each
        // reference points to the list that holds both.
        let state = [
            &b"}X\x01\x00\x00\x00l](X\x01\x00\x00\x00a"[..],
            &plain(&reference(6)),
            &plain(&reference(7)),
            b"es",
        ]
        .concat();
        let (json, found) = decode_found(&state);
        assert!(json["l"].is_array(), "{json}");
        let placed = found.place(&oids(&record(&state)));
        assert_eq!(references(&placed), [(6, "/l"), (7, "/l")]);
    }

    #[test]
    fn the_holder_of_pointers_is_their_common_value() {
        let holder = |pointers: &[&str]| {
            let mut holder = None;
            for pointer in pointers {
                narrow(&mut holder, pointer);
            }
            holder.unwrap_or_default()
        };
        assert_eq!(holder(&["/a/b"]), "/a/b");
        assert_eq!(holder(&["/a/b", "/a/c/0"]), "/a");
        assert_eq!(holder(&["/a/b", "/a"]), "/a");
        assert_eq!(holder(&["/ab", "/ac"]), "");
        assert_eq!(holder(&["/a", ""]), "");
        assert_eq!(holder(&[]), "");
        let long = format!("/{}/b", "a".repeat(MAX_POINTER));
        assert_eq!(cap(&long), "");
        assert_eq!(cap("/a/b"), "/a/b");
    }

    /// Tell if the value at `at` in `state` holds a reference to `oid`: a
    /// strong `$ref` of the oid, or a value that can hide a reference.
    fn holds(state: &Value, at: &str, oid: Oid) -> bool {
        let Some(value) = state.pointer(at) else {
            return false;
        };
        let mut stack = vec![value];
        while let Some(value) = stack.pop() {
            match value {
                Value::Object(map) => {
                    let strong =
                        map.get("$ref") == Some(&json!(oid_repr(oid))) && map.get("weak").is_none();
                    let hides = ["$opaque", "$persistent", "$elided"]
                        .iter()
                        .any(|tag| map.contains_key(*tag));
                    if strong || hides {
                        return true;
                    }
                    stack.extend(map.values());
                }
                Value::Array(items) => stack.extend(items),
                _ => {}
            }
        }
        false
    }

    #[test]
    fn a_reference_through_memo_gets_into_an_opaque_node_points_to_a_holder() {
        // As ZODB writes {a: Holder(y), m: z, b: y, c: Holder(w)}: the
        // first reference to y memoizes its oid and its class inside the
        // opaque Holder, and the second one reads them with gets.
        let first_y = b"C\x08\0\0\0\0\0\0\0\x01q\x09cm\nC\nq\x0a\x86Q";
        let state = [
            &b"}X\x04\x00\x00\x00data}(X\x01\x00\x00\x00a"[..],
            &plain(first_y),
            b"X\x01\x00\x00\x00m",
            &reference(2),
            b"X\x01\x00\x00\x00b",
            b"h\x09h\x0a\x86Q",
            b"X\x01\x00\x00\x00c",
            &plain(&reference(3)),
            b"us",
        ]
        .concat();
        let (json, found) = decode_found(&state);
        assert!(json["data"]["b"]["$persistent"].is_array(), "{json}");
        let oids = oids(&record(&state));
        assert_eq!(oids, [Oid(1), Oid(2), Oid(1), Oid(3)]);
        let placed = found.place(&oids);
        assert_eq!(
            references(&placed),
            [(1, "/data"), (2, "/data/m"), (1, "/data"), (3, "/data")]
        );
        for reference in &placed {
            assert!(holds(&json, &reference.at, reference.oid), "{reference:?}");
        }
    }

    #[test]
    fn the_same_oid_shown_once_and_hidden_once_points_to_true_places() {
        // 5 inside a plain object, 5 as a reference, 6 inside a plain
        // object. The state shows one 5. The other 5 and the 6 point to a
        // value that holds both plain objects.
        let state = [
            &b"("[..],
            &plain(&reference(5)),
            &reference(5),
            &plain(&reference(6)),
            b"t",
        ]
        .concat();
        let (json, found) = decode_found(&state);
        let placed = found.place(&oids(&record(&state)));
        assert_eq!(references(&placed), [(5, "/1"), (5, ""), (6, "")]);
        for reference in &placed {
            assert!(holds(&json, &reference.at, reference.oid), "{reference:?}");
        }
    }

    #[test]
    fn a_reference_in_the_class_pickle_points_to_the_root() {
        // An instance of a persistent class: its class pickle is a
        // persistent id. The state does not hold it.
        let record = [
            &b"\x80\x03C\x08\0\0\0\0\0\0\0\x09cm\nK\n\x86Q.\x80\x03}X\x01\x00\x00\x00a"[..],
            &[b'C', 8, 0, 0, 0, 0, 0, 0, 0, 5],
            b"cm\nC\n\x86Qs.",
        ]
        .concat();
        let tree = zoe_pickle::decode_record(&record).unwrap();
        let (_, found) = super::state(&tree);
        let placed = found.place(&oids(&record));
        assert_eq!(references(&placed), [(9, ""), (5, "/a")]);
    }

    #[test]
    fn weak_references_and_other_databases_are_not_references() {
        // A weak reference and a reference to another database, inside
        // and outside an opaque node.
        let weak = b"(X\x01\x00\x00\x00w(C\x08\0\0\0\0\0\0\0\x05tlQ".to_vec();
        let other = b"(X\x01\x00\x00\x00n(X\x02\x00\x00\x00dbC\x08\0\0\0\0\0\0\0\x06tlQ".to_vec();
        let state = [&b"("[..], &weak, &other, &plain(&weak), &reference(7), b"t"].concat();
        let (_, found) = decode_found(&state);
        let placed = found.place(&oids(&record(&state)));
        assert_eq!(references(&placed), [(7, "/3")]);
    }
}
