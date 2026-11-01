//! The steps of the split merge and of the order merge. A step stops
//! before the end of its merge with an answer, or with an error of the
//! store. [`Loader`] loads the records of one merge in one heap.

use zoe_pickle::Limits;
use zoe_pickle::python::{Heap, Value};

use super::budget;
use crate::error::ZoeError;
use crate::types::Oid;

/// Why a step of a merge stops before its end: the answer `A` of the
/// merge, or an error of the store.
pub(super) enum Stop<A> {
    Answer(A),
    Store(ZoeError),
}

impl<A> From<ZoeError> for Stop<A> {
    fn from(error: ZoeError) -> Self {
        Stop::Store(error)
    }
}

impl<A: From<&'static str>> From<&'static str> for Stop<A> {
    /// No answer, for this reason.
    fn from(why: &'static str) -> Self {
        Stop::Answer(A::from(why))
    }
}

/// The result of a step of a merge whose answer is `A`.
pub(super) type Step<T, A> = std::result::Result<T, Stop<A>>;

/// Returns the stop of no answer, for `why`.
pub(super) fn no<T, A: From<&'static str>>(why: &'static str) -> Step<T, A> {
    Err(Stop::from(why))
}

/// A heap that loads the records of one merge within the step budget of
/// their bytes, so that each persistent reference is one object.
pub(super) struct Loader {
    pub(super) heap: Heap,
    /// The bytes of the records that the heap loaded.
    len: usize,
}

impl Loader {
    pub(super) fn new() -> Self {
        Loader {
            heap: Heap::with_instances(),
            len: 0,
        }
    }

    /// Decodes and loads `record`: its class and its state. Each budget is
    /// the length of the records that the heap loaded, so a merge takes a
    /// number of steps in proportion to its records. Returns why when the
    /// record does not load.
    pub(super) fn load(&mut self, record: &[u8]) -> Result<(Value, Value), &'static str> {
        let limits = Limits {
            node_budget: record.len(),
        };
        let Ok(tree) = zoe_pickle::decode_record_with(record, limits) else {
            return Err("a record that does not decode");
        };
        self.len = self.len.saturating_add(record.len());
        self.heap.set_budget(budget(self.len));
        self.heap.load(&tree).map_err(|unsupported| unsupported.0)
    }

    /// Returns the oid of the persistent reference `value` to an object of
    /// the same database. Returns why when it is not one: `not_reference`
    /// for a value that is not a persistent reference.
    pub(super) fn oid(
        &self,
        value: Value,
        not_reference: &'static str,
    ) -> Result<Oid, &'static str> {
        let Some(reference) = self.heap.reference(value) else {
            return Err(not_reference);
        };
        if reference.database.is_some() || reference.weak {
            return Err("a reference to another database, or a weak reference");
        }
        let Ok(oid) = <[u8; 8]>::try_from(&reference.oid[..]) else {
            return Err("an oid that is not 8 bytes");
        };
        Ok(Oid(u64::from_be_bytes(oid)))
    }
}
