//! `zoe migrate`: move a store to the format of this build (ADR 0060,
//! section 2).
//!
//! `zoe_core::Store::migrate` does the work. The command runs on a store
//! folder that no process holds: it takes the lock of the store, so a held
//! store fails. A store of `ZOE format 3` moves to `ZOE format 4`: the
//! command folds its write-ahead log, then writes the format name and
//! creates the tables of the text deltas in one backend write with a sync.
//! A store of `ZOE format 4` stays as it is, and the command exits with 0.
//! Any other format fails with exit code 1 and gets no write.

use std::io::Write;
use std::path::PathBuf;

use clap::Args;
use zoe_core::{Migrated, Store};

use super::{Failure, store_failure};

/// The options of `zoe migrate`.
#[derive(Debug, Args)]
pub(super) struct MigrateArgs {
    /// The directory of the store. No process may hold it.
    #[arg(env = "ZOE_PATH", value_name = "STORE")]
    store: PathBuf,
}

/// Run `zoe migrate` and write its report to `out`.
pub(super) fn run(args: &MigrateArgs, out: &mut dyn Write) -> Result<(), Failure> {
    let path = &args.store;
    let migrated = Store::migrate(path).map_err(|error| store_failure(path, error))?;
    match migrated {
        Migrated::Moved => writeln!(out, "Moved the store {} to ZOE format 4", path.display())?,
        Migrated::Current => writeln!(
            out,
            "The store {} has ZOE format 4 already: nothing changed",
            path.display()
        )?,
    }
    Ok(())
}
