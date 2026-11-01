"""Damage the site of a store, as a defect of a run would, and commit.

    run.py ZOPE_CONF damage.py MODE PATH [TITLE]

MODE is one of:

- `title`: give the object at PATH the title TITLE and reindex it, as if
  the site lost the last edit of the object. The catalog agrees with the
  object.
- `uncatalog`: remove the catalog entry of the object at PATH. The object
  stays.
- `uncatalog-keep-count`: remove the catalog entry of the object at PATH,
  then add one to the count of the catalog, so that the length of the
  catalog does not change, as when a merge of a bucket loses an entry.

The tests run it on a copy of the store of a run, never on a fixture.
"""

import sys

import transaction
from AccessControl.SecurityManagement import newSecurityManager
from AccessControl.SpecialUsers import system
from zope.component.hooks import setSite

mode, path, *rest = sys.argv[1:]
newSecurityManager(None, system)
site = app["Plone"]  # noqa: F821
setSite(site)
catalog = site.portal_catalog
obj = site.unrestrictedTraverse(path)
if mode == "title":
    (title,) = rest
    obj.setTitle(title)
    obj.reindexObject()
elif mode in ("uncatalog", "uncatalog-keep-count"):
    length = len(catalog)
    catalog.uncatalog_object("/".join(obj.getPhysicalPath()))
    assert len(catalog) == length - 1, (length, len(catalog))
    if mode == "uncatalog-keep-count":
        catalog._catalog._length.change(1)
        assert len(catalog) == length
else:
    sys.exit(f"damage: unknown mode {mode}")
transaction.commit()
print(f"damage: {mode} {path}")
app._p_jar.db().close()  # noqa: F821
