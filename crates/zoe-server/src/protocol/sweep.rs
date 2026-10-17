//! The sweep of the ephemeral KV namespaces of the stores (ADR 0043).
//!
//! Once a minute, the server runs a sweep step for each writable store
//! that has the table of the ephemeral namespaces, on a blocking thread.
//! When a step takes `SWEEP_LIMIT` keys, the next step runs at once. A
//! step removes the expired keys without a pack. A read-only store and a
//! halted store run no step.
//!
//! The task keeps only weak references to the stores, so it never keeps a
//! store open, and `Storages::held` does not count it. It is not a task of
//! the stop: the stop of the runtime drops it, and a step that runs ends
//! first.

use std::sync::{Arc, Weak};

use zoe_core::{SWEEP_INTERVAL, SWEEP_LIMIT, Store, ZoeError};

use super::{Stopping, Storages};

/// Return the writable stores of `storages`, each with its name.
pub(crate) fn stores(storages: &Storages) -> Vec<(String, Weak<Store>)> {
    storages
        .storages
        .iter()
        .filter(|storage| !storage.read_only)
        .map(|storage| (storage.name.clone(), Arc::downgrade(&storage.store)))
        .collect()
}

/// Run a sweep step of each store of `stores` once a minute, until the
/// stop.
pub(crate) async fn run(stores: Vec<(String, Weak<Store>)>, stopping: Stopping) {
    if stores.is_empty() {
        return;
    }
    loop {
        tokio::time::sleep(SWEEP_INTERVAL).await;
        if stopping.is_set() {
            return;
        }
        for (name, store) in &stores {
            let Some(store) = store.upgrade() else {
                continue;
            };
            let (name, stopping) = (name.clone(), stopping.clone());
            // A step that panics ends only its thread.
            let _ = tokio::task::spawn_blocking(move || sweep(&name, &store, &stopping)).await;
        }
    }
}

/// Run sweep steps on `store` while each takes `SWEEP_LIMIT` keys, until
/// the stop. Log an error of a step, except for a halted store, which runs
/// no step.
fn sweep(name: &str, store: &Store, stopping: &Stopping) {
    loop {
        match store.sweep(SWEEP_LIMIT) {
            Ok(swept) if swept.taken == SWEEP_LIMIT && !stopping.is_set() => {}
            Ok(_) | Err(ZoeError::Halted) => return,
            Err(error) => {
                tracing::warn!(
                    storage = name,
                    error = %error,
                    "a sweep step of the ephemeral namespaces failed"
                );
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use zoe_core::Flavour;
    use zoe_core::vfs::MemVfs;

    use super::*;

    #[test]
    fn a_sweep_runs_steps_until_a_step_takes_fewer_keys_than_the_limit() {
        let store = Store::open_vfs(MemVfs::new()).unwrap();
        // More keys than one step takes, with a TTL that ends at once.
        let mut txn = store.tpc_begin(b"", b"", b"").unwrap();
        for i in 0..(SWEEP_LIMIT as u32 + 10) {
            let key = i.to_be_bytes();
            txn.kv_set(
                Flavour::Ephemeral,
                "s",
                &key,
                b"v",
                Some(Duration::from_millis(1)),
            )
            .unwrap();
        }
        txn.vote().unwrap();
        txn.finish().unwrap();
        std::thread::sleep(Duration::from_millis(5));
        let (_signal, stopping) = Stopping::new();
        sweep("1", &store, &stopping);
        assert_eq!(store.sweep(SWEEP_LIMIT).unwrap().taken, 0);
        assert!(
            store
                .kv_scan(Flavour::Ephemeral, "s", None, None, 10)
                .unwrap()
                .is_empty()
        );
    }
}
