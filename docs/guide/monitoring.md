# Monitoring

`zoe serve` reports through four channels: the admin port, the metrics, the logs and the traces. The in-process storage has no admin port and no metrics.

## The admin port

`--admin-listen HOST:PORT`, or the key `admin-listen` of the configuration file, opens the admin port: a small HTTP server for the probes of a supervisor and for Prometheus. Without it, the server opens no admin port. Put it on a private address: it has no authentication.

| Path | 200 when | 503 when |
| --- | --- | --- |
| `/healthz` | every store is open | the stores still open |
| `/readyz` | every store is open and ready | the stores still open; the server stops; the lock table of a store held entries at the open |
| `/metrics` | every store is open | the stores still open |

The body of `/healthz` has one line for each storage, such as `1: open`, or the line `starting`. During a stop, `/healthz` still answers 200. The body of `/readyz` has one line for each storage, such as `1: ready`, or the line `starting` or `stopping`. A store whose lock table held entries at the open answers `NAME: not ready: the lock table held entries at the open`; see [Troubleshooting](troubleshooting.md#readyz-answers-not-ready).

Use `/healthz` for a liveness probe and `/readyz` for a readiness probe. An open after a stop that was not clean repairs the database file, and takes longer on a large store: give the startup probe time.

## Metrics

`/metrics` gives the metrics in the Prometheus text format. Each metric has the label `storage`, the storage name. Some metrics to watch:

| Metric | Watch for |
| --- | --- |
| `zoe_finish_latency_seconds` | the time of a commit until it is durable. It follows the fsync latency of the disk. |
| `zoe_vote_latency_seconds`, `zoe_lock_wait_seconds` | votes that wait for the locks of hot objects |
| `zoe_conflicts_raised_total` | the conflicts that the clients see, which make Zope retry a request |
| `zoe_conflicts_resolved_total` | the conflicts that the store merged (`path="native"`) or that the client resolved (`path="python"`) |
| `zoe_structural_merges_total` | the results of the merges, by tier, mode and outcome, also in shadow mode |
| `zoe_writes_on_get_total` | the records that GET and HEAD requests wrote, by class. `zoe.plone` marks these requests. |
| `zoe_clients_connected` | the connected clients |
| `zoe_invalidation_queue_depth` | a client that reads its invalidations too slowly. Past `--client-queue-size`, the server closes its connection. |

[Admin port, metrics and logs](../reference/metrics.md) lists every metric and the buckets of the histograms.

## The Grafana dashboard

[`deploy/grafana/zoe.json`](https://github.com/adeweb-be/ZOE/blob/main/deploy/grafana/zoe.json) is a dashboard of these metrics. Import it in Grafana, and choose a Prometheus data source that scrapes the admin port. It plots, for each storage: the commit latency, the vote latency, the fsync batch size, the lock wait, the conflicts, the invalidation queue depth, the commits and loads per second, the load latency and the connected clients.

## Logs

The logs go to stderr, from the level INFO on. `--log-format json` writes one JSON object on each line, for a log collector. Each line of a connection names the client: the label that the client set, or its address. A log line holds no pickle bytes, no user and no description of a transaction.

Lines to look for:

| Message | Means |
| --- | --- |
| `lock held past lock-warn` | a commit held the lock of an object longer than `--lock-warn`. The line names the oid. |
| `transaction open past transaction-warn` | a transaction stayed open longer than `--transaction-warn` |
| `repaired the store` | the store was not closed cleanly, and the open repaired it |
| `transaction timeout after the vote: closing the connection` | a client voted and then did not finish within `--transaction-timeout` |
| `no frame within the idle timeout: closing the connection` | a client sent nothing for `--idle-timeout` |
| `the shutdown timeout ends: closing the connections that remain` | a stop reached `--shutdown-timeout` |

`--lock-warn` and `--transaction-warn` are off by default. Turn them on to find slow commits.

## Traces

`zoe serve` exports OpenTelemetry spans over OTLP when the variable `ZOE_OTLP_ENDPOINT` names a collector, such as `http://127.0.0.1:4318`. Each call of a client is a span, and so are each lock wait, each fsync and each batch of a pack. `zoe.zeo.ClientStorage` sends its trace context when the package `opentelemetry` is installed in the instance, so a slow Plone request and the work of the server show in one trace. [Traces](../reference/metrics.md#traces) gives the variables.
