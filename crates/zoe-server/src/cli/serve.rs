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

    /// The most transactions that getInvalidations covers. A client that
    /// reconnects further behind gets nil and verifies its cache.
    #[arg(
        long,
        env = "ZOE_INVALIDATION_QUEUE_SIZE",
        value_name = "TRANSACTIONS",
        default_value_t = 100_000
    )]
    invalidation_queue_size: usize,

    /// The most invalidations that wait for one client. Past it, the server
    /// closes the connection, and the client reconnects and verifies its
    /// cache. The replies of the client wait in a queue of the same size.
    #[arg(
        long,
        env = "ZOE_CLIENT_QUEUE_SIZE",
        value_name = "MESSAGES",
        default_value_t = 1_000,
        value_parser = clap::value_parser!(u64).range(1..)
    )]
    client_queue_size: u64,
}

/// Serve until SIGTERM. Write the line `listening <address>` to `out`.
#[cfg(feature = "serve")]
pub(super) fn run(args: &ServeArgs, out: &mut dyn Write) -> Result<(), Failure> {
    use crate::serve::{ServeOptions, Server};

    let options = ServeOptions::new(&args.path)
        .listen(args.listen)
        .invalidation_queue_size(args.invalidation_queue_size)
        .client_queue_size(usize::try_from(args.client_queue_size).unwrap_or(usize::MAX));
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

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::super::{Cli, Command};
    use super::*;

    /// Parse `zoe serve --path store` with `args` after it.
    fn serve(args: &[&str]) -> Result<ServeArgs, clap::Error> {
        let line = ["zoe", "serve", "--path", "store"].iter().chain(args);
        match Cli::try_parse_from(line)?.command {
            Command::Serve(args) => Ok(args),
            other => panic!("not serve: {other:?}"),
        }
    }

    #[test]
    fn the_queue_sizes_have_their_defaults_and_a_client_queue_holds_one_message() {
        let args = serve(&[]).unwrap();
        assert_eq!(args.invalidation_queue_size, 100_000);
        assert_eq!(args.client_queue_size, 1_000);
        let args = serve(&["--invalidation-queue-size", "0", "--client-queue-size", "1"]).unwrap();
        assert_eq!(args.invalidation_queue_size, 0);
        assert_eq!(args.client_queue_size, 1);
        let error = serve(&["--client-queue-size", "0"]).unwrap_err();
        assert_eq!(error.kind(), clap::error::ErrorKind::ValueValidation);
    }
}
