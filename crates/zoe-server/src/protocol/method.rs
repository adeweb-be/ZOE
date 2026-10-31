//! The methods that the server serves, in one table.
//!
//! Each row gives the name of a method on the wire, the arm of the dispatch
//! that runs it (`Op`), where its call runs (`Lane`), whether a stock
//! client waits for its reply, and the extension that adds it. A new method
//! is one row here and one arm in `Session::dispatch` or
//! `Transaction::dispatch`. The server keeps no other list of method names.

use zoe_wire::extension::{KV, LOCK_INTENT, SEQUENCES};

/// Where the call of a method runs, once the connection is registered.
/// Before `register`, each call runs alone on the read pool.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Lane {
    /// The read pool. The call takes a turn in the window of the read pool
    /// of its connection.
    Read,
    /// The read pool, with a turn. The reader starts the next call of the
    /// connection only after the end of this call.
    ReadBeforeNext,
    /// The read pool, without a turn. The reader starts the next call of
    /// the connection only after the end of this call.
    BeforeNext,
    /// The lane of the transaction of the connection: one thread runs these
    /// calls in their order.
    Transaction,
    /// A thread of its own: the call waits for its client, or runs long.
    Own,
}

impl Lane {
    /// Tell if the call takes a turn in the window of the read pool.
    pub(crate) fn takes_turn(self) -> bool {
        matches!(self, Self::Read | Self::ReadBeforeNext)
    }

    /// Tell if the reader waits for the end of the call before it starts
    /// the next call of the connection.
    pub(crate) fn ends_before_next(self) -> bool {
        matches!(self, Self::ReadBeforeNext | Self::BeforeNext)
    }
}

/// The arm of the dispatch that runs a method.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Op {
    LoadBefore,
    Register,
    LastTransaction,
    GetInfo,
    GetSizeInfo,
    ServerStatus,
    Ping,
    SetClientLabel,
    GetTid,
    NewOids,
    LoadSerial,
    GetInvalidations,
    History,
    UndoLog,
    UndoInfo,
    RecordIternext,
    IteratorStart,
    IteratorNext,
    IteratorRecordStart,
    IteratorRecordNext,
    IteratorGc,
    Pack,
    SendBlob,
    SequenceNext,
    KvGet,
    KvScan,
    KvGeneration,
    LockIntent,
    UnlockIntent,
    TpcBegin,
    Storea,
    Restorea,
    CheckCurrentSerial,
    DeleteObject,
    KvSet,
    KvDelete,
    Undoa,
    Vote,
    TpcFinish,
    TpcAbort,
    StoreBlobStart,
    StoreBlobChunk,
    StoreBlobEnd,
    StoreBlobShared,
}

/// One method of the protocol.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Method {
    /// The name of the method on the wire.
    pub(crate) name: &'static str,
    pub(crate) op: Op,
    pub(crate) lane: Lane,
    /// False for a call that a stock client sends without waiting for a
    /// reply. The server takes the flag of each call from its message: the
    /// tests send each call with the flag of this field.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) replies: bool,
    /// The extension that adds the method, or `None` for a method of ZEO.
    /// A connection calls the method only after it declared the extension.
    pub(crate) extension: Option<&'static str>,
}

impl Method {
    /// A method of ZEO with a reply.
    pub(crate) const fn new(name: &'static str, op: Op, lane: Lane) -> Self {
        Self {
            name,
            op,
            lane,
            replies: true,
            extension: None,
        }
    }

    /// The method, for a call without a reply.
    const fn no_reply(mut self) -> Self {
        self.replies = false;
        self
    }

    /// The method, which `extension` adds.
    pub(crate) const fn of(mut self, extension: &'static str) -> Self {
        self.extension = Some(extension);
        self
    }
}

use Lane::{BeforeNext, Own, Read, ReadBeforeNext, Transaction};

/// The methods that the server serves. `loadBefore` comes first because a
/// lookup reads the table in order, and most calls are loads.
///
/// `set_client_label` ends before the next call, so each later call logs
/// with the label. `iterator_gc` ends before the next call, so a later call
/// for an iterator that it removed fails with `KeyError`. `sendBlob` waits
/// while its client reads, and `pack` reads the whole store, so each runs
/// on a thread of its own. The extension `sequences` adds `sequence_next`,
/// `lock_intent` adds `lock_intent` and `unlock_intent` (ADR 0045), and
/// `kv` adds three reads and two writes in the transaction (ADR 0043).
pub(crate) const METHODS: &[Method] = &[
    Method::new("loadBefore", Op::LoadBefore, Read),
    Method::new("register", Op::Register, Read),
    Method::new("lastTransaction", Op::LastTransaction, Read),
    Method::new("get_info", Op::GetInfo, Read),
    Method::new("get_size_info", Op::GetSizeInfo, Read),
    Method::new("server_status", Op::ServerStatus, Read),
    Method::new("ping", Op::Ping, Read),
    Method::new("set_client_label", Op::SetClientLabel, BeforeNext).no_reply(),
    Method::new("getTid", Op::GetTid, Read),
    Method::new("new_oids", Op::NewOids, Read),
    Method::new("loadSerial", Op::LoadSerial, Read),
    Method::new("getInvalidations", Op::GetInvalidations, Read),
    Method::new("history", Op::History, Read),
    Method::new("undoLog", Op::UndoLog, Read),
    Method::new("undoInfo", Op::UndoInfo, Read),
    Method::new("record_iternext", Op::RecordIternext, Read),
    Method::new("iterator_start", Op::IteratorStart, Read),
    Method::new("iterator_next", Op::IteratorNext, Read),
    Method::new("iterator_record_start", Op::IteratorRecordStart, Read),
    Method::new("iterator_record_next", Op::IteratorRecordNext, Read),
    Method::new("iterator_gc", Op::IteratorGc, ReadBeforeNext).no_reply(),
    Method::new("pack", Op::Pack, Own),
    Method::new("sendBlob", Op::SendBlob, Own),
    Method::new("sequence_next", Op::SequenceNext, Read).of(SEQUENCES),
    Method::new("kv_get", Op::KvGet, Read).of(KV),
    Method::new("kv_scan", Op::KvScan, Read).of(KV),
    Method::new("kv_generation", Op::KvGeneration, Read).of(KV),
    Method::new("lock_intent", Op::LockIntent, Transaction).of(LOCK_INTENT),
    Method::new("unlock_intent", Op::UnlockIntent, Transaction).of(LOCK_INTENT),
    Method::new("tpc_begin", Op::TpcBegin, Transaction).no_reply(),
    Method::new("storea", Op::Storea, Transaction).no_reply(),
    Method::new("restorea", Op::Restorea, Transaction).no_reply(),
    Method::new(
        "checkCurrentSerialInTransaction",
        Op::CheckCurrentSerial,
        Transaction,
    )
    .no_reply(),
    Method::new("deleteObject", Op::DeleteObject, Transaction).no_reply(),
    Method::new("kv_set", Op::KvSet, Transaction)
        .no_reply()
        .of(KV),
    Method::new("kv_delete", Op::KvDelete, Transaction)
        .no_reply()
        .of(KV),
    Method::new("undoa", Op::Undoa, Transaction).no_reply(),
    Method::new("vote", Op::Vote, Transaction),
    Method::new("tpc_finish", Op::TpcFinish, Transaction),
    Method::new("tpc_abort", Op::TpcAbort, Transaction),
    Method::new("storeBlobStart", Op::StoreBlobStart, Transaction).no_reply(),
    Method::new("storeBlobChunk", Op::StoreBlobChunk, Transaction).no_reply(),
    Method::new("storeBlobEnd", Op::StoreBlobEnd, Transaction).no_reply(),
    Method::new("storeBlobShared", Op::StoreBlobShared, Transaction).no_reply(),
];

/// Return the method named `name`, or `None` for a method that the server
/// does not serve.
pub(crate) fn find(name: &str) -> Option<&'static Method> {
    METHODS.iter().find(|method| method.name == name)
}

/// Return where a call of `name` runs. A call of a method that the server
/// does not serve runs on the read pool, and closes the connection there.
pub(crate) fn lane(name: &str) -> Lane {
    find(name).map_or(Read, |method| method.lane)
}

/// Tell if a call of `name` takes a turn in the window of the read pool,
/// once the connection is registered.
pub(crate) fn on_read_pool(name: &str) -> bool {
    lane(name).takes_turn()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_calls_of_the_read_pool_are_the_calls_that_read() {
        for method in [
            "loadBefore",
            "loadSerial",
            "history",
            "new_oids",
            "iterator_next",
            "sequence_next",
        ] {
            assert!(on_read_pool(method), "{method}");
        }
        let transaction_calls = METHODS.iter().filter(|m| m.lane == Transaction);
        for method in transaction_calls.map(|m| m.name) {
            assert!(!on_read_pool(method), "{method}");
        }
        for method in ["set_client_label", "sendBlob", "pack"] {
            assert!(!on_read_pool(method), "{method}");
        }
    }
}
