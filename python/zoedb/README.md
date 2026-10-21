# zoedb

ZOE, the Zope Object Engine: a ZODB storage written in Rust, for Plone 6.1
and later. The import name is `zoe`. The `plone` extra adds `zoe.plone`,
and `msgpack` for a ZEO client of `zoe serve`.

What works today:

- `zoe.Storage` and the `<zoe>` section of `zope.conf` run a Zope instance
  on a ZOE store, in-process.
- `zoe.zeo.ClientStorage`, a drop-in for `ZEO.ClientStorage`, is a client
  of `zoe serve`. Its native client is in Rust.
- `zoe.plone` gives `portal_uidgenerator` its ids from a store sequence.
  Its opt-in backends keep the locks, the catalog operations and the
  registries of Plone in the store.
- The `zoe` console script runs `zoe convert`, `zoe verify`, `zoe pack`,
  `zoe backup` and `zoe restore`. `zoe serve` and `zoe mcp` need the `zoe`
  binary.

Much of the specification is not built yet. See the
[documentation](https://adeweb-be.github.io/ZOE/).
