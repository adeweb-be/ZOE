//! Loupe, the desktop inspector of ZOE stores.
//!
//! The window calls the IPC commands of this file. Each command returns an
//! object of `zoe-loupe`, with oids and tids as ZODB-form strings, or a
//! `Failure`.
//!
//! The "as of" switch holds a tid or nothing. `root`, `object` and
//! `resolve` read the open folder as of that tid, or as of the last
//! transaction when the switch holds nothing.
//!
//! The commands of the space screen walk the whole store or a large part
//! of it. Each one runs as a background job in a thread of the blocking
//! pool, and sends its progress to the window as `job-progress` events.
//! When the folder closes, its jobs stop at their next step, and the close
//! waits for them, so that the store lock goes.

mod failure;

use std::ops::ControlFlow;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use serde::Serialize;
use tauri::{AppHandle, Emitter, State};
use tauri_plugin_dialog::DialogExt;
use zoe_core::Tid;
use zoe_loupe::{
    Change, Folder, Object, PathSpace, Progress, Reclaim, Revision, Snapshot, Space, Walk,
};

use crate::failure::Failure;

/// The event that carries the progress of a background job.
const PROGRESS_EVENT: &str = "job-progress";

/// The shortest time between two progress events of one job.
const PROGRESS_INTERVAL: Duration = Duration::from_millis(100);

/// How often a close checks that the jobs of the folder have stopped.
const CLOSE_POLL: Duration = Duration::from_millis(10);

/// An open folder and the tid of the "as of" switch.
struct Open {
    /// The folder. Each background job holds a clone of it.
    folder: Arc<Folder>,
    /// The tid of the switch. `None` reads as of the last transaction.
    as_of: Option<Tid>,
    /// Set when the folder closes. The jobs of the folder then stop.
    stop: Arc<AtomicBool>,
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

    /// Return the open folder and the stop flag of its jobs.
    fn job_folder(&self) -> Result<(Arc<Folder>, Arc<AtomicBool>), Failure> {
        match &*self.open() {
            Some(open) => Ok((Arc::clone(&open.folder), Arc::clone(&open.stop))),
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

/// The progress of a background job, as a `job-progress` event gives it.
#[derive(Clone, Serialize)]
struct JobProgress {
    /// The command that runs the job.
    job: &'static str,
    /// The walk that runs.
    walk: Walk,
    /// The number of items that the walk has read.
    done: u64,
}

/// Run `job` on the open folder as a background job, in a thread of the
/// blocking pool. Send its progress as `job-progress` events with the name
/// `name`, at most one each `PROGRESS_INTERVAL`. Stop the job at its next
/// step when the folder closes: it then fails with `stopped`.
async fn run_job<T: Send + 'static>(
    app: AppHandle,
    loupe: &Loupe,
    name: &'static str,
    job: impl FnOnce(&Folder, &mut dyn FnMut(Progress) -> ControlFlow<()>) -> zoe_loupe::Result<T>
    + Send
    + 'static,
) -> Result<T, Failure> {
    let (folder, stop) = loupe.job_folder()?;
    let result = tauri::async_runtime::spawn_blocking(move || {
        let mut last: Option<Instant> = None;
        let mut progress = |progress: Progress| {
            if stop.load(Ordering::Relaxed) {
                return ControlFlow::Break(());
            }
            let now = Instant::now();
            if last.is_none_or(|last| now - last >= PROGRESS_INTERVAL) {
                last = Some(now);
                let event = JobProgress {
                    job: name,
                    walk: progress.walk,
                    done: progress.done,
                };
                // A window that does not listen loses nothing.
                let _ = app.emit(PROGRESS_EVENT, event);
            }
            ControlFlow::Continue(())
        };
        job(&folder, &mut progress)
    })
    .await
    .map_err(Failure::other)?;
    Ok(result?)
}

/// Close the folder `open`: stop its jobs and wait until they have
/// dropped the folder. Then the store is closed and its lock is free.
async fn close(open: Open) -> Result<(), Failure> {
    open.stop.store(true, Ordering::Relaxed);
    let folder = Arc::downgrade(&open.folder);
    drop(open);
    tauri::async_runtime::spawn_blocking(move || {
        // A pack dry run has no steps, so a close waits for its end.
        while folder.strong_count() > 0 {
            std::thread::sleep(CLOSE_POLL);
        }
    })
    .await
    .map_err(Failure::other)
}

/// Open the store folder `path` for reads only. Close the folder that is
/// open first, so that its lock goes. The "as of" switch holds nothing
/// after the open.
#[tauri::command]
async fn open_folder(loupe: State<'_, Loupe>, path: String) -> Result<FolderInfo, Failure> {
    let previous = loupe.open().take();
    if let Some(previous) = previous {
        close(previous).await?;
    }
    let folder = Folder::open(&path)?;
    let info = FolderInfo {
        path,
        last_tid: zoe_loupe::tid_repr(folder.last_tid()),
    };
    *loupe.open() = Some(Open {
        folder: Arc::new(folder),
        as_of: None,
        stop: Arc::new(AtomicBool::new(false)),
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

/// Measure the space of the open folder as of its last transaction: each
/// class, the blobs, the `largest` largest records and the growth. A
/// background job, `space`.
#[tauri::command]
async fn space(app: AppHandle, loupe: State<'_, Loupe>, largest: usize) -> Result<Space, Failure> {
    run_job(app, &loupe, "space", move |folder, progress| {
        folder.space_with(largest, progress)
    })
    .await
}

/// Measure the Plone path `path` and its items as of the last transaction.
/// A background job, `pathSpace`.
#[tauri::command]
async fn path_space(
    app: AppHandle,
    loupe: State<'_, Loupe>,
    path: String,
) -> Result<PathSpace, Failure> {
    run_job(app, &loupe, "pathSpace", move |folder, progress| {
        folder.path_space(&path, progress)
    })
    .await
}

/// Return what a pack at `tid` would remove, by the dry run of the core. A
/// tid of null, or after the last transaction, packs at the last
/// transaction. A background job, `reclaimable`, without progress steps.
#[tauri::command]
async fn reclaimable(
    app: AppHandle,
    loupe: State<'_, Loupe>,
    tid: Option<String>,
) -> Result<Reclaim, Failure> {
    let at = tid.as_deref().map(zoe_loupe::parse_tid).transpose()?;
    run_job(app, &loupe, "reclaimable", move |folder, _| {
        let last = folder.last_tid();
        let at = at.map_or(last, |at| at.min(last));
        Ok(Reclaim::new(at, &folder.reclaimable(at)?))
    })
    .await
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
            diff,
            space,
            path_space,
            reclaimable
        ])
        .run(tauri::generate_context!())
        .expect("Loupe failed to start");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_progress_event_names_the_job_the_walk_and_the_count() {
        let event = JobProgress {
            job: "space",
            walk: Walk::Records,
            done: 3,
        };
        assert_eq!(
            serde_json::to_value(event).unwrap(),
            serde_json::json!({"job": "space", "walk": "records", "done": 3})
        );
    }
}
