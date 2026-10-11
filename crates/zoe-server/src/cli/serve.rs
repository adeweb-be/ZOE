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
use std::time::Duration;

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

/// Serve until SIGTERM. Write the line `listening <address>` to `out`.
#[cfg(feature = "serve")]
pub(super) fn run(args: &ServeArgs, out: &mut dyn Write) -> Result<(), Failure> {
    use crate::serve::{ServeOptions, Server};

    let mut options = ServeOptions::new(&args.path).listen(args.listen);
    if let Some(timeout) = args.lock_timeout {
        options = options.lock_timeout(timeout);
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

#[cfg(test)]
mod tests {
    use super::*;

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
