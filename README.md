# ZOE

ZOE, the Zope Object Engine, is a ZODB storage written in Rust. Plone 6.1 and later use it unchanged: in-process for one Zope instance (`<zoe>` in `zope.conf`), or behind `zoe serve`, a server that speaks the ZEO 6 protocol, for a cluster.

Nothing is usable yet: the project is in phase 0 of its roadmap.

- [Documentation](https://adeweb-be.github.io/zoe/): the technical specification, the working methods and roadmap, the architecture decision records.
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
