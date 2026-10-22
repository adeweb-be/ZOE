# Plone and `zoe.plone`

A Plone 6.1 site runs on ZOE with no change: on `<zoe>` in-process, or as a client of `zoe serve`. The package `zoe.plone`, from the extra `zoedb[plone]`, adds features that cut the conflicts of a busy site. Each feature is opt-in, except the ids of `portal_uidgenerator`.

| Feature | How it turns on | Store |
| --- | --- | --- |
| the ids of `portal_uidgenerator` from a sequence of the store | the wheel in the instance | `<zoe>` or `<zoeclient>` |
| the merge policy, from the Dexterity types | the profile `zoe.plone:default` | any: the store merges at the vote |
| the append-only workflow history | the behaviour `zoe.plone.history`, with the profile | any |
| one record for the properties of each member and group | the profile | any |
| the locks of edit forms in the store | the backend `locking` | `<zoe>` or `<zoeclient>` |
| queued indexing | the backend `indexing` | `<zoe>` or `<zoeclient>` |
| the registry in the store | the backend `registry`, then a move | `<zoe>` or `<zoeclient>` |

A feature that needs `<zoe>` or `<zoeclient>` uses the sequences or the KV namespaces of the store. A stock `<zeoclient>` has neither, and there the feature keeps the stock behaviour of Plone.

After the import of the profile, every instance of the site needs the wheel. An instance without `zoedb` loads the classes of `zoe.plone` as broken objects (see [Uninstall](#uninstall)).

## Install

Install the wheel with the extra `plone` into the instance (see [Install](install.md)). Plone loads `zoe.plone` through `plone.autoinclude`, with no ZCML to add.

At the start, `zoe.plone` gives `portal_uidgenerator` its ids from a sequence of the store. Two editors who create content at the same time then do not conflict on that tool. The ids stay unique and above the ids that the stock tool gave.

## The profile `zoe.plone:default`

The profile has the title ZOE. Import it in each site, with the tab Import of `portal_setup`, or in a script that runs in the instance:

```python
setup = site.portal_setup
setup.runAllImportStepsFromProfile("profile-zoe.plone:default")
```

The profile does three things:

1. It writes the merge policy of the site into the store.
2. It moves the workflow history of each object whose type has the behaviour `zoe.plone.history`.
3. It moves the properties of each member and group into one record for each principal. Two logins of one member then merge: each time field takes the newer value.

### The merge policy

When two transactions change one object, ZODB raises a `ConflictError`, and Zope retries the request. The merge policy tells the store how to merge two changes of one object at the vote, so neither request retries. The policy is the object `root['zoe.merge_policy']` of the store. The server and the in-process storage both read it.

The import step derives one entry for the class of each Dexterity type:

- the class merges field by field: two changes of different fields merge;
- two changes of one field conflict, except `modification_date`, which takes the newer value;
- the fields of an `@invariant` form one group, and both sides must agree on each field of the group;
- a class with an `@invariant` whose group would be empty never merges.

The derived entries start in shadow mode: the store computes each merge and records it, but the conflict ends as it ends without the merge. So you can check the merges before they change data. Two classes start on: the `Folder` of plone.app.contenttypes and the Plone site. With them, two creations in one folder do not conflict on the folder.

To read the merges of shadow mode, give the store a resolution log: the key `resolution-log` of `<zoe>`, or `--resolution-log` of `zoe serve`. Each line is one JSON object. The metric `zoe_structural_merges_total` of the server counts them too (see [Monitoring](monitoring.md)).

To switch a class on, add the file `zoe-merge-policy.json` to a profile of your add-on, and import that profile:

```json
{
  "classes": {
    "my.addon.content.Report": {"tier2": "on"}
  }
}
```

The class name is the `__module__` and the `__qualname__` of the class. An override cannot unblock a class that never merges. A change of a Dexterity type derives the policy again. [Python API](../reference/python-api.md#zoeplone) lists the modules of `zoe.plone`.

### The append-only workflow history

Two transitions of one object from one snapshot conflict in stock Plone. Give a type the behaviour `zoe.plone.history`, "Append-only workflow history", then import the profile again. Its objects then keep their workflow history in a class whose transitions merge. Two transitions from one snapshot both commit when the first one to commit keeps the state, such as a comment. Two changes of state from one state still conflict.

## The backends

Turn a backend on in each instance, with the variable `ZOE_PLONE_BACKENDS` in `zope.conf`:

```xml
<environment>
  ZOE_PLONE_BACKENDS locking,indexing
</environment>
```

With plone.recipe.zope2instance, set `environment-vars = ZOE_PLONE_BACKENDS locking,indexing`. You can also include the ZCML file of a backend in your add-on: `zoe.plone:locking.zcml`, `zoe.plone:indexing.zcml` or `zoe.plone:registry.zcml`. An unknown name stops the start with `ValueError`.

| Backend | What it does |
| --- | --- |
| `locking` | The lock of an edit form goes to an ephemeral namespace of the store, with its time to live. Opening an edit form then writes no object. |
| `indexing` | A save writes its catalog operations to a queue in the store, and writes no object of the catalog. One indexer for each site applies the queue. |
| `registry` | `zoe.plone.registry.move(site.portal_registry)` moves the records of the registry to a namespace of the store. A write of the registry then writes only the records that change. |

### Queued indexing

With `indexing`, each Zope process starts a thread that applies the queue of each site. One process holds the lease of a site at a time. A commit wakes the thread. A save shows in the navigation, the folder views and the searches after its batch. The user who saved waits for the batch at the next request, at most 2 seconds, so that user sees the save.

`@@zoe-indexing` on a site, for a Manager, gives the status of the queue as JSON:

| Field | Meaning |
| --- | --- |
| `pending` | the operations in the queue |
| `oldest_age` | the age of the oldest operation, in seconds |
| `failed` | the operations that failed 5 times. They wait in a namespace of their own. |
| `lease` | true when a process applies the queue |
| `delays` | the count, p50, p95 and maximum of the delays of the operations that this process applied |

A direct call of the catalog, such as `clearFindAndRebuild` or an upgrade step, still writes the catalog in its own transaction.

### The registry backend

`registry` only allows the move. Run the move in a script of the instance, and commit:

```python
import transaction
from zoe.plone import registry

registry.move(site.portal_registry)
transaction.commit()
```

`registry.move_back(site.portal_registry)` and a commit give the records back to the registry, with nothing lost. Move the registry back before you leave ZOE, before you copy the site, and before you delete it.

## Uninstall

A client without `zoedb` cannot read the classes of `zoe.plone`. It loads the properties of the members and groups, and the workflow histories of the types with the behaviour, as broken objects. So remove `zoe.plone` in this order:

1. On each site, move each registry back with `registry.move_back`, and commit.
2. With `indexing`, let the indexer empty the queue: `pending` of `@@zoe-indexing` is 0.
3. On each site, import the profile `zoe.plone:uninstall`, "ZOE (uninstall)". It removes the behaviour from the types, gives each workflow history back as a `PersistentMapping`, gives the properties back as dicts, and switches off the merges that the profile turned on.
4. Pack the store with garbage collection, at a time after the uninstall (see [Pack](pack.md)). The pack removes the old records of the classes of `zoe.plone`.
5. Remove the backends from `ZOE_PLONE_BACKENDS`, then remove the wheel.

`zoe convert --to-data-fs` checks steps 1, 3 and 4. It refuses a store that still holds a moved registry or current records of the classes of `zoe.plone`, and it writes nothing (see [Convert](convert.md#guards)).

A copy of the store to a `Data.fs` leaves out the KV namespaces. So it leaves out the queue of queued indexing and the locks of edit forms. An empty queue loses nothing. A lock lives for minutes.
