# ZOE

ZOE, the Zope Object Engine, is a ZODB storage written in Rust. Plone 6.1 and later use it unchanged: in-process for one Zope instance, or behind a server that speaks the ZEO protocol for a cluster.

What works today:

- In-process: `zoe.Storage` in the `zoedb` wheel, and the `<zoe>` section of `zope.conf`, run a Zope instance on a ZOE store.
- Server: `zoe serve` serves ZOE stores to stock `ZEO.ClientStorage` clients over the ZEO 6 protocol. The scenarios under `plone/` create a Plone site in-process and through the server.
- Command line: `zoe convert` converts a `Data.fs` to a store and back, `zoe verify` checks a store, `zoe pack` packs it, `zoe backup` and `zoe restore` write and read dumps, and `zoe mcp` gives an agent read-only tools on a store folder.
- Loupe: the desktop inspector opens a store folder read-only. It browses objects, shows their history and the diff of two revisions, and measures the space of the store.
- `zoe-client`: a Rust client of the ZEO 6 protocol.

Much of the specification is not built yet. The roadmap in the working methods gives the order of the work.

- The [technical specification](spec/technical-specification.md) says what ZOE is and how it works.
- The [working methods and roadmap](spec/working-methods.md) say how ZOE gets built.
- The [architecture decision records](adr/README.md) record each decision with its reasons.
- The [glossary](glossary.md) gives the vocabulary.

The source is at [github.com/adeweb-be/ZOE](https://github.com/adeweb-be/ZOE), under the Apache-2.0 licence.
