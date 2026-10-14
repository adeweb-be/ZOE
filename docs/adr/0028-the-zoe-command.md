# 0028. The zoe command: one binary in zoe-server, one code path for the console script

Status: accepted

## Context

The naming table asks for one CLI, `zoe`, as one binary and one console script of the same name. F14 asks for `zoe convert`, F8 and F26 for `zoe pack` and its dry run. `zoe serve` joins the same command in phase 2. Every option has a `ZOE_*` environment twin.

The crate names are fixed: `zoe-core`, `zoe-server`, `zoe-client`, `zoe-py` and the others of the naming table. `zoe-server` holds `zoe serve` and `zoe mcp`, which use tokio. The extension module `zoe._zoe` of the `zoedb` wheel must not link tokio.

Three places can hold the binary: a `[[bin]]` in `zoe-server`, a thin binary crate, or a `main` inside the extension module that the console script calls. The wheel can ship a compiled binary beside the extension, or a console script that calls the extension.

## Decision

**One binary in `zoe-server`.** `crates/zoe-server` declares `[[bin]] name = "zoe"`. No new crate is added, because the crate names are fixed, and `zoe serve` joins the same binary in phase 2.

**The CLI is a library module.** `zoe_server::cli::main(args: Vec<OsString>) -> i32` parses the arguments with clap, runs one subcommand and returns the exit code. The `main` of the binary only calls it and exits with its code. The function never exits the process, so a caller in another process, such as the extension, can call it.

**The console script goes through the extension.** The extension exposes `zoe._zoe.main(argv) -> int`, which runs `zoe_server::cli::main` with the GIL released. `zoe/__main__.py` calls it with `sys.argv`, and the wheel declares the console script `zoe = "zoe.__main__:main"`. So the binary, the console script and `python -m zoe` run the same code. The entry point restores the default handler of SIGINT before the call, because Python handles a signal only when it runs Python code: Ctrl-C then stops the process, as it stops the binary.

**`serve` sits behind a feature.** `zoe-server` has the cargo feature `serve`, on by default. The server code and the MCP server of `zoe mcp` compile only with it, and only it may bring in tokio. `zoe mcp` brings in tokio and rmcp through `zoe-mcp`. Without the feature, `zoe serve` and `zoe mcp` still parse, and fail with 1 and a message that names the `zoe` binary. The workspace dependency on `zoe-server` turns the default features off, and `zoe-py` uses it without `serve`. So the extension holds the CLI and the converters, and never links tokio or rmcp. `cargo tree -p zoe-py -e normal` shows it.

**`--path` everywhere.** The store is `--path` in every subcommand, with the twin `ZOE_PATH`, as in `zoe serve --path`.

**`--data-fs-blob-dir` for the blob directory of a `Data.fs`, `--blob-dir` for the one of the store.** The blob directory of the store belongs to the store and to `zoe serve`. It is `--blob-dir` with the twin `ZOE_BLOB_DIR`, the twin of the key `blob-dir` of the `<zoe>` section, and `<path>/blobs` by default. The blob directory of the `Data.fs` gets its own name, `--data-fs-blob-dir` with the twin `ZOE_DATA_FS_BLOB_DIR`. The store does not record its blob directory (ADR 0027), so `zoe verify` and `zoe convert --to-data-fs` refuse a store with blob records whose blob directory has no layout file: they fail with 1 before they walk the store, write nothing, and name the blob directory and `--blob-dir`. `zoe convert --from-data-fs` puts the blobs of the store into `--blob-dir`.

**The command line.**

```
zoe convert --from-data-fs FILE [--data-fs-blob-dir DIR] --path STORE [--blob-dir DIR]
zoe convert --to-data-fs FILE --data-fs-blob-dir DIR --path STORE [--blob-dir DIR]
zoe pack --path STORE --days N [--dry-run]
zoe mcp --path STORE
zoe verify --path STORE [--blob-dir DIR] [--format text|json] [--classes FILE]
zoe verify --path STORE [--blob-dir DIR] --backup DIR [--format text|json]
zoe backup --path STORE --to DIR
zoe restore --from DIR --path STORE
```

The twins are `ZOE_PATH`, `ZOE_BLOB_DIR`, `ZOE_FROM_DATA_FS`, `ZOE_TO_DATA_FS`, `ZOE_DATA_FS_BLOB_DIR`, `ZOE_DAYS`, `ZOE_DRY_RUN`, `ZOE_FORMAT`, `ZOE_CLASSES`, `ZOE_BACKUP`, `ZOE_TO` and `ZOE_FROM`. clap reads them, and the help of each option names its twin. An option on the command line wins over its twin. `ZOE_DRY_RUN` takes `true` or `false`, and the other words that clap reads as a boolean. `--dry-run` alone means true, and `--dry-run=false` turns off a dry run that the twin asks for. A conversion takes exactly one direction: two directions are a usage error, also when one of them comes from a twin. `--backup` and `--classes` together are a usage error too.

**`zoe mcp`.** `zoe mcp --path STORE` serves the read-only ops tools of the store to an agent, over MCP on stdin and stdout (F20). `zoe-mcp` holds the server, and the tools call Loupe's functions. stdout carries only MCP messages, so the command writes no report. It ends with 0 when the client closes stdin, also before the `initialize` request. A request before `initialize` gets a reply, and the session waits for `initialize`. It ends with 1 when the store does not open, or when the first message is a notification. `cli::main` does not hold the lock of stdout, because the MCP transport writes to stdout from another thread.

**`zoe backup`, `zoe restore` and `zoe verify --backup`.** `zoe backup` writes a dump of the store into the backup directory `--to`: the full dump the first time, then the transactions after the last dump. With nothing new, it writes nothing and exits with 0. `zoe restore` rebuilds a store at `--path` from the dumps of `--from`. `zoe verify --backup DIR` compares the dumps with the store, in place of the check of the store, and a difference is a finding. ADR 0030 gives the layout of a dump and the rules of the three commands. `zoe-convert` holds the library: `backup`, `restore` and `verify_backup`.

**Exit codes and output.** The exit code is 0 on success, 1 on a failure, and 2 on a usage error. A conversion whose report lists a problem fails: this covers a store that does not match its `Data.fs`, and the `Mismatch` of `zoe-convert`. A verification whose report lists a finding fails too, after the report goes out, and so does a comparison of dumps that lists a difference. A damaged or missing dump fails `zoe restore` and `zoe verify --backup`. The report goes to stdout, a message to stderr. The help and the version go to stdout with the code 0. A reader that closes stdout before the end of the report, as `head` does, does not change the code, because the work is done before the report goes out. The command parses all its arguments before it opens a file, so a usage error changes nothing on disk.

**`python -m zoe.serve` runs the binary.** The wheel ships no server binary: users install the static `zoe` binary, or run the image. For buildout users, `python -m zoe.serve -C FILE` reads the `<zoe-server>` section of a ZConfig file and runs `zoe serve` with it. The binary is the key `binary` of the section, else `ZOE_BINARY`, else the `zoe` binary on the PATH, without a script such as the console script. `zoe.serve` writes the section as a configuration file of `zoe serve` into a memory file and replaces itself with `zoe serve --config` on that file. So the TOML file stays the only configuration of the server, no Python stays in its process, and no value of the section goes into its arguments.

**The pack tid.** `--days N` gives the tid of the time of the clock minus N days of 86,400 seconds, by ZODB's `TimeStamp` rule (`zoe_core::timestamp`), as ZODB's `DB.pack(days=N)` computes it. The command reads the system clock through the `Clock` trait, not the clock of the store, which is not public. A test gives its own clock. A time before the Unix epoch gives the tid of the epoch.

**The opens.** A command that only reads opens the store read-only: `zoe convert --to-data-fs`, `zoe pack --dry-run`, `zoe verify`, `zoe backup` and `zoe mcp`. `zoe mcp` opens it through Loupe's `Folder`, and holds the lock of the store while it runs. Such an open changes no byte and creates nothing. `zoe convert --from-data-fs` opens the store for writes and creates it, after it checks that the source is a regular file that starts with `FS30`. So a wrong source creates no store. `zoe pack` opens an existing store for writes: a directory without `zoe.redb` fails, and the command creates nothing. `zoe restore` creates a store only in a new or an empty directory, and checks every dump before it writes anything.

## Consequences

- The wheel carries clap, `zoe-convert` and the CLI module inside the extension. The extension grows by that code.
- `zoe-py` depends on `zoe-server`. A server dependency that is not optional behind `serve` would reach the extension. The feature split and the `cargo tree` check guard this.
- In the console script, `zoe mcp` fails with 1 and names the `zoe` binary, because the extension compiles without `serve`. An agent runs the binary `zoe`.
- A workspace build, such as `cargo test --workspace`, unifies the features, so there `zoe-py` compiles with `serve`. maturin builds `zoe-py` alone, without it.
- The console script writes to the file descriptors of the process, not to `sys.stdout` and `sys.stderr`. A Python caller that replaces `sys.stdout` does not capture the report.
- Ctrl-C stops a conversion or a pack at once. The converters resume after the last copied transaction, and a pack that stops leaves a store that opens without repair (ADR 0025).
- A store that a crash left open does not open read-only: `zoe convert --to-data-fs`, the dry run, `zoe verify`, `zoe verify --backup` and `zoe backup` fail with `NotClosedCleanly` until a writer opens the store once.

## Alternatives

- **A thin binary crate.** It needs a crate name outside the fixed list, and `zoe serve` would move into it or call into `zoe-server` in phase 2.
- **The binary in the wheel through maturin's binary packaging.** maturin packages the targets of the crate that it builds, `zoe-py`. The binary is in another crate, so the wheel would need a second build step, and it would hold a second compiled copy of the code.
- **A Python CLI.** An argparse parser in `zoe/__main__.py` would be a second parser, with its own options, twins and exit codes, beside the one of the binary.
- **`zoe.serve` as the parent of the server.** A Python process that starts the binary and waits for it must pass on each signal, and stays in the process tree of the supervisor. `execve` needs neither.
- **The settings as options of the command line.** Each value would be visible in the arguments of the process. The configuration file keeps them out.
- **No feature for `serve`.** The extension would link tokio as soon as phase 2 adds it.
