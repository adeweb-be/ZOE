//! The Python extension module `zoe._zoe`. The `zoe` package of the
//! `zoedb` wheel imports it.
//!
//! The module does not use the GIL, so that the GIL stays off when
//! CPython 3.14t loads it. It imports ZODB only when it raises an
//! exception of ZODB.
//!
//! `main` runs the `zoe` command. The console script `zoe` of the wheel
//! calls it through `zoe.__main__`.
//!
//! The open of a store installs the handlers of SIGTERM and SIGHUP that
//! close the stores of the process (`signals`).
//!
//! `Client` is the native client of `zoe.zeo.ClientStorage` (`client`).
//! Its tokio runtime starts when the first client opens, never at the
//! import of the module.

mod cli;
mod client;
mod errors;
mod publish;
mod sequence;
mod signals;
mod storage;

/// The extension module.
#[pyo3::pymodule(gil_used = false)]
mod _zoe {
    use pyo3::prelude::*;

    #[pymodule_export]
    use crate::cli::main;
    #[pymodule_export]
    use crate::client::NativeClient;
    #[pymodule_export]
    use crate::sequence::Sequence;
    #[pymodule_export]
    use crate::storage::{Storage, Transactions};

    #[pymodule_init]
    fn init(m: &Bound<'_, PyModule>) -> PyResult<()> {
        m.add("__version__", env!("CARGO_PKG_VERSION"))
    }
}
