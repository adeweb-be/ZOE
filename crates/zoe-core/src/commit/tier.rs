//! The tier that merges the records of a class, for the vote and for an
//! undo.

use crate::merge::policy::Rules;
use crate::merge::{self, Merge};

/// The tier that merges the records of a class, and its mode: tier 1 for
/// a class of BTrees or `Length`, tier 2 for a class that the merge policy
/// names.
pub(super) struct Tier<'r> {
    pub(super) tier: u8,
    pub(super) class: String,
    pub(super) mode: merge::Mode,
    /// The entry of the merge policy of a class of tier 2.
    entry: Option<&'r merge::policy::ClassPolicy>,
}

impl<'r> Tier<'r> {
    /// Return the tier of the record `new`, from its class pickle alone, or
    /// `None` when no tier merges its class or the mode of the tier is
    /// `Off`.
    pub(super) fn of(new: &[u8], rules: &'r Rules<'_>) -> Option<Self> {
        let tier = if let Some(class) = merge::tier1_class(new) {
            let mode = rules.tier1_mode(&class);
            Tier {
                tier: 1,
                class,
                mode,
                entry: None,
            }
        } else {
            let class = merge::class_name(new)?;
            let entry = rules.tier2(&class)?;
            Tier {
                tier: 2,
                class,
                mode: entry.tier2,
                entry: Some(entry),
            }
        };
        (tier.mode != merge::Mode::Off).then_some(tier)
    }

    /// Merge the three records of a conflict in this tier: `old` is the
    /// base, `committed` the committed record and `new` the record that
    /// goes over it.
    pub(super) fn merge(
        &self,
        old: &[u8],
        committed: &[u8],
        new: &[u8],
        rules: &Rules<'_>,
    ) -> (merge::Mode, Merge) {
        let merge = match self.entry {
            None => {
                merge::tier1(old, committed, new, rules.posting_dicts()).expect("a class of tier 1")
            }
            Some(entry) => merge::tier2(old, committed, new, &self.class, entry),
        };
        (self.mode, merge)
    }
}

/// Merge the three records of an undo through the tiers: `old` is the
/// record of the undone transaction, `committed` the current record and
/// `new` the record before the undone transaction. Return `None` when no
/// tier merges the class of `new`, or the mode of its tier is `Off`.
///
/// The undo calls it. The vote picks its tier with the same `Tier::of`
/// and merges with the same `Tier::merge`, apart, so that it reads the
/// record that its transaction read only for a class of a tier.
///
/// In an undo, a field of strategy `lww` that both sides changed refuses,
/// as `strict` does (ADR 0046): the undo would drop the later change
/// without a word.
pub(super) fn undo_tiers(
    old: &[u8],
    committed: &[u8],
    new: &[u8],
    rules: &Rules<'_>,
) -> Option<(merge::Mode, Merge)> {
    let tier = Tier::of(new, rules)?;
    let strict = tier.entry.map(without_lww);
    let tier = Tier {
        entry: strict.as_ref().or(tier.entry),
        ..tier
    };
    Some(tier.merge(old, committed, new, rules))
}

/// Return `entry` with the strategy `strict` in place of each `lww`.
fn without_lww(entry: &merge::policy::ClassPolicy) -> merge::policy::ClassPolicy {
    use merge::policy::Strategy;
    let strict = |strategy: Strategy| match strategy {
        Strategy::Lww => Strategy::Strict,
        other => other,
    };
    let mut entry = entry.clone();
    entry.default = strict(entry.default);
    for strategy in entry.fields.values_mut() {
        *strategy = strict(*strategy);
    }
    entry
}
