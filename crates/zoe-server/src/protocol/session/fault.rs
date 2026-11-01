//! The exceptions that a call gives to its client, with the class names
//! and the arguments of ZODB's exceptions.

use std::sync::Arc;

use rmpv::Value;
use zoe_core::{Oid, Store, Tid, Txn, ZoeError};
use zoe_wire::class::{
    CONFLICT_ERROR, EXCEPTION, KEY_ERROR, MULTIPLE_UNDO_ERRORS as UNDO_ERRORS, POS_KEY_ERROR,
    READ_CONFLICT_ERROR, READ_ONLY_ERROR, STORAGE_ERROR, STORAGE_SYSTEM_ERROR,
    STORAGE_TRANSACTION_ERROR as TRANSACTION_ERROR, TYPE_ERROR, VALUE_ERROR,
};
use zoe_wire::p64;

use crate::protocol::iteration::{self, Refused};

/// The text of a conflict error of ZODB.
pub(super) const CONFLICT: &str = "database conflict error";

/// The text of a read conflict error of ZODB.
pub(super) const READ_CONFLICT: &str = "database read conflict error";

/// An exception for the client: its class and its arguments.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Fault {
    pub(super) class: &'static str,
    pub(super) args: Value,
}

impl Fault {
    pub(super) fn new(class: &'static str, text: impl Into<String>) -> Self {
        Self {
            class,
            args: Value::Array(vec![Value::from(text.into())]),
        }
    }

    pub(super) fn value_error(text: impl Into<String>) -> Self {
        Self::new(VALUE_ERROR, text)
    }

    pub(super) fn type_error(text: impl Into<String>) -> Self {
        Self::new(TYPE_ERROR, text)
    }

    pub(super) fn transaction(text: impl Into<String>) -> Self {
        Self::new(TRANSACTION_ERROR, text)
    }

    /// The error of a call for the transaction `given` when `current` is
    /// the open transaction. The args are `(current or None, given)`, as
    /// ZEO gives them.
    pub(super) fn not_current(current: Option<&Value>, given: &Value) -> Self {
        Self {
            class: TRANSACTION_ERROR,
            args: Value::Array(vec![current.cloned().unwrap_or(Value::Nil), given.clone()]),
        }
    }

    pub(super) fn read_only() -> Self {
        Self {
            class: READ_ONLY_ERROR,
            args: Value::Array(vec![]),
        }
    }

    pub(super) fn storage(text: impl Into<String>) -> Self {
        Self::new(STORAGE_ERROR, text)
    }

    /// An undo that `reason` refuses, on `oid` when an object stops it. A
    /// stock client rebuilds it as a `MultipleUndoErrors` of one error,
    /// which is an `UndoError`. Each error is `(oid, reason)`, as in the
    /// `MultipleUndoErrors` of FileStorage.
    pub(super) fn undo(reason: &str, oid: Option<Oid>) -> Self {
        let oid = oid.map_or(Value::Nil, |oid| p64(oid.0));
        let error = Value::Array(vec![oid, Value::from(reason)]);
        Self {
            class: UNDO_ERRORS,
            args: Value::Map(vec![(Value::from("_errs"), Value::Array(vec![error]))]),
        }
    }

    /// The error of a call that names no object `oid`: a `POSKeyError` of
    /// the oid as the client sent it.
    pub(super) fn no_object(oid: Value) -> Self {
        Self {
            class: POS_KEY_ERROR,
            args: Value::Array(vec![oid]),
        }
    }

    /// The error of a call that names no iterator `id`: a `KeyError` of the
    /// id, as ZEO gives it.
    pub(super) fn no_iterator(id: &Value) -> Self {
        Self {
            class: KEY_ERROR,
            args: Value::Array(vec![id.clone()]),
        }
    }

    /// The error of a call on the iterator `id` that the iterators refused.
    pub(super) fn of_iterator(refused: Refused, id: &Value) -> Self {
        match refused {
            Refused::Unknown => Self::no_iterator(id),
            Refused::OutOfOrder(tid) => Self::new(
                EXCEPTION,
                format!(
                    "Out-of-order request for record iterator for transaction {:016x}",
                    tid.0
                ),
            ),
            Refused::Full => Self::storage(format!(
                "a connection keeps at most {} open iterators; iterator_gc or a new connection frees them",
                iteration::MAX_OPEN
            )),
        }
    }

    /// Return the exception of ZODB for an error of a pack, as
    /// `zoe.Storage.pack` raises it. A record that fails its checksum or
    /// does not decode stops the pack with `StorageSystemError`. A pack
    /// that the stop of the server stopped fails with `StorageError`.
    pub(super) fn of_pack(error: &ZoeError) -> Self {
        match error {
            ZoeError::CorruptRecord { .. } => {
                Self::new(STORAGE_SYSTEM_ERROR, format!("pack stopped: {error}"))
            }
            ZoeError::Format(text) => {
                Self::new(STORAGE_SYSTEM_ERROR, format!("pack stopped: {text}"))
            }
            ZoeError::Closed => Self::storage("pack stopped: the server stops"),
            _ => Self::of(error),
        }
    }

    /// Tell if the fault is a conflict. The names of the conflict
    /// exceptions of ZODB end in `ConflictError`, and they all derive from
    /// `ConflictError`.
    pub(super) fn is_conflict(&self) -> bool {
        self.class.ends_with("ConflictError")
    }

    /// A conflict on `oid`, as ZEO sends it: the `__dict__` of the
    /// exception, with the current serial and the serial that the
    /// transaction read, and the class of the object.
    pub(super) fn conflict(
        class: &'static str,
        message: &str,
        oid: Oid,
        serials: Option<(Tid, Tid)>,
        class_name: Option<String>,
    ) -> Self {
        let serials = serials.map_or(Value::Nil, |(current, read)| {
            Value::Array(vec![p64(current.0), p64(read.0)])
        });
        Self {
            class,
            args: Value::Map(vec![
                (Value::from("message"), Value::from(message)),
                (Value::from("oid"), p64(oid.0)),
                (
                    Value::from("class_name"),
                    class_name.map_or(Value::Nil, Value::from),
                ),
                (Value::from("serials"), serials),
            ]),
        }
    }

    /// Return the exception of ZODB for an error of the vote of `txn`. A
    /// conflict carries the serials of the stale oid. A `ConflictError`
    /// also carries the class of the object that `txn` stored, and a
    /// `ReadConflictError` has none, as in ZEO.
    pub(super) fn of_vote(error: &ZoeError, txn: &Txn<Arc<Store>>) -> Self {
        match *error {
            ZoeError::Conflict { oid } => Self::conflict(
                CONFLICT_ERROR,
                CONFLICT,
                oid,
                txn.conflict_serials(),
                txn.staged_class_name(oid),
            ),
            ZoeError::ReadConflict { oid } => Self::conflict(
                READ_CONFLICT_ERROR,
                READ_CONFLICT,
                oid,
                txn.conflict_serials(),
                None,
            ),
            _ => Self::of(error),
        }
    }

    /// Return the exception of ZODB for `error`.
    pub(super) fn of(error: &ZoeError) -> Self {
        let text = error.to_string();
        match *error {
            ZoeError::NotFound { oid } => Self::no_object(p64(oid.0)),
            ZoeError::Conflict { oid } => Self::conflict(CONFLICT_ERROR, CONFLICT, oid, None, None),
            ZoeError::ReadConflict { oid } => {
                Self::conflict(READ_CONFLICT_ERROR, READ_CONFLICT, oid, None, None)
            }
            ZoeError::ReadOnly => Self::read_only(),
            ZoeError::Undo { oid, reason } => Self::undo(reason, oid),
            ZoeError::Protocol(_) | ZoeError::LockOrder { .. } | ZoeError::WrongThread => {
                Self::transaction(text)
            }
            ZoeError::InvalidKv(_) => Self::value_error(text),
            ZoeError::Halted
            | ZoeError::CorruptRecord { .. }
            | ZoeError::CorruptBlob { .. }
            | ZoeError::CorruptTxn { .. }
            | ZoeError::CorruptSequence { .. }
            | ZoeError::CorruptKv { .. }
            | ZoeError::Backend(_) => Self::new(STORAGE_SYSTEM_ERROR, text),
            _ => Self::new(STORAGE_ERROR, text),
        }
    }
}

impl From<ZoeError> for Fault {
    fn from(error: ZoeError) -> Self {
        Self::of(&error)
    }
}
