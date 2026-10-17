//! Tier 1 of the conflict resolution: the structural merges of BTrees and
//! `Length`, in Rust, at the vote.
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
//! The posting-dict rule (ADR 0042) is a merge where ZODB's resolver
//! refuses. It is off by default. In shadow mode it computes its merge for
//! the resolution log, and the conflict ends as in ZODB's resolver. See
//! `posting.rs`.
//!
//! No Python runs here.

mod btrees;
mod posting;

use std::collections::BTreeMap;
use std::time::SystemTime;

use zoe_pickle::Limits;
use zoe_pickle::btrees::classify;

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

/// The settings of the structural merges (tier 1) of a store.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StructuralMerges {
    /// The mode of each class of tier 1 that `classes` does not name.
    pub mode: Mode,
    /// The mode of a class of tier 1, by its `module.name`.
    pub classes: BTreeMap<String, Mode>,
    /// The mode of the posting-dict rule of ADR 0042, inside the classes
    /// where tier 1 runs.
    pub posting_dicts: Mode,
}

impl Default for StructuralMerges {
    /// Tier 1 on for every class, and the posting-dict rule off.
    fn default() -> Self {
        StructuralMerges {
            mode: Mode::On,
            classes: BTreeMap::new(),
            posting_dicts: Mode::Off,
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

/// The result of tier 1 for one conflict.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Merge {
    /// The class of the record, as `module.name`.
    pub class: String,
    /// The outcome.
    pub outcome: Outcome,
    /// The keys that the new record changed, inserted or deleted, as text,
    /// in key order. At most [`MAX_KEYS`]; `changed` counts them all.
    pub keys: Vec<String>,
    /// The number of keys that the new record changed, inserted or
    /// deleted.
    pub changed: usize,
    /// With the posting-dict rule in shadow mode, the outcome that the rule
    /// gives when a dict took part in the merge. The outcome of the merge
    /// is then the refusal of BTrees, as in ZODB's resolver.
    pub shadow: Option<Outcome>,
}

/// The most keys that a [`Merge`] lists.
pub const MAX_KEYS: usize = 20;

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
/// `posting_dicts` is the mode of the posting-dict rule.
///
/// Returns `None` when the class of `new` is not a class of tier 1.
pub fn tier1(old: &[u8], committed: &[u8], new: &[u8], posting_dicts: Mode) -> Option<Merge> {
    let (module, name) = zoe_pickle::decode_class_name(new).ok()??;
    let class = classify(&module, &name)?;
    let mut merge = Merge {
        class: format!("{module}.{name}"),
        outcome: Outcome::Undecided(""),
        keys: Vec::new(),
        changed: 0,
        shadow: None,
    };
    merge.outcome = match decode(old, committed, new) {
        Ok(trees) => btrees::merge(&trees, class, posting_dicts, &mut merge),
        Err(why) => Outcome::Undecided(why),
    };
    Some(merge)
}

/// The decoded records of one conflict, in the order of ZODB's resolver:
/// new, old, committed.
struct Trees {
    new: zoe_pickle::Tree,
    old: zoe_pickle::Tree,
    committed: zoe_pickle::Tree,
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
    })
}

/// What tier 1 did for one conflict of a vote: one entry of the resolution
/// log.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Resolution {
    /// The oid in conflict.
    pub oid: Oid,
    /// The class of the record, as `module.name`.
    pub class: String,
    /// The mode of the class: `On` or `Shadow`.
    pub mode: Mode,
    /// What tier 1 gave.
    pub outcome: Resolved,
    /// True when the outcome is the outcome of the vote: the vote stored
    /// the merged record and passed, or failed on the refusal. Always false
    /// in shadow mode.
    pub applied: bool,
    /// The keys that the new record changed, inserted or deleted, as text.
    /// At most [`MAX_KEYS`].
    pub keys: Vec<String>,
    /// The number of keys that the new record changed, inserted or deleted.
    pub changed: usize,
    /// The serial of the committed record.
    pub committed: Tid,
    /// The serial that the transaction read.
    pub read: Tid,
    /// With the posting-dict rule in shadow mode, what the rule gives when
    /// a dict took part in the merge; `outcome` is then the refusal of
    /// BTrees.
    pub shadow: Option<Resolved>,
}

/// What tier 1 gave for a conflict, without the merged record.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Resolved {
    /// Tier 1 merged the records.
    Merged,
    /// Tier 1 refused the merge.
    Refused(Reason),
    /// Tier 1 gave no answer. The text says why.
    Undecided(&'static str),
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
    field("oid", json(&format!("0x{:016x}", resolution.oid.0)));
    field("class", json(&resolution.class));
    field("tier", "1".to_owned());
    field("mode", json(resolution.mode.as_str()));
    let (outcome, code, reason) = resolution.outcome.json_parts();
    field("outcome", outcome);
    field("code", code);
    field("reason", reason);
    let shadow = resolution.shadow.map_or_else(
        || "null".to_owned(),
        |shadow| {
            let (outcome, code, reason) = shadow.json_parts();
            format!("{{\"outcome\":{outcome},\"code\":{code},\"reason\":{reason}}}")
        },
    );
    field("shadow", shadow);
    field("applied", resolution.applied.to_string());
    let keys: Vec<String> = resolution.keys.iter().map(|key| json(key)).collect();
    field("keys", format!("[{}]", keys.join(",")));
    field("changed", resolution.changed.to_string());
    field(
        "committed",
        json(&format!("0x{:016x}", resolution.committed.0)),
    );
    field("read", json(&format!("0x{:016x}", resolution.read.0)));
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
    pub(crate) fn of(oid: Oid, mode: Mode, merge: &Merge, committed: Tid, read: Tid) -> Self {
        Resolution {
            oid,
            class: merge.class.clone(),
            mode,
            outcome: Resolved::of(&merge.outcome),
            applied: false,
            keys: merge.keys.clone(),
            changed: merge.changed,
            committed,
            read,
            shadow: merge.shadow.as_ref().map(Resolved::of),
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
    /// as JSON values.
    fn json_parts(self) -> (String, String, String) {
        let (outcome, code, reason) = match self {
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
