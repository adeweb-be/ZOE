# ZOE

ZOE, the Zope Object Engine, is a ZODB storage written in Rust. It takes the place of FileStorage and of the ZEO server under a Zope or Plone site, and the application does not change. Plone 6.1 and later run on it in two modes:

- **in-process**: one Zope instance opens the store with the `<zoe>` section of `zope.conf`, as it opens a `Data.fs` with `<filestorage>`;
- **server**: `zoe serve` serves the store to the ZEO clients of several instances. It speaks the ZEO 6 protocol, so a stock `ZEO.ClientStorage` connects.

Both modes use one format of the store. A store changes its mode without a conversion, and `zoe convert` copies a `Data.fs` into a store and back.

ZOE works against `ConflictError`. A commit locks each object that it writes, not the whole store, and the store merges more kinds of concurrent changes at the vote. With `zoe.plone`, a Plone site gives the store a merge policy for its content types.

## What works today

- **In-process**: `zoe.Storage` in the `zoedb` wheel, with the `<zoe>` section of `zope.conf` and the `zoe://` URI.
- **Server**: `zoe serve`, with named stores, read-only stores, unix domain sockets, TLS, a configuration file, an admin port with health, readiness and metrics, logs and traces.
- **Clients**: the stock ZEO client, and `zoe.zeo.ClientStorage`, a client in Rust with the ZOE extensions.
- **Plone**: `zoe.plone`, with the ids of `portal_uidgenerator` from the store, the merge policy, the append-only workflow history, and the opt-in backends for the locks of edit forms, queued indexing and the registry.
- **Tools**: `zoe convert` from and to `Data.fs`, `zoe verify`, `zoe pack`, `zoe backup` and `zoe restore`, and `zoe mcp`, read-only tools for an AI agent.
- **Loupe**: a desktop window that inspects a store: its objects, their history and its space.
- **Deployment**: a systemd unit, a supervisor program, Kubernetes manifests for one server, and a Grafana dashboard.

Nothing is published yet: you build ZOE from the source. Some parts of the design are not built, such as a backup through the server, servers with replicas and a Helm chart. [Known issues](guide/known-issues.md) lists the limits that a user meets.

## Where to start

- **The user guide** tells how to install, run and operate ZOE. Start with [Install](guide/install.md), then [Run in-process](guide/in-process.md) or [Run a server](guide/server.md). To move an existing site, read [Convert](guide/convert.md).
- **The reference** lists every setting: [the zoe command](reference/cli.md), [the sections of zope.conf](reference/zope-conf.md), [the configuration file](reference/config-file.md), [the environment variables](reference/environment.md), [the metrics](reference/metrics.md) and more, from the [settings reference](reference/README.md).
- **The design** tells how ZOE works and why: the [technical specification](spec/technical-specification.md), the [architecture decision records](adr/README.md), and the [working methods and roadmap](spec/working-methods.md). The [glossary](glossary.md) gives the vocabulary.

The source is at [github.com/adeweb-be/ZOE](https://github.com/adeweb-be/ZOE), under the Apache-2.0 licence.
