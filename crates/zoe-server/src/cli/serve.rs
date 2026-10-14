//! `zoe serve`: serve stores with the ZEO 6 msgpack protocol, so that an
//! unmodified `ZEO.ClientStorage` connects.
//!
//! Each store has a storage name, and a client selects a store with the
//! storage name that it gives to `register`. `--path` gives the store of
//! the storage name `1`, the default of ZEO. `--storage NAME=PATH` adds a
//! store, and so does each `[storages.NAME]` table of the configuration
//! file of `--config`.
//!
//! The command writes the line `listening <address>` to stdout once it
//! accepts connections, so a port of 0 gives a free port. With
//! `--admin-listen`, it writes the line `admin <address>` before it. The logs go to
//! stderr. SIGTERM or SIGINT stops the server within `--shutdown-timeout`:
//! the command closes the connections and the stores, and exits with 0.
//!
//! The console script of the wheel has no server: its extension never
//! links tokio. There, the command fails with 1 and names the `zoe` binary.

use std::io::Write;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use clap::Args;

use super::Failure;

/// The options of `zoe serve`.
#[derive(Debug, Args)]
pub(super) struct ServeArgs {
    /// The directory of the store of the storage name 1, the default of
    /// ZEO. The server creates a store that does not exist.
    #[arg(long, env = "ZOE_PATH", value_name = "STORE")]
    path: Option<PathBuf>,

    /// A store that the server serves under the storage name NAME, which a
    /// client gives to register. Repeat the option to serve more stores.
    /// The twin gives one store. The server creates a store that does not
    /// exist. A name that comes twice is an error.
    #[arg(
        long,
        env = "ZOE_STORAGE",
        value_name = "NAME=PATH",
        value_parser = named_storage
    )]
    storage: Vec<Storage>,

    /// A TOML file of options. Its top-level keys are the options of zoe
    /// serve without --, except config and storage, such as listen =
    /// "127.0.0.1:8100". Each [storages.NAME] table adds a store, with the
    /// keys path, blob-dir and read-only. An option on the command line or
    /// in its twin wins over the file. An unknown key is an error.
    #[arg(long, env = "ZOE_CONFIG", value_name = "FILE")]
    config: Option<PathBuf>,

    /// The address of the ZEO port, as HOST:PORT. A port of 0 takes a free
    /// port.
    #[arg(
        long,
        env = "ZOE_LISTEN",
        value_name = "HOST:PORT",
        default_value = "127.0.0.1:8100"
    )]
    listen: SocketAddr,

    /// The address of the admin port, as HOST:PORT: an HTTP port for the
    /// probes. GET /healthz gives 200 while every store is open. GET
    /// /readyz gives 200 when every store is open and ready, and 503
    /// before and from a stop on. The port does not speak ZEO. A port of 0
    /// takes a free port. Without it, the server opens no admin port.
    #[arg(long, env = "ZOE_ADMIN_LISTEN", value_name = "HOST:PORT")]
    admin_listen: Option<SocketAddr>,

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
    /// cache.
    #[arg(
        long,
        env = "ZOE_CLIENT_QUEUE_SIZE",
        value_name = "MESSAGES",
        default_value_t = 1_000,
        value_parser = clap::value_parser!(u64).range(1..)
    )]
    client_queue_size: u64,
    /// The longest time that a vote waits for the lock of an object, in
    /// whole seconds, such as 10, 10s or 2m, as the key lock-timeout of the
    /// <zoe> section takes it. A longer wait fails the vote with
    /// ConflictError. The default is the default of the store, 10 seconds.
    #[arg(
        long,
        env = "ZOE_LOCK_TIMEOUT",
        value_name = "SECONDS",
        value_parser = whole_seconds
    )]
    lock_timeout: Option<Duration>,

    /// The longest frame that a client can send, in bytes. A longer frame
    /// closes its connection before the server reserves memory for it. A
    /// storea or restorea frame holds a whole record, so keep this limit
    /// above the largest record: a copy through the server, as zodbconvert
    /// does, fails on a record of the source that is longer. The default is
    /// 268435456 bytes, 256 MiB.
    #[arg(
        long,
        env = "ZOE_MAX_FRAME_SIZE",
        value_name = "BYTES",
        value_parser = clap::value_parser!(u32).range(1..)
    )]
    max_frame_size: Option<u32>,

    /// The longest time that a stop on SIGTERM or SIGINT takes, in whole
    /// seconds, such as 30, 30s or 1m. The server stops accepting
    /// connections, commits each tpc_finish that it received and sends its
    /// reply, aborts the other transactions, closes each connection and
    /// closes the stores. Near the end of the time, it closes the
    /// connections that remain at once. When a call still runs at the end,
    /// such as a running pack, the store of that session does not close
    /// cleanly, and the other stores close. The command exits with 0.
    #[arg(
        long,
        env = "ZOE_SHUTDOWN_TIMEOUT",
        value_name = "SECONDS",
        default_value = "30",
        value_parser = whole_seconds
    )]
    shutdown_timeout: Duration,

    /// The longest time that a connection sends no frame, in whole seconds,
    /// such as 300, 300s or 5m. A heartbeat is a frame, and a stock client
    /// sends one every 60 seconds. The server closes a connection that is
    /// idle for longer: its open transaction aborts and its locks go, as
    /// when the client closes. So a client that stalls during a commit, or
    /// a host that dies, holds its locks at most this long. The time while
    /// the server reads no calls of the connection, because 4 calls wait,
    /// does not count. 0 closes no idle connection.
    #[arg(
        long,
        env = "ZOE_IDLE_TIMEOUT",
        value_name = "SECONDS",
        default_value = "300",
        value_parser = whole_seconds
    )]
    idle_timeout: Duration,

    /// The stores to serve: the store of `--path`, then the stores of
    /// `--storage`, then the stores of the configuration file.
    /// [`configure`] sets them.
    #[arg(skip)]
    storages: Vec<Storage>,
}

/// A store that the server serves under a storage name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Storage {
    pub(super) name: String,
    pub(super) path: PathBuf,
    /// The directory of the blob files. `None` is `blobs` in the store
    /// directory.
    pub(super) blob_dir: Option<PathBuf>,
}

/// Parse the value of `--storage`: `NAME=PATH`. The name ends at the first
/// `=`. Neither the name nor the path is empty.
fn named_storage(value: &str) -> Result<Storage, String> {
    match value.split_once('=') {
        Some((name, path)) if !name.is_empty() && !path.is_empty() => Ok(Storage {
            name: name.to_owned(),
            path: PathBuf::from(path),
            blob_dir: None,
        }),
        _ => Err(format!("{value} is not NAME=PATH")),
    }
}

/// Parse a time in whole seconds, as ZConfig's `time-interval` does: a
/// whole number, 0 or more, with the suffix `s`, `m`, `h` or `d` in any
/// case, or none for seconds.
fn whole_seconds(value: &str) -> Result<Duration, String> {
    let text = value.trim().to_ascii_lowercase();
    let (number, unit) = match text.as_bytes().last() {
        Some(b's') => (&text[..text.len() - 1], 1),
        Some(b'm') => (&text[..text.len() - 1], 60),
        Some(b'h') => (&text[..text.len() - 1], 3_600),
        Some(b'd') => (&text[..text.len() - 1], 86_400),
        _ => (text.as_str(), 1),
    };
    number
        .parse::<u64>()
        .ok()
        .and_then(|n| n.checked_mul(unit))
        .map(Duration::from_secs)
        .ok_or_else(|| format!("{value} is not a whole number of seconds, 0 or more"))
}

/// Complete the arguments `cli` of the command line `args`, whose parse
/// gave `matches`. For `zoe serve`, add the options and the stores of the
/// configuration file, and set the stores to serve. A configuration file
/// that does not read or does not parse, an unknown key, a store without
/// a path, no store and a storage name that comes twice are usage errors.
#[cfg(feature = "serve")]
pub(super) fn configure(
    cli: super::Cli,
    matches: &clap::ArgMatches,
    mut args: Vec<std::ffi::OsString>,
) -> Result<super::Cli, clap::Error> {
    use clap::{CommandFactory, Parser};

    use super::{Cli, Command};

    let Command::Serve(serve) = &cli.command else {
        return Ok(cli);
    };
    let config = match (&serve.config, matches.subcommand_matches("serve")) {
        (Some(path), Some(matches)) => {
            let command = Cli::command();
            let subcommand = command.find_subcommand("serve").unwrap_or(&command);
            super::config::read(path, subcommand, matches).map_err(usage_error)?
        }
        _ => super::config::Config::default(),
    };
    // The options of the file come last, and only the options that the
    // command line and the twins do not give.
    let mut cli = if config.args.is_empty() {
        cli
    } else {
        args.extend(config.args);
        Cli::try_parse_from(args)?
    };
    if let Command::Serve(serve) = &mut cli.command {
        serve.storages = storages(serve, config.storages).map_err(usage_error)?;
    }
    Ok(cli)
}

/// The command line has no configuration file in a build without server:
/// the command fails before it reads one.
#[cfg(not(feature = "serve"))]
pub(super) fn configure(
    cli: super::Cli,
    _matches: &clap::ArgMatches,
    _args: Vec<std::ffi::OsString>,
) -> Result<super::Cli, clap::Error> {
    Ok(cli)
}

/// Return the usage error of `zoe serve` with `message`.
#[cfg(feature = "serve")]
fn usage_error(message: String) -> clap::Error {
    use clap::CommandFactory;
    use clap::error::ErrorKind;

    let mut command = super::Cli::command();
    command.build();
    match command.find_subcommand_mut("serve") {
        Some(serve) => serve.error(ErrorKind::ValueValidation, message),
        None => command.error(ErrorKind::ValueValidation, message),
    }
}

/// Return the stores to serve: the store of `--path` under the storage
/// name 1, then the stores of `--storage`, then the stores of the file,
/// `file`. Fail when there is no store, or when a name comes twice.
#[cfg(feature = "serve")]
fn storages(args: &ServeArgs, file: Vec<Storage>) -> Result<Vec<Storage>, String> {
    let path = args.path.iter().map(|path| Storage {
        name: crate::protocol::STORAGE_NAME.to_owned(),
        path: path.clone(),
        blob_dir: None,
    });
    let storages: Vec<Storage> = path
        .chain(args.storage.iter().cloned())
        .chain(file)
        .collect();
    if storages.is_empty() {
        return Err(
            "no store to serve: give --path, --storage or a [storages.NAME] table of --config"
                .to_owned(),
        );
    }
    for (n, storage) in storages.iter().enumerate() {
        if storages[..n].iter().any(|other| other.name == storage.name) {
            return Err(format!("the storage name {} comes twice", storage.name));
        }
    }
    Ok(storages)
}

/// Serve until SIGTERM or SIGINT, then stop. Write the line
/// `admin <address>` when the server has an admin port, then the line
/// `listening <address>`, to `out`.
#[cfg(feature = "serve")]
pub(super) fn run(args: &ServeArgs, out: &mut dyn Write) -> Result<(), Failure> {
    use std::num::NonZeroU32;

    use crate::serve::{ServeOptions, Server, StorageOptions};

    let storages = args.storages.iter().map(|storage| {
        let options = StorageOptions::new(&storage.name, &storage.path);
        match &storage.blob_dir {
            Some(dir) => options.blob_dir(dir),
            None => options,
        }
    });
    let mut options = ServeOptions::with_storages(storages)
        .listen(args.listen)
        .invalidation_queue_size(args.invalidation_queue_size)
        .client_queue_size(usize::try_from(args.client_queue_size).unwrap_or(usize::MAX))
        .shutdown_timeout(args.shutdown_timeout)
        .idle_timeout(args.idle_timeout);
    if let Some(timeout) = args.lock_timeout {
        options = options.lock_timeout(timeout);
    }
    if let Some(addr) = args.admin_listen {
        options = options.admin_listen(addr);
    }
    if let Some(bytes) = args.max_frame_size.and_then(NonZeroU32::new) {
        options = options.max_frame_size(bytes);
    }
    let server = Server::start(options).map_err(|error| Failure::Message(error.to_string()))?;
    if let Some(admin) = server.admin_addr() {
        writeln!(out, "admin {admin}")?;
    }
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

    #[test]
    fn the_shutdown_timeout_is_30_seconds_by_default_and_takes_whole_seconds() {
        assert_eq!(
            serve(&[]).unwrap().shutdown_timeout,
            Duration::from_secs(30)
        );
        let args = serve(&["--shutdown-timeout", "2m"]).unwrap();
        assert_eq!(args.shutdown_timeout, Duration::from_secs(120));
        let args = serve(&["--shutdown-timeout", "0"]).unwrap();
        assert_eq!(args.shutdown_timeout, Duration::ZERO);
        let error = serve(&["--shutdown-timeout", "1.5"]).unwrap_err();
        assert_eq!(error.kind(), clap::error::ErrorKind::ValueValidation);
    }

    #[test]
    fn the_idle_timeout_is_300_seconds_by_default_and_takes_whole_seconds() {
        assert_eq!(serve(&[]).unwrap().idle_timeout, Duration::from_secs(300));
        let args = serve(&["--idle-timeout", "2m"]).unwrap();
        assert_eq!(args.idle_timeout, Duration::from_secs(120));
        let args = serve(&["--idle-timeout", "0"]).unwrap();
        assert_eq!(args.idle_timeout, Duration::ZERO);
        let error = serve(&["--idle-timeout", "1.5"]).unwrap_err();
        assert_eq!(error.kind(), clap::error::ErrorKind::ValueValidation);
    }

    #[test]
    fn a_lock_timeout_is_in_whole_seconds_as_the_stanza_key_takes_it() {
        for (value, seconds) in [
            ("0", 0),
            ("1", 1),
            ("10s", 10),
            ("2m", 120),
            ("1H", 3_600),
            ("1d", 86_400),
            (" 7 ", 7),
        ] {
            assert_eq!(
                whole_seconds(value),
                Ok(Duration::from_secs(seconds)),
                "{value}"
            );
        }
        for value in ["", "s", "-1", "1.5", "ten", "1w", "18446744073709551615d"] {
            assert!(whole_seconds(value).is_err(), "{value}");
        }
    }
}
