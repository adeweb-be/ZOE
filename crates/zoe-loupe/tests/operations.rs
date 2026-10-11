//! Acceptance test of the operations screen of Loupe (F15, F25, N15).
//!
//! Each operation asks for a confirmation in the window, and the Rust side
//! checks it:
//!
//! - `Folder::verify()` runs the checks of `zoe verify` and gives the same
//!   findings. It reads only.
//! - `Folder::backup(dir)` writes a dump into `dir`, as `zoe backup --path`
//!   does: full, then incremental, nothing when nothing is new. It reads
//!   only.
//! - `Folder::pack_dry_run(at)` gives the report of the dry run of the
//!   core. `Folder::pack(at, &report)` packs only when a new dry run gives
//!   the same report: the operator confirmed what the pack removes. A
//!   folder opens read-only; the pack reopens the store for writes for its
//!   own run and then read-only again.
//! - In server mode, `Remote::clients()` gives the number of clients that
//!   the server reports, and `Remote::pack(at)` packs through the server.

mod common;

use zoe_core::Tid;
use zoe_loupe::{Folder, Remote};
use zoe_server::serve::{ServeOptions, Server};

use common::{fresh_dir, write_store};

fn files(dir: &std::path::Path) -> usize {
    common::files(dir).len()
}

#[test]
fn verify_finds_nothing_on_a_sound_store_and_changes_nothing() {
    let dir = fresh_dir("ops-verify");
    write_store(&dir);
    let before = common::files(&dir);
    let folder = Folder::open(&dir).unwrap();
    let report = folder.verify().unwrap();
    assert!(report.is_clean(), "{report:?}");
    drop(folder);
    assert_eq!(common::files(&dir), before);
}

#[test]
fn a_backup_writes_a_dump_and_then_nothing_new() {
    let dir = fresh_dir("ops-backup");
    write_store(&dir);
    let dumps = fresh_dir("ops-backup-dumps");
    let folder = Folder::open(&dir).unwrap();
    folder.backup(&dumps).unwrap();
    let first = files(&dumps);
    assert!(first > 0);
    folder.backup(&dumps).unwrap();
    assert_eq!(files(&dumps), first);
}

#[test]
fn a_pack_runs_only_on_the_report_that_the_operator_confirmed() {
    let dir = fresh_dir("ops-pack");
    let fixture = write_store(&dir);
    let at = fixture.last_tid();
    let mut folder = Folder::open(&dir).unwrap();
    let report = folder.pack_dry_run(at).unwrap();

    // A report of another tid is not a confirmation of this pack.
    let other = folder.pack_dry_run(Tid(at.0 - 1)).unwrap();
    if other != report {
        assert!(folder.pack(at, &other).is_err());
    }

    let done = folder.pack(at, &report).unwrap();
    assert_eq!(done, report);
    // The folder is read-only again, and a second dry run finds nothing.
    let again = folder.pack_dry_run(at).unwrap();
    assert_ne!(again, report);
}

#[test]
fn the_server_mode_shows_clients_and_packs_through_the_server() {
    let dir = fresh_dir("ops-server");
    let fixture = write_store(&dir);
    let server = Server::start(ServeOptions::new(&dir)).unwrap();
    let remote = Remote::connect(server.addr(), "1").unwrap();
    assert!(remote.clients().unwrap() >= 1);
    remote.pack(fixture.last_tid()).unwrap();
    drop(remote);
    server.stop();
}
