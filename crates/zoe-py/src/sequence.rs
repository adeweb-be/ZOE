//! `zoe._zoe.Sequence`: a sequence of a storage.
//!
//! `Storage.sequence(name)` gives it. It holds the storage, not the store,
//! so each call reads the store as the calls of the storage do: after
//! `close`, a call raises `StorageError`.

use pyo3::PyTraverseError;
use pyo3::gc::PyVisit;
use pyo3::prelude::*;

use crate::storage::Storage;

/// A named counter of a storage. `next()` gives values that are unique and
/// strictly increasing per store and per name, from 1.
#[pyclass(frozen, module = "zoe._zoe", name = "Sequence")]
pub(crate) struct Sequence {
    storage: Py<Storage>,
    name: String,
}

impl Sequence {
    pub(crate) fn new(storage: Py<Storage>, name: String) -> Self {
        Self { storage, name }
    }
}

#[pymethods]
impl Sequence {
    /// Return the next value of the sequence.
    ///
    /// The value is not part of a transaction: an abort does not give it
    /// back. Each reopen of the store can skip up to 100 values. A
    /// read-only storage raises `ReadOnlyError`.
    fn next(&self, py: Python<'_>) -> PyResult<u64> {
        self.storage.get().next_value(py, &self.name)
    }

    /// Give the storage to the garbage collector. A cycle such as
    /// `storage.x = storage.sequence(name)` is then collectable: the
    /// collector clears the attributes of the storage.
    fn __traverse__(&self, visit: PyVisit<'_>) -> Result<(), PyTraverseError> {
        visit.call(&self.storage)
    }
}
