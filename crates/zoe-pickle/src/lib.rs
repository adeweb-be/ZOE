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
//! refuses, and a record that needs more than [`NODE_BUDGET`] nodes. No
//! operation on a tree calls itself once per nesting level, so a deep
//! value cannot overflow the call stack.

mod decode;
mod encode;
mod error;
mod iterative;
mod opcode;
mod text;
mod tree;

pub use decode::{NODE_BUDGET, decode_record};
pub use encode::encode_record;
pub use error::Error;
pub use tree::{
    Batch, Bool, ByteString, Bytes, Dict, Float, Get, Global, Int, List, MemoForm, Node, Opaque,
    Put, Reduce, Tree, Tuple, Unicode, Walk, Width,
};
