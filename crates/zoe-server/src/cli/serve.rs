//! `zoe serve`: serve a store with the ZEO 6 msgpack protocol, so that an
//! unmodified `ZEO.ClientStorage` connects.
//!
//! The command writes the line `listening <address>` to stdout once it
//! accepts connections, so a port of 0 gives a free port. The logs go to
//! stderr. SIGTERM closes the store, and the command exits with 0.
//!
//! The console script of the wheel has no server: its extension never
//! links tokio. There, the command fails with 1 and names the `zoe` binary.

use std::io::Write;
use std::net::SocketAddr;
use std::path::PathBuf;

use clap::Args;

use super::Failure;

/// The options of `zoe serve`.
#[derive(Debug, Args)]
pub(super) struct ServeArgs {
    /// The directory of the store. The server serves it under the storage
    /// name 1, the default of ZEO, and creates it when it does not exist.
    #[arg(long, env = "ZOE_PATH", value_name = "STORE")]
    path: PathBuf,

    /// The address of the ZEO port, as HOST:PORT. A port of 0 takes a free
    /// port.
    #[arg(
        long,
        env = "ZOE_LISTEN",
        value_name = "HOST:PORT",
        default_value = "127.0.0.1:8100"
    )]
    listen: SocketAddr,

    /// The longest frame that a client can send, in bytes. A longer frame
    /// closes its connection before the server reserves memory for it. A
    /// storea frame holds a whole record, so keep this limit above the
    /// largest record. The default is 268435456 bytes, 256 MiB.
    #[arg(
        long,
        env = "ZOE_MAX_FRAME_SIZE",
        value_name = "BYTES",
        value_parser = clap::value_parser!(u32).range(1..)
    )]
    max_frame_size: Option<u32>,
}

/// Serve until SIGTERM. Write the line `listening <address>` to `out`.
#[cfg(feature = "serve")]
pub(super) fn run(args: &ServeArgs, out: &mut dyn Write) -> Result<(), Failure> {
    use crate::serve::{ServeOptions, Server};

    let mut options = ServeOptions::new(&args.path).listen(args.listen);
    if let Some(bytes) = args.max_frame_size {
        options = options.max_frame_size(bytes);
    }
    let server = Server::start(options).map_err(|error| Failure::Message(error.to_string()))?;
    server.serve_until_signal(|addr| {
        writeln!(out, "listening {addr}")?;
        out.flush()
    })?;
    Ok(())
}

/// Fail: this build has no server.
#[cfg(not(feature = "serve"))]
pub(super) fn run(_args: &ServeArgs, _out: &mut dyn Write) -> Result<(), Failure> {
    Err(Failure::Message(
        "this build has no server; run the `zoe` binary".to_owned(),
    ))
}
