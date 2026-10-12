# Probes of turmoil and madsim

Three small crates outside the workspace. Each has its own `[workspace]`
table, so the workspace of ZOE does not build them. Run them inside
`devenv shell`, with a target directory outside the repository:

- `tokio-probe`: tokio alone, the baseline of the build times.
- `turmoil-probe`: `cargo test -- --nocapture --test-threads 1`, then
  again with `RUSTFLAGS="--cfg tokio_unstable"`. `tests/select_outside.rs`
  polls `select!` outside any tokio runtime.
- `madsim-probe`: `RUSTFLAGS="--cfg madsim" cargo test -- --nocapture
  --test-threads 1`.
