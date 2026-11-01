//! The vote of a transaction: the undos that it stages, the encodes that
//! it waits for, and the conflicts that it lists for the client.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::SystemTime;

use rmpv::Value;
use zoe_core::{Oid, Store, Txn, undo_tid};
use zoe_wire::p64;

use super::args::{Args, u64_of};
use super::{Fault, Transaction};
use crate::protocol::compress::{Failed, Job};
use crate::protocol::status::Stats;

/// The reason of an undo that a later transaction stops, as in FileStorage.
const CHANGED_LATER: &str = "Some data were modified by a later transaction";

impl Transaction {
    /// Vote, and return the conflicts that the client resolves, as ZEO 6
    /// does with client conflict resolution. An empty list means that the
    /// vote passed.
    ///
    /// The first vote is `vote_resolving` of the core. It takes the locks
    /// once, in oid order, and runs tier 1 on each stale oid of a class of
    /// BTrees or `Length`: the reply gives each oid that tier 1 merged as a
    /// bare oid, as a ZEO 6 server gives an oid that it resolved, and a
    /// refusal of tier 1 fails the vote with `ConflictError`. Another stale
    /// written oid that the client can resolve does not fail it: the reply
    /// lists the oid, and the transaction keeps its locks. The client
    /// resolves each conflict, stores the resolved pickle over the
    /// committed serial and votes again. The second vote takes no lock and
    /// checks every serial again. No Python runs on the server.
    ///
    /// Before it takes a lock, the first vote stages the undos that `undoa`
    /// queued, in order. The reply gives each oid of an undone transaction
    /// as a bare oid, as ZEO's server does. An undo that no tier merges
    /// fails the vote with `UndoError`. Then the vote waits for each job of
    /// the compression pool of the transaction, and attaches its payload to
    /// the staged record. A job that failed fails the vote with
    /// `StorageError`.
    ///
    /// A failed vote ends the transaction, as in ZEO: the core transaction
    /// drops, which releases its locks. So the `tpc_abort` that follows
    /// finds no transaction.
    pub(super) fn vote(&mut self, args: Args<'_>) -> Result<Value, Fault> {
        let [id] = args.exactly::<1>()?;
        let storage = Arc::clone(&self.session.storage);
        let open = self.current(id)?;
        let listed = std::mem::take(&mut open.listed);
        let native = std::mem::take(&mut open.native);
        let jobs = std::mem::take(&mut open.jobs);
        let undos = std::mem::take(&mut open.undos);
        let voted = match &mut open.txn {
            Ok(txn) if listed > 0 => {
                let voted = txn
                    .vote()
                    .map(|()| Voted::default())
                    .map_err(|error| Fault::of_vote(&error, txn));
                // The second vote compares the merges of shadow mode with
                // the records of the client: log and count what it found.
                storage.metrics.count_verifications(txn.verifications());
                if let Some(log) = &storage.resolution_log
                    && !txn.verifications().is_empty()
                {
                    log.write(&txn.verification_log_lines(SystemTime::now(), Some(&storage.name)));
                }
                voted
            }
            Ok(txn) => {
                let voted = undo(txn, &undos).and_then(|(undone, seen)| {
                    attach(txn, jobs)?;
                    let mut voted = vote_resolving(txn)?;
                    voted
                        .native
                        .retain(|oid| u64_of(oid).map_or(true, |oid| !seen.contains(&Oid(oid))));
                    voted.undone = undone.into_iter().map(|oid| p64(oid.0)).collect();
                    Ok(voted)
                });
                // The vote that takes the locks runs tier 1: log and count
                // its results, also when the vote failed.
                storage.metrics.count_resolutions(txn.resolutions());
                if let Some(log) = &storage.resolution_log
                    && !txn.resolutions().is_empty()
                {
                    log.write(&txn.resolution_log_lines(SystemTime::now(), Some(&storage.name)));
                }
                voted
            }
            Err(fault) => Err(fault.clone()),
        };
        match voted {
            Ok(Voted {
                undone,
                native: merged,
                conflicts,
            }) => {
                open.voted = true;
                open.listed = conflicts.len() as u64;
                // A vote that passes counts the conflicts that the client
                // resolved and the oids that tier 1 merged. A first vote
                // that lists conflicts keeps its merged oids for the
                // second vote.
                if conflicts.is_empty() {
                    Stats::add_n(&storage.stats.conflicts_resolved, listed);
                    let merges = native + merged.len() as u64;
                    Stats::add_n(&storage.metrics.conflicts_native, merges);
                } else {
                    open.native = merged.len() as u64;
                }
                Ok(Value::Array(
                    undone.into_iter().chain(merged).chain(conflicts).collect(),
                ))
            }
            Err(fault) => {
                self.open = None;
                Err(fault)
            }
        }
    }
}

/// Wait for each job in `jobs`, and attach its payload to the staged
/// record of its oid in `txn`. A payload that does not match the staged
/// pickle stays out, and the finish encodes the pickle. The first job that
/// failed fails the call, and the drop of the other jobs cancels them.
fn attach(txn: &mut Txn<Arc<Store>>, jobs: Vec<Job>) -> Result<(), Fault> {
    for job in jobs {
        let (oid, encoded) = job.wait().map_err(|Failed(oid)| {
            Fault::storage(format!(
                "the encode of the pickle of oid {:#x} failed",
                oid.0
            ))
        })?;
        txn.set_encoded(oid, encoded)?;
    }
    Ok(())
}

/// The items of the reply of a vote.
#[derive(Default)]
struct Voted {
    /// The oids of the undone transactions, each as a bare oid.
    undone: Vec<Value>,
    /// The oids that tier 1 merged, each as a bare oid.
    native: Vec<Value>,
    /// The conflicts that the client resolves.
    conflicts: Vec<Value>,
}

/// Stage in `txn` the undo of each undo id of `ids`, in order. Return the
/// oids of the undone transactions, each once, in the order of the undos,
/// and their set.
///
/// An undo id that names no transaction, and an undo that the core
/// refuses, fail with `UndoError`. So does an undo that no tier of the
/// core merges: the in-process storage resolves such an undo in Python,
/// and no Python runs on the server.
fn undo(txn: &mut Txn<Arc<Store>>, ids: &[Value]) -> Result<(Vec<Oid>, HashSet<Oid>), Fault> {
    let mut oids = Vec::new();
    let mut seen = HashSet::new();
    for id in ids {
        let tid = match id {
            Value::Binary(id) => undo_tid(id),
            Value::String(id) => undo_tid(id.as_bytes()),
            _ => None,
        }
        .ok_or_else(|| Fault::undo("the undo id names no transaction", None))?;
        let undone = txn.undo(tid)?;
        if let Some(conflict) = undone.conflicts.first() {
            return Err(Fault::undo(CHANGED_LATER, Some(conflict.oid)));
        }
        for oid in undone.oids {
            if seen.insert(oid) {
                oids.push(oid);
            }
        }
    }
    Ok((oids, seen))
}

/// Vote with `vote_resolving` of the core. Return each oid that tier 1
/// merged, and each stale written oid that the client can resolve, as ZEO 6
/// lists a conflict in the reply of `vote`: `{oid, serials: [committed,
/// read], data}`. `data` is the pickle that the transaction stored. The
/// client loads the two other states with `loadSerial`.
fn vote_resolving(txn: &mut Txn<Arc<Store>>) -> Result<Voted, Fault> {
    let conflicts = txn
        .vote_resolving()
        .map_err(|error| Fault::of_vote(&error, txn))?;
    let native = txn
        .natively_resolved()
        .into_iter()
        .map(|oid| p64(oid.0))
        .collect();
    let conflicts = conflicts
        .into_iter()
        .map(|(oid, committed)| {
            // A staged pickle in the spill file is read from the file, and
            // that read can fail.
            let (read, data) = txn
                .staged(oid)?
                .ok_or_else(|| Fault::storage("a conflict has no staged pickle"))?;
            Ok(Value::Map(vec![
                (Value::from("oid"), p64(oid.0)),
                (
                    Value::from("serials"),
                    Value::Array(vec![p64(committed.tid.0), p64(read.0)]),
                ),
                (Value::from("data"), Value::Binary(data.into_owned())),
            ]))
        })
        .collect::<Result<Vec<_>, Fault>>()?;
    Ok(Voted {
        undone: Vec::new(),
        native,
        conflicts,
    })
}
