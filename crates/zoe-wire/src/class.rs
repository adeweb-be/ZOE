//! The class names of the exceptions that the replies of `zoe serve` name,
//! and that a client reads or builds. A stock client rebuilds an exception
//! from its class name, so these names are part of the protocol.

/// The conflict error of ZODB.
pub const CONFLICT_ERROR: &str = "ZODB.POSException.ConflictError";

/// The read conflict error of ZODB.
pub const READ_CONFLICT_ERROR: &str = "ZODB.POSException.ReadConflictError";

/// The error of ZODB for an oid without a record.
pub const POS_KEY_ERROR: &str = "ZODB.POSException.POSKeyError";

/// The error of ZODB for a write to a read-only storage.
pub const READ_ONLY_ERROR: &str = "ZODB.POSException.ReadOnlyError";

/// The storage error of ZODB.
pub const STORAGE_ERROR: &str = "ZODB.POSException.StorageError";

/// The error of ZODB for a fault of the storage system.
pub const STORAGE_SYSTEM_ERROR: &str = "ZODB.POSException.StorageSystemError";

/// The transaction error of ZODB.
pub const STORAGE_TRANSACTION_ERROR: &str = "ZODB.POSException.StorageTransactionError";

/// The undo errors of ZODB that a stock client rebuilds: a
/// `MultipleUndoErrors` is an `UndoError`.
pub const MULTIPLE_UNDO_ERRORS: &str = "ZODB.POSException.MultipleUndoErrors";

/// The value error of Python.
pub const VALUE_ERROR: &str = "builtins.ValueError";

/// The type error of Python.
pub const TYPE_ERROR: &str = "builtins.TypeError";

/// The key error of Python.
pub const KEY_ERROR: &str = "builtins.KeyError";

/// The base exception of Python.
pub const EXCEPTION: &str = "builtins.Exception";
