# Admin port, metrics and logs

`zoe serve` reports through four channels: the admin port, the logs on stderr, the traces over OTLP, and two lines on stdout.

## stdout

stdout carries only two lines:

- `admin <address>`, when the server has an admin port;
- `listening <address>`, once the server accepts connections on the ZEO port.

So a port of 0 gives a free port, and a supervisor reads the address. With a unix domain socket, the address is the path.

## Admin port

`--admin-listen HOST:PORT` (`ZOE_ADMIN_LISTEN`, the key `admin-listen`) opens the admin port, a small HTTP/1.1 server for the probes of a supervisor. Without it, the server opens no admin port.

- The port binds after the ZEO port and before the stores open. A port that does not bind fails the start, and the server creates no store.
- The port never speaks ZEO and stays plain HTTP, also when the ZEO port speaks TLS.
- It reads one request on each connection, sends one reply and closes the connection.
- The request head must arrive within 2 seconds, with at most 8 KiB. A longer head gets 431. A slower client gets no reply.
- It serves at most 64 connections at once. So put it on a private address, never on a public one.
- It answers until the end of a stop.

| Path | 200 | 503 | Body |
| --- | --- | --- | --- |
| `/healthz` | while every store is open | before every store is open | one line for each storage, `NAME: open`, or `starting` |
| `/readyz` | when every store is open, its open loaded its last tid, and its lock table held no entry after the open | before; from the stop signal on; and when the lock table of a store held entries at the open | one line for each storage, `NAME: ready` or `NAME: not ready: the lock table held entries at the open`, or `starting`, or `stopping` |
| `/metrics` | once every store is open | before every store is open | with 200, the metrics in the Prometheus text format, with the content type `text/plain; version=0.0.4; charset=utf-8`; with 503, `starting` |

A known path with a method other than `GET` or `HEAD` gets 405. Another path gets 404. A request head that does not parse gets 400. A reply to `HEAD` has no body.

## Metrics

Each metric has the label `storage`, the storage name. A duration is a histogram in seconds, and a counter ends in `_total`. The counters and the histograms of a storage count the work of all its connections. The gauges are read at each scrape.

| Metric | Type | Labels | Meaning |
| --- | --- | --- | --- |
| `zoe_loads_total` | counter | `storage` | The `loadBefore` calls. |
| `zoe_load_latency_seconds` | histogram | `storage` | The time of each `loadBefore` call. |
| `zoe_commits_total` | counter | `storage` | The commits that `tpc_finish` made. |
| `zoe_vote_latency_seconds` | histogram | `storage` | The time of each `vote` call, its lock wait included. |
| `zoe_finish_latency_seconds` | histogram | `storage` | The time of each `tpc_finish` call that ended a transaction, until the commit is durable and published. |
| `zoe_fsync_batch_size` | histogram | `storage` | The commits that one sync made durable, in commits. |
| `zoe_fold_seconds` | histogram | `storage` | The time of each slice of the fold: one durable write of commits of the write-ahead log into the tables (ADR 0056). |
| `zoe_fold_commits_total` | counter | `storage` | The commits that the fold wrote from the write-ahead log into the tables. |
| `zoe_backlog_wait_seconds` | histogram | `storage` | The time of each wait of a commit batch for the fold: for room in the backlog, or for an empty backlog before a batch that goes into the tables. |
| `zoe_lock_wait_seconds` | histogram | `storage` | The time of each vote to take its locks, a wait past the lock timeout included. |
| `zoe_lock_handoffs_total` | counter | `storage` | The locks that a finish handed to the vote at the head of their queues (ADR 0044). |
| `zoe_intent_locks_total` | counter | `storage` | The early write-intent locks that the sessions took (ADR 0044, part B). |
| `zoe_intent_lock_wait_seconds` | histogram | `storage` | The time of each `lock_intent` call to take its locks, a wait past the lock timeout included. |
| `zoe_conflicts_resolved_total` | counter | `storage`, `path` | The oids in conflict that a vote passed after their resolution. `path="native"`: tier 1 or tier 2 merged them on the server. `path="python"`: the client resolved them, and a second vote passed. |
| `zoe_structural_merges_total` | counter | `storage`, `tier`, `mode`, `outcome` | The results of tier 1 and tier 2 in the votes. `tier` is `1` or `2`, `mode` is `on` or `shadow`, `outcome` is `merged`, `refused` or `undecided`. |
| `zoe_shadow_verifications_total` | counter | `storage`, `tier`, `same` | The merges of shadow mode that a second vote compared with the record of the client. `same` is `true` when the two records are the same. |
| `zoe_conflicts_raised_total` | counter | `storage` | The transactions that raised a conflict: a vote that failed with a conflict, or a transaction that ended between the two votes, so that the client raised `ConflictError`. |
| `zoe_writes_on_get_total` | counter | `storage`, `class` | The records that the transactions of GET and HEAD requests committed, by class. `zoe.plone` marks such a transaction with the key `zoe.request_method` of its extension. A deletion and a pickle whose class does not decode count nothing. The metric has no sample before the first such record. |
| `zoe_clients_connected` | gauge | `storage` | The connections that registered. |
| `zoe_invalidation_queue_depth` | gauge | `storage` | The messages that wait in the deepest hub queue among the connections of the storage. |
| `zoe_backlog_bytes` | gauge | `storage` | The backlog of the write-ahead log, in bytes: the commits that the log holds and the tables do not hold yet. |

The buckets of a histogram in seconds have the upper bounds 0.0001, 0.00025, 0.0005, 0.001, 0.0025, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1, 2.5, 5, 10 and `+Inf`. The buckets of `zoe_fsync_batch_size` have the upper bounds 1, 2, 4, 8, 16, 32, 64, 128 and `+Inf`.

`deploy/grafana/zoe.json` is a Grafana dashboard of these metrics.

## Logs

The logs go to stderr, from the level INFO on.

- `--log-format text`, the default, writes one line of text for each event. `--log-format json` writes one JSON object on each line, with the fields of the event and of its spans.
- Each line of a connection has the field `client`: the label of the last `set_client_label`, with each control character replaced by a space and at most 200 characters, or else the address of the peer. A client on a unix domain socket is `PATH pid PID`. After `register`, the line also has `storage`.
- A text that a client sends goes only into a quoted field, never into the message. So it cannot add a log line.
- A log line holds no pickle bytes, no user and no description of a transaction.
- A usage error and a failure of the start are messages of the command, not log lines: they stay plain text.

Some lines that an operator looks for:

| Level | Message | Fields |
| --- | --- | --- |
| WARN | `lock held past lock-warn` | `oid`, `duration_ms`; at most 10 lines for one release, then `summary of the locks held past lock-warn` with `locks` and `max_duration_ms` |
| WARN | `transaction open past transaction-warn` | `duration_ms`, `oids`, and `oid` and `tid` when they are known |
| WARN | the repair of a store that a writer left open, which holds `repaired the store` | `storage` |
| ERROR | `the open cut the damaged last entry of the write-ahead log: the commits after previous_tid up to last_tid are lost` | `storage`, `previous_tid`, `last_tid` |
| ERROR | `transaction timeout after the vote: closing the connection` | `seconds` |
| INFO | `no frame within the idle timeout: closing the connection` | `client` |
| WARN | `the shutdown timeout ends: closing the connections that remain` | none |

## Traces

`zoe serve` exports its spans over OTLP when `ZOE_OTLP_ENDPOINT` is set and not empty. The variable has no option and no key of the configuration file.

- `OTEL_EXPORTER_OTLP_PROTOCOL` picks the protocol: `grpc`, to the endpoint; `http/protobuf`, the default, or `http/json`, to `<endpoint>/v1/traces`. Another value fails the start with 1. The exporter has no TLS.
- `OTEL_EXPORTER_OTLP_TRACES_ENDPOINT` and `OTEL_EXPORTER_OTLP_TRACES_PROTOCOL` change nothing. The exporter still reads the headers, `OTEL_EXPORTER_OTLP_TRACES_HEADERS` else `OTEL_EXPORTER_OTLP_HEADERS`, and the timeout, `OTEL_EXPORTER_OTLP_TRACES_TIMEOUT` else `OTEL_EXPORTER_OTLP_TIMEOUT`. So a header that the host sets for another collector, such as a token, also goes to `ZOE_OTLP_ENDPOINT`.
- The service name is `zoe`, unless `OTEL_SERVICE_NAME` gives one.
- Each call of a session is a span named after its method. A lock wait is a span `lock_wait`, with `oid`. The write and sync of a commit batch is a span `fsync`, with `commits`. Each batch of a pack is a span `pack_batch`, with `records`. Each span has the attribute `zoe.storage`.
- A call of a client that negotiated the extension `trace_context` joins the trace of that client (see [Protocol](protocol.md)). Any other call is the root of its trace.
- The traces take the spans of the ZOE crates only, and no event. No attribute holds pickle bytes, a user or a description.
- The server flushes its spans when it stops.
