//! The catalog mirror. It keeps the ZCatalog indexes in memory and answers
//! queries as of the snapshot tid of the client.
//!
//! On this spike branch it holds only the prototype of the text index.

pub mod text;
