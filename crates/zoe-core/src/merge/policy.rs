//! The merge policy (ADR 0046): the persistent object
//! `root['zoe.merge_policy']`, of the class `zoe.policy.MergePolicy`.
//!
//! The state of the policy holds plain values only:
//!
//! ```text
//! {"version": 3, "structural_merges": "on", "posting_dicts": "on",
//!  "split_merges": "on", "folder_orders": "on",
//!  "classes": {"module.Class": {"tier1": "shadow", "tier2": "on",
//!              "default": "strict", "fields": {"field": "max", "data": "keys"},
//!              "coupled": [["start", "end"]],
//!              "options": {"append": {"state": "review_state"}}}}}
//! ```
//!
//! [`Rules`] gives the settings of a vote: a switch of the policy wins
//! over the key of the store, and a switch that the policy does not give
//! keeps the value of the key. `split_merges`, the switch of the split
//! merge (ADR 0048), has no key of the store: without it in the policy,
//! the split merge is off. The same holds for `folder_orders`, the switch
//! of the order merge (ADR 0052).

use std::collections::BTreeMap;
use std::sync::Arc;

use zoe_pickle::Limits;
use zoe_pickle::python::{Heap, Object, Value};

use super::{Mode, StructuralMerges};
use crate::backend::Backend;
use crate::error::Result;
use crate::types::{Oid, Tid};

/// The name of the policy in the root mapping.
pub const ROOT_NAME: &str = "zoe.merge_policy";

/// The class of the policy, as `module` and `name`.
const CLASS: (&str, &str) = ("zoe.policy", "MergePolicy");

/// What tier 2 does with a field that both sides changed to different
/// values.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Strategy {
    /// Refuse.
    Strict,
    /// The value of the transaction that votes.
    Lww,
    /// The greater of the two values.
    Max,
    /// `committed + new - old`, for ints.
    Sum,
    /// The items of both sides, each once.
    Union,
    /// The committed items, then the items that the new side added after
    /// the old value.
    Append,
    /// A dict, merged key by key.
    Keys,
}

impl Strategy {
    /// Returns the name of the strategy.
    pub fn as_str(self) -> &'static str {
        match self {
            Strategy::Strict => "strict",
            Strategy::Lww => "lww",
            Strategy::Max => "max",
            Strategy::Sum => "sum",
            Strategy::Union => "union",
            Strategy::Append => "append",
            Strategy::Keys => "keys",
        }
    }

    fn parse(text: &str) -> Option<Self> {
        Some(match text {
            "strict" => Strategy::Strict,
            "lww" => Strategy::Lww,
            "max" => Strategy::Max,
            "sum" => Strategy::Sum,
            "union" => Strategy::Union,
            "append" => Strategy::Append,
            "keys" => Strategy::Keys,
            _ => return None,
        })
    }
}

/// The entry of one class in the policy.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClassPolicy {
    /// The mode of tier 1 for the class, if the entry gives one.
    pub tier1: Option<Mode>,
    /// The mode of tier 2 for the class. `Off` when the entry gives none.
    pub tier2: Mode,
    /// The strategy of a field that `fields` does not name.
    pub default: Strategy,
    /// The strategy of each field that the entry names.
    pub fields: BTreeMap<String, Strategy>,
    /// The groups of coupled fields.
    pub coupled: Vec<Vec<String>>,
    /// True for a class that never merges in tier 2, whatever `tier2`
    /// says: `zoe.plone` blocks a class whose invariant it cannot read.
    pub blocked: bool,
    /// The option `state` of `append`: the key of the state in each item
    /// of a field of strategy `append` (ADR 0046, section 12). `None` when
    /// the entry does not give it.
    pub append_state: Option<String>,
}

impl ClassPolicy {
    /// Returns the strategy of `field`.
    pub fn strategy(&self, field: &str) -> Strategy {
        self.fields.get(field).copied().unwrap_or(self.default)
    }
}

/// A merge policy.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Policy {
    /// The version: each write adds 1.
    pub version: u64,
    /// The mode of tier 1, if the policy gives one.
    pub structural_merges: Option<Mode>,
    /// The mode of the rules of ADR 0042, if the policy gives one.
    pub posting_dicts: Option<Mode>,
    /// The mode of the split merge of ADR 0048, if the policy gives one.
    pub split_merges: Option<Mode>,
    /// The mode of the order merge of ADR 0052, if the policy gives one.
    pub folder_orders: Option<Mode>,
    /// The entry of each class, by its `module.name`.
    pub classes: BTreeMap<String, ClassPolicy>,
}

impl Policy {
    /// Reads the record of a policy. Returns why when the record does not
    /// decode, is of another class, or breaks the rules of the policy.
    pub fn read(record: &[u8]) -> std::result::Result<Policy, &'static str> {
        let mut heap = Heap::new();
        let (class, state) = load(&mut heap, record)?;
        if heap.class(class) != Some(CLASS) {
            return Err("a record of another class than zoe.policy.MergePolicy");
        }
        let mut policy = Policy {
            version: 0,
            structural_merges: None,
            posting_dicts: None,
            split_merges: None,
            folder_orders: None,
            classes: BTreeMap::new(),
        };
        let mut version = None;
        for (key, value) in str_dict(&heap, state).ok_or("a policy state that is not a dict")? {
            if value == Value::None {
                continue;
            }
            match key {
                "version" => match value {
                    Value::Int(int) if int >= 0 => {
                        version = Some(u64::try_from(int).map_err(|_| "a version too large")?);
                    }
                    _ => return Err("a version that is not an int of 0 or more"),
                },
                "structural_merges" => policy.structural_merges = Some(mode(&heap, value)?),
                "posting_dicts" => policy.posting_dicts = Some(mode(&heap, value)?),
                "split_merges" => policy.split_merges = Some(mode(&heap, value)?),
                "folder_orders" => policy.folder_orders = Some(mode(&heap, value)?),
                "classes" => {
                    let entries = str_dict(&heap, value).ok_or("classes that are not a dict")?;
                    for (name, entry) in entries {
                        if !name.contains('.') {
                            return Err("a class that is not module.name");
                        }
                        policy
                            .classes
                            .insert(name.to_owned(), class_policy(&heap, entry)?);
                    }
                }
                _ => return Err("a policy with an unknown key"),
            }
        }
        policy.version = version.ok_or("a policy without a version")?;
        Ok(policy)
    }

    /// Tells if the policy sets a shadow mode.
    pub fn has_shadow(&self) -> bool {
        self.structural_merges == Some(Mode::Shadow)
            || self.posting_dicts == Some(Mode::Shadow)
            || self.split_merges == Some(Mode::Shadow)
            || self.folder_orders == Some(Mode::Shadow)
            || self.classes.values().any(|entry| {
                entry.tier1 == Some(Mode::Shadow) || (entry.tier2 == Mode::Shadow && !entry.blocked)
            })
    }
}

/// Returns the oid of the policy that the record of the root names under
/// [`ROOT_NAME`], or `None` when it names none. The root is a
/// `PersistentMapping`: its state is `{'data': {name: value}}`.
pub fn policy_oid(root: &[u8]) -> std::result::Result<Option<Oid>, &'static str> {
    // The root can hold objects of other classes beside the policy.
    let mut heap = Heap::with_instances();
    let (_, state) = load(&mut heap, root)?;
    let fields = str_dict(&heap, state).ok_or("a root state that is not a dict")?;
    let Some(&(_, data)) = fields
        .iter()
        .find(|(name, _)| *name == "data" || *name == "_container")
    else {
        return Err("a root state without data");
    };
    let names = str_dict(&heap, data).ok_or("a root mapping that is not a dict")?;
    let Some(&(_, value)) = names.iter().find(|(name, _)| *name == ROOT_NAME) else {
        return Ok(None);
    };
    let reference = heap
        .reference(value)
        .ok_or("a policy that is not a persistent object")?;
    if reference.database.is_some() || reference.weak {
        return Err("a policy in another database, or a weak reference");
    }
    let oid: [u8; 8] = reference.oid[..]
        .try_into()
        .map_err(|_| "a policy oid that is not 8 bytes")?;
    Ok(Some(Oid(u64::from_be_bytes(oid))))
}

/// The settings of one vote: the keys of the store and the policy, if the
/// store has one.
#[derive(Clone, Copy, Debug)]
pub struct Rules<'a> {
    /// The keys of the store.
    pub config: &'a StructuralMerges,
    /// The policy of the store.
    pub policy: Option<&'a Policy>,
}

impl Rules<'_> {
    /// Returns the mode of tier 1 for the class `module.name`: the `tier1`
    /// of its entry, else `structural-merge-class`, else the
    /// `structural_merges` of the policy, else `structural-merges`.
    pub fn tier1_mode(&self, class: &str) -> Mode {
        let entry = self
            .policy
            .and_then(|policy| policy.classes.get(class))
            .and_then(|entry| entry.tier1);
        entry
            .or_else(|| self.config.classes.get(class).copied())
            .or_else(|| self.policy.and_then(|policy| policy.structural_merges))
            .unwrap_or(self.config.mode)
    }

    /// Returns the mode of the rules of ADR 0042.
    pub fn posting_dicts(&self) -> Mode {
        self.policy
            .and_then(|policy| policy.posting_dicts)
            .unwrap_or(self.config.posting_dicts)
    }

    /// Returns the mode of the split merge of ADR 0048: the `split_merges`
    /// of the policy, else `Off`. The rules of the split merge run only
    /// where the mode of tier 1 of the class is not `Off` too.
    pub fn split_merges(&self) -> Mode {
        self.policy
            .and_then(|policy| policy.split_merges)
            .unwrap_or(Mode::Off)
    }

    /// Returns the mode of the order merge of ADR 0052: the
    /// `folder_orders` of the policy, else `Off`. The merge renumbers the
    /// records of the positions of a folder only where the mode of tier 1
    /// of their class is not `Off` too.
    pub fn folder_orders(&self) -> Mode {
        self.policy
            .and_then(|policy| policy.folder_orders)
            .unwrap_or(Mode::Off)
    }

    /// Returns the entry of the class `module.name` when tier 2 runs on it:
    /// its mode is `On` or `Shadow`, and it is not blocked.
    pub fn tier2(&self, class: &str) -> Option<&ClassPolicy> {
        self.policy?
            .classes
            .get(class)
            .filter(|entry| entry.tier2 != Mode::Off && !entry.blocked)
    }

    /// Returns the version of the policy.
    pub fn version(&self) -> Option<u64> {
        self.policy.map(|policy| policy.version)
    }
}

/// The policy that a store read last, with the serials that it read it
/// at. The store reads it again when a serial changed.
#[derive(Debug, Default)]
pub(crate) struct PolicyCache {
    /// The serial of the root record and of the policy record at the last
    /// read, or `None` before the first read.
    key: Option<(Option<Tid>, Option<Tid>)>,
    /// The oid of the policy that the root named at the last read.
    oid: Option<Oid>,
    /// The policy of the last read, if it read one.
    policy: Option<Arc<Policy>>,
}

impl PolicyCache {
    /// Returns the policy of the store. Reads the current serials of the
    /// root and of the policy, and reads the policy again when one of them
    /// changed. A root or a policy that does not read counts as no policy:
    /// the store writes one warning for each serial of it. `config` tells
    /// if the skin writes the resolution log.
    pub(crate) fn get(
        &mut self,
        backend: &dyn Backend,
        config: &StructuralMerges,
    ) -> Result<Option<Arc<Policy>>> {
        let mut oids = std::iter::once(Oid(0)).chain(self.oid);
        let serials = backend.current_serials(&mut oids)?;
        let key = (
            serials.first().copied().flatten(),
            serials.get(1).copied().flatten(),
        );
        if self.key == Some(key) {
            return Ok(self.policy.clone());
        }
        let (root, record) = self.reload(backend)?;
        self.key = Some((root, record));
        if let Some(policy) = &self.policy
            && policy.has_shadow()
            && !config.resolution_log
        {
            tracing::warn!(
                version = policy.version,
                "the merge policy sets a shadow mode, and no resolution log tells what it finds"
            );
        }
        Ok(self.policy.clone())
    }

    /// Reads the root and the policy again. Returns the serials of the two
    /// records that it read.
    fn reload(&mut self, backend: &dyn Backend) -> Result<(Option<Tid>, Option<Tid>)> {
        self.oid = None;
        self.policy = None;
        let Some(root) = backend.load_before(Oid(0), Tid::MAX)? else {
            return Ok((None, None));
        };
        let oid = match policy_oid(&root.data) {
            Ok(Some(oid)) => oid,
            Ok(None) => return Ok((Some(root.tid), None)),
            Err(why) => {
                if !root.data.is_empty() {
                    tracing::warn!(
                        serial = root.tid.0,
                        why,
                        "the root does not read: no merge policy"
                    );
                }
                return Ok((Some(root.tid), None));
            }
        };
        self.oid = Some(oid);
        let Some(record) = backend.load_before(oid, Tid::MAX)? else {
            return Ok((Some(root.tid), None));
        };
        match Policy::read(&record.data) {
            Ok(policy) => self.policy = Some(Arc::new(policy)),
            Err(why) => {
                tracing::warn!(
                    oid = oid.0,
                    serial = record.tid.0,
                    why,
                    "the merge policy does not read: the store runs without it"
                );
            }
        }
        Ok((Some(root.tid), Some(record.tid)))
    }
}

/// Decodes a record and loads its class and its state. The budget is the
/// length of the record.
fn load(heap: &mut Heap, record: &[u8]) -> std::result::Result<(Value, Value), &'static str> {
    let limits = Limits {
        node_budget: record.len(),
    };
    let tree = zoe_pickle::decode_record_with(record, limits)
        .map_err(|_| "a record that does not decode")?;
    heap.load(&tree)
        .map_err(|_| "a record of values that a policy does not hold")
}

/// Returns the items of a dict whose keys are all str.
fn str_dict(heap: &Heap, value: Value) -> Option<Vec<(&str, Value)>> {
    heap.dict(value)?
        .iter()
        .map(|&(key, value)| Some((heap.str(key)?, value)))
        .collect()
}

/// Reads a mode: `on`, `shadow` or `off`.
fn mode(heap: &Heap, value: Value) -> std::result::Result<Mode, &'static str> {
    heap.str(value)
        .and_then(|text| text.parse().ok())
        .ok_or("a mode that is not on, shadow or off")
}

/// Reads the entry of one class.
fn class_policy(heap: &Heap, value: Value) -> std::result::Result<ClassPolicy, &'static str> {
    let mut entry = ClassPolicy {
        tier1: None,
        tier2: Mode::Off,
        default: Strategy::Strict,
        fields: BTreeMap::new(),
        coupled: Vec::new(),
        blocked: false,
        append_state: None,
    };
    let strategy = |value: Value| {
        heap.str(value)
            .and_then(Strategy::parse)
            .ok_or("a strategy that is not strict, lww, max, sum, union, append or keys")
    };
    for (key, value) in str_dict(heap, value).ok_or("a class entry that is not a dict")? {
        if value == Value::None {
            continue;
        }
        match key {
            "blocked" => match value {
                Value::Bool(blocked) => entry.blocked = blocked,
                _ => return Err("a blocked that is not True or False"),
            },
            "tier1" => entry.tier1 = Some(mode(heap, value)?),
            "tier2" => entry.tier2 = mode(heap, value)?,
            "default" => entry.default = strategy(value)?,
            "fields" => {
                for (field, value) in str_dict(heap, value).ok_or("fields that are not a dict")? {
                    entry.fields.insert(field.to_owned(), strategy(value)?);
                }
            }
            "coupled" => {
                for group in sequence(heap, value).ok_or("coupled that is not a list")? {
                    let fields: Option<Vec<String>> = sequence(heap, *group)
                        .and_then(|items| {
                            items
                                .iter()
                                .map(|&item| heap.str(item).map(str::to_owned))
                                .collect()
                        })
                        .filter(|fields: &Vec<String>| !fields.is_empty());
                    entry
                        .coupled
                        .push(fields.ok_or("a coupled group that is not a list of fields")?);
                }
            }
            "options" => {
                for (name, options) in str_dict(heap, value).ok_or("options that are not a dict")? {
                    if name != "append" {
                        return Err("options of a strategy that takes none");
                    }
                    let options = str_dict(heap, options).ok_or("options that are not a dict")?;
                    for (option, value) in options {
                        if option != "state" {
                            return Err("an option that append does not take");
                        }
                        let state = heap
                            .str(value)
                            .filter(|state| !state.is_empty())
                            .ok_or("a state that is not a non-empty str")?;
                        entry.append_state = Some(state.to_owned());
                    }
                }
            }
            _ => return Err("a class entry with an unknown key"),
        }
    }
    Ok(entry)
}

/// Returns the items of a list or a tuple.
fn sequence(heap: &Heap, value: Value) -> Option<&[Value]> {
    match heap.object(value)? {
        Object::List(items) | Object::Tuple(items) => Some(items),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zoe_pickle::python::Pickler;

    /// Returns the record of a policy of `state`, built by `make`.
    fn record(make: impl FnOnce(&mut Heap) -> Value) -> Vec<u8> {
        let mut heap = Heap::new();
        let class = heap.new_class(CLASS.0, CLASS.1);
        let state = make(&mut heap);
        let mut pickler = Pickler::new(&heap);
        pickler.dump(class).unwrap();
        pickler.dump(state).unwrap();
        pickler.into_bytes()
    }

    fn text(heap: &mut Heap, text: &str) -> Value {
        heap.new_str(text)
    }

    #[test]
    fn a_policy_reads_with_its_classes_and_switches() {
        let record = record(|heap| {
            let fields = {
                let items = vec![
                    (text(heap, "modification_date"), text(heap, "max")),
                    (text(heap, "data"), text(heap, "keys")),
                ];
                heap.new_dict(items)
            };
            let group = {
                let start = text(heap, "start");
                let end = text(heap, "end");
                heap.new_tuple(vec![start, end])
            };
            let coupled = heap.new_tuple(vec![group]);
            let entry = {
                let items = vec![
                    (text(heap, "tier2"), text(heap, "on")),
                    (text(heap, "fields"), fields),
                    (text(heap, "coupled"), coupled),
                ];
                heap.new_dict(items)
            };
            let classes = {
                let name = text(heap, "m.Document");
                heap.new_dict(vec![(name, entry)])
            };
            let items = vec![
                (text(heap, "version"), Value::Int(3)),
                (text(heap, "posting_dicts"), text(heap, "shadow")),
                (text(heap, "split_merges"), text(heap, "on")),
                (text(heap, "folder_orders"), text(heap, "shadow")),
                (text(heap, "classes"), classes),
            ];
            heap.new_dict(items)
        });
        let policy = Policy::read(&record).unwrap();
        assert_eq!(policy.version, 3);
        assert_eq!(policy.posting_dicts, Some(Mode::Shadow));
        assert_eq!(policy.split_merges, Some(Mode::On));
        assert_eq!(policy.folder_orders, Some(Mode::Shadow));
        assert!(policy.has_shadow());
        let entry = &policy.classes["m.Document"];
        assert_eq!(entry.tier2, Mode::On);
        assert_eq!(entry.strategy("modification_date"), Strategy::Max);
        assert_eq!(entry.strategy("data"), Strategy::Keys);
        assert_eq!(entry.strategy("title"), Strategy::Strict);
        assert_eq!(entry.coupled, [["start", "end"]]);
        // The policy wins over the keys, and a switch that it does not give
        // keeps its key.
        let config = StructuralMerges::default();
        let rules = Rules {
            config: &config,
            policy: Some(&policy),
        };
        assert_eq!(rules.posting_dicts(), Mode::Shadow);
        assert_eq!(rules.split_merges(), Mode::On);
        assert_eq!(rules.folder_orders(), Mode::Shadow);
        assert_eq!(rules.tier1_mode("BTrees.IOBTree.IOBucket"), Mode::On);
        assert!(rules.tier2("m.Document").is_some());
        assert!(rules.tier2("m.Other").is_none());
    }

    #[test]
    fn a_blocked_class_never_merges_in_tier_2() {
        let record = record(|heap| {
            let entry = {
                let items = vec![
                    (text(heap, "tier2"), text(heap, "on")),
                    (text(heap, "blocked"), Value::Bool(true)),
                ];
                heap.new_dict(items)
            };
            let classes = {
                let name = text(heap, "m.Document");
                heap.new_dict(vec![(name, entry)])
            };
            let items = vec![
                (text(heap, "version"), Value::Int(1)),
                (text(heap, "classes"), classes),
            ];
            heap.new_dict(items)
        });
        let policy = Policy::read(&record).unwrap();
        assert!(policy.classes["m.Document"].blocked);
        let config = StructuralMerges::default();
        let rules = Rules {
            config: &config,
            policy: Some(&policy),
        };
        assert!(rules.tier2("m.Document").is_none());
    }

    #[test]
    fn a_policy_that_breaks_its_rules_does_not_read() {
        for (key, value) in [
            ("version", "x"),
            ("posting_dicts", "sometimes"),
            ("split_merges", "sometimes"),
            ("colour", "on"),
        ] {
            let record = record(|heap| {
                let items = vec![(text(heap, key), text(heap, value))];
                heap.new_dict(items)
            });
            assert!(Policy::read(&record).is_err(), "{key}");
        }
        // A version is needed.
        let record = record(|heap| heap.new_dict(Vec::new()));
        assert_eq!(Policy::read(&record), Err("a policy without a version"));
    }

    /// Returns the record of a policy with one class, `m.History`, whose
    /// entry gives the strategy `append` and `options`, built by `make`.
    fn with_options(make: impl FnOnce(&mut Heap) -> Value) -> Vec<u8> {
        record(|heap| {
            let options = make(heap);
            let entry = {
                let items = vec![
                    (text(heap, "tier2"), text(heap, "on")),
                    (text(heap, "default"), text(heap, "append")),
                    (text(heap, "options"), options),
                ];
                heap.new_dict(items)
            };
            let classes = {
                let name = text(heap, "m.History");
                heap.new_dict(vec![(name, entry)])
            };
            let items = vec![
                (text(heap, "version"), Value::Int(1)),
                (text(heap, "classes"), classes),
            ];
            heap.new_dict(items)
        })
    }

    /// Returns `{strategy: {option: value}}`.
    fn options(heap: &mut Heap, strategy: &str, option: &str, value: Value) -> Value {
        let inner = {
            let option = text(heap, option);
            heap.new_dict(vec![(option, value)])
        };
        let strategy = text(heap, strategy);
        heap.new_dict(vec![(strategy, inner)])
    }

    #[test]
    fn an_entry_reads_the_option_state_of_append() {
        let read = with_options(|heap| {
            let state = text(heap, "review_state");
            options(heap, "append", "state", state)
        });
        let policy = Policy::read(&read).unwrap();
        let entry = &policy.classes["m.History"];
        assert_eq!(
            entry.strategy("simple_publication_workflow"),
            Strategy::Append
        );
        assert_eq!(entry.append_state.as_deref(), Some("review_state"));
        // An entry without options has no state.
        let plain = record(|heap| {
            let entry = {
                let items = vec![(text(heap, "default"), text(heap, "append"))];
                heap.new_dict(items)
            };
            let classes = {
                let name = text(heap, "m.History");
                heap.new_dict(vec![(name, entry)])
            };
            let items = vec![
                (text(heap, "version"), Value::Int(1)),
                (text(heap, "classes"), classes),
            ];
            heap.new_dict(items)
        });
        let policy = Policy::read(&plain).unwrap();
        assert_eq!(policy.classes["m.History"].append_state, None);
    }

    #[test]
    fn options_that_break_their_rules_do_not_read() {
        // Each case: the reason, and the options that it reads.
        type Case = (&'static str, fn(&mut Heap) -> Value);
        let cases: [Case; 5] = [
            ("options of a strategy that takes none", |heap| {
                let state = text(heap, "review_state");
                options(heap, "union", "state", state)
            }),
            ("an option that append does not take", |heap| {
                let state = text(heap, "review_state");
                options(heap, "append", "colour", state)
            }),
            ("a state that is not a non-empty str", |heap| {
                options(heap, "append", "state", Value::Int(1))
            }),
            ("a state that is not a non-empty str", |heap| {
                let state = text(heap, "");
                options(heap, "append", "state", state)
            }),
            ("options that are not a dict", |heap| text(heap, "append")),
        ];
        for (why, make) in cases {
            assert_eq!(Policy::read(&with_options(make)), Err(why));
        }
    }

    #[test]
    fn the_mode_of_a_class_follows_the_order_of_precedence() {
        let mut config = StructuralMerges::default();
        let mut policy = Policy {
            version: 1,
            structural_merges: Some(Mode::Shadow),
            posting_dicts: None,
            split_merges: None,
            folder_orders: None,
            classes: BTreeMap::new(),
        };
        let class = "BTrees.Length.Length";
        let rules = |config: &StructuralMerges, policy: &Policy| {
            Rules {
                config,
                policy: Some(policy),
            }
            .tier1_mode(class)
        };
        assert_eq!(rules(&config, &policy), Mode::Shadow);
        config.classes.insert(class.to_owned(), Mode::Off);
        assert_eq!(rules(&config, &policy), Mode::Off);
        policy.classes.insert(
            class.to_owned(),
            ClassPolicy {
                tier1: Some(Mode::On),
                tier2: Mode::Off,
                default: Strategy::Strict,
                fields: BTreeMap::new(),
                coupled: Vec::new(),
                blocked: false,
                append_state: None,
            },
        );
        assert_eq!(rules(&config, &policy), Mode::On);
        let none = Rules {
            config: &config,
            policy: None,
        };
        assert_eq!(none.tier1_mode(class), Mode::Off);
        assert_eq!(none.posting_dicts(), Mode::Off);
        // Without a policy, the split merge and the order merge are off.
        assert_eq!(none.split_merges(), Mode::Off);
        assert_eq!(none.folder_orders(), Mode::Off);
    }
}
