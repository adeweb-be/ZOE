# Settings reference

These pages list every setting that a user of ZOE gives, and what the code does with it. They state what the code does today. Where the [technical specification](../spec/technical-specification.md) says more, the specification describes the design, and parts of it are not built.

| Page | What it lists |
| --- | --- |
| [The zoe command](cli.md) | each subcommand of `zoe`, each option, its environment twin, its default, and the exit codes |
| [zope.conf sections and the zoe:// URI](zope-conf.md) | the keys of `<zoe>`, `<zoeclient>` and its `<ssl>`, `<zoe-server>` and its `<storage NAME>`, and the query of a `zoe://` URI |
| [The configuration file of zoe serve](config-file.md) | the TOML file of `zoe serve --config`, and the precedence of its sources |
| [Environment variables](environment.md) | each `ZOE_*` variable, and the variables of development and test apart |
| [Admin port, metrics and logs](metrics.md) | the probes, each metric of `/metrics`, the log lines and the traces of `zoe serve` |
| [Python API](python-api.md) | the public names of the package `zoe` and of `zoe.plone` |
| [MCP tools](mcp.md) | the tools and the prompts of `zoe mcp` |
| [Protocol](protocol.md) | the ZEO compatibility matrix and the ZOE extensions of the protocol |
| [Compatibility](compatibility.md) | the versions of CPython, the operating systems, ZODB, ZEO and Plone |

A test keeps these pages in step with the code: `python/zoedb/tests/test_reference.py`, which `just test` runs. It reads each subcommand and option of `zoe` and their twins from the help of the binary, each key of `component.xml`, the twins of `<zoe>`, and each metric that a running `zoe serve` exports. It fails with the names that the pages lack. A name counts only in the first cell of a table row, so a new setting needs a row here.
