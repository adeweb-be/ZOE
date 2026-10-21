//! Decode and encode ZODB records (the class pickle and the state pickle)
//! as a generic tree. Decode then encode gives the same bytes.
//!
//! [`decode_record`] reads a record into a [`Tree`]. It never unpickles:
//! class names come from the class pickle. [`encode_record`] writes a tree
//! back to the same bytes. A construct that the tree does not model is a
//! [`Node::Opaque`] that keeps its original bytes.
//!
//! The decoder reads the opcodes of protocols 0 to 3, so it reads the
//! records of Python 2 stores too. It refuses what Python's unpickler
//! refuses. It also refuses a record that needs more nodes than its budget:
//! [`NODE_BUDGET`] for [`decode_record`], or the budget that a caller gives
//! to [`decode_record_with`]. No operation on a tree calls itself once per
//! nesting level, so a deep value cannot overflow the call stack.
//!
//! [`persistent_ids`] lists the persistent references of a record, also
//! those inside opaque nodes, for pack and the other tools that follow
//! references. [`decode_class_name`] and [`decode_class_key`] read the
//! class of a record from its class pickle alone.
//!
//! [`python`] models the objects that Python's unpickler builds from a
//! record, with their identity, and pickles them again as zodbpickle does.
//! [`btrees`] reads and builds the states of the classes of BTrees on that
//! model, for the conflict resolution of the core.

pub mod btrees;
mod decode;
mod encode;
mod error;
mod iterative;
mod opcode;
pub mod python;
mod references;
mod text;
mod tree;
mod zones;

pub use decode::{
    Limits, NODE_BUDGET, decode_class_key, decode_class_name, decode_record, decode_record_with,
};
pub use encode::encode_record;
pub use error::Error;
pub use references::{PersistentId, persistent_ids};
pub use text::{raw_unicode, unquote};
pub use tree::{
    Batch, Bool, ByteString, Bytes, Dict, Float, Get, Global, Int, List, MemoForm, Node, Opaque,
    Put, Reduce, Resolver, Tree, Tuple, Unicode, Walk, Width,
};
