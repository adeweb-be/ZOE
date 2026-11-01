//! The Rust side of Loupe, the desktop inspector. The window code lives
//! in `apps/loupe/`.
//!
//! [`Folder`] opens a store folder for reads only and holds the lock of
//! the store. It shows each object as an [`Object`]: the class, the oid,
//! the serial, the size, the decoded state as JSON and the outbound
//! references. It resolves the paths of a Plone site. It writes to the
//! folder only to repair a store that a writer left open, at the open.
//!
//! Loupe also travels in time. [`Folder::as_of`] gives a [`Snapshot`]: the
//! store as it was after a transaction. [`Folder::history`] lists the
//! revisions of an object, and [`Folder::diff`] compares the decoded
//! states of two revisions.
//!
//! The space screen measures the store. [`Folder::space`] gives the space
//! of each class, the blobs, the largest records and the growth.
//! [`Folder::path_size`] and [`Folder::path_space`] give the size of a
//! path and of its items. [`Folder::reclaimable`] gives what a pack would
//! remove. A long call takes a progress function that can stop it.

mod diff;
mod folder;
mod ids;
mod model;
mod path;
mod snapshot;
mod space;
mod state;
mod text;

use zoe_core::{Oid, Tid, ZoeError};

pub use diff::Change;
pub use folder::{Folder, HISTORY_LIMIT};
pub use ids::{oid_repr, parse_oid, parse_tid, tid_repr};
pub use model::{MAX_NOTE, Object, Reference, Revision, note};
pub use snapshot::Snapshot;
pub use space::{
    Growth, ITEM_LIMIT, Large, Others, PathItem, PathSpace, Progress, Reclaim, Space, Totals, Walk,
};

/// An error of Loupe.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The folder holds no store.
    #[error("{path} holds no store")]
    NoStore {
        /// The path of the folder.
        path: String,
    },

    /// The store failed. For example, another process holds the lock, the
    /// repair of a store that was not closed cleanly failed, or the oid has
    /// no record.
    #[error(transparent)]
    Store(#[from] ZoeError),

    /// A path leads to no object.
    #[error("{path}: {segment} leads to no object")]
    NoObject {
        /// The path.
        path: String,
        /// The segment of the path where the walk stops.
        segment: String,
    },

    /// zoe-pickle refuses a record that a path goes through.
    #[error("oid {}: {source}", .oid.repr())]
    Refused {
        /// The oid of the record.
        oid: Oid,
        /// The refusal of the decoder.
        source: zoe_pickle::Error,
    },

    /// A text is not an oid or a tid in ZODB form.
    #[error("{0}: not a ZODB id")]
    BadId(String),

    /// The object has no record at or before the tid of a view. A later
    /// transaction creates it.
    #[error("oid {}: no record as of tid {}", .oid.repr(), .tid.repr())]
    NoRecordAsOf {
        /// The oid of the object.
        oid: Oid,
        /// The tid of the view.
        tid: Tid,
    },

    /// The progress function of a long call stopped it.
    #[error("the walk stopped before its end")]
    Stopped,
}

/// The result type of Loupe.
pub type Result<T> = std::result::Result<T, Error>;

/// Return a new, empty directory for the unit test `name`. The drop of
/// the directory removes it, except when the test panics.
#[cfg(test)]
fn fresh_dir(name: &str) -> zoe_core::testing::TempDir {
    zoe_core::testing::fresh_dir(&format!("loupe-{name}"))
}
