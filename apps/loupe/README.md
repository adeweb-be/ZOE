# Loupe

Loupe is the desktop inspector and editor for ZOE stores and servers: a Tauri 2 application with a React and TypeScript window, built with Vite. Its Rust side is the `zoe-loupe` crate.

Loupe v0 has folder mode and the object browser. It opens a store folder read-only and holds the lock of the store, so a running instance and Loupe never share a store. It shows the root, follows references and resolves Plone paths. For each object it shows the class, the oid, the serial, the size, the decoded state and the outbound references. It never writes to the folder.

## Recipes

The Tauri toolchain is in the `loupe` profile of the shell, not in the default shell. Run the recipes in it:

    devenv --profile loupe shell -- just loupe <command>

| Recipe | Does |
| --- | --- |
| `just loupe` | builds a release app into `target/loupe` and runs it |
| `just loupe dev` | runs the app on the Vite dev server, with reload |
| `just loupe check` | type-checks the window, checks the format of the Tauri crate, runs clippy on it and runs its tests |
| `just loupe e2e` | builds a debug app and runs the end-to-end tests under `xvfb-run` |

Outside the profile, the recipe stops and prints this command. The recipes run `npm ci` when `package-lock.json` is newer than the installed modules. `just check` does not check the window.

## Layout

| Path | Holds |
| --- | --- |
| `src/App.tsx` | the window: the folder bar, the object tree and the object pane |
| `src/ObjectTree.tsx` | the virtualised tree: the root, and below each object its outbound references |
| `src/ObjectPane.tsx` | the path bar and the facts, the state and the references of one object |
| `src/format.ts` | the text of a state: JSON, with bytes as bytes literals, up to a limit |
| `src/api.ts` | the IPC calls, their types and the UI strings of their errors |
| `src/i18n/` | the translation catalog: `en.json` holds every UI string, and `t()` reads it |
| `src-tauri/` | the Tauri crate, its IPC commands and their errors |
| `tests/e2e/` | the end-to-end tests |
| `wdio.conf.ts` | the WebdriverIO configuration of the end-to-end tests |

`src-tauri/` is a Cargo workspace of its own. It links WebKitGTK, and the tests and lints of the main workspace must build without it. Its lock file keeps `tauri` and its companion crates on one release: a `tauri-runtime` of a later release does not build with this `tauri`. The bundle identifier is `dev.adeweb.zoe.loupe`, from the naming table of the specification.

## IPC commands

Oids and tids are strings in ZODB form, as `ZODB.utils.oid_repr` writes them: `0x00`, `0x0100`.

| Command | Arguments | Returns |
| --- | --- | --- |
| `open_folder` | `path` | `{ path, lastTid }`. It closes the open folder first. |
| `pick_folder` | none | the folder that the native dialog gives, or null |
| `root` | none | the root object |
| `object` | `oid` | the object `oid` |
| `resolve` | `path` | the object that the Plone path leads to, from `root["Application"]` |

An object is `{ oid, serial, class, size, state, references, error }`. `state` is the decoded state as JSON; the module documentation of `crates/zoe-loupe/src/state.rs` gives the form of each value and the limits on its size. Each reference is `{ oid, at }`, where `at` is a JSON pointer into `state`. `error` holds the refusal of zoe-pickle when the record does not decode.

A command that fails gives `{ kind, ...parameters }`, for example `{ "kind": "noObject", "path": "/site/x", "segment": "x" }`. The kinds are `noFolder`, `noStore`, `locked`, `needsRepair`, `noRecord`, `noObject`, `refused`, `badId` and `other`. The window shows the UI string `error.<kind>` of the catalog, with the parameters.

The pane shows the start of a large state, and a button shows all of it.

## End-to-end tests

The tests use WebdriverIO, `tauri-driver` and WebKitWebDriver. The `loupe` profile of the shell has all three. `just loupe e2e` runs them under `xvfb-run`, so they need no display, and WebKitGTK renders with Mesa in software. The window has a typed path field next to the native dialog, because a driver cannot reach a native dialog.

`wdio.conf.ts` picks two free ports for `tauri-driver` and WebKitWebDriver. `tauri-driver` leads a process group of its own, and the configuration stops the whole group after the session, so a failed run leaves no app behind.

Each run writes the fixture store of the `zoe-loupe` tests into a new temporary directory: the spec runs the test `the_fixture_store_opens` with `ZOE_LOUPE_FIXTURE` set to that directory.

## Not in v0

Server mode, time travel, the live tail, space, operations, editing, inbound references and catalog paths.
