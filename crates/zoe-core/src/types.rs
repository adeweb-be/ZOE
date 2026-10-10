//! Identifiers of the data model.

/// An object id. ZODB writes it as 8 big-endian bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Oid(pub u64);

/// A transaction id. It is the 8-byte ZODB `TimeStamp` and it always
/// increases. The serial of a record is the tid that wrote it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Tid(pub u64);
