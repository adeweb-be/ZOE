//! The block of SIGTERM and SIGINT while `zoe serve` installs its handlers.
//!
//! A signal that comes while a handler is installed can be lost: the
//! handler of signal-hook-registry finds no action until the registry
//! stores it after `sigaction`, and tokio subscribes its listener after it
//! installs the handler. The lost signal then does not stop the server.
//! While every thread blocks the signal, the kernel keeps it pending. It
//! comes when a thread unblocks it, after the handlers exist.

use std::io;

/// Block SIGTERM and SIGINT in the calling thread. Each thread that this
/// thread starts later inherits the block.
pub(crate) fn block_stop_signals() -> io::Result<()> {
    mask(libc::SIG_BLOCK)
}

/// Unblock SIGTERM and SIGINT in the calling thread. A signal that is
/// pending comes at once.
pub(super) fn unblock_stop_signals() -> io::Result<()> {
    mask(libc::SIG_UNBLOCK)
}

/// Change the signal mask of the calling thread for SIGTERM and SIGINT
/// with `how`.
#[allow(unsafe_code)]
fn mask(how: libc::c_int) -> io::Result<()> {
    let mut set = std::mem::MaybeUninit::<libc::sigset_t>::uninit();
    // SAFETY: `set` points to a local `sigset_t`. `sigemptyset` writes the
    // whole set before `sigaddset` and `pthread_sigmask` read it. The
    // signal numbers are valid. `pthread_sigmask` changes only the mask of
    // the calling thread and writes no old mask, because that pointer is
    // null.
    let result = unsafe {
        libc::sigemptyset(set.as_mut_ptr());
        libc::sigaddset(set.as_mut_ptr(), libc::SIGTERM);
        libc::sigaddset(set.as_mut_ptr(), libc::SIGINT);
        libc::pthread_sigmask(how, set.as_ptr(), std::ptr::null_mut())
    };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::from_raw_os_error(result))
    }
}
