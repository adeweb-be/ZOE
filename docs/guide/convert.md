# Convert

`zoe convert` copies the transactions of a `Data.fs` into a ZOE store, and back. The tids, the oids, the pickles and the blobs do not change. The way back is your exit path: a store becomes a `Data.fs` again, which FileStorage opens.

The command runs from the binary or from the console script of the wheel.

## From a `Data.fs` to a store

1. Stop the instance, so that the `Data.fs` does not change during the conversion.
2. Convert:

   ```sh
   zoe convert --from-data-fs var/filestorage/Data.fs --data-fs-blob-dir var/blobstorage --path var/zoe --blob-dir var/zoe/blobs
   ```

3. Read the report. The command verifies the store against the `Data.fs` and exits with 0 when the report lists no problem.
4. Check the store (see [Verify and repair](verify-repair.md)):

   ```sh
   zoe verify --path var/zoe --blob-dir var/zoe/blobs
   ```

5. In `zope.conf`, replace the `<filestorage>` section with a `<zoe>` section with `path var/zoe` and `blob-dir var/zoe/blobs` (see [Run in-process](in-process.md)). Or serve the store with `zoe serve --path var/zoe --blob-dir var/zoe/blobs` (see [Run a server](server.md)).
6. Start the instance.

The output of a conversion:

```text
Converted var/filestorage/Data.fs into the store var/zoe
  transactions  5
  copied        5
  records       68
  blobs         1
  last tid      0x040c8044afd8db00
  problems      0
```

- `--path` is the store directory. The command creates the store when it does not exist.
- `--data-fs-blob-dir` is the blob directory of the `Data.fs`. Without it, the command copies no blob. The report then counts a problem, and the command exits with 1:

  ```text
    problems      1
      no blob directory  1
      the Data.fs has 1 blob records, and no blob directory was given
  zoe convert: the conversion does not verify: the report lists the problems
  ```

- `--blob-dir` sets the blob directory of the store. Without it, the blobs go to `blobs` in the store directory, as in the example. Give the same directory to `<zoe>` or to `zoe serve` later.
- Never give the store the blob directory of the `Data.fs`, such as `var/blobstorage`. The store writes its new blobs there, and a pack of the store removes blob files there. The `Data.fs` and its blob directory are your way back: keep them as they are.
- A conversion that stops resumes: run the same command again. It copies the transactions after the last transaction of the store.

`zodbconvert` of ZODB also works: give it a source `<filestorage>` and a destination `<zoe>` section, with `%import zoe`. `zoe convert` also verifies the result.

## Guards

The command checks its input before it writes anything:

- `--from-data-fs` must name a regular file that starts with the magic `FS30` of FileStorage. Another file creates no store.
- `--to-data-fs` needs `--data-fs-blob-dir`, and the store must exist. The directory of the `Data.fs` must exist too.
- When the store has blobs, its blob directory must hold the layout file of its blobs. A wrong `--blob-dir` fails the command before it writes anything.
- `--to-data-fs` refuses a store that holds data of `zoe.plone` that a `Data.fs` cannot hold:
  - a registry that the backend `registry` moved into a namespace of the store;
  - records of the classes `zoe.plone.properties.PrincipalProperties` and `zoe.plone.history.WorkflowHistory`, which a client without `zoedb` loads as broken objects.

  The message names each registry and each class, and says what to do: move each registry back, import the profile `zoe.plone:uninstall`, then pack the store with garbage collection (see [Plone](plone.md#uninstall)). The command exits with 1.

`--force` converts such a store anyway, and writes a warning that names each registry and each class. Use it only when the copy does not need these data. `--force=false` refuses, also when `ZOE_FORCE` is set.

A command line error, such as `--force false` without `=`, exits with 2 and changes nothing on disk.

## From a store to a `Data.fs`

Stop the process that holds the store. Create the directory of the `Data.fs`, then convert:

```sh
mkdir -p var/export
zoe convert --path var/zoe --to-data-fs var/export/Data.fs --data-fs-blob-dir var/export/blobstorage
```

The output:

```text
Converted the store var/zoe into var/export/Data.fs
  transactions  5
  copied        5
  records       68
  blobs         1
  last tid      0x040c7f8a7e3a0155
  problems      0
```

FileStorage then opens `var/export/Data.fs` with the blob directory `var/export/blobstorage`. A second run copies only the transactions after the last transaction of the `Data.fs`.

A `Data.fs` holds no KV namespace and no sequence. The report counts the keys of the durable namespaces that it leaves out, with a warning. So the copy loses the queue of queued indexing and the locks of edit forms. Empty the queue before the conversion.

## Switch between in-process and server

Both modes use one format of the store. To change the mode, change the configuration. Convert nothing.

From in-process to a server:

1. Stop the Zope instance. It closes the store.
2. Start `zoe serve` on the store directory, with the blob directory of the store: `zoe serve --path var/zoe --blob-dir var/zoe/blobs`.
3. In `zope.conf`, replace the `<zoe>` section with a `<zeoclient>` or a `<zoeclient>` section (see [Connect clients](clients.md)). Give the client a blob cache of its own, not the blob directory of the store.
4. Start the instance.

From a server to in-process: stop the clients, stop the server, then replace the client section with a `<zoe>` section on the same store directory and blob directory.
