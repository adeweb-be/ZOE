//! `zoe._zoe.main`: the `zoe` command for the console script of the wheel.
//!
//! The function runs `zoe_server::cli::main`, the code of the `zoe`
//! binary. So the console script and the binary run the same code.

use std::ffi::OsString;

use pyo3::prelude::*;

/// Run the `zoe` command with `argv`, the program name first, and return
/// the exit code: 0 on success, 1 on a failure, 2 on a usage error.
///
/// The command runs without the GIL. It writes its report to the stdout
/// of the process and its messages to the stderr of the process, not to
/// `sys.stdout` and `sys.stderr`.
#[pyfunction]
pub(crate) fn main(py: Python<'_>, argv: Vec<OsString>) -> i32 {
    py.detach(move || zoe_server::cli::main(argv))
}
