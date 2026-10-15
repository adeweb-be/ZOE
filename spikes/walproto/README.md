# walproto: the prototype of ZOE-184

A scratch crate, not a member of the workspace. It takes `zoe-core` as a path dependency for the layout `main`, and holds the layout `log`: a group commit on a write-ahead log, an in-memory table, a checkpoint thread that folds into the redb tables of the core, loads that read the table then the tree, and an open that replays the log.

```sh
cargo build --release
./target/release/walproto commit layout=log scenario=update size=1024 c=16 txns=20 dir=/tmp/w
./target/release/walproto reads table=10000 size=1024 dir=/tmp/w
./target/release/walproto recovery mb=64 size=1024 dir=/tmp/w
bash run.sh results.jsonl      # the matrix of the finding
python3 summary.py results.jsonl
```

`results*.jsonl` are the raw results of the finding `docs/spikes/write-log.md`. The path of `zoe-core` in `Cargo.toml` is the one of the machine of the spike.
