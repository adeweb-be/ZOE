//! The error of the decoder and the encoder.

/// An error from [`decode_record`](crate::decode_record) or
/// [`encode_record`](crate::encode_record).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    /// The bytes are not a record that the decoder can read.
    #[error("cannot decode at offset {offset}: {message}")]
    Decode {
        /// The offset of the problem in the record.
        offset: usize,
        /// What is wrong.
        message: String,
    },
    /// The record needs more nodes than the node budget allows. The bytes
    /// can be valid: a larger budget can decode them.
    #[error("cannot decode at offset {offset}: the record needs more than {budget} nodes")]
    Budget {
        /// The offset where the decoder stops.
        offset: usize,
        /// The node budget.
        budget: usize,
    },
    /// The tree holds a value that its opcode cannot write.
    #[error("cannot encode: {0}")]
    Encode(String),
}
