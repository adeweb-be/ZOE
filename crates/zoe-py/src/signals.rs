//! The close of the stores of the process at SIGTERM and SIGHUP.
//!
//! When `zoe.Storage` opens a store, `install` installs a handler of each
//! signal whose action Python sees as `signal.SIG_DFL`. An ignored signal
//! and a handler of the application keep their action. The handler of
//! signal-hook only writes to a pipe, which is safe in a signal handler.
//!
//! The watcher thread reads the pipe. At a signal, it closes each open
//! storage of the process, as `close` does, and waits at most `BOUND` for
//! the calls that hold a store. Then it restores the default action of the
//! signal and raises the signal again. So the process ends with the status
//! of the signal, as without the handler.
//!
//! The watcher never attaches to the interpreter: it runs no Python code,
//! takes no GIL and drops no Python reference.
//!
//! A handler that the application sets later with `signal.signal` replaces
//! the handler of the extension. The extension never puts its handler
//! back. A child of `os.fork()` has no watcher, so the at-fork hook gives
//! it the default action back.

use std::ffi::c_int;
use std::panic::{self, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::thread;
use std::time::{Duration, Instant};

use pyo3::prelude::*;
use pyo3::sync::PyOnceLock;
use pyo3::types::PyDict;
use signal_hook::consts::{SIGHUP, SIGTERM};
use signal_hook::iterator::{Handle, Signals};
use signal_hook::low_level::emulate_default_handler;
use zoe_client::lock;

use crate::storage::{Shared, StoreRef};

/// The longest time from a signal to the end of the process, for all the
/// storages together. Docker and supervisord wait 10 seconds before they
/// send SIGKILL.
const BOUND: Duration = Duration::from_secs(5);

/// The time between two checks of the stores that a call holds.
const POLL: Duration = Duration::from_millis(1);

/// A signal that closes the stores.
struct Watched {
    signum: c_int,
    /// True when the extension installs or installed its handler.
    installed: AtomicBool,
}

/// The signals that close the stores.
static WATCHED: [Watched; 2] = [
    Watched {
        signum: SIGTERM,
        installed: AtomicBool::new(false),
    },
    Watched {
        signum: SIGHUP,
        installed: AtomicBool::new(false),
    },
];

/// The open storages of the process.
static OPEN: Mutex<Vec<Weak<Shared>>> = Mutex::new(Vec::new());

/// The handle of the watcher. The first install starts the watcher.
static WATCHER: PyOnceLock<Handle> = PyOnceLock::new();

/// Add a storage to the open storages of the process.
pub(crate) fn register(shared: &Arc<Shared>) {
    let mut open = lock(&OPEN);
    open.retain(|storage| storage.strong_count() > 0);
    open.push(Arc::downgrade(shared));
}

/// Install the handler of each watched signal that has no handler of the
/// extension yet, when Python sees its action as `signal.SIG_DFL`.
///
/// A return of `None` from `signal.getsignal` is a handler that Python did
/// not install. It is not the default, so the extension installs nothing
/// for that signal.
pub(crate) fn install(py: Python<'_>) -> PyResult<()> {
    if WATCHED
        .iter()
        .all(|watched| watched.installed.load(Ordering::Acquire))
    {
        return Ok(());
    }
    let signal = py.import("signal")?;
    let default = signal.getattr("SIG_DFL")?;
    for watched in &WATCHED {
        if watched.installed.load(Ordering::Acquire) {
            continue;
        }
        let action = signal.call_method1("getsignal", (watched.signum,))?;
        if !action.is(&default) {
            continue;
        }
        let watcher = WATCHER.get_or_try_init(py, || start(py))?;
        // The flag comes first, so that a fork during the install gives
        // the child the default action. Two opens can install at the same
        // time: `add_signal` installs a handler once per signal.
        watched.installed.store(true, Ordering::Release);
        if let Err(error) = watcher.add_signal(watched.signum) {
            watched.installed.store(false, Ordering::Release);
            return Err(error.into());
        }
    }
    Ok(())
}

/// Register the at-fork hook, then start the watcher thread. Return the
/// handle that installs the handlers.
fn start(py: Python<'_>) -> PyResult<Handle> {
    let hook = wrap_pyfunction!(after_fork_in_child, py)?;
    let kwargs = PyDict::new(py);
    kwargs.set_item("after_in_child", hook)?;
    py.import("os")?
        .call_method("register_at_fork", (), Some(&kwargs))?;
    let signals = Signals::new(std::iter::empty::<c_int>())?;
    let handle = signals.handle();
    thread::Builder::new()
        .name("zoe-signals".to_owned())
        .spawn(move || watch(signals))?;
    Ok(handle)
}

/// Give the child of a fork the default action of each watched signal
/// that has a handler of the extension and whose action Python still sees
/// as `signal.SIG_DFL`. The child has no watcher, so the handler of the
/// extension would never end it. A handler of the application stays.
#[pyfunction]
fn after_fork_in_child(py: Python<'_>) -> PyResult<()> {
    let signal = py.import("signal")?;
    let default = signal.getattr("SIG_DFL")?;
    for watched in &WATCHED {
        if watched.installed.load(Ordering::Acquire)
            && signal
                .call_method1("getsignal", (watched.signum,))?
                .is(&default)
        {
            signal.call_method1("signal", (watched.signum, &default))?;
        }
    }
    Ok(())
}

/// The body of the watcher thread. Wait for the first signal, close the
/// open storages, then end the process with the status of the signal.
///
/// A second signal during the close changes nothing.
fn watch(mut signals: Signals) {
    if let Some(signum) = signals.forever().next() {
        let deadline = Instant::now() + BOUND;
        // A close that fails or panics does not stop the end of the
        // process.
        let _ = panic::catch_unwind(AssertUnwindSafe(|| close_all(deadline)));
        // For SIGTERM and SIGHUP, this ends the process.
        let _ = emulate_default_handler(signum);
    }
    // The default action did not end the process.
    std::process::abort();
}

/// Close each open storage of the process. Stop at `deadline`.
///
/// A store that a call still holds at `deadline` stays open, and the
/// others close.
fn close_all(deadline: Instant) {
    let open: Vec<Arc<Shared>> = lock(&OPEN).iter().filter_map(Weak::upgrade).collect();
    let mut closing: Vec<Closing> = open.into_iter().map(Closing::start).collect();
    loop {
        closing.retain_mut(|storage| !storage.step());
        if closing.is_empty() || Instant::now() >= deadline {
            break;
        }
        thread::sleep(POLL);
    }
    // The process ends next. No close starts after the deadline, so the
    // shares of the stores that calls hold are not dropped.
    std::mem::forget(closing);
}

/// The close of one storage by the watcher.
struct Closing {
    shared: Arc<Shared>,
    /// The share of the store that the watcher took. It is `None` when
    /// `close` or the drop of the storage took the store first.
    store: Option<StoreRef>,
}

impl Closing {
    /// Mark the storage closed and take its store.
    fn start(shared: Arc<Shared>) -> Self {
        let store = shared.mark_closed();
        Self { shared, store }
    }

    /// Abort the transactions that no call uses, then close the store if
    /// no call holds it. Return true when the database is closed.
    fn step(&mut self) -> bool {
        self.shared.abort_idle();
        match self.store.take() {
            // `close` or the drop closes the store. Wait until that close
            // ends.
            None => self.shared.database_closed(),
            Some(store) => match store.try_close() {
                Ok(()) => true,
                Err(store) => {
                    self.store = Some(store);
                    false
                }
            },
        }
    }
}
