# Loupe

Loupe is a desktop window that inspects a ZOE store. It opens a store folder read-only. It shows each object with its class, its state and its history, the difference between two revisions, and where the space of the store goes.

No package of Loupe is published yet. You build and run it from the source, on Linux.

## Run Loupe

The tools of Loupe are in the `loupe` profile of the shell, not in the default shell (see [Install](install.md#get-the-source-and-the-shell)):

```sh
devenv --profile loupe shell -- just loupe
```

The recipe builds a release app into `target/loupe` and starts it. The recipes of Loupe need Linux: on macOS, the profile gives only the Tauri command line.

## Open a store

Type the path of a store folder in the field "Store folder" and press "Open", or press "Browse". Loupe holds the lock of the store while the folder is open. So stop the Zope instance or the server first, or open a copy of the store: a restore of a backup is one (see [Back up and restore](backup-restore.md)).

Loupe writes nothing to a store that was closed cleanly. A store that a writer left open gets a repair at the open, and the window says so: `<path>: the store was not closed cleanly: repaired the store` (see [Verify and repair](verify-repair.md#a-store-that-was-not-closed-cleanly)).

## The object browser

- The tree starts at the root object. Below each object are the objects that it refers to.
- The pane shows the class, the oid, the serial, the size, the decoded state and the references of one object. The pane shows the start of a large state, and a button shows all of it.
- The field "Path" of the pane takes a path from the Zope application, such as `/Plone/news`. Press Enter to open that object.
- The buttons "Objects" and "Space" switch between the object browser and the space screen.

## Time travel

- The field "As of" takes a tid, such as `0x03f0b0c0d0e0f000`. The tree and the pane then show each object as it was after the last transaction at or before that tid. An empty field shows the last transaction.
- The history of an object lists its revisions, newest first: the tid, the user, the description and the size of each. The button of a row sets the field "As of" to its tid.
- The diff compares the decoded states of two revisions, which you pick in the columns "From" and "To" of the history. Each change shows where it is in the state, and the value before and after it.

The user and the description of a transaction show at most 4 KiB of the record. A longer text ends with `…`.

## The space screen

The space screen measures the store as of its last transaction:

- the size of each path, as a treemap, from the Zope application down. A click on an item opens its path;
- the size of each class: the current records, their bytes and their share, and the blob files;
- the 10 largest objects;
- the growth of the store over time;
- what a pack would remove, by class, at a tid or at the last transaction.

Each measure reads the whole store, so it takes time on a large store. The screen shows the progress.

## Not built yet

Loupe does not connect to a server, does not follow a live store, and does not edit. It shows no inbound references.
