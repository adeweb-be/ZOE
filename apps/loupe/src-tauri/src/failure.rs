//! The errors of the IPC commands. Each error is a kind with parameters,
//! and no text for people. The window turns the kind into the UI string
//! `error.<kind>` of its translation catalog.

use serde::Serialize;
use zoe_core::ZoeError;
use zoe_loupe::{Error, oid_repr};

/// An error of an IPC command.
#[derive(Debug, Serialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub(crate) enum Failure {
    /// No folder is open.
    NoFolder,
    /// The folder holds no store.
    NoStore { path: String },
    /// Another process holds the lock of the store.
    Locked,
    /// The store was not closed cleanly. A writer must open it once.
    NeedsRepair,
    /// The oid has no record.
    NoRecord { oid: String },
    /// A path leads to no object.
    NoObject { path: String, segment: String },
    /// zoe-pickle refuses a record that a path goes through.
    Refused { oid: String, message: String },
    /// A text is not an oid in ZODB form.
    BadId { text: String },
    /// Another error. `message` is the text of the error, for a report.
    Other { message: String },
}

impl Failure {
    /// Return another error with its text.
    pub(crate) fn other(error: impl std::fmt::Display) -> Self {
        Failure::Other {
            message: error.to_string(),
        }
    }
}

impl From<Error> for Failure {
    fn from(error: Error) -> Self {
        match error {
            Error::NoStore { path } => Failure::NoStore { path },
            Error::Store(ZoeError::Locked) => Failure::Locked,
            Error::Store(ZoeError::NeedsRepair) => Failure::NeedsRepair,
            Error::Store(ZoeError::NotFound { oid }) => Failure::NoRecord { oid: oid_repr(oid) },
            Error::NoObject { path, segment } => Failure::NoObject { path, segment },
            Error::Refused { oid, source } => Failure::Refused {
                oid: oid_repr(oid),
                message: source.to_string(),
            },
            Error::BadId(text) => Failure::BadId { text },
            other => Failure::other(other),
        }
    }
}

#[cfg(test)]
mod tests {
    use zoe_core::Oid;

    use super::*;

    #[test]
    fn a_failure_is_a_kind_with_parameters() {
        let failure = Failure::from(Error::Store(ZoeError::NotFound { oid: Oid(1) }));
        assert_eq!(
            serde_json::to_value(failure).unwrap(),
            serde_json::json!({"kind": "noRecord", "oid": "0x01"})
        );
        let failure = Failure::from(Error::Store(ZoeError::NeedsRepair));
        assert_eq!(
            serde_json::to_value(failure).unwrap(),
            serde_json::json!({"kind": "needsRepair"})
        );
    }
}
