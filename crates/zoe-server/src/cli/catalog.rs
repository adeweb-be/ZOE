//! `zoe catalog`: the tools of the catalogs of a store. `zoe catalog
//! rebuild-text` rebuilds the text index of each registered catalog from
//! the live texts of the deltas (ADR 0060, section 15).
//!
//! The command runs on a store folder that no process holds. It opens the
//! store read-only, which takes the lock of the store, so a held store
//! fails. It writes nothing in the tables: on a store that was closed
//! cleanly, it changes no byte outside `text/`. `zoe_catalog::text::rebuild`
//! does the work. The report gives the documents and the time of each
//! catalog, never a text.

use std::io::Write;
use std::path::PathBuf;

use clap::{Args, Subcommand};
use zoe_catalog::text::rebuild;
use zoe_core::{Oid, Options};

use super::{Failure, open_read_only};

/// The options of `zoe catalog`.
#[derive(Debug, Args)]
pub(super) struct CatalogArgs {
    #[command(subcommand)]
    command: CatalogCommand,
}

/// The commands of `zoe catalog`.
#[derive(Debug, Subcommand)]
enum CatalogCommand {
    /// Rebuild the text index of each registered catalog of a store that
    /// no process holds, from the live texts of the deltas at the last
    /// tid. The tables do not change.
    RebuildText(RebuildTextArgs),
}

/// The options of `zoe catalog rebuild-text`.
#[derive(Debug, Args)]
struct RebuildTextArgs {
    /// The directory of the store. No process may hold it.
    #[arg(env = "ZOE_PATH", value_name = "STORE")]
    store: PathBuf,

    /// Rebuild the text index of this catalog only: the oid of its Catalog
    /// object, as 0x and 1 to 16 hexadecimal digits. The other text indexes
    /// stay as they are.
    #[arg(long, env = "ZOE_CATALOG", value_name = "OID", value_parser = parse_oid)]
    catalog: Option<Oid>,
}

/// Read an oid in ZODB form.
fn parse_oid(text: &str) -> Result<Oid, String> {
    Oid::parse(text).ok_or_else(|| "an oid is 0x and 1 to 16 hexadecimal digits".to_owned())
}

/// Run `zoe catalog` and write its report to `out`.
pub(super) fn run(args: &CatalogArgs, out: &mut dyn Write) -> Result<(), Failure> {
    match &args.command {
        CatalogCommand::RebuildText(args) => rebuild_text(args, out),
    }
}

/// Run `zoe catalog rebuild-text` and write its report to `out`.
fn rebuild_text(args: &RebuildTextArgs, out: &mut dyn Write) -> Result<(), Failure> {
    let path = &args.store;
    let store = open_read_only(path, Options::default())?;
    let fail = |why: String| Failure::Message(format!("{}: {why}", path.display()));
    let catalogs = rebuild::catalogs(&store, args.catalog.map(|o| o.0)).map_err(fail)?;
    if catalogs.is_empty() {
        writeln!(
            out,
            "No catalog of the store {} has a text index: nothing changed",
            path.display()
        )?;
        return Ok(());
    }
    writeln!(
        out,
        "The text indexes of the store {} at tid {}:",
        path.display(),
        store.last_transaction().hex()
    )?;
    for (catalog, settings) in &catalogs {
        let done = rebuild::rebuild(&store, path, *catalog, settings, &|_| {}).map_err(fail)?;
        let seconds = done.time.as_secs_f64();
        writeln!(
            out,
            "  catalog {}  rebuilt  {} documents  {seconds:.2} s",
            Oid(done.catalog).hex(),
            done.docs
        )?;
    }
    Ok(())
}
