//! The prototype of the client cache for the spike. Not for merge.
//!
//! `cache` holds the layout. With the feature `python`, `_cachex` is a Python
//! module whose `Cache` has the methods of `ZEO.cache.ClientCache` that
//! `ZEO.ClientStorage` calls.

pub mod cache;

#[cfg(feature = "python")]
mod python {
    use std::path::PathBuf;
    use std::sync::atomic::Ordering;

    use pyo3::exceptions::{PyOSError, PyValueError};
    use pyo3::prelude::*;
    use pyo3::types::{PyBytes, PyDict};

    use crate::cache::{Cache as Core, Config, Mode, OpenError, Session};

    fn u64_of(b: &[u8]) -> PyResult<u64> {
        let a: [u8; 8] = b
            .try_into()
            .map_err(|_| PyValueError::new_err("an oid or a tid has 8 bytes"))?;
        Ok(u64::from_be_bytes(a))
    }

    fn tid_of(b: Option<&[u8]>) -> PyResult<u64> {
        match b {
            None => Ok(0),
            Some(b) => u64_of(b),
        }
    }

    #[pyclass(frozen, name = "Cache")]
    struct Cache {
        s: Session,
    }

    #[pymethods]
    impl Cache {
        #[new]
        #[pyo3(signature = (path=None, size=200*1024*1024, shared=false, compress_min=None, level=1))]
        fn new(
            path: Option<PathBuf>,
            size: u64,
            shared: bool,
            compress_min: Option<usize>,
            level: i32,
        ) -> PyResult<Self> {
            let mode = match (&path, shared) {
                (None, _) => Mode::Memory,
                (Some(_), false) => Mode::Private,
                (Some(_), true) => Mode::Shared,
            };
            let cfg = Config {
                ring: size.max(1 << 16) / 8 * 8,
                mode,
                compress_min,
                level,
            };
            let core = Core::open(path.as_deref(), &cfg).map_err(|e| match e {
                OpenError::Io(e) => PyOSError::new_err(e.to_string()),
                OpenError::Locked => PyOSError::new_err("the cache file is locked"),
                OpenError::BadShared => PyOSError::new_err("the shared cache file is bad"),
            })?;
            Ok(Cache {
                s: Session::new(core),
            })
        }

        #[pyo3(name = "loadBefore")]
        fn load_before<'py>(
            &self,
            py: Python<'py>,
            oid: &[u8],
            before: &[u8],
        ) -> PyResult<Option<(Bound<'py, PyBytes>, Bound<'py, PyBytes>, Option<Bound<'py, PyBytes>>)>>
        {
            let oid = u64_of(oid)?;
            let before = u64_of(before)?;
            let r = self.s.cache.load_before_with(oid, before, |n, fill| {
                PyBytes::new_with(py, n, |buf| {
                    if fill(buf) {
                        Ok(())
                    } else {
                        Err(PyValueError::new_err("bad entry"))
                    }
                })
                .ok()
            });
            Ok(r.map(|(data, hit)| {
                let start = PyBytes::new(py, &hit.start.to_be_bytes());
                let end = (hit.end != 0).then(|| PyBytes::new(py, &hit.end.to_be_bytes()));
                (data, start, end)
            }))
        }

        #[pyo3(signature = (oid, start, end, data))]
        fn store(&self, oid: &[u8], start: &[u8], end: Option<&[u8]>, data: &[u8]) -> PyResult<()> {
            self.s.store(u64_of(oid)?, u64_of(start)?, tid_of(end)?, data);
            Ok(())
        }

        fn invalidate(&self, oid: &[u8], tid: Option<&[u8]>) -> PyResult<()> {
            let tid = match tid {
                None => None,
                Some(t) => Some(u64_of(t)?),
            };
            self.s.invalidate(u64_of(oid)?, tid);
            Ok(())
        }

        #[pyo3(name = "setLastTid")]
        fn set_last_tid(&self, tid: &[u8]) -> PyResult<()> {
            let t = u64_of(tid)?;
            if t != 0 {
                self.s.set_last_tid(t);
            }
            Ok(())
        }

        #[pyo3(name = "getLastTid")]
        fn get_last_tid<'py>(&self, py: Python<'py>) -> Bound<'py, PyBytes> {
            PyBytes::new(py, &self.s.last_tid().to_be_bytes())
        }

        fn clear(&self) {
            self.s.clear();
        }

        fn close(&self) {
            self.s.cache.close();
        }

        fn __len__(&self) -> usize {
            self.s.cache.len()
        }

        fn contents<'py>(&self, py: Python<'py>) -> Vec<(Bound<'py, PyBytes>, Bound<'py, PyBytes>)> {
            self.s
                .cache
                .contents()
                .into_iter()
                .map(|(o, t)| {
                    (
                        PyBytes::new(py, &o.to_be_bytes()),
                        PyBytes::new(py, &t.to_be_bytes()),
                    )
                })
                .collect()
        }

        /// The tid up to which the node applied the invalidations.
        fn applied<'py>(&self, py: Python<'py>) -> Bound<'py, PyBytes> {
            PyBytes::new(py, &self.s.cache.applied().to_be_bytes())
        }

        fn opened(&self) -> String {
            format!("{:?}", self.s.cache.opened)
        }

        fn stats<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyDict>> {
            let d = PyDict::new(py);
            let s = &self.s.cache.stats;
            for (k, v) in [
                ("hits", &s.hits),
                ("misses", &s.misses),
                ("bad", &s.bad),
                ("stores", &s.stores),
                ("applied", &s.applied),
                ("skipped", &s.skipped),
                ("refused", &s.refused),
                ("rearranged", &s.rearranged),
            ] {
                d.set_item(k, v.load(Ordering::Relaxed))?;
            }
            Ok(d)
        }
    }

    #[pymodule(gil_used = false)]
    fn _cachex(m: &Bound<'_, PyModule>) -> PyResult<()> {
        m.add_class::<Cache>()
    }
}
