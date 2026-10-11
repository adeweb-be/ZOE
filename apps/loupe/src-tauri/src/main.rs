//! Loupe, the desktop inspector of ZOE stores.
//!
//! The window calls the IPC commands of this file. Each command returns an
//! object of `zoe-loupe`, with oids and tids as ZODB-form strings, or a
//! `Failure`.

mod failure;

use std::sync::{Mutex, MutexGuard};

use serde::Serialize;
use tauri::{AppHandle, State};
use tauri_plugin_dialog::DialogExt;
use zoe_loupe::{Folder, Object};

use crate::failure::Failure;

/// The state of the app: the open folder, if any.
#[derive(Default)]
struct Loupe {
    folder: Mutex<Option<Folder>>,
}

impl Loupe {
    fn folder(&self) -> MutexGuard<'_, Option<Folder>> {
        self.folder.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Run `read` on the open folder.
    fn with_folder<T>(
        &self,
        read: impl FnOnce(&Folder) -> zoe_loupe::Result<T>,
    ) -> Result<T, Failure> {
        match &*self.folder() {
            Some(folder) => Ok(read(folder)?),
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
/// open first, so that its lock goes.
#[tauri::command]
async fn open_folder(loupe: State<'_, Loupe>, path: String) -> Result<FolderInfo, Failure> {
    let mut folder = loupe.folder();
    *folder = None;
    let opened = Folder::open(&path)?;
    let info = FolderInfo {
        path,
        last_tid: zoe_loupe::tid_repr(opened.last_tid()),
    };
    *folder = Some(opened);
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
    loupe.with_folder(Folder::root)
}

/// Return the object `oid`, an oid in ZODB form.
#[tauri::command]
async fn object(loupe: State<'_, Loupe>, oid: String) -> Result<Object, Failure> {
    let oid = zoe_loupe::parse_oid(&oid)?;
    loupe.with_folder(|folder| folder.object(oid))
}

/// Return the object that the Plone path `path` leads to.
#[tauri::command]
async fn resolve(loupe: State<'_, Loupe>, path: String) -> Result<Object, Failure> {
    loupe.with_folder(|folder| folder.resolve(&path))
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
            resolve
        ])
        .run(tauri::generate_context!())
        .expect("Loupe failed to start");
}
