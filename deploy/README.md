# Deployment

This directory holds the files that run `zoe serve`: the Tier 1 manifests for Kubernetes, the units for a host, and the Grafana dashboard of the metrics. `just deploy check` checks them. The Helm chart comes in phase 5.

The user guide gives the steps for a user: [Deploy on a host](../docs/guide/deploy-host.md), [Deploy on Kubernetes](../docs/guide/deploy-kubernetes.md) and [Monitoring](../docs/guide/monitoring.md).

| Path | Holds |
| --- | --- |
| `tier1/` | the Tier 1 manifests: one server as a StatefulSet |
| `systemd/zoe.service` | the systemd unit of `zoe serve` on a host |
| `supervisor/zoe.conf` | the supervisor program of `zoe serve` on a host |
| `zoe.toml` | an example of the configuration file on a host |
| `grafana/zoe.json` | the Grafana dashboard |
| `tests/` | the tests that `just deploy check` runs |

## Tier 1 on Kubernetes

Tier 1 is one `zoe serve` as a StatefulSet with one replica, and one volume for the store and its blobs. The Plone instances are ZEO clients of the Service `zoe` on port 8100, under the storage name `1`.

| File | Object |
| --- | --- |
| `tier1/configmap.yaml` | the ConfigMap `zoe` with the configuration file `zoe.toml` |
| `tier1/statefulset.yaml` | the StatefulSet `zoe`, with the volume claim template `data` |
| `tier1/services.yaml` | the Service `zoe` on the ZEO port, and the headless Service `zoe-admin` on the admin port |
| `tier1/servicemonitor.yaml` | the ServiceMonitor `zoe`, which scrapes `/metrics` on the admin port |
| `tier1/poddisruptionbudget.yaml` | the PodDisruptionBudget `zoe` with `maxUnavailable: 0` |

Every object has the label `app.kubernetes.io/name: zoe`. Before you apply them, set:

- the tag of the image `ghcr.io/adeweb-be/zoe` in the StatefulSet, to a release version;
- the storage class of the volume claim template, to an SSD class of the cluster. The fsync latency of the class sets the write throughput: benchmark the class before you choose it;
- the size of the volume claim and the resources of the container;
- the labels of the ServiceMonitor that the `serviceMonitorSelector` of your Prometheus selects. The ServiceMonitor needs the CRDs of the Prometheus Operator; without them, do not apply `servicemonitor.yaml`.

Then apply the directory in a namespace:

    kubectl apply -n <namespace> -f deploy/tier1/

The probes use the admin port. The startup probe waits for `/healthz` while the stores open: an open can take long, for example when it rebuilds the free-space map after a stop that was not clean, and `/healthz` gives 503 until the end of the open. The liveness probe on `/healthz` starts after it. The readiness probe on `/readyz` keeps clients away until the stores are ready, and from SIGTERM on.

On SIGTERM, the server completes the `tpc_finish` calls that it received, aborts the other transactions and closes the stores, in at most `shutdown-timeout` (30 seconds). `terminationGracePeriodSeconds` is 45, above it. If you change `shutdown-timeout` in the ConfigMap, keep the grace period above it.

The PodDisruptionBudget stops a node drain from evicting the server, because one replica means that an eviction stops all writes. Move the pod in a manual window: delete it, and the StatefulSet starts it again.

The manifests hold no backup job yet. The backup job of Tier 1 comes with the backup of a store through the admin port of the server. Until then, `zoe backup --path` opens the store itself, and a running server holds the lock of the store: the backup runs only while the server is stopped (see [Back up and restore](../docs/guide/backup-restore.md) in the user guide).

## A host with systemd or supervisor

The server is one static binary, with no Python on the host. Install it as `/usr/local/bin/zoe`, create the user `zoe`, and install `zoe.toml` as `/etc/zoe/zoe.toml`. Set the addresses in it: the ZEO port on the private address that the clients reach, and the admin port on a private address, never on a public one.

- systemd: copy `systemd/zoe.service` to `/etc/systemd/system/`, then run `systemctl daemon-reload` and `systemctl enable --now zoe`. systemd creates `/var/lib/zoe` for the user `zoe`.
- supervisor: create `/var/lib/zoe` and `/var/log/zoe`, owned by `zoe`, copy `supervisor/zoe.conf` into the include directory of supervisord, then run `supervisorctl update`.

Both stop the server with SIGTERM and wait 45 seconds, above `shutdown-timeout`, before they send SIGKILL. Both restart the server when it fails; a stop on SIGTERM exits with 0, and is not a failure.

## The Grafana dashboard

Import `grafana/zoe.json` in Grafana and choose a Prometheus data source that scrapes the admin port. The dashboard plots, for each storage: the commit latency (`zoe_finish_latency_seconds`), the vote latency, the fsync batch size (`zoe_fsync_batch_size`), the lock wait (`zoe_lock_wait_seconds`), the conflicts resolved by path (`zoe_conflicts_resolved_total`) and raised, the invalidation queue depth (`zoe_invalidation_queue_depth`), the commits and loads per second, the load latency and the connected clients.
