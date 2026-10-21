//! Tiers 1 and 2 of the conflict resolution, in Rust, at the vote: the
//! structural merges of BTrees and `Length` (tier 1), and the merges of the
//! fields of the classes that the merge policy marks mergeable (tier 2,
//! `fields.rs`, ADR 0046). `policy.rs` reads the merge policy.
//!
//! [`tier1`] takes the record that a transaction read (old), the record
//! that another transaction committed since (committed) and the record of
//! the transaction (new). It gives the merged record, a conflict with its
//! reason, or no answer:
//!
//! - BTree buckets, sets, BTrees and tree sets of every family but `fs`
//!   merge with the rules of `bucket_merge` in `MergeTemplate.c` of BTrees.
//!   A BTree or a tree set merges only when it holds one bucket inline, as
//!   in BTrees.
//! - `BTrees.Length.Length` merges as its `_p_resolveConflict`:
//!   `committed + new - old`.
//! - The merged record is the record that ZODB's resolver writes, byte for
//!   byte: `zoe_pickle::python` keeps the identity of each object and
//!   pickles as zodbpickle does.
//!
//! Tier 1 never guesses. When Python would compare two values that the
//! model cannot compare (values of mixed types, for example), when a state
//! does not decode, or when a state holds a construct that the model does
//! not model, it gives no answer, and the conflict goes on as without it.
//! A refusal is a case where ZODB's resolver refuses too.
//!
//! The posting-dict rule and the first-key rule (ADR 0042) are merges
//! where ZODB's resolver refuses. One mode switches both, and it is off by
//! default. In shadow mode the rules compute their merge for the
//! resolution log, then the merge runs again without them, and that
//! outcome ends the conflict. See `posting.rs` and `btrees.rs`.
//!
//! The split merge (ADR 0048, `split.rs`) is a merge where ZODB's resolver
//! refuses too: it merges the splits of a BTree node with the buckets that
//! a transaction split, and lets a read-current node through when another
//! transaction split another of its children. The switch `split_merges`
//! of the merge policy switches it, and it is off by default.
//!
//! The order merge (ADR 0052, `order.rs`) merges the order list of a
//! folder of plone.folder and renumbers the positions beside it. The
//! switch `folder_orders` of the merge policy switches it, and it is off
//! by default.
//!
//! No Python runs here.

mod btrees;
mod fields;
pub(crate) mod order;
pub mod policy;
mod posting;
pub(crate) mod split;
mod step;

use std::collections::BTreeMap;
use std::time::SystemTime;

use zoe_pickle::Limits;
use zoe_pickle::btrees::classify;
use zoe_pickle::python::{Heap, Pickler, Value};

use crate::error;
use crate::types::{Oid, Tid};

/// What tier 1 does for a class.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// Tier 1 does not run: the conflict goes on as without it.
    Off,
    /// Tier 1 runs and its result goes to the resolution log, but the
    /// conflict goes on as without it.
    Shadow,
    /// Tier 1 runs, and its merge or its refusal is the outcome.
    On,
}

impl Mode {
    /// Returns the name of the mode: `on`, `shadow` or `off`.
    pub fn as_str(self) -> &'static str {
        match self {
            Mode::Off => "off",
            Mode::Shadow => "shadow",
            Mode::On => "on",
        }
    }

    /// Returns the lower of two modes: `Off`, then `Shadow`, then `On`.
    pub(crate) fn lower(self, other: Mode) -> Mode {
        match (self, other) {
            (Mode::Off, _) | (_, Mode::Off) => Mode::Off,
            (Mode::Shadow, _) | (_, Mode::Shadow) => Mode::Shadow,
            _ => Mode::On,
        }
    }
}

impl std::str::FromStr for Mode {
    type Err = String;

    /// Reads `on`, `shadow` or `off`, in any case.
    fn from_str(text: &str) -> Result<Self, String> {
        match text.trim().to_ascii_lowercase().as_str() {
            "on" => Ok(Mode::On),
            "shadow" => Ok(Mode::Shadow),
            "off" => Ok(Mode::Off),
            _ => Err(format!("{text:?} is not on, shadow or off")),
        }
    }
}

/// Reads the mode of one class: `CLASS=MODE`, such as
/// `BTrees.IOBTree.IOBucket=shadow`. The class is `module.name`.
pub fn class_mode(text: &str) -> Result<(String, Mode), String> {
    let (class, mode) = text
        .split_once('=')
        .ok_or_else(|| format!("{text:?} is not CLASS=MODE"))?;
    let class = class.trim();
    if class.is_empty() || !class.contains('.') {
        return Err(format!("{class:?} is not a class as module.name"));
    }
    Ok((class.to_owned(), mode.parse()?))
}

/// The settings of the structural merges (tier 1) of a store: the keys of
/// `<zoe>` and of `zoe serve`. The merge policy of the store wins over
/// them (`policy::Rules`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StructuralMerges {
    /// The mode of each class of tier 1 that `classes` does not name.
    pub mode: Mode,
    /// The mode of a class of tier 1, by its `module.name`.
    pub classes: BTreeMap<String, Mode>,
    /// The mode of the posting-dict rule and of the first-key rule of ADR
    /// 0042, inside the classes where tier 1 runs.
    pub posting_dicts: Mode,
    /// True when the skin writes the resolution log. Without it, the store
    /// warns at each version of a policy that sets a shadow mode.
    pub resolution_log: bool,
}

impl Default for StructuralMerges {
    /// Tier 1 on for every class, and the rules of ADR 0042 off.
    fn default() -> Self {
        StructuralMerges {
            mode: Mode::On,
            classes: BTreeMap::new(),
            posting_dicts: Mode::Off,
            resolution_log: false,
        }
    }
}

impl StructuralMerges {
    /// Returns the mode of the class `module.name`.
    pub fn mode(&self, class: &str) -> Mode {
        self.classes.get(class).copied().unwrap_or(self.mode)
    }
}

/// Why tier 1 refuses a merge.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reason {
    /// A reason code of BTrees, 0 to 13: the `reason` of its
    /// `BTreesConflictError`.
    BTrees(u8),
    /// Python raises when it compares two values: a persistent reference
    /// compares only with an equal one.
    Uncomparable,
    /// The posting-dict rule: one entry changed differently on both sides.
    PostingChanged,
    /// The posting-dict rule: one of the three values is not a dict.
    PostingNotDict,
    /// The posting-dict rule: the merged dict holds more than 10 entries.
    PostingTooLarge,
    /// The posting-dict rule: the merged dict is empty.
    PostingEmpty,
    /// Tier 2: both sides changed a field to different values, and its
    /// strategy is `strict`.
    FieldChanged,
    /// Tier 2: one side deleted a field that the other side changed.
    FieldDeleted,
    /// Tier 2: both sides changed a group of coupled fields.
    Coupled,
    /// Tier 2: a field of the strategy `append` that is not an append to
    /// its old value on both sides.
    NotAppend,
    /// Tier 2, the strategy `keys`: both sides changed a key of a mapping
    /// to different values.
    KeyChanged,
    /// Tier 2, the strategy `keys`: one side deleted a key of a mapping
    /// that the other side changed.
    KeyDeleted,
    /// Tier 2, the strategy `append` with the option `state`: an item that
    /// the committed side appended does not keep the state of the old
    /// value.
    StateChanged,
    /// The order merge (ADR 0052): both sides added the same id to the
    /// order of a folder.
    OrderSameId,
}

impl Reason {
    /// Returns the reason code of BTrees, or `None` for a reason of tier 1
    /// that BTrees does not have.
    pub fn code(self) -> Option<u8> {
        match self {
            Reason::BTrees(code) => Some(code),
            _ => None,
        }
    }

    /// Returns the message of the reason, as `BTreesConflictError` gives it
    /// for a code of BTrees.
    pub fn message(self) -> &'static str {
        match self {
            Reason::BTrees(code) => match code {
                0 => "Conflicting bucket split",
                1 => "Conflicting changes",
                2 | 3 => "Conflicting delete and change",
                4 => "Conflicting inserts or deletes",
                5 | 9 => "Conflicting deletes",
                6 => "Conflicting inserts",
                7 | 8 => "Conflicting deletes, or delete and change",
                10 => "Empty bucket from deleting all keys",
                11 => "Conflicting changes in an internal BTree node",
                12 => "Empty bucket in a transaction",
                13 => "Delete of first key",
                _ => "Unknown reason",
            },
            Reason::Uncomparable => "Values that Python cannot compare",
            Reason::PostingChanged => "Posting entry changed on both sides",
            Reason::PostingNotDict => "Posting value that is not a dict",
            Reason::PostingTooLarge => "Posting dict of more than 10 entries",
            Reason::PostingEmpty => "Posting dict without entries",
            Reason::FieldChanged => "Field changed on both sides",
            Reason::FieldDeleted => "Field deleted on one side and changed on the other",
            Reason::Coupled => "Coupled fields changed on both sides",
            Reason::NotAppend => "Field that is not an append to its old value",
            Reason::KeyChanged => "Key of a mapping changed on both sides",
            Reason::KeyDeleted => "Key of a mapping deleted on one side and changed on the other",
            Reason::StateChanged => "State changed by a status of the committed side",
            Reason::OrderSameId => "An id that both sides added",
        }
    }
}

/// What tier 1 gives for one conflict.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// The merged record, to store over the committed record.
    Merged(Vec<u8>),
    /// A refusal: the transaction fails with a conflict.
    Refused(Reason),
    /// No answer: the conflict goes on as without tier 1. The text says
    /// why.
    Undecided(&'static str),
}

/// The result of tier 1 or tier 2 for one conflict.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Merge {
    /// The tier: 1 or 2.
    pub tier: u8,
    /// The class of the record, as `module.name`.
    pub class: String,
    /// The outcome.
    pub outcome: Outcome,
    /// The keys that the new record changed, inserted or deleted, as text,
    /// in key order: the fields for tier 2, and the keys of the dict of a
    /// field that the strategy `keys` merged or refused, in place of that
    /// field. At most [`MAX_KEYS`]; `changed` counts them all. With the
    /// rules of ADR 0042 in shadow mode, they are the keys of the merge of
    /// the rules.
    pub keys: Vec<String>,
    /// The number of keys that the new record changed, inserted or
    /// deleted.
    pub changed: usize,
    /// With the rules of ADR 0042 in shadow mode, the outcome that the
    /// rules give when a rule compared a dict or let a first key go. The
    /// outcome of the merge is then the outcome without the rules.
    pub shadow: Option<Outcome>,
    /// For a refusal of tier 2, the fields that it names: the field whose
    /// strategy refused, or the coupled group. Empty otherwise.
    pub fields: Vec<String>,
}

/// The most keys that a [`Merge`] lists.
pub const MAX_KEYS: usize = 20;

/// The steps that the comparisons of one merge may take, for each byte of
/// its three records.
const STEPS_PER_BYTE: usize = 64;

/// Returns the step budget of a merge whose three records hold `len`
/// bytes in all. A merge of records of normal shapes takes a few steps for
/// each value; a value that shares its parts many times takes more, and
/// past the budget the merge gives no answer.
pub(crate) fn budget(len: usize) -> usize {
    len.saturating_mul(STEPS_PER_BYTE).saturating_add(4096)
}

/// Returns the pickle of a record of `class` and `state` in `heap`, or why
/// the pickler cannot write it.
fn pickle(heap: &Heap, class: Value, state: Value) -> Result<Vec<u8>, &'static str> {
    let mut pickler = Pickler::new(heap);
    match pickler.dump(class).and_then(|()| pickler.dump(state)) {
        Ok(()) => Ok(pickler.into_bytes()),
        Err(unsupported) => Err(unsupported.0),
    }
}

/// The records that a merge of a vote reads: the records that the
/// transaction stored, and the records of its store.
pub(crate) trait Records {
    /// Returns the serial that the transaction read and the pickle that it
    /// stored for `oid`, or `None` when it stored none, restored it or
    /// deleted it.
    fn staged(&self, oid: Oid) -> error::Result<Option<(Tid, Vec<u8>)>>;

    /// Returns the serial and the pickle of the current record of `oid`, or
    /// `None` when it has none or is deleted.
    fn current(&self, oid: Oid) -> error::Result<Option<(Tid, Vec<u8>)>>;

    /// Returns the pickle of the record of `oid` at `serial`.
    fn at(&self, oid: Oid, serial: Tid) -> error::Result<Option<Vec<u8>>>;
}

/// Returns the class of `record` as `module.name` when tier 1 merges that
/// class. Reads the class pickle alone.
pub fn tier1_class(record: &[u8]) -> Option<String> {
    let (module, name) = zoe_pickle::decode_class_name(record).ok()??;
    classify(&module, &name)?;
    Some(format!("{module}.{name}"))
}

/// Merges the conflict of a record of a class of tier 1: `old` is the
/// record that the transaction read, `committed` the record that another
/// transaction committed after it, `new` the record of the transaction.
/// `posting_dicts` is the mode of the rules of ADR 0042: the posting-dict
/// rule and the first-key rule.
///
/// Returns `None` when the class of `new` is not a class of tier 1.
pub fn tier1(old: &[u8], committed: &[u8], new: &[u8], posting_dicts: Mode) -> Option<Merge> {
    let (module, name) = zoe_pickle::decode_class_name(new).ok()??;
    let class = classify(&module, &name)?;
    let mut merge = Merge {
        tier: 1,
        class: format!("{module}.{name}"),
        outcome: Outcome::Undecided(""),
        keys: Vec::new(),
        changed: 0,
        shadow: None,
        fields: Vec::new(),
    };
    merge.outcome = match decode(old, committed, new) {
        Ok(trees) => btrees::merge(&trees, class, posting_dicts, &mut merge),
        Err(why) => Outcome::Undecided(why),
    };
    Some(merge)
}

/// Merges the conflict of a record whose class has the entry `entry` in the
/// merge policy, field by field (tier 2): `old`, `committed` and `new` as
/// for [`tier1`]. `class` is the class of `new`, as `module.name`.
pub fn tier2(
    old: &[u8],
    committed: &[u8],
    new: &[u8],
    class: &str,
    entry: &policy::ClassPolicy,
) -> Merge {
    let mut merge = Merge {
        tier: 2,
        class: class.to_owned(),
        outcome: Outcome::Undecided(""),
        keys: Vec::new(),
        changed: 0,
        shadow: None,
        fields: Vec::new(),
    };
    merge.outcome = match decode(old, committed, new) {
        Ok(trees) => fields::merge(&trees, entry, &mut merge),
        Err(why) => Outcome::Undecided(why),
    };
    merge
}

/// Returns the class of `record` as `module.name`, from its class pickle
/// alone.
pub fn class_name(record: &[u8]) -> Option<String> {
    zoe_pickle::decode_class_key(record).ok()?
}

/// The decoded records of one conflict, in the order of ZODB's resolver:
/// new, old, committed.
struct Trees {
    new: zoe_pickle::Tree,
    old: zoe_pickle::Tree,
    committed: zoe_pickle::Tree,
    /// The bytes of the three records, for the step budget.
    len: usize,
}

/// Decodes the three records. Each budget is the length of its record, so
/// the decoder refuses nothing that the record can hold.
fn decode(old: &[u8], committed: &[u8], new: &[u8]) -> Result<Trees, &'static str> {
    let read = |record: &[u8]| {
        let limits = Limits {
            node_budget: record.len(),
        };
        zoe_pickle::decode_record_with(record, limits).map_err(|_| "a record that does not decode")
    };
    Ok(Trees {
        new: read(new)?,
        old: read(old)?,
        committed: read(committed)?,
        len: old.len() + committed.len() + new.len(),
    })
}

/// What tier 1 or tier 2 did for one conflict of a vote: one entry of the
/// resolution log.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Resolution {
    /// The oid in conflict.
    pub oid: Oid,
    /// The tier: 1 or 2.
    pub tier: u8,
    /// The class of the record, as `module.name`.
    pub class: String,
    /// The mode of the class in its tier: `On` or `Shadow`.
    pub mode: Mode,
    /// The version of the merge policy of the vote, if the store has one.
    pub policy: Option<u64>,
    /// What the tier gave.
    pub outcome: Resolved,
    /// True when the outcome is the outcome of the vote: the vote stored
    /// the merged record and passed, or failed on the refusal. Always false
    /// in shadow mode.
    pub applied: bool,
    /// True when the entry is the merge of an undo (`Txn::undo`): the
    /// undone record is the base, and `read` is the undone tid.
    pub undo: bool,
    /// The keys that the new record changed, inserted or deleted, as text.
    /// At most [`MAX_KEYS`].
    pub keys: Vec<String>,
    /// The number of keys that the new record changed, inserted or deleted.
    pub changed: usize,
    /// The serial of the committed record.
    pub committed: Tid,
    /// The serial that the transaction read.
    pub read: Tid,
    /// With the rules of ADR 0042 in shadow mode, what the rules give when
    /// a rule compared a dict or let a first key go; `outcome` is then the
    /// outcome without the rules.
    pub shadow: Option<Resolved>,
    /// For a refusal of tier 2, the field whose strategy refused, or the
    /// coupled group. Empty otherwise.
    pub fields: Vec<String>,
    /// The rule of the split merge (ADR 0048) that gave the outcome or the
    /// shadow outcome: 1, 2 or 3. `None` for the other merges. An outcome
    /// `Merged` of rule 1 is a read-current oid that passed: the vote
    /// wrote nothing for it.
    pub split: Option<u8>,
    /// The part of the order merge (ADR 0052) that the record is: `list`
    /// for the order list of a folder, `pos` for a record of its
    /// positions. `None` for the other merges.
    pub order: Option<&'static str>,
}

/// What a tier gave for a conflict, without the merged record.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Resolved {
    /// The tier merged the records.
    Merged,
    /// The tier refused the merge.
    Refused(Reason),
    /// The tier gave no answer. The text says why.
    Undecided(&'static str),
}

/// A check of a merge of shadow mode against the resolution of the client
/// (ADR 0046): the client stored its own resolution of the oid, and the
/// vote after `vote_resolving` compared it with the merge of the tier.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Verification {
    /// The oid in conflict.
    pub oid: Oid,
    /// The tier of the merge: 1 or 2.
    pub tier: u8,
    /// The class of the record, as `module.name`.
    pub class: String,
    /// The version of the merge policy of the merge, if any.
    pub policy: Option<u64>,
    /// True when the record of the client is the merged record, byte for
    /// byte.
    pub same: bool,
    /// The serial of the committed record.
    pub committed: Tid,
    /// The serial that the transaction read.
    pub read: Tid,
}

/// What a line of the resolution log gives beside a [`Resolution`].
#[derive(Clone, Copy, Debug)]
pub struct LogContext<'a> {
    /// The time of the vote.
    pub time: SystemTime,
    /// The storage name, for a server.
    pub storage: Option<&'a str>,
    /// The user of the transaction that voted.
    pub user: &'a [u8],
    /// The description of the transaction that voted.
    pub description: &'a [u8],
    /// The user of the transaction that wrote the committed record, when
    /// the store still has it.
    pub committed_user: Option<&'a [u8]>,
    /// The description of the transaction that wrote the committed record.
    pub committed_description: Option<&'a [u8]>,
}

/// Returns the line of the resolution log of `resolution`: one JSON
/// object, without the line feed (ADR 0042). A text that is not UTF-8 is
/// written with U+FFFD in place of its bad bytes.
pub fn log_line(resolution: &Resolution, context: &LogContext<'_>) -> String {
    let time = context
        .time
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default();
    let mut line = format!("{{\"time\":{}.{:06}", time.as_secs(), time.subsec_micros());
    let mut field = |name: &str, value: String| {
        line.push_str(",\"");
        line.push_str(name);
        line.push_str("\":");
        line.push_str(&value);
    };
    field(
        "storage",
        context.storage.map_or_else(|| "null".to_owned(), json),
    );
    field("oid", json(&resolution.oid.hex()));
    field("class", json(&resolution.class));
    field("tier", resolution.tier.to_string());
    field("mode", json(resolution.mode.as_str()));
    field("policy", version(resolution.policy));
    let (outcome, code, reason) = resolution.outcome.json_parts(resolution.split);
    field("outcome", outcome);
    field("code", code);
    field("reason", reason);
    let shadow = resolution.shadow.map_or_else(
        || "null".to_owned(),
        |shadow| {
            let (outcome, code, reason) = shadow.json_parts(resolution.split);
            format!("{{\"outcome\":{outcome},\"code\":{code},\"reason\":{reason}}}")
        },
    );
    field("shadow", shadow);
    // Only a line of the split merge has the field.
    if let Some(rule) = resolution.split {
        field("split", rule.to_string());
    }
    // Only a line of the order merge has the field.
    if let Some(part) = resolution.order {
        field("order", json(part));
    }
    field("applied", resolution.applied.to_string());
    field("undo", resolution.undo.to_string());
    let keys: Vec<String> = resolution.keys.iter().map(|key| json(key)).collect();
    field("keys", format!("[{}]", keys.join(",")));
    field("changed", resolution.changed.to_string());
    let fields: Vec<String> = resolution.fields.iter().map(|name| json(name)).collect();
    field("fields", format!("[{}]", fields.join(",")));
    field("committed", json(&resolution.committed.hex()));
    field("read", json(&resolution.read.hex()));
    let text = |bytes: Option<&[u8]>| {
        bytes.map_or_else(|| "null".to_owned(), |b| json(&String::from_utf8_lossy(b)))
    };
    field("user", text(Some(context.user)));
    field("description", text(Some(context.description)));
    field("committed_user", text(context.committed_user));
    field("committed_description", text(context.committed_description));
    line.push('}');
    line
}

/// Returns the line of the resolution log of `verification`: one JSON
/// object, without the line feed (ADR 0046).
pub fn verification_line(verification: &Verification, context: &LogContext<'_>) -> String {
    let time = context
        .time
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default();
    let text = |bytes: Option<&[u8]>| {
        bytes.map_or_else(|| "null".to_owned(), |b| json(&String::from_utf8_lossy(b)))
    };
    format!(
        "{{\"time\":{}.{:06},\"storage\":{},\"oid\":{},\"class\":{},\"tier\":{},\"mode\":\"shadow\",\
         \"policy\":{},\"outcome\":\"verified\",\"same\":{},\"committed\":{},\"read\":{},\
         \"user\":{},\"description\":{},\"committed_user\":{},\"committed_description\":{}}}",
        time.as_secs(),
        time.subsec_micros(),
        context.storage.map_or_else(|| "null".to_owned(), json),
        json(&verification.oid.hex()),
        json(&verification.class),
        verification.tier,
        version(verification.policy),
        verification.same,
        json(&verification.committed.hex()),
        json(&verification.read.hex()),
        text(Some(context.user)),
        text(Some(context.description)),
        text(context.committed_user),
        text(context.committed_description),
    )
}

/// Returns the version of a policy as a JSON value.
fn version(policy: Option<u64>) -> String {
    policy.map_or_else(|| "null".to_owned(), |version| version.to_string())
}

/// Returns `text` as a JSON string.
fn json(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

impl Resolution {
    /// Returns the entry of a merge in `mode` of `oid`, not applied yet.
    pub(crate) fn of(
        oid: Oid,
        mode: Mode,
        policy: Option<u64>,
        merge: &Merge,
        committed: Tid,
        read: Tid,
    ) -> Self {
        Resolution {
            oid,
            tier: merge.tier,
            class: merge.class.clone(),
            mode,
            policy,
            outcome: Resolved::of(&merge.outcome),
            applied: false,
            undo: false,
            keys: merge.keys.clone(),
            changed: merge.changed,
            committed,
            read,
            shadow: merge.shadow.as_ref().map(Resolved::of),
            fields: merge.fields.clone(),
            split: None,
            order: None,
        }
    }
}

impl Resolved {
    fn of(outcome: &Outcome) -> Self {
        match *outcome {
            Outcome::Merged(_) => Resolved::Merged,
            Outcome::Refused(reason) => Resolved::Refused(reason),
            Outcome::Undecided(why) => Resolved::Undecided(why),
        }
    }

    /// Returns the name, the code of BTrees and the reason of the outcome,
    /// as JSON values. `split` is the rule of the split merge, if any: a
    /// merge of rule 1 is `passed`.
    fn json_parts(self, split: Option<u8>) -> (String, String, String) {
        let (outcome, code, reason) = match self {
            Resolved::Merged if split == Some(1) => ("passed", None, None),
            Resolved::Merged => ("merged", None, None),
            Resolved::Refused(reason) => ("refused", reason.code(), Some(reason.message())),
            Resolved::Undecided(why) => ("undecided", None, Some(why)),
        };
        (
            json(outcome),
            code.map_or_else(|| "null".to_owned(), |code| code.to_string()),
            reason.map_or_else(|| "null".to_owned(), json),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_refusal_of_the_option_state_names_its_reason_and_its_field() {
        let merge = Merge {
            tier: 2,
            class: "m.History".to_owned(),
            outcome: Outcome::Refused(Reason::StateChanged),
            keys: vec!["wf".to_owned()],
            changed: 1,
            shadow: None,
            fields: vec!["wf".to_owned()],
        };
        let resolution = Resolution::of(Oid(1), Mode::On, Some(2), &merge, Tid(3), Tid(4));
        let context = LogContext {
            time: SystemTime::UNIX_EPOCH,
            storage: None,
            user: b"",
            description: b"",
            committed_user: None,
            committed_description: None,
        };
        let line = log_line(&resolution, &context);
        assert!(line.contains("\"tier\":2,"), "{line}");
        assert!(
            line.contains("\"outcome\":\"refused\",\"code\":null,"),
            "{line}"
        );
        assert!(
            line.contains("\"reason\":\"State changed by a status of the committed side\""),
            "{line}"
        );
        assert!(line.contains("\"fields\":[\"wf\"]"), "{line}");
    }
}
