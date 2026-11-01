# Environment variables

ZOE reads two kinds of `ZOE_*` variables:

- the twins: each option of the `zoe` command and each key of `<zoe>` has a twin, `ZOE_` and the name of the option or key in upper case, with `_` for `-`. One name breaks this rule: the twin of `--structural-merge-class` and of the key `structural-merge-class` is `ZOE_STRUCTURAL_MERGE_CLASSES`, with `CLASS=MODE` pairs separated by commas;
- a few variables that are not twins, such as `ZOE_OTLP_ENDPOINT`.

A twin gives a value only when its option or key is not set: the command line and the `<zoe>` section win over the environment. An empty twin of `<zoe>` counts as not set. A twin takes the same values as its option or key (see [Values](cli.md#values)). A boolean twin of the `zoe` command takes the words of clap, and a boolean twin of `<zoe>` the words of ZConfig, which do not include `1` and `0` (see [zope.conf sections](zope-conf.md)). `ZOE_READ_ONLY` and `ZOE_PACK_GC` have both readers, so give them `true` or `false`: both readers accept these.

One twin can serve several readers. For example, `ZOE_PATH` gives the store of each `zoe` subcommand and of the `<zoe>` section in the same environment. `<zoeclient>` has one twin, `ZOE_PREFETCH_REFERENCES`, which `zoe.zeo.ClientStorage` reads too; its other keys have none, as `<zeoclient>` has none. `python -m zoe.serve` removes from the environment of the server the twin of each key that its `<zoe-server>` section sets.

## Twins

| Variable | Read by | Value |
| --- | --- | --- |
| `ZOE_PATH` | every `zoe` subcommand, `<zoe>` | the directory of the store |
| `ZOE_BLOB_DIR` | `zoe convert`, `zoe serve`, `zoe mcp`, `zoe verify`, `<zoe>` | the blob directory of the store |
| `ZOE_BACKEND` | `<zoe>` | the backend of the store: `redb` |
| `ZOE_LOCK_TIMEOUT` | `zoe serve`, `<zoe>` | the longest lock wait of a vote, in whole seconds |
| `ZOE_EARLY_LOCKS` | `<zoe>` | a boolean: early write-intent locks of the hot classes |
| `ZOE_EARLY_LOCK_CLASSES` | `<zoe>` | the hot classes, each as `module.name`, separated by spaces or commas |
| `ZOE_PREFETCH_REFERENCES` | `<zoeclient>`, `zoe.zeo.ClientStorage` | a boolean: the native client prefetches the objects that a record from the server refers to; `true` when not set |
| `ZOE_READ_ONLY` | `zoe serve`, `<zoe>` | a boolean: serve or open read-only |
| `ZOE_PACK_GC` | `zoe pack`, `zoe serve`, `<zoe>` | a boolean: pack with garbage collection |
| `ZOE_COMPRESSION` | `zoe serve`, `<zoe>` | `none`, `zstd` or `zstd:LEVEL` |
| `ZOE_STRUCTURAL_MERGES` | `zoe serve`, `<zoe>` | the mode of tier 1: `on`, `shadow` or `off` |
| `ZOE_STRUCTURAL_MERGE_CLASSES` | `zoe serve`, `<zoe>` | `CLASS=MODE` pairs separated by commas |
| `ZOE_POSTING_DICTS` | `zoe serve`, `<zoe>` | the mode of the rules of ADR 0042: `on`, `shadow` or `off` |
| `ZOE_RESOLUTION_LOG` | `zoe serve`, `<zoe>` | the file of the resolution log |
| `ZOE_FROM_DATA_FS` | `zoe convert` | the `Data.fs` to copy into the store |
| `ZOE_TO_DATA_FS` | `zoe convert` | the `Data.fs` to copy the store into |
| `ZOE_DATA_FS_BLOB_DIR` | `zoe convert` | the blob directory of the `Data.fs` |
| `ZOE_FORCE` | `zoe convert` | a boolean: convert a store with the data of `zoe.plone` into a `Data.fs` |
| `ZOE_DAYS` | `zoe pack` | pack as of this number of days ago |
| `ZOE_DRY_RUN` | `zoe pack` | a boolean: report and change nothing |
| `ZOE_STORAGE` | `zoe serve` | one store, as `NAME=PATH` |
| `ZOE_CONFIG` | `zoe serve` | the configuration file |
| `ZOE_LISTEN` | `zoe serve` | the address of the ZEO port |
| `ZOE_ADMIN_LISTEN` | `zoe serve` | the address of the admin port |
| `ZOE_INVALIDATION_QUEUE_SIZE` | `zoe serve` | the most transactions that `getInvalidations` covers |
| `ZOE_INVALIDATION_AGE` | `zoe serve` | the age under which `getInvalidations` covers every transaction |
| `ZOE_CLIENT_QUEUE_SIZE` | `zoe serve` | the most messages that wait for one client |
| `ZOE_CACHE_SIZE` | `zoe serve` | the page cache of each store |
| `ZOE_LOCK_WARN` | `zoe serve` | log each lock held longer than this |
| `ZOE_TRANSACTION_WARN` | `zoe serve` | log each transaction open longer than this |
| `ZOE_LOG_FORMAT` | `zoe serve` | `text` or `json` |
| `ZOE_MAX_FRAME_SIZE` | `zoe serve` | the longest frame that a client can send |
| `ZOE_SHUTDOWN_TIMEOUT` | `zoe serve` | the longest time of a stop |
| `ZOE_IDLE_TIMEOUT` | `zoe serve` | the longest idle time of a connection |
| `ZOE_TRANSACTION_TIMEOUT` | `zoe serve` | the longest time of a transaction after its vote |
| `ZOE_BLOCKING_THREADS` | `zoe serve` | the threads of the read pool |
| `ZOE_SHARED_BLOB_DIR` | `zoe serve` | a boolean: accept `storeBlobShared` |
| `ZOE_TLS_CERT` | `zoe serve` | the certificate chain of the server |
| `ZOE_TLS_KEY` | `zoe serve` | the private key of the server |
| `ZOE_TLS_CLIENT_CA` | `zoe serve` | the CA certificates of the clients |
| `ZOE_TLS_READ_ONLY_CLIENT_CA` | `zoe serve` | the CA certificates of the read-only clients |
| `ZOE_FORMAT` | `zoe verify` | the format of the report: `text` or `json` |
| `ZOE_CLASSES` | `zoe verify` | the file of the classes of the application |
| `ZOE_BACKUP` | `zoe verify` | the backup directory to compare with the store |
| `ZOE_SCRATCH` | `zoe verify` | the directory of the copy of `--backup` |
| `ZOE_TO` | `zoe backup` | the backup directory |
| `ZOE_FROM` | `zoe restore` | the backup directory |

## Other variables

| Variable | Read by | Value |
| --- | --- | --- |
| `ZOE_OTLP_ENDPOINT` | `zoe serve` | The OTLP endpoint of the traces, such as `http://127.0.0.1:4318`. Without it, or empty, no exporter starts. It has no option and no key of the configuration file. See [Traces](metrics.md#traces). |
| `OTEL_EXPORTER_OTLP_PROTOCOL` | `zoe serve` | The OTLP protocol, with `ZOE_OTLP_ENDPOINT`: `grpc`, `http/protobuf`, the default, or `http/json`. Another value fails the start with 1. |
| `OTEL_SERVICE_NAME` | `zoe serve` | The service name of the traces. The default is `zoe`. |
| `ZOE_BINARY` | `python -m zoe.serve`, `zoe.testing` | The `zoe` binary to run. `python -m zoe.serve` takes the key `binary` first, then this variable, then the `zoe` binary on the PATH. `zoe.testing` takes this variable, else the debug build of the repository. |
| `ZOE_PLONE_BACKENDS` | `zoe.plone`, `zoe.plone.testing` | The backends of `zoe.plone` to install, separated by commas: `locking`, `indexing` and `registry`. An instance sets it with `<environment>` in `zope.conf`; the ZCML of `zoe.plone` reads it when it loads. An unknown name raises `ValueError`. |

`OTEL_EXPORTER_OTLP_TRACES_ENDPOINT` and `OTEL_EXPORTER_OTLP_TRACES_PROTOCOL` change nothing in `zoe serve`. The exporter still reads the headers, `OTEL_EXPORTER_OTLP_TRACES_HEADERS` else `OTEL_EXPORTER_OTLP_HEADERS`, and the timeout, `OTEL_EXPORTER_OTLP_TRACES_TIMEOUT` else `OTEL_EXPORTER_OTLP_TIMEOUT`. So a header that the host sets for another collector, such as a token, also goes to `ZOE_OTLP_ENDPOINT`.

## Development and test variables

The tests, the benchmarks and the recipes of the repository read these variables. A deployment does not set them.

| Variable | Read by | Value |
| --- | --- | --- |
| `ZOE_SIM_SEED`, `ZOE_SIM_SEEDS` | the simulation tests of `zoe-server` | one seed to run, or the number of seeds |
| `ZOE_FLIP_SEED`, `ZOE_FLIPS` | the bit-flip harness of `zoe-core` | the seed, and the number of flips of each run |
| `ZOE_KILL_LOOP_SEED`, `ZOE_KILL_LOOP_RUNS` | the kill loop, `just crash` | the seed, and the number of rounds |
| `ZOE_FUZZ_SEED`, `ZOE_FUZZ_ROUNDS` | the fuzz smoke test of `zoe serve` | the seed, and the number of inputs |
| `ZOE_LARGE_STORE_MB`, `ZOE_CACHE_SIZE_MB`, `ZOE_RSS_LIMIT_MB` | the large-store test of `zoe serve` | the size of the store, the page cache, and the bound of the resident memory |
| `ZOE_LOAD_OBJECTS`, `ZOE_LOAD_DIR` | the load-time test of `zoe-core` | the number of objects, and the directory of the stores |
| `ZOE_FINISH_TXNS`, `ZOE_FINISH_SIZE`, `ZOE_FINISH_DIR`, `ZOE_FINISH_ATTACH` | the finish-time test of `zoe-core` | the transactions, the record size, the directory, and the encode before the finish |
| `ZOE_EXIT_PATH_DIR` | the exit-path tests of `zoe convert` | a directory with a `Data.fs` to convert and back |
| `ZOE_LOUPE_FIXTURE` | the tests of Loupe | a store to open |
| `ZOE_PLONE_PORT`, `ZOE_PLONE_ADMIN`, `ZOE_PLONE_ADMIN_FILE`, `ZOE_PLONE_ZEO_ADDRESS` | the Plone scenarios of `plone/`, `just plone` | the HTTP port, the admin user, the file of the admin user, the address of the server of the suites |
| `ZOE_BENCH_*` | the benchmarks of `bench/` and `zoe-bench`, `just bench` | the commit, the machine, the PostgreSQL database, the lock and the switches of the runs |
| `ZOE_CXX_LIBS` | `devenv.nix`, the benchmarks of Plone | the directory of the C++ library of the shell |
| `ZOE_FLIP_CHILD`, `ZOE_CLIENT_TLS_TEST_CHILD`, `ZOE_WALKING_SKELETON_WRITER` | tests that start a child process | set by the test for its child; do not set them |
