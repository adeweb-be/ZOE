//! The `zoe` command and the `zoe serve` process.
//!
//! [`cli`] parses the command line and runs the subcommands. The binary
//! `zoe` and the console script of the `zoedb` wheel both call
//! [`cli::main`].
//!
//! `zoe serve` is a tokio server that speaks the ZEO 6 msgpack protocol, so
//! that an unmodified `ZEO.ClientStorage` connects. Its code compiles only
//! with the feature `serve`. This crate uses tokio only under that
//! feature, for `zoe serve` and for `zoe mcp`, so the extension module
//! never links it. It has no Python inside.
//!
//! The feature `fuzz` adds the module `fuzz`, the entry of the fuzz
//! targets in `fuzz/`.

#[cfg(feature = "serve")]
mod admin;
pub mod cli;
#[cfg(all(feature = "serve", any(test, feature = "fuzz")))]
pub use protocol::fuzz;
#[cfg(feature = "serve")]
mod protocol;
#[cfg(feature = "serve")]
pub mod serve;
