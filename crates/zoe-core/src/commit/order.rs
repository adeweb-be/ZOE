//! The order merge of ADR 0052 in the vote that takes the locks.
//!
//! The first conflict of a vote whose merge policy sets `folder_orders` to
//! `on` or `shadow` makes a [`Plan`] for the stale order lists of the
//! transaction, beside the plan of the split merge. The plan runs
//! `merge::order::plan` on each of them.
//!
//! In mode `on`, the check of the written serials skips each order that the
//! plan merges. After the check of every serial, the vote renumbers the ids
//! of the transaction in the records of the positions that it stages, and
//! stages the merged list (`Txn::finish_orders`). Tier 2 never merges an
//! order that the plan found. In mode `shadow`, the vote goes on as
//! without the merge, and the line of each order carries its outcome.

use std::collections::{BTreeMap, BTreeSet};
use std::ops::Deref;

use super::source::TxnSource;
use super::{READ_CHUNK, Txn};
use crate::error::Result;
use crate::merge::order::{self, Order, Planned, Renumbered};
use crate::merge::policy::{Policy, Rules};
use crate::merge::{MAX_KEYS, Mode, Resolution, Resolved};
use crate::store::Store;
use crate::types::{Oid, Tid};

/// What the order merge decided for the stale order lists of a vote.
#[derive(Default)]
pub(super) struct Plan {
    /// The stale lists that are the order of a folder: tier 2 never merges
    /// them.
    pub(super) known: BTreeSet<Oid>,
    /// The orders that the vote merges, in mode `on`.
    pub(super) merges: Vec<Order>,
    /// The line of each order without a merge, which the vote writes when
    /// it reaches the order: no answer, or the outcome of mode `shadow`.
    pub(super) lines: BTreeMap<Oid, Resolution>,
    /// The order of a refusal in mode `on`, with its current serial, the
    /// serial that the transaction read and its line: the vote fails on it.
    pub(super) refused: Option<(Oid, Tid, Tid, Resolution)>,
}

impl Plan {
    /// Tells if the vote merges the order `oid`.
    pub(super) fn covers(&self, oid: Oid) -> bool {
        self.merges.iter().any(|order| order.list == oid)
    }
}

/// Returns the mode of the order merge for a vote with `rules`: the mode of
/// the switch, and of tier 1 of each class of the positions.
pub(super) fn mode(rules: &Rules<'_>) -> Mode {
    order::POS_CLASSES
        .iter()
        .fold(rules.folder_orders(), |mode, class| {
            mode.lower(rules.tier1_mode(class))
        })
}

/// Returns `ids` as the keys of a line: each id as tier 1 writes a str, at
/// most `MAX_KEYS` of them.
pub(super) fn keys(ids: &[String]) -> Vec<String> {
    ids.iter()
        .take(MAX_KEYS)
        .map(|id| format!("{id:?}"))
        .collect()
}

/// Returns the line of the list of `merge` in mode `on`, not applied yet.
fn list_line(merge: &Order, version: Option<u64>, outcome: Resolved) -> Resolution {
    Resolution::new(
        merge.list,
        order::LIST_CLASS,
        Mode::On,
        version,
        outcome,
        merge.current,
        merge.read,
    )
    .with_keys(keys(&merge.ids), merge.ids.len())
    .with_order("list")
}

/// The reason of a list in mode `shadow`.
const SHADOW: &str = "an order in shadow mode";

impl<S: Deref<Target = Store>> Txn<S> {
    /// Makes the plan of the order merge of the vote, with the settings of
    /// `rules`. The vote holds its locks.
    pub(super) fn order_plan(&self, rules: &Rules<'_>) -> Result<Plan> {
        let mut plan = Plan::default();
        // The plan finds the orders whenever the switch is not `off`, so
        // tier 2 never merges one. It merges only where tier 1 of the
        // positions runs too.
        if rules.folder_orders() == Mode::Off {
            return Ok(plan);
        }
        let mode = mode(rules);
        // The stored lists that the transaction read at a serial.
        let lists: Vec<(Oid, Tid)> = self
            .staged
            .iter()
            .filter(|(_, staged)| {
                !staged.restored && staged.pickle_len() > 0 && staged.serial != Tid::ZERO
            })
            .map(|(oid, staged)| (oid, staged.serial))
            .filter(|&(oid, _)| self.staged_class_name(oid).as_deref() == Some(order::LIST_CLASS))
            .collect();
        if lists.is_empty() {
            return Ok(plan);
        }
        let source = TxnSource::new(self);
        for chunk in lists.chunks(READ_CHUNK) {
            let current = self.current_serials(chunk.iter().map(|&(oid, _)| oid))?;
            for (&(oid, read), current) in chunk.iter().zip(current) {
                let current = current.unwrap_or(Tid::ZERO);
                if current == read {
                    continue;
                }
                let line = |outcome: Resolved, shadow: Option<Resolved>, ids: &[String]| {
                    let version = rules.version();
                    Resolution::new(
                        oid,
                        order::LIST_CLASS,
                        mode,
                        version,
                        outcome,
                        current,
                        read,
                    )
                    .with_keys(keys(ids), ids.len())
                    .with_shadow(shadow)
                    .with_order("list")
                };
                // In mode `shadow`, the outcome goes to the field `shadow`.
                let decided = |outcome: Resolved, ids: &[String]| match mode {
                    Mode::Shadow => line(Resolved::Undecided(SHADOW), Some(outcome), ids),
                    _ => line(outcome, None, ids),
                };
                let planned = order::plan(&source, oid)?;
                if matches!(planned, Planned::NotOrder) {
                    continue;
                }
                plan.known.insert(oid);
                if mode == Mode::Off {
                    continue;
                }
                match planned {
                    Planned::NotOrder => {}
                    Planned::Merge(merge) => {
                        if mode == Mode::Shadow {
                            let outcome = self.shadow_outcome(&merge)?;
                            plan.lines.insert(oid, decided(outcome, &merge.ids));
                        } else {
                            plan.merges.push(merge);
                        }
                    }
                    Planned::Refused { reason, ids } => {
                        let entry = decided(Resolved::Refused(reason), &ids);
                        if mode == Mode::On && plan.refused.is_none() {
                            let mut entry = entry;
                            entry.applied = true;
                            plan.refused = Some((oid, current, read, entry));
                        } else {
                            plan.lines.insert(oid, entry);
                        }
                    }
                    Planned::Undecided { why, ids } => {
                        plan.lines
                            .insert(oid, decided(Resolved::Undecided(why), &ids));
                    }
                }
            }
        }
        Ok(plan)
    }

    /// Returns the outcome of mode `shadow` of `merge`: the renumbering and
    /// its self-check, on the records of the positions that the transaction
    /// stored. The vote fails on the order before it merges the other
    /// records of the positions, so the check reads these.
    fn shadow_outcome(&self, merge: &Order) -> Result<Resolved> {
        let mut records = Vec::new();
        for &oid in &merge.stored {
            if let Some((_, pickle)) = self.staged.get(oid)? {
                records.push((oid, pickle.into_owned()));
            }
        }
        Ok(match order::renumber(merge, &records) {
            Ok(_) => Resolved::Merged,
            Err(why) => Resolved::Undecided(why),
        })
    }

    /// Renumbers the ids of each order that `plan` merges in the records of
    /// its positions that the vote stages, and adds the merged list.
    ///
    /// `merged` holds the records that tier 1 and the split merge merged.
    /// The records of the positions that the transaction stored and that
    /// are not stale join it when the renumbering changes them. Returns the
    /// order of the first merge without an answer, with its serials: the
    /// vote fails on it, or gives it to the caller.
    pub(super) fn finish_orders(
        &mut self,
        plan: &Plan,
        policy: Option<&Policy>,
        merged: &mut Vec<(Oid, Tid, Vec<u8>)>,
        onto: &[(Oid, std::sync::Arc<super::InFlight>)],
        stored_bytes: &mut u64,
    ) -> Result<Option<(Oid, Tid, Tid)>> {
        for merge in &plan.merges {
            match self.finish_order(merge, policy, merged, onto, stored_bytes)? {
                Ok(()) => {}
                Err(why) => {
                    let version = policy.map(|policy| policy.version);
                    let entry = list_line(merge, version, Resolved::Undecided(why));
                    self.resolutions.push(entry);
                    return Ok(Some((merge.list, merge.current, merge.read)));
                }
            }
        }
        Ok(None)
    }

    /// Renumbers one order. Returns why when it gives no answer, and then
    /// changes nothing.
    fn finish_order(
        &mut self,
        merge: &Order,
        policy: Option<&Policy>,
        merged: &mut Vec<(Oid, Tid, Vec<u8>)>,
        onto: &[(Oid, std::sync::Arc<super::InFlight>)],
        stored_bytes: &mut u64,
    ) -> Result<std::result::Result<(), &'static str>> {
        let version = policy.map(|policy| policy.version);
        if onto.iter().any(|(oid, _)| merge.positions.contains(oid)) {
            return Ok(Err(
                "a record of the positions merged onto a commit in flight",
            ));
        }
        if merge.positions.iter().any(|oid| {
            self.handed
                .get(oid)
                .is_some_and(|h| h.commit.ended().is_none())
        }) {
            return Ok(Err("a record of the positions with a commit in flight"));
        }
        // The records of the positions that the vote stages: the merged
        // ones, then the stored ones that are not stale and not merged.
        let mut records: Vec<(Oid, Tid, Vec<u8>)> = merged
            .iter()
            .filter(|(oid, _, _)| merge.positions.contains(oid))
            .cloned()
            .collect();
        for &oid in &merge.positions {
            if records.iter().any(|(merged, _, _)| *merged == oid) || self.dropping.contains(&oid) {
                continue;
            }
            let Some(staged) = self.staged.record(oid) else {
                continue;
            };
            if staged.restored || staged.pickle_len() == 0 {
                continue;
            }
            let serial = staged.serial;
            let current = self.current_serial(oid)?;
            if current != serial {
                // A stale record of the positions that no merge decided
                // goes to the caller: the order stays without an answer.
                return Ok(Err("a record of the positions without a merge"));
            }
            if let Some((_, pickle)) = self.staged.get(oid)? {
                records.push((oid, serial, pickle.into_owned()));
            }
        }
        let pairs: Vec<(Oid, Vec<u8>)> = records
            .iter()
            .map(|(oid, _, pickle)| (*oid, pickle.clone()))
            .collect();
        let renumbered = match order::renumber(merge, &pairs) {
            Ok(renumbered) => renumbered,
            Err(why) => return Ok(Err(why)),
        };
        // The sizes, before anything changes.
        let mut size = *stored_bytes;
        let Some(total) = self.merged_size(merge.list, &merge.pickle, size) else {
            return Ok(Err("a merged record over a size limit"));
        };
        size = total;
        for Renumbered { oid, pickle, .. } in &renumbered {
            // A record that a merge gave counts with the length of that
            // merge, not with the length of the staged pickle.
            let replaced = merged
                .iter()
                .find(|(record, _, _)| record == oid)
                .map(|(_, _, earlier)| earlier.len() as u64);
            let Some(total) = self.merged_size_replacing(*oid, pickle, size, replaced) else {
                return Ok(Err("a merged record over a size limit"));
            };
            size = total;
        }
        *stored_bytes = size;
        merged.push((merge.list, merge.current, merge.pickle.clone()));
        self.resolutions
            .push(list_line(merge, version, Resolved::Merged));
        for Renumbered { oid, pickle, ids } in renumbered {
            let serial = records
                .iter()
                .find(|(record, _, _)| *record == oid)
                .map_or(Tid::ZERO, |(_, serial, _)| *serial);
            match merged.iter_mut().find(|(record, _, _)| *record == oid) {
                Some(entry) => entry.2 = pickle,
                None => merged.push((oid, serial, pickle)),
            }
            let start = self.undo_resolutions;
            match self.resolutions[start..]
                .iter_mut()
                .find(|entry| entry.oid == oid && entry.outcome == Resolved::Merged)
            {
                Some(entry) => entry.order = Some("pos"),
                None => {
                    let class = self
                        .staged_class_name(oid)
                        .unwrap_or_else(|| order::POS_CLASSES[1].to_owned());
                    let outcome = Resolved::Merged;
                    self.resolutions.push(
                        Resolution::new(oid, class, Mode::On, version, outcome, serial, serial)
                            .with_keys(keys(&ids), ids.len())
                            .with_order("pos"),
                    );
                }
            }
        }
        Ok(Ok(()))
    }
}
