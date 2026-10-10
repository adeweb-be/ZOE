# Plone instances

The Plone instances that the suites and the benchmarks run against. zc.buildout builds them; `just plone` is the entry point. One configuration per scenario:

| File | Storage |
| --- | --- |
| `filestorage.cfg` | FileStorage, the baseline |
| `zeo.cfg` | a ZEO 6 server and a ZEO client, the baseline |
| `zoe.cfg` | the in-process `<zoe>` storage |
| `zeoclient.cfg` | a ZEO client of `zoe serve` |

`versions.cfg` pins the Plone 6.1 release. `zoedb` comes from the wheel that `uv build` writes to `target/wheels`, through `find-links`. `templates/zope.conf.in` carries the `<zoe>` stanza. The instances run on CPython 3.13, the newest version that Plone 6.1 supports.
