//! `zoe serve`: serve stores with the ZEO 6 msgpack protocol, so that an
//! unmodified `ZEO.ClientStorage` connects.
//!
//! Each store has a storage name, and a client selects a store with the
//! storage name that it gives to `register`. `--path` gives the store of
//! the storage name `1`, the default of ZEO. `--storage NAME=PATH` adds a
//! store, and so does each `[storages.NAME]` table of the configuration
//! file of `--config`.
//!
//! `--listen` takes `HOST:PORT` or, with a `/`, the path of a unix domain
//! socket, as the `address` of the `<zeo>` section of ZEO does.
//!
//! The command writes the line `listening <address>` to stdout once it
//! accepts connections, so a port of 0 gives a free port. With
//! `--admin-listen`, it writes the line `admin <address>` before it. The
//! logs go to stderr. SIGTERM or SIGINT stops the server within `--shutdown-timeout`:
//! the command closes the connections and the stores, and exits with 0.
//! The command handles both signals before it creates anything on disk. A
//! signal during the start stops the server when the start ends, so a
//! store that the start creates is complete and closes cleanly.
//!
//! The console script of the wheel has no server: its extension is built
//! without the feature `serve`. There, the command fails with 1 and names
//! the `zoe` binary.

use std::io::Write;
use std::net::SocketAddr;
use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::time::Duration;

use clap::Args;
use zoe_core::{Compression, Mode};

use super::{Boolish, Failure};
use crate::defaults;

/// The options of `zoe serve`.
#[derive(Debug, Args)]
pub(super) struct ServeArgs {
    /// The directory of the store of the storage name 1, the default of
    /// ZEO. The server creates a store that does not exist.
    #[arg(long, env = "ZOE_PATH", value_name = "STORE")]
    path: Option<PathBuf>,

    /// The blob directory of the store of --path, the key blob-dir of the
    /// <zoe> section. The default is blobs in the store directory. The
    /// store does not record its blob directory, so give the same one at
    /// each start. The key blob-dir of a [storages.NAME] table sets the
    /// blob directory of that store.
    #[arg(long, env = "ZOE_BLOB_DIR", value_name = "DIR")]
    blob_dir: Option<PathBuf>,

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
    /// keys path, blob-dir, read-only and pack-gc. An option on the command
    /// line or in its twin wins over the file. An unknown key is an error.
    #[arg(long, env = "ZOE_CONFIG", value_name = "FILE")]
    config: Option<PathBuf>,

    /// The address of the ZEO port: HOST:PORT, or the path of a unix domain
    /// socket. A value with a / is a path, such as ./zeo.sock, as the
    /// address of the <zeo> section of ZEO. A port of 0 takes a free port.
    /// The server binds a relative path as it is, relative to its current
    /// directory, so the path must be shorter than 108 bytes. At its start,
    /// the server removes a socket file on which no process accepts
    /// connections, and fails when a process accepts connections there. At
    /// its stop, it removes its socket file. TLS needs HOST:PORT.
    #[arg(
        long,
        env = "ZOE_LISTEN",
        value_name = "ADDRESS",
        default_value = "127.0.0.1:8100",
        value_parser = listen_address
    )]
    listen: Listen,

    /// The address of the admin port, as HOST:PORT: an HTTP port for the
    /// probes. GET /healthz gives 200 while every store is open. GET
    /// /readyz gives 200 when every store is open and ready, and 503
    /// before and from a stop on. GET /metrics gives the metrics in the
    /// Prometheus text format. The port does not speak ZEO. A port of 0
    /// takes a free port. Without it, the server opens no admin port.
    #[arg(long, env = "ZOE_ADMIN_LISTEN", value_name = "HOST:PORT")]
    admin_listen: Option<SocketAddr>,

    /// The most transactions that getInvalidations covers. A client that
    /// reconnects further behind gets nil and verifies its cache.
    #[arg(
        long,
        env = "ZOE_INVALIDATION_QUEUE_SIZE",
        value_name = "TRANSACTIONS",
        default_value_t = defaults::INVALIDATION_QUEUE_SIZE
    )]
    invalidation_queue_size: usize,

    /// Past --invalidation-queue-size, getInvalidations also covers every
    /// transaction after the tid of a client when the time of that tid is
    /// less than this before the clock of the server, as the key
    /// invalidation-age of the <zeo> section of ZEO. In whole seconds,
    /// such as 3600, 60m or 1h. A client that comes back within this time
    /// keeps its cache. The server reads the transaction index of the
    /// store for it. 0, the default, turns it off.
    #[arg(
        long,
        env = "ZOE_INVALIDATION_AGE",
        value_name = "SECONDS",
        default_value = "0",
        value_parser = whole_seconds
    )]
    invalidation_age: Duration,

    /// The most invalidations that wait for one client. Past it, the server
    /// closes the connection, and the client reconnects and verifies its
    /// cache.
    #[arg(
        long,
        env = "ZOE_CLIENT_QUEUE_SIZE",
        value_name = "MESSAGES",
        default_value_t = defaults::CLIENT_QUEUE_SIZE as u64,
        value_parser = clap::value_parser!(u64).range(1..)
    )]
    client_queue_size: u64,

    /// The most bytes that the page cache of each store holds, such as
    /// 1073741824, 512MB or 1GB: KB, MB and GB in any case are powers of
    /// 1024, as in ZConfig's byte-size. The cache does not grow with the
    /// size of the store. With the blocking threads, the client queues and
    /// the staging memory of the open transactions, it bounds the resident
    /// memory of the server. The default is the default of the store, 1GB.
    #[arg(
        long,
        env = "ZOE_CACHE_SIZE",
        value_name = "BYTES",
        value_parser = byte_size
    )]
    cache_size: Option<usize>,

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

    /// Log each lock held longer than this, in whole seconds, such as 5,
    /// 5s or 1m, when the lock is released: at the finish, the abort or the
    /// end of the connection. The log line gives the oid, the client and
    /// the storage. 0 or no value logs no lock.
    #[arg(
        long,
        env = "ZOE_LOCK_WARN",
        value_name = "SECONDS",
        value_parser = whole_seconds
    )]
    lock_warn: Option<Duration>,

    /// Log each transaction open longer than this, in whole seconds, such
    /// as 30, 30s or 1m, when it ends. The log line gives the client and
    /// the storage. 0 or no value logs no transaction.
    #[arg(
        long,
        env = "ZOE_TRANSACTION_WARN",
        value_name = "SECONDS",
        value_parser = whole_seconds
    )]
    transaction_warn: Option<Duration>,

    /// The format of the log lines on stderr: text, or json for one JSON
    /// object on each line. Each log line of a connection gives its client
    /// (the label of set_client_label, or the address) and its storage.
    #[arg(
        long,
        env = "ZOE_LOG_FORMAT",
        value_name = "FORMAT",
        value_enum,
        default_value_t = LogFormat::Text
    )]
    log_format: LogFormat,

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
        default_value = defaults::SHUTDOWN_TIMEOUT,
        value_parser = whole_seconds
    )]
    shutdown_timeout: Duration,

    /// The longest time that a connection sends no frame, in whole seconds,
    /// such as 300, 300s or 5m. A heartbeat is a frame, and a stock client
    /// sends one every 60 seconds. The server closes a connection that is
    /// idle for longer: its open transaction aborts and its locks go, as
    /// when the client closes. The time while a call of the connection
    /// runs does not count: a client that waits for its reply is not idle.
    /// So a client that stalls between two calls, or a host that dies while
    /// no call runs, holds its locks at most this long. While a call runs,
    /// the time starts at the end of the call. 0 closes no idle connection.
    #[arg(
        long,
        env = "ZOE_IDLE_TIMEOUT",
        value_name = "SECONDS",
        default_value = defaults::IDLE_TIMEOUT,
        value_parser = whole_seconds
    )]
    idle_timeout: Duration,

    /// The longest time that a transaction stays open after its vote, in
    /// whole seconds, such as 30, 30s or 1m, as the key
    /// transaction-timeout of ZEO. The transaction holds its locks from
    /// its vote until tpc_finish or tpc_abort. Past this time, the server
    /// closes its connection: the transaction aborts and its locks go. A
    /// connection without a voted transaction stays open. 0 closes no
    /// connection for this reason.
    #[arg(
        long,
        env = "ZOE_TRANSACTION_TIMEOUT",
        value_name = "SECONDS",
        default_value = "0",
        value_parser = whole_seconds
    )]
    transaction_timeout: Duration,

    /// The number of threads that run the calls that read, such as
    /// loadBefore, for all the connections together. The default is the
    /// number of cores. A transaction, a download and a pack each run on a
    /// thread outside this pool, so a vote that waits for a lock never
    /// stops the loads.
    #[arg(
        long,
        env = "ZOE_BLOCKING_THREADS",
        value_name = "THREADS",
        value_parser = clap::value_parser!(NonZeroUsize)
    )]
    blocking_threads: Option<NonZeroUsize>,

    /// The compression of the records that the stores write: `none`, `zstd`,
    /// or `zstd:LEVEL`, such as `zstd:3`. A store reads every compression,
    /// whatever this option. The default is the default of the store, none:
    /// the records are raw.
    #[arg(
        long,
        env = "ZOE_COMPRESSION",
        value_name = "CODEC",
        value_parser = clap::builder::ValueParser::new(str::parse::<Compression>)
    )]
    compression: Option<Compression>,

    /// Serve every store for reads only. The server opens each store
    /// read-only, and holds its lock, so no writer opens the store
    /// meanwhile. It changes no byte of a store that was closed cleanly. It
    /// writes zoe.redb only when it repairs a store that a writer left
    /// open, and then logs a warning. The store must exist, and a
    /// store with blobs must get its blob directory. A client that
    /// registers to write gets ReadOnlyError; a client with
    /// read_only_fallback then registers read-only. A transaction fails at
    /// its vote, and new_oids and pack fail, with ReadOnlyError. The key
    /// read-only of a [storages.NAME] table makes one store read-only.
    #[arg(long, env = "ZOE_READ_ONLY", boolish = defaults::READ_ONLY)]
    read_only: bool,

    /// Pack each store with garbage collection, the default. With false, a
    /// pack removes old revisions only, as the pack-gc of FileStorage does:
    /// it keeps the newest revision of each object at or before the pack
    /// time, also when no reader can reach the object. An object whose
    /// newest revision there is a deletion goes. The key pack-gc = false of
    /// a [storages.NAME] table sets it for one store. The store records
    /// nothing, so give it at each start.
    #[arg(long, env = "ZOE_PACK_GC", boolish = defaults::PACK_GC)]
    pack_gc: bool,

    /// The mode of the structural merges (tier 1) of each class that
    /// --structural-merge-class does not name (ADR 0042). on, the default,
    /// merges BTree buckets, sets, BTrees of one bucket and Length at the
    /// vote, or refuses them as BTrees does. shadow runs the merges and
    /// writes their results to the resolution log, then lets the client
    /// resolve the conflict. off does not run them. A shadow mode needs
    /// --resolution-log.
    #[arg(
        long,
        env = "ZOE_STRUCTURAL_MERGES",
        value_name = "MODE",
        default_value = "on",
        value_parser = clap::builder::ValueParser::new(str::parse::<Mode>)
    )]
    structural_merges: Mode,

    /// The mode of one class of tier 1, as CLASS=MODE, such as
    /// BTrees.IOBTree.IOBucket=shadow. Repeat the option for more classes.
    /// The twin takes pairs separated by commas, and the configuration file
    /// a table [structural-merge-classes] of CLASS = MODE.
    #[arg(
        long,
        env = "ZOE_STRUCTURAL_MERGE_CLASSES",
        value_name = "CLASS=MODE",
        value_delimiter = ',',
        value_parser = class_mode
    )]
    structural_merge_class: Vec<(String, Mode)>,

    /// The mode of the posting-dict rule and the first-key rule of ADR
    /// 0042, two merges where ZODB's resolver refuses: on merges the
    /// posting dicts of ZCTextIndex entry by entry in the buckets with
    /// integer keys, and lets a delete of the first key by the committed
    /// transaction through in a tree of one bucket; shadow writes the
    /// merge of the rules to the resolution log, then ends the conflict as
    /// off does; off, the default, does not run the rules. A shadow mode
    /// needs --resolution-log.
    #[arg(
        long,
        env = "ZOE_POSTING_DICTS",
        value_name = "MODE",
        default_value = "off",
        value_parser = clap::builder::ValueParser::new(str::parse::<Mode>)
    )]
    posting_dicts: Mode,

    /// The resolution log: a file of JSON lines, one for each result of
    /// tier 1, with the user and the description of both transactions and
    /// the keys that it merged (ADR 0042). The server opens it in append
    /// mode at its start, and never syncs it. Without it, the server writes
    /// no such line.
    #[arg(long, env = "ZOE_RESOLUTION_LOG", value_name = "FILE")]
    resolution_log: Option<PathBuf>,

    /// Accept storeBlobShared: the server and its clients share the blob
    /// directory of each store, as with the option shared-blob-dir of a ZEO
    /// client. A client writes the blob file in the directory of its oid
    /// and sends only its name, which ends in .tmp or .tmp-. The server
    /// takes the file into the transaction by rename, and the client reads
    /// each committed blob file in place. At the start, the server writes
    /// the layout file of each blob directory. A blob directory without a
    /// layout file that holds other entries, such as the directories of an
    /// oid that a client made, fails the start. The clients have the trust
    /// of the user of the server, as with ZEO: run them as the same user. A
    /// client can put a symbolic link in place of a blob file after the
    /// check of the server. Without the option, storeBlobShared fails the
    /// vote with an error that names shared-blob-dir.
    #[arg(long, env = "ZOE_SHARED_BLOB_DIR", boolish = false)]
    shared_blob_dir: bool,

    /// The certificate chain of the server, as PEM, leaf first. With
    /// --tls-key, the ZEO port speaks TLS, so a ZEO client connects with
    /// its ssl option. A client that does not speak TLS gets no reply. A
    /// failed handshake closes only its connection. The admin port stays
    /// plain HTTP. The server reads the files at its start only.
    #[arg(long, env = "ZOE_TLS_CERT", value_name = "FILE", requires = "tls_key")]
    tls_cert: Option<PathBuf>,

    /// The private key of --tls-cert, as PEM.
    #[arg(long, env = "ZOE_TLS_KEY", value_name = "FILE", requires = "tls_cert")]
    tls_key: Option<PathBuf>,

    /// The CA certificates of the clients, as PEM. The server then requires
    /// a client certificate that chains to one of them, as the key
    /// authenticate of the <ssl> section of ZEO does, and refuses the
    /// handshake of another client. Needs --tls-cert.
    #[arg(
        long,
        env = "ZOE_TLS_CLIENT_CA",
        value_name = "FILE",
        requires = "tls_cert"
    )]
    tls_client_ca: Option<PathBuf>,

    /// The CA certificates of the read-only clients, as PEM. The server
    /// also accepts a client certificate that chains to one of them, and
    /// its holder registers read-only only: a register to write gets
    /// ReadOnlyError, and a client with read_only_fallback then registers
    /// read-only. A certificate that chains to --tls-client-ca registers
    /// as the client asks. With this option, a client certificate is
    /// necessary. Needs --tls-cert.
    #[arg(
        long,
        env = "ZOE_TLS_READ_ONLY_CLIENT_CA",
        value_name = "FILE",
        requires = "tls_cert"
    )]
    tls_read_only_client_ca: Option<PathBuf>,

    /// The stores to serve: the store of `--path`, then the stores of
    /// `--storage`, then the stores of the configuration file.
    /// [`configure`] sets them. Only the server reads them: a build without
    /// the feature `serve` fails before it runs the command.
    #[arg(skip)]
    #[cfg_attr(not(feature = "serve"), allow(dead_code))]
    storages: Vec<Storage>,
}

/// The value of `--listen`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Listen {
    /// `HOST:PORT`, with an IP address as host.
    Tcp(SocketAddr),
    /// The path of a unix domain socket, as the value gives it.
    Unix(PathBuf),
}

/// Parse the value of `--listen`. A value that holds a `/` is the path of
/// a unix domain socket, as ZConfig reads the `address` of the `<zeo>`
/// section. Any other value is `HOST:PORT`.
fn listen_address(value: &str) -> Result<Listen, String> {
    if value.contains('/') {
        return Ok(Listen::Unix(PathBuf::from(value)));
    }
    value.parse().map(Listen::Tcp).map_err(|_| {
        format!("{value} is neither HOST:PORT nor the path of a unix domain socket with a /")
    })
}

/// The format of the log lines of `zoe serve`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub(super) enum LogFormat {
    /// One line of text for each event.
    Text,
    /// One JSON object on each line.
    Json,
}

/// A store that the server serves under a storage name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Storage {
    pub(super) name: String,
    pub(super) path: PathBuf,
    /// The directory of the blob files. `None` is `blobs` in the store
    /// directory.
    pub(super) blob_dir: Option<PathBuf>,
    /// True when the server serves the store for reads only.
    pub(super) read_only: bool,
    /// False when a pack of the store keeps the objects that no reader can
    /// reach.
    pub(super) pack_gc: bool,
}

impl Storage {
    /// The store in `path` under the storage name `name`, with the
    /// defaults of `serve::StorageOptions`.
    pub(super) fn new(name: impl Into<String>, path: impl Into<PathBuf>) -> Self {
        Self {
            name: name.into(),
            path: path.into(),
            blob_dir: None,
            read_only: defaults::READ_ONLY,
            pack_gc: defaults::PACK_GC,
        }
    }
}

/// Parse the value of `--storage`: `NAME=PATH`. The name ends at the first
/// `=`. Neither the name nor the path is empty.
fn named_storage(value: &str) -> Result<Storage, String> {
    match value.split_once('=') {
        Some((name, path)) if !name.is_empty() && !path.is_empty() => Ok(Storage::new(name, path)),
        _ => Err(format!("{value} is not NAME=PATH")),
    }
}

/// Parse the value of `--structural-merge-class`: `CLASS=MODE`.
fn class_mode(value: &str) -> Result<(String, Mode), String> {
    zoe_core::merge::class_mode(value)
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

/// Parse a size in bytes, as ZConfig's `byte-size` does: a whole number,
/// 0 or more, with the suffix `KB`, `MB` or `GB` in any case, or none for
/// bytes. A suffix is a power of 1024.
fn byte_size(value: &str) -> Result<usize, String> {
    let text = value.trim().to_ascii_lowercase();
    let (number, unit) = match text.len().checked_sub(2).map(|n| text.split_at(n)) {
        Some((number, "kb")) => (number, 1 << 10),
        Some((number, "mb")) => (number, 1 << 20),
        Some((number, "gb")) => (number, 1 << 30),
        _ => (text.as_str(), 1),
    };
    number
        .trim_end()
        .parse::<usize>()
        .ok()
        .and_then(|n| n.checked_mul(unit))
        .ok_or_else(|| format!("{value} is not a whole number of bytes, 0 or more"))
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
        if let (Listen::Unix(path), Some(_)) = (&serve.listen, &serve.tls_cert) {
            return Err(usage_error(format!(
                "--tls-cert needs --listen HOST:PORT: TLS does not apply to the unix \
                 domain socket {}",
                path.display()
            )));
        }
        if shadow(serve) && serve.resolution_log.is_none() {
            return Err(usage_error(
                "a shadow mode needs --resolution-log: the log is where shadow mode writes what \
                 it finds"
                    .to_owned(),
            ));
        }
        serve.storages = storages(serve, config.storages).map_err(usage_error)?;
    }
    Ok(cli)
}

/// Tells if tier 1, a class of tier 1 or the rules of ADR 0042 are in shadow
/// mode.
#[cfg(feature = "serve")]
fn shadow(args: &ServeArgs) -> bool {
    args.structural_merges == Mode::Shadow
        || args.posting_dicts == Mode::Shadow
        || args
            .structural_merge_class
            .iter()
            .any(|(_, mode)| *mode == Mode::Shadow)
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
    if args.path.is_none() && args.blob_dir.is_some() {
        return Err(
            "--blob-dir is the blob directory of the store of --path: give --path too, \
             or the key blob-dir of a [storages.NAME] table"
                .to_owned(),
        );
    }
    let path = args.path.iter().map(|path| Storage {
        blob_dir: args.blob_dir.clone(),
        ..Storage::new(crate::protocol::STORAGE_NAME, path.clone())
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
/// `listening <address>`, to `out`. Handle both signals from before the
/// start: a signal during the start stops the server when the start ends.
#[cfg(feature = "serve")]
pub(super) fn run(args: &ServeArgs, out: &mut dyn Write) -> Result<(), Failure> {
    use std::num::NonZeroU32;

    use crate::serve::{Address, ServeOptions, Server, StorageOptions, TlsOptions};

    let storages = args.storages.iter().map(|storage| {
        let options = StorageOptions::new(&storage.name, &storage.path)
            .read_only(storage.read_only)
            .pack_gc(storage.pack_gc);
        match &storage.blob_dir {
            Some(dir) => options.blob_dir(dir),
            None => options,
        }
    });
    let listen = match &args.listen {
        Listen::Tcp(addr) => Address::Tcp(*addr),
        Listen::Unix(path) => Address::Unix(path.clone()),
    };
    let mut options = ServeOptions::with_storages(storages)
        .listen(listen)
        .invalidation_queue_size(args.invalidation_queue_size)
        .invalidation_age(args.invalidation_age)
        .client_queue_size(usize::try_from(args.client_queue_size).unwrap_or(usize::MAX))
        .shutdown_timeout(args.shutdown_timeout)
        .idle_timeout(args.idle_timeout)
        .transaction_timeout(args.transaction_timeout)
        .read_only(args.read_only)
        .pack_gc(args.pack_gc)
        .shared_blob_dir(args.shared_blob_dir)
        .structural_merges(zoe_core::StructuralMerges {
            mode: args.structural_merges,
            classes: args.structural_merge_class.iter().cloned().collect(),
            posting_dicts: args.posting_dicts,
            resolution_log: args.resolution_log.is_some(),
        });
    if let Some(path) = &args.resolution_log {
        options = options.resolution_log(path);
    }
    if let Some(timeout) = args.lock_timeout {
        options = options.lock_timeout(timeout);
    }
    if let Some(bytes) = args.cache_size {
        options = options.cache_size(bytes);
    }
    if let Some(compression) = args.compression {
        options = options.compression(compression);
    }
    if let Some(threshold) = args.lock_warn.filter(|t| !t.is_zero()) {
        options = options.lock_warn(threshold);
    }
    if let Some(threshold) = args.transaction_warn.filter(|t| !t.is_zero()) {
        options = options.transaction_warn(threshold);
    }
    // The traces flush when `_traces` drops, after the stop of the server.
    let _traces = init_logs(args.log_format)?;
    if let Some(addr) = args.admin_listen {
        options = options.admin_listen(addr);
    }
    if let Some(bytes) = args.max_frame_size.and_then(NonZeroU32::new) {
        options = options.max_frame_size(bytes);
    }
    if let Some(threads) = args.blocking_threads {
        options = options.blocking_threads(threads);
    }
    if let (Some(cert), Some(key)) = (&args.tls_cert, &args.tls_key) {
        let mut tls = TlsOptions::new(cert, key);
        if let Some(ca) = &args.tls_client_ca {
            tls = tls.client_ca(ca);
        }
        if let Some(ca) = &args.tls_read_only_client_ca {
            tls = tls.read_only_client_ca(ca);
        }
        options = options.tls(tls);
    }
    let server =
        Server::start_with_signals(options).map_err(|error| Failure::Message(error.to_string()))?;
    if let Some(admin) = server.admin_addr() {
        writeln!(out, "admin {admin}")?;
    }
    server.serve_until_signal(|addr| {
        writeln!(out, "listening {addr}")?;
        out.flush()
    })?;
    Ok(())
}

/// Send the log lines of the process to stderr, in `format`, from the
/// level INFO on. A JSON line holds the fields of the event and of its
/// spans. With `ZOE_OTLP_ENDPOINT`, also export the spans over OTLP, and
/// return the exporter. A process that has a subscriber already keeps it,
/// and exports nothing.
#[cfg(feature = "serve")]
fn init_logs(format: LogFormat) -> Result<Option<crate::telemetry::Telemetry>, Failure> {
    use tracing_subscriber::Layer;
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;

    use crate::telemetry::{Select, Telemetry, is_logged};

    let traces = Telemetry::start().map_err(Failure::Message)?;
    let logs = tracing_subscriber::fmt::layer().with_writer(std::io::stderr);
    let logs = match format {
        LogFormat::Text => logs.boxed(),
        LogFormat::Json => logs
            .json()
            .with_current_span(true)
            .with_span_list(true)
            .boxed(),
    };
    let installed = tracing_subscriber::registry()
        .with(logs.with_filter(Select(is_logged)))
        .with(traces.as_ref().map(Telemetry::layer))
        .try_init()
        .is_ok();
    Ok(traces.filter(|_| installed))
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
            Command::Serve(args) => Ok(*args),
            other => panic!("not serve: {other:?}"),
        }
    }

    #[test]
    fn listen_takes_host_and_port_or_a_path_with_a_slash() {
        assert_eq!(
            serve(&[]).unwrap().listen,
            Listen::Tcp("127.0.0.1:8100".parse().unwrap())
        );
        let args = serve(&["--listen", "[::1]:0"]).unwrap();
        assert_eq!(args.listen, Listen::Tcp("[::1]:0".parse().unwrap()));
        for path in ["./zeo.sock", "/run/zoe/zeo.sock", "dir/zeo.sock"] {
            let args = serve(&["--listen", path]).unwrap();
            assert_eq!(args.listen, Listen::Unix(PathBuf::from(path)), "{path}");
        }
        for value in ["zeo.sock", "localhost:8100", "8100"] {
            let error = serve(&["--listen", value]).unwrap_err();
            assert_eq!(
                error.kind(),
                clap::error::ErrorKind::ValueValidation,
                "{value}"
            );
        }
    }

    #[cfg(feature = "serve")]
    #[test]
    fn the_key_listen_of_the_configuration_file_takes_a_path() {
        use std::ffi::OsString;

        let file = std::env::temp_dir().join(format!("zoe-listen-{}.toml", std::process::id()));
        std::fs::write(&file, "path = \"store\"\nlisten = \"./zeo.sock\"\n").unwrap();
        let line = ["zoe", "serve", "--config"].map(OsString::from);
        let parsed = super::super::parse([&line[..], &[file.clone().into()]].concat());
        std::fs::remove_file(&file).unwrap();
        let Command::Serve(args) = parsed.unwrap().command else {
            panic!("not serve");
        };
        assert_eq!(args.listen, Listen::Unix(PathBuf::from("./zeo.sock")));
    }

    #[cfg(feature = "serve")]
    #[test]
    fn tls_on_a_unix_domain_socket_is_a_usage_error_that_names_both_options() {
        let line = [
            "zoe",
            "serve",
            "--path",
            "store",
            "--listen",
            "./zeo.sock",
            "--tls-cert",
            "c.pem",
            "--tls-key",
            "k.pem",
        ];
        let error = super::super::parse(line.map(Into::into).to_vec()).unwrap_err();
        assert_eq!(error.kind(), clap::error::ErrorKind::ValueValidation);
        let text = error.to_string();
        assert!(
            text.contains("--tls-cert") && text.contains("--listen"),
            "{text}"
        );
    }

    #[test]
    fn a_shadow_mode_without_a_resolution_log_is_a_usage_error() {
        let parse = |extra: &[&str]| {
            let line = ["zoe", "serve", "--path", "store"].iter().chain(extra);
            super::super::parse(line.map(Into::into).collect())
        };
        for shadow in [
            &["--structural-merges", "shadow"][..],
            &["--structural-merge-class", "BTrees.Length.Length=shadow"],
            &["--posting-dicts", "shadow"],
        ] {
            let error = parse(shadow).unwrap_err();
            assert_eq!(error.kind(), clap::error::ErrorKind::ValueValidation);
            assert!(error.to_string().contains("--resolution-log"), "{error}");
            let logged = [shadow, &["--resolution-log", "merges.log"]].concat();
            assert!(parse(&logged).is_ok(), "{logged:?}");
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
    fn the_invalidation_age_is_off_by_default_and_takes_whole_seconds() {
        assert_eq!(serve(&[]).unwrap().invalidation_age, Duration::ZERO);
        let args = serve(&["--invalidation-age", "1h"]).unwrap();
        assert_eq!(args.invalidation_age, Duration::from_secs(3_600));
        let args = serve(&["--invalidation-age", "100"]).unwrap();
        assert_eq!(args.invalidation_age, Duration::from_secs(100));
        let error = serve(&["--invalidation-age", "1.5"]).unwrap_err();
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
    fn the_transaction_timeout_is_off_by_default_and_takes_whole_seconds() {
        assert_eq!(serve(&[]).unwrap().transaction_timeout, Duration::ZERO);
        let args = serve(&["--transaction-timeout", "1m"]).unwrap();
        assert_eq!(args.transaction_timeout, Duration::from_secs(60));
        let error = serve(&["--transaction-timeout", "1.5"]).unwrap_err();
        assert_eq!(error.kind(), clap::error::ErrorKind::ValueValidation);
    }

    #[test]
    fn the_blocking_threads_are_the_cores_by_default_and_never_0() {
        assert_eq!(serve(&[]).unwrap().blocking_threads, None);
        let args = serve(&["--blocking-threads", "1"]).unwrap();
        assert_eq!(args.blocking_threads, NonZeroUsize::new(1));
        let error = serve(&["--blocking-threads", "0"]).unwrap_err();
        assert_eq!(error.kind(), clap::error::ErrorKind::ValueValidation);
    }

    #[test]
    fn the_logs_are_text_by_default_and_the_warnings_are_off() {
        let args = serve(&[]).unwrap();
        assert_eq!(args.log_format, LogFormat::Text);
        assert_eq!((args.lock_warn, args.transaction_warn), (None, None));
        let args = serve(&[
            "--log-format",
            "json",
            "--lock-warn",
            "5",
            "--transaction-warn",
            "1m",
        ])
        .unwrap();
        assert_eq!(args.log_format, LogFormat::Json);
        assert_eq!(args.lock_warn, Some(Duration::from_secs(5)));
        assert_eq!(args.transaction_warn, Some(Duration::from_secs(60)));
        let error = serve(&["--log-format", "xml"]).unwrap_err();
        assert_eq!(error.kind(), clap::error::ErrorKind::InvalidValue);
    }

    #[test]
    fn read_only_is_false_by_default_and_takes_a_boolean() {
        assert!(!serve(&[]).unwrap().read_only);
        assert!(serve(&["--read-only"]).unwrap().read_only);
        assert!(serve(&["--read-only=true"]).unwrap().read_only);
        assert!(!serve(&["--read-only=false"]).unwrap().read_only);
        let error = serve(&["--read-only=maybe"]).unwrap_err();
        assert_eq!(error.kind(), clap::error::ErrorKind::ValueValidation);
    }

    #[test]
    fn the_structural_merges_are_on_and_the_posting_rule_off_by_default() {
        let args = serve(&[]).unwrap();
        assert_eq!(args.structural_merges, Mode::On);
        assert!(args.structural_merge_class.is_empty());
        assert_eq!(args.posting_dicts, Mode::Off);
        assert_eq!(args.resolution_log, None);
        let args = serve(&[
            "--structural-merges",
            "shadow",
            "--structural-merge-class",
            "BTrees.IOBTree.IOBucket=on",
            "--structural-merge-class",
            "BTrees.Length.Length=off,BTrees.OOBTree.OOBucket=shadow",
            "--posting-dicts",
            "shadow",
            "--resolution-log",
            "merges.log",
        ])
        .unwrap();
        assert_eq!(args.structural_merges, Mode::Shadow);
        assert_eq!(
            args.structural_merge_class,
            [
                ("BTrees.IOBTree.IOBucket".to_owned(), Mode::On),
                ("BTrees.Length.Length".to_owned(), Mode::Off),
                ("BTrees.OOBTree.OOBucket".to_owned(), Mode::Shadow),
            ]
        );
        assert_eq!(args.posting_dicts, Mode::Shadow);
        assert_eq!(args.resolution_log, Some(PathBuf::from("merges.log")));
        for bad in [
            &["--structural-merges", "maybe"][..],
            &["--structural-merge-class", "IOBucket=on"],
            &["--posting-dicts", "perhaps"],
        ] {
            assert!(serve(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn pack_gc_is_true_by_default_and_takes_a_boolean() {
        assert!(serve(&[]).unwrap().pack_gc);
        assert!(serve(&["--pack-gc"]).unwrap().pack_gc);
        assert!(serve(&["--pack-gc=true"]).unwrap().pack_gc);
        assert!(!serve(&["--pack-gc=false"]).unwrap().pack_gc);
        let error = serve(&["--pack-gc=maybe"]).unwrap_err();
        assert_eq!(error.kind(), clap::error::ErrorKind::ValueValidation);
    }

    #[test]
    fn the_compression_keeps_the_default_of_the_store_and_takes_none_or_zstd() {
        assert_eq!(serve(&[]).unwrap().compression, None);
        for (value, compression) in [
            ("none", Compression::None),
            ("NONE", Compression::None),
            ("zstd", Compression::default()),
            ("zstd:1", Compression::Zstd { level: 1 }),
            ("zstd:19", Compression::Zstd { level: 19 }),
        ] {
            let args = serve(&["--compression", value]).unwrap();
            assert_eq!(args.compression, Some(compression), "{value}");
        }
        for value in ["lz4", "zstd:", "zstd:high", ""] {
            let error = serve(&["--compression", value]).unwrap_err();
            assert_eq!(
                error.kind(),
                clap::error::ErrorKind::ValueValidation,
                "{value}"
            );
        }
    }

    #[test]
    fn the_cache_size_keeps_the_default_of_the_store_and_takes_a_byte_size() {
        assert_eq!(serve(&[]).unwrap().cache_size, None);
        let args = serve(&["--cache-size", "16MB"]).unwrap();
        assert_eq!(args.cache_size, Some(16 << 20));
        let error = serve(&["--cache-size", "16 MiB"]).unwrap_err();
        assert_eq!(error.kind(), clap::error::ErrorKind::ValueValidation);
        for (value, bytes) in [
            ("0", 0),
            ("4096", 4096),
            ("1kb", 1 << 10),
            ("512MB", 512 << 20),
            ("1Gb", 1 << 30),
            (" 2 GB ", 2 << 30),
        ] {
            assert_eq!(byte_size(value), Ok(bytes), "{value}");
        }
        for value in [
            "",
            "mb",
            "-1",
            "1.5MB",
            "ten",
            "1TB",
            "1 KiB",
            "18446744073709551615kb",
        ] {
            assert!(byte_size(value).is_err(), "{value}");
        }
    }

    #[test]
    fn shared_blob_dir_is_false_by_default_and_takes_a_boolean() {
        assert!(!serve(&[]).unwrap().shared_blob_dir);
        assert!(serve(&["--shared-blob-dir"]).unwrap().shared_blob_dir);
        assert!(serve(&["--shared-blob-dir=true"]).unwrap().shared_blob_dir);
        assert!(!serve(&["--shared-blob-dir=false"]).unwrap().shared_blob_dir);
        let error = serve(&["--shared-blob-dir=maybe"]).unwrap_err();
        assert_eq!(error.kind(), clap::error::ErrorKind::ValueValidation);
    }

    #[test]
    fn tls_needs_a_certificate_and_its_key() {
        let args = serve(&[]).unwrap();
        assert_eq!((args.tls_cert, args.tls_key), (None, None));
        let args = serve(&[
            "--tls-cert",
            "c.pem",
            "--tls-key",
            "k.pem",
            "--tls-client-ca",
            "rw.pem",
            "--tls-read-only-client-ca",
            "ro.pem",
        ])
        .unwrap();
        assert_eq!(args.tls_cert, Some(PathBuf::from("c.pem")));
        assert_eq!(args.tls_key, Some(PathBuf::from("k.pem")));
        assert_eq!(args.tls_client_ca, Some(PathBuf::from("rw.pem")));
        assert_eq!(args.tls_read_only_client_ca, Some(PathBuf::from("ro.pem")));
        for line in [
            &["--tls-cert", "c.pem"][..],
            &["--tls-key", "k.pem"],
            &["--tls-client-ca", "rw.pem"],
            &["--tls-read-only-client-ca", "ro.pem"],
        ] {
            let error = serve(line).unwrap_err();
            assert_eq!(
                error.kind(),
                clap::error::ErrorKind::MissingRequiredArgument,
                "{line:?}"
            );
        }
    }

    #[cfg(feature = "serve")]
    #[test]
    fn blob_dir_is_the_blob_directory_of_the_store_of_path() {
        let args = serve(&["--blob-dir", "elsewhere"]).unwrap();
        let stores = storages(&args, Vec::new()).unwrap();
        assert_eq!(stores[0].blob_dir, Some(PathBuf::from("elsewhere")));
        assert_eq!(
            storages(&serve(&[]).unwrap(), Vec::new()).unwrap()[0].blob_dir,
            None
        );

        let line = ["zoe", "serve", "--storage", "a=a", "--blob-dir", "b"];
        let Command::Serve(args) = Cli::try_parse_from(line).unwrap().command else {
            panic!("not serve");
        };
        let error = storages(&args, Vec::new()).unwrap_err();
        assert!(error.contains("--path"), "{error}");
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
