# Loupe

Loupe is the desktop inspector and editor for ZOE stores and servers: a Tauri 2 application with a React and TypeScript window, built with Vite. Its Rust side is the `zoe-loupe` crate.

Loupe v0 has folder mode and the object browser. It opens a store folder read-only and holds the lock of the store, so a running instance and Loupe never share a store. It shows the root, follows references and resolves Plone paths. For each object it shows the class, the oid, the serial, the size, the decoded state and the outbound references. It never writes to the folder.

## Recipes

Run them inside `devenv shell`.

| Recipe | Does |
| --- | --- |
| `just loupe` | builds a release app into `target/loupe` and runs it |
| `just loupe dev` | runs the app on the Vite dev server, with reload |
| `just loupe check` | type-checks the window, checks the format and runs clippy on the Tauri crate |
| `just loupe e2e` | builds a debug app and runs the end-to-end tests under `xvfb-run` |

`just check` type-checks the window and checks the format of the Tauri crate. It does not build the Tauri crate. The recipes run `npm ci` when `package-lock.json` is newer than the installed modules.

## Layout

| Path | Holds |
| --- | --- |
| `src/App.tsx` | the window: the folder bar, the object tree and the object pane |
| `src/ObjectTree.tsx` | the virtualised tree: the root, and below each object its outbound references |
| `src/ObjectPane.tsx` | the path bar and the facts, the state and the references of one object |
| `src/api.ts` | the IPC calls and their types |
| `src/i18n/` | the translation catalog: `en.json` holds every UI string, and `t()` reads it |
| `src-tauri/` | the Tauri crate and its IPC commands |
| `tests/e2e/` | the end-to-end tests |
| `wdio.conf.ts` | the WebdriverIO configuration of the end-to-end tests |

`src-tauri/` is a Cargo workspace of its own. It links WebKitGTK, and the tests and lints of the main workspace must build without it.

## IPC commands

Oids and tids are strings in ZODB form, as `ZODB.utils.oid_repr` writes them: `0x00`, `0x0100`.

| Command | Arguments | Returns |
| --- | --- | --- |
| `open_folder` | `path` | `{ path, lastTid }`. It closes the open folder first. |
| `pick_folder` | none | the folder that the native dialog gives, or null |
| `root` | none | the root object |
| `object` | `oid` | the object `oid` |
| `resolve` | `path` | the object that the Plone path leads to, from `root["Application"]` |

An object is `{ oid, serial, class, size, state, references, error }`. `state` is the decoded state as JSON; the module documentation of `crates/zoe-loupe/src/state.rs` gives the form of each value. Each reference is `{ oid, at }`, where `at` is a JSON pointer into `state`. `error` holds the refusal of zoe-pickle when the record does not decode.

## End-to-end tests

The tests use WebdriverIO, `tauri-driver` and WebKitWebDriver. The devenv shell has all three. `just loupe e2e` runs them under `xvfb-run`, so they need no display. The window has a typed path field next to the native dialog, because a driver cannot reach a native dialog.

Each run writes the fixture store of the `zoe-loupe` tests into a new temporary directory: the spec runs the test `the_fixture_store_opens` with `ZOE_LOUPE_FIXTURE` set to that directory.

## Not in v0

Server mode, time travel, the live tail, space, operations, editing, inbound references and catalog paths.
