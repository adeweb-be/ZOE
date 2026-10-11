//! The `zoe` command and the `zoe serve` process.
//!
//! [`cli`] parses the command line and runs the subcommands. The binary
//! `zoe` and the console script of the `zoedb` wheel both call
//! [`cli::main`].
//!
//! `zoe serve` is a tokio server that speaks the ZEO 6 msgpack protocol, so
//! that an unmodified `ZEO.ClientStorage` connects. Its code compiles only
//! with the feature `serve`. This is the only crate that uses tokio, and
//! only under that feature. It has no Python inside.

pub mod cli;
#[cfg(feature = "serve")]
mod protocol;
