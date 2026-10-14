# ZOE-077 POC: where `zoe serve` spends its time

**This is a proof of concept. Do not merge it.** The branch holds prototypes behind environment toggles, timing counters, and the scripts and results of the quick experiments of ZOE-077 (#132). Each remedy that the results support becomes a card of its own, written from scratch.

The findings are in the comment of #132 that links to this branch.

## The toggles of the `zoe` binary

Each toggle is read once, when the process starts. With no toggle, the binary runs as `main` at the base commit of the branch.

| Variable | Effect |
| --- | --- |
| `ZOE_POC_PROFILE=1` | Count the time of each commit term. `zoe serve` prints one `POC {...}` line on stderr when it stops. |
| `ZOE_POC_TIDONLY=1` | The vote reads the current tid of each oid from the key, without the record. It reads a full record only for a stale written oid. |
| `ZOE_POC_STORECOMPRESS=1` | Each `storea` compresses its record on the thread of the transaction. The batch leader only frames and inserts. |
| `ZOE_POC_PRECOMPRESS=1` | `tpc_finish` compresses the records before it joins the batch. The batch leader only frames and inserts. |
| `ZOE_POC_COMPRESSION=none` or `zstd:N` | Write new records with this codec. |
| `ZOE_POC_ZSTD_TLS=1` | One zstd context per thread, for compression and decompression. |
| `ZOE_POC_NOSYNC=1` | Every commit uses `Durability::None`. A bound, not a remedy: a crash loses commits. |
| `ZOE_POC_QUEUE=N` | The call window of a connection, 4 on `main`. |
| `ZOE_POC_BUFREAD=1` | A 64 KiB buffered reader on the socket (TCP without TLS only). |

## Run

Inside `devenv shell`:

```sh
cargo build --release --locked -p zoe-server --bin zoe
just bench test        # once: creates target/bench/venv-3.13 with zodbshootout
cd bench/poc/ZOE-077
./ab-tmpfs.sh          # or another script below
python3 summarize.py results/ab-tmpfs.jsonl
```

`common.sh` finds the Python of the bench environment and the `zoe` binary. `POC_PYTHON` and `POC_ZOE` override them. The tmpfs scripts put each store in `POC_WORK`, `/dev/shm/zoe-poc-077` by default, and remove it after the run.

| Script | Runs |
| --- | --- |
| `driver.py` | One server (zeo, zeo with msgpack, or zoe with toggles), one zodbshootout cell, one JSON line: ops/s, server and client CPU, `/metrics` deltas, bytes written, the `POC` counters |
| `matrix-disk.sh` | Every toggle, stores on the disk of the working directory |
| `matrix-tmpfs.sh` | Every toggle, stores on tmpfs, and `read_cold` |
| `ab-tmpfs.sh`, `ab-threads.sh`, `ab-processes-update.sh` | Interleaved A/B rounds of the best prototypes against ZEO |
| `client-cpu.sh` | The CPU of the zodbshootout processes against each server |
| `wrap-strace.sh` | A wrapper for `driver.py --wrap` that counts the syscalls of `zoe serve` |
| `fsyncx.py` | The cost of `fdatasync` and `fsync` after 4 KiB to 800 KiB of writes, sequential and scattered |
| `extract_samples.py` | Real pickles from a FileStorage, for `codecx` |
| `redbx/` | A crate of its own with three micro-benchmarks: `redbx` (bytes per commit of two record layouts), `codecx` (zstd and xxh3 on real pickles), `readx` (one redb read transaction per load against a shared one) |

## Results

`results/` holds the raw JSON lines of the runs that the comment of #132 reports. They come from a laptop, not the reference machine: Intel Core Ultra 7 155H, 30 GB, a consumer NVMe, CPython 3.13. The label `zoe-base` is a build of `main` without the toggles. The `strace-*.txt` files are the `strace -c` summaries of threads `update` runs.
