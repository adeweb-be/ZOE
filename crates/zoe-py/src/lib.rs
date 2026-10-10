//! The Python extension module `zoe._zoe`. The `zoe` package of the
//! `zoedb` wheel imports it.
//!
//! The module does not use the GIL, so that the GIL stays off when
//! CPython 3.14t loads it.

/// The extension module.
#[pyo3::pymodule(gil_used = false)]
mod _zoe {
    use pyo3::prelude::*;

    #[pymodule_init]
    fn init(m: &Bound<'_, PyModule>) -> PyResult<()> {
        m.add("__version__", env!("CARGO_PKG_VERSION"))
    }
}
