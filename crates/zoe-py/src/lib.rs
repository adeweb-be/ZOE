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
//!
//! `Reader` is the base of `zoe.reader.Reader`, the reader of ZOE, which
//! decodes a ZODB record in Rust (`reader`, ADR 0057).
//!
//! `Storage.catalog_query` answers a catalog query from the catalog mirrors
//! of the store (`catalog`, ADR 0059). Their thread starts at the open of a
//! store with a registered catalog, never at the import of the module.
//! `_catalog_throwaway` answers one query from a throwaway mirror, for the
//! mode `compare` of `zoe.plone.catalog` on a test storage. The text
//! methods of the storages (`text`, ADR 0060) stage the text deltas and
//! read the text index of each catalog, which the mirrors host.

mod catalog;
mod cli;
mod client;
mod errors;
mod kv;
mod publish;
mod reader;
mod sequence;
mod signals;
mod storage;
mod text;

/// The extension module.
#[pyo3::pymodule(gil_used = false)]
mod _zoe {
    use pyo3::prelude::*;

    #[pymodule_export]
    use crate::catalog::_catalog_throwaway;
    #[pymodule_export]
    use crate::cli::main;
    #[pymodule_export]
    use crate::client::{NativeClient, NativeIntents, NativeTransaction};
    #[pymodule_export]
    use crate::reader::{_reader_counts, Reader};
    #[pymodule_export]
    use crate::sequence::Sequence;
    #[pymodule_export]
    use crate::storage::{Intents, Storage, Transactions};

    #[pymodule_init]
    fn init(m: &Bound<'_, PyModule>) -> PyResult<()> {
        m.add("__version__", env!("CARGO_PKG_VERSION"))?;
        m.add(
            "OtherProtocol",
            m.py().get_type::<crate::client::OtherProtocol>(),
        )
    }
}
