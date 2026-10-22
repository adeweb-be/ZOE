# Deploy on Kubernetes

Tier 1 is one `zoe serve` as a StatefulSet with one replica, and one volume for the store and its blobs. The Plone instances are ZEO clients of the Service `zoe` on port 8100, with the storage name `1`. The manifests are in [`deploy/tier1/`](https://github.com/adeweb-be/ZOE/tree/main/deploy/tier1) of the repository. `just deploy check` tests them.

Tier 1 has one writer. A reschedule of the pod stops the server for some seconds, and the clients reconnect. A Helm chart and the tiers with replicas are not built yet.

## The objects

| File | Object |
| --- | --- |
| `configmap.yaml` | the ConfigMap `zoe`, with the configuration file `zoe.toml` |
| `statefulset.yaml` | the StatefulSet `zoe`, with the volume claim template `data` |
| `services.yaml` | the Service `zoe` on the ZEO port, and the headless Service `zoe-admin` on the admin port |
| `servicemonitor.yaml` | the ServiceMonitor `zoe`, which scrapes `/metrics` on the admin port |
| `poddisruptionbudget.yaml` | the PodDisruptionBudget `zoe`, with `maxUnavailable: 0` |

Every object has the label `app.kubernetes.io/name: zoe`.

## Before you apply them

Set these values in the files:

- the image of the StatefulSet: the image that you pushed to your registry, with a release tag (see [Install](install.md#push-the-image-to-your-registry)). The file names `ghcr.io/adeweb-be/zoe:latest`, which is not published yet;
- the storage class of the volume claim template, to an SSD class of your cluster. The fsync latency of the class sets the write throughput, so measure the class before you choose it;
- the size of the volume claim, 20 GiB in the file, and the resources of the container;
- the labels of the ServiceMonitor that the `serviceMonitorSelector` of your Prometheus selects. The ServiceMonitor needs the CRDs of the Prometheus Operator. Without them, do not apply `servicemonitor.yaml`.

Then apply the directory in a namespace:

```sh
kubectl apply -n NAMESPACE -f deploy/tier1/
```

## How the pod runs

- The image sets `ZOE_PATH=/var/lib/zoe` and `ZOE_LISTEN=0.0.0.0:8100`. The volume `data` mounts at `/var/lib/zoe`. The ConfigMap gives the other settings, in `ZOE_CONFIG=/etc/zoe/zoe.toml`. An environment variable wins over a key of the file.
- The container runs as the user 65532, with a read-only root file system and no capability.
- The startup probe waits for `/healthz` while the store opens, up to one hour. An open after a stop that was not clean repairs the database file, and takes longer on a large store. The liveness probe on `/healthz` starts after it.
- The readiness probe on `/readyz` keeps the clients away until the store is ready, and from SIGTERM on.

## Stop and move the pod

On SIGTERM, the server completes the commits that it received, aborts the other transactions and closes the store, in at most `shutdown-timeout`, 30 seconds. `terminationGracePeriodSeconds` is 45, above it. If you raise `shutdown-timeout` in the ConfigMap, raise the grace period above it.

The PodDisruptionBudget stops a node drain from evicting the server: one replica means that an eviction stops all writes. Move the pod in a planned window: delete it, and the StatefulSet starts it again, on the same volume.

## Back up

The manifests hold no backup job yet. `zoe backup` opens the store itself, and the running server holds the lock of the store. So a backup needs the server stopped, for example with the StatefulSet scaled to 0 and a job that mounts the volume. An online backup through the server is not built yet. See [Back up and restore](backup-restore.md).
