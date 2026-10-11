//! The Python extension module `zoe._zoe`. The `zoe` package of the
//! `zoedb` wheel imports it.
//!
//! The module does not use the GIL, so that the GIL stays off when
//! CPython 3.14t loads it. It imports ZODB only when it raises an
//! exception of ZODB.
//!
//! `main` runs the `zoe` command. The console script `zoe` of the wheel
//! calls it through `zoe.__main__`.

mod cli;
mod errors;
mod publish;
mod sequence;
mod storage;

/// The extension module.
#[pyo3::pymodule(gil_used = false)]
mod _zoe {
    use pyo3::prelude::*;

    #[pymodule_export]
    use crate::cli::main;
    #[pymodule_export]
    use crate::sequence::Sequence;
    #[pymodule_export]
    use crate::storage::Storage;

    #[pymodule_init]
    fn init(m: &Bound<'_, PyModule>) -> PyResult<()> {
        m.add("__version__", env!("CARGO_PKG_VERSION"))
    }
}
