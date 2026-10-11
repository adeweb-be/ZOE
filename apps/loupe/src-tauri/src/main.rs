//! Loupe, the desktop inspector of ZOE stores.
//!
//! The window calls the IPC commands of this file. Each command returns an
//! object of `zoe-loupe`, with oids and tids as ZODB-form strings, or a
//! `Failure`.
//!
//! The "as of" switch holds a tid or nothing. `root`, `object` and
//! `resolve` read the open folder as of that tid, or as of the last
//! transaction when the switch holds nothing.

mod failure;

use std::sync::{Mutex, MutexGuard};

use serde::Serialize;
use tauri::{AppHandle, State};
use tauri_plugin_dialog::DialogExt;
use zoe_core::Tid;
use zoe_loupe::{Change, Folder, Object, Revision, Snapshot};

use crate::failure::Failure;

/// An open folder and the tid of the "as of" switch.
struct Open {
    folder: Folder,
    /// The tid of the switch. `None` reads as of the last transaction.
    as_of: Option<Tid>,
}

/// The state of the app: the open folder, if any.
#[derive(Default)]
struct Loupe {
    open: Mutex<Option<Open>>,
}

impl Loupe {
    fn open(&self) -> MutexGuard<'_, Option<Open>> {
        self.open.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Run `read` on the open folder.
    fn with_folder<T>(
        &self,
        read: impl FnOnce(&Folder) -> zoe_loupe::Result<T>,
    ) -> Result<T, Failure> {
        match &*self.open() {
            Some(open) => Ok(read(&open.folder)?),
            None => Err(Failure::NoFolder),
        }
    }

    /// Run `read` on the view of the open folder as of the tid of the
    /// switch.
    fn with_view<T>(
        &self,
        read: impl FnOnce(&Snapshot<'_>) -> zoe_loupe::Result<T>,
    ) -> Result<T, Failure> {
        match &*self.open() {
            Some(open) => {
                let tid = open.as_of.unwrap_or_else(|| open.folder.last_tid());
                Ok(read(&open.folder.as_of(tid))?)
            }
            None => Err(Failure::NoFolder),
        }
    }
}

/// What the window shows of an open folder.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct FolderInfo {
    /// The path of the folder.
    path: String,
    /// The tid of the last transaction, in ZODB form.
    last_tid: String,
}

/// Open the store folder `path` for reads only. Close the folder that is
/// open first, so that its lock goes. The "as of" switch holds nothing
/// after the open.
#[tauri::command]
async fn open_folder(loupe: State<'_, Loupe>, path: String) -> Result<FolderInfo, Failure> {
    let mut open = loupe.open();
    *open = None;
    let folder = Folder::open(&path)?;
    let info = FolderInfo {
        path,
        last_tid: zoe_loupe::tid_repr(folder.last_tid()),
    };
    *open = Some(Open {
        folder,
        as_of: None,
    });
    Ok(info)
}

/// Ask for a folder in the native dialog. Return `None` when the user
/// cancels.
#[tauri::command]
async fn pick_folder(app: AppHandle) -> Result<Option<String>, Failure> {
    let picked =
        tauri::async_runtime::spawn_blocking(move || app.dialog().file().blocking_pick_folder())
            .await
            .map_err(Failure::other)?;
    Ok(picked.map(|path| path.to_string()))
}

/// Return the root object.
#[tauri::command]
async fn root(loupe: State<'_, Loupe>) -> Result<Object, Failure> {
    loupe.with_view(|view| view.root())
}

/// Return the object `oid`, an oid in ZODB form.
#[tauri::command]
async fn object(loupe: State<'_, Loupe>, oid: String) -> Result<Object, Failure> {
    let oid = zoe_loupe::parse_oid(&oid)?;
    loupe.with_view(|view| view.object(oid))
}

/// Return the object that the Plone path `path` leads to.
#[tauri::command]
async fn resolve(loupe: State<'_, Loupe>, path: String) -> Result<Object, Failure> {
    loupe.with_view(|view| view.resolve(&path))
}

/// Return the revisions of the object `oid`, newest first.
#[tauri::command]
async fn history(loupe: State<'_, Loupe>, oid: String) -> Result<Vec<Revision>, Failure> {
    let oid = zoe_loupe::parse_oid(&oid)?;
    loupe.with_folder(|folder| folder.history(oid))
}

/// Set the "as of" switch to the tid `tid`, or to nothing for the last
/// transaction. The tid is in ZODB form. The view includes the
/// transaction `tid`.
#[tauri::command]
async fn set_as_of(loupe: State<'_, Loupe>, tid: Option<String>) -> Result<(), Failure> {
    let tid = tid.as_deref().map(zoe_loupe::parse_tid).transpose()?;
    match &mut *loupe.open() {
        Some(open) => {
            open.as_of = tid;
            Ok(())
        }
        None => Err(Failure::NoFolder),
    }
}

/// Return the changes of the object `oid` from its state as of `tid_a`
/// to its state as of `tid_b`.
#[tauri::command]
async fn diff(
    loupe: State<'_, Loupe>,
    oid: String,
    tid_a: String,
    tid_b: String,
) -> Result<Vec<Change>, Failure> {
    let oid = zoe_loupe::parse_oid(&oid)?;
    let (before, after) = (zoe_loupe::parse_tid(&tid_a)?, zoe_loupe::parse_tid(&tid_b)?);
    loupe.with_folder(|folder| folder.diff(oid, before, after))
}

fn main() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .manage(Loupe::default())
        .invoke_handler(tauri::generate_handler![
            open_folder,
            pick_folder,
            root,
            object,
            resolve,
            history,
            set_as_of,
            diff
        ])
        .run(tauri::generate_context!())
        .expect("Loupe failed to start");
}
