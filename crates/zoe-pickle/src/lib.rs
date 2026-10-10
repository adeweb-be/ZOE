//! Decode and encode ZODB records (the class pickle and the state pickle)
//! as a generic tree. Decode then encode gives the same bytes.
//!
//! [`decode_record`] reads a record into a [`Tree`]. It never unpickles:
//! class names come from the class pickle. [`encode_record`] writes a tree
//! back to the same bytes. A construct that the tree does not model is a
//! [`Node::Opaque`] that keeps its original bytes.

mod decode;
mod encode;
mod error;
mod opcode;
mod tree;

pub use decode::decode_record;
pub use encode::encode_record;
pub use error::Error;
pub use tree::{
    Batch, Bytes, Get, Global, Int, Node, Opaque, Put, Reduce, Tree, Tuple, Walk, Width,
};
