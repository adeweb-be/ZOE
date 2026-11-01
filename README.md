# ZOE

ZOE, the Zope Object Engine, is a ZODB storage written in Rust. Plone 6.1 and later use it unchanged: in-process for one Zope instance (`<zoe>` in `zope.conf`), or behind `zoe serve`, a server that speaks the ZEO 6 protocol, for a cluster.

What works today:

- In-process: `zoe.Storage` in the `zoedb` wheel, and the `<zoe>` section of `zope.conf`, run a Zope instance on a ZOE store.
- Server: `zoe serve` serves ZOE stores to stock `ZEO.ClientStorage` clients over the ZEO 6 protocol.
- Command line: `zoe convert` converts a `Data.fs` to a store and back, `zoe verify` checks a store, `zoe pack` packs it, `zoe backup` and `zoe restore` write and read dumps, and `zoe mcp` gives an agent read-only tools on a store folder.
- Loupe: the desktop inspector opens a store folder read-only.

Much of the specification is not built yet. The roadmap in the working methods gives the order of the work.

- [Documentation](https://adeweb-be.github.io/ZOE/): the technical specification, the working methods and roadmap, the architecture decision records.
- [CONTEXT.md](CONTEXT.md): the glossary.
- [CLAUDE.md](CLAUDE.md): how to build and test, for humans and agents.

## Build and test

```console
$ devenv shell
$ just check
$ just test
$ just docs
```

`just` lists every recipe.

## Licence

Apache-2.0. See [LICENSE](LICENSE).
