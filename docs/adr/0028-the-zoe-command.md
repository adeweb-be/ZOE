# 0028. The zoe command: one binary in zoe-server, one code path for the console script, and tools that repair what they read

Status: accepted.

## Context

The naming table asks for one CLI, `zoe`: one binary and one console script. The crate names are fixed. `zoe serve` and `zoe mcp` use tokio. The extension links tokio only through `zoe-client`, and never links rmcp (ADR 0001).

redb clears its recovery flag and writes its allocator state only at a clean close. After a kill, every acknowledged commit is in the file, but the read-only open of redb fails with `RepairAborted` (`NotClosedCleanly` in the core). Only a writer open repairs the file. So after a crash, when the operator most needs to inspect a store, a strict read-only open refuses it.

## Decision

### One binary, one code path

`crates/zoe-server` declares `[[bin]] name = "zoe"`. `zoe_server::cli::main(args: Vec<OsString>) -> i32` parses with clap, runs one subcommand and returns the exit code. It never exits the process; the `main` of the binary only calls it. `zoe._zoe.main(argv) -> int` runs it with the GIL released, and `zoe/__main__.py` calls that with `sys.argv`. The console script is `zoe = "zoe.__main__:main"`. So the binary, the console script and `python -m zoe` run the same code. The Python entry point restores the default handler of SIGINT first, so Ctrl-C stops the process.

### `serve` sits behind a feature

`zoe-server` has the cargo feature `serve`, on by default. Only with it do the server and `zoe mcp` (tokio and rmcp, through `zoe-mcp`) compile. Without it, `zoe serve` and `zoe mcp` fail with 1 and a message that names the `zoe` binary. The workspace dependency on `zoe-server` turns the default features off, and `zoe-py` uses it without `serve`. So the extension holds the CLI and the converters, not the server or rmcp. `cargo tree -p zoe-py -e normal` shows it.

### The command line

```
zoe convert --from-data-fs FILE [--data-fs-blob-dir DIR] --path STORE [--blob-dir DIR]
zoe convert --to-data-fs FILE --data-fs-blob-dir DIR --path STORE [--blob-dir DIR]
zoe pack --path STORE --days N [--dry-run]
zoe mcp --path STORE
zoe verify --path STORE [--blob-dir DIR] [--format text|json] [--classes FILE]
zoe verify --path STORE [--blob-dir DIR] --backup DIR [--scratch SCRATCH] [--format text|json]
zoe backup --path STORE --to DIR
zoe restore --from DIR --path STORE
```

The store is `--path` in every subcommand. `--blob-dir` is its blob directory (`<path>/blobs` by default), `--data-fs-blob-dir` the one of the `Data.fs`. Each option has a `ZOE_*` twin: `ZOE_PATH`, `ZOE_BLOB_DIR`, `ZOE_FROM_DATA_FS`, `ZOE_TO_DATA_FS`, `ZOE_DATA_FS_BLOB_DIR`, `ZOE_DAYS`, `ZOE_DRY_RUN`, `ZOE_FORMAT`, `ZOE_CLASSES`, `ZOE_BACKUP`, `ZOE_SCRATCH`, `ZOE_TO` and `ZOE_FROM`. The help of each option names its twin, and the command line wins over the twin. A boolean twin takes the words that clap reads as a boolean. `--dry-run` alone means true, and `--dry-run=false` overrides the twin. Usage errors: two directions of a conversion, also from a twin; `--backup` with `--classes`; `--scratch` without `--backup`. Later options follow these rules.

The options of each command: specification, "Loupe: desktop inspector and editor", "Converters, `zoe convert` and a Loupe screen", "`zoe verify`" and "`zoe backup` and `zoe restore`: dumps (N15)"; and "AI-native: MCP server and retrieval", "MCP server, `zoe mcp`". ADR 0030 gives the layout of a dump.

`zoe mcp` also ends with 0 when stdin closes before `initialize`, and with 1 when the first message is a notification. A request before `initialize` gets a reply, and the session waits. `cli::main` does not hold the lock of stdout, because the MCP transport writes to it from another thread.

### Exit codes and output

The exit code is 0 on success, 1 on a failure, and 2 on a usage error. A conversion that reports a problem (`Mismatch` included), a verification that reports a finding and a comparison of dumps that reports a difference fail with 1 after the report. A damaged or missing dump fails `zoe restore` and `zoe verify --backup`. The report goes to stdout, a message to stderr, the help and the version to stdout with 0. A reader that closes stdout early does not change the code. The command parses all its arguments before it opens a file.

### `python -m zoe.serve` runs the binary

The wheel ships no server binary. `python -m zoe.serve -C FILE` writes the `<zoe-server>` section as a configuration file with no name on disk, and replaces itself with `zoe serve --config` on it. So no Python stays in the server process, and no value goes into its arguments (specification, "Server mode", "The `<zoe-server>` section").

### The pack tid

`--days N` gives the tid of the clock minus N days of 86,400 seconds, by ZODB's `TimeStamp` rule (`zoe_core::timestamp`), as `DB.pack(days=N)` does. The command reads the system clock through the `Clock` trait. A time before the Unix epoch gives the tid of the epoch.

### The opens

These commands only read, and use the repairing open of the next section: `zoe convert --to-data-fs`, `zoe pack --dry-run`, `zoe verify`, `zoe backup` and `zoe mcp`. `zoe mcp` opens the store through Loupe's `Folder`, and holds the lock of the store while it runs. `zoe convert --from-data-fs` creates the store, after it checks that the source is a regular file that starts with `FS30`. `zoe pack` opens an existing store and creates nothing. `zoe restore` creates a store only in a new or empty directory, after it checks every dump.

### A tool that reads a store repairs it

**The rule.** When a writer left the store open, a tool that reads it repairs it, then opens it read-only. On a store that was closed cleanly, it changes no byte and creates nothing.

**`Store::open_read_only_repairing(dir, options)`.** It holds the lock of the store from its start to its end. On a clean store, it is `Store::open_read_only_with`. Only on `RepairAborted`, it opens `zoe.redb` for writes with `redb::Builder::open`, which creates no file, drops it, then opens it read-only. `Store::repaired` tells the caller.

**`Store::open_read_only` stays strict.** It refuses a store that a writer left open with `NotClosedCleanly`. The tests of a clean stop use it.

**The repair writes only `zoe.redb`.** It runs no recovery of the store: it removes no file, replays no intent file and writes nothing in the blob directory. It keeps every durable commit and changes no part of the format.

**A failed repair is `RepairFailed`.** Its text says that the store was not closed cleanly, that the repair failed, and gives the error of the system. The open for writes fails before redb writes a byte. zoe-py maps `RepairFailed` to `StorageError`.

**Each tool says that it repaired the store.** The core logs nothing. A tool that repaired a store writes one line with the stable phrase `repaired the store`, for example `zoe verify: /srv/store: the store was not closed cleanly: repaired the store`: the commands and `zoe mcp` on stderr, `zoe serve` as a WARN log line that names the storage, Loupe in its window. On `RepairFailed`, the commands, `zoe serve` and `zoe mcp` exit with 1, and Loupe gives `notRepaired`.

## Consequences

- The extension grows by clap, `zoe-convert` and the CLI.
- A workspace build unifies the features, so there `zoe-py` has `serve`. maturin builds `zoe-py` alone, without it.
- The console script writes to the file descriptors: a replaced `sys.stdout` does not capture the report.
- A repair reads every page, shows no progress and cannot stop. It needs write access to `zoe.redb`.
- The kill loop runs `zoe verify`, which repairs, so it does not prove a clean stop. The tests with `Store::open_read_only` prove it.
- The scratch copy of `zoe verify --backup` uses the strict open, because the command closes it itself.

## Alternatives

- **A thin binary crate.** Its name is outside the fixed list.
- **The binary in the wheel through maturin.** A second build and compiled copy.
- **A Python CLI.** A second parser with its own options and exit codes.
- **`zoe.serve` as the parent of the server.** It must pass on each signal.
- **The settings as arguments.** The process list shows them.
- **No feature for `serve`.** The extension links the server and rmcp.
- **A strict read-only open in every tool.** Every tool refuses the store after a crash.
- **A repairing `Store::open_read_only`.** The tests of a clean stop pass on an unclean store.
- **A writer open, a close, then a read-only open.** It runs the whole recovery and drops the lock between the opens.
- **A save of the allocator state at each commit.** Each commit writes more. The human declined it.
- **A rebuild of the allocator state in memory.** It needs a change to redb.

## Merged ADRs

- ADR 0032, a tool that reads a store repairs it when a writer left it open: Decision, "A tool that reads a store repairs it", with the list of tools in "The opens"; its consequences and alternatives are in Consequences and Alternatives.
