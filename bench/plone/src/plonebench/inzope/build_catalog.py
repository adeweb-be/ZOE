"""Build the catalog fixture of a spec in the storage of the Zope
configuration.

    run.py ZOPE_CONF build_catalog.py SPEC_JSON PLAN_JSON

SPEC_JSON holds a `plonebench.catalog.plan.CatalogSpec`. The script adds
the Plone site `Plone`, gives its catalog the generic indexes and metadata
columns of `xlarge`, removes the entries that the creation of the site
catalogued, then catalogs a stub for each entry of the plan with
`Catalog.catalogObject` of ZCatalog. No content object is added. It commits
after each `batch` entries and then minimises the object cache. It writes
PLAN_JSON: the counts of the plan, the query set with the hits of the plan,
and the seconds of each phase.

ZCatalog draws the record ids of the catalog and the word ids of a
lexicon from the module `random`. The script gives each of the two modules
a generator of its own, seeded from the spec, so a seed gives one set of
ids. The build process runs with a fixed PYTHONHASHSEED: KeywordIndex keeps
the keywords of an entry in the order of a set. A generated schema
interface of plone.dexterity names the time of its type information, which
is the time of the build; `object_provides` names the first date of the
plan in its place.
"""

import json
import random
import resource
import sys
import time

import transaction
from AccessControl.SecurityManagement import newSecurityManager
from AccessControl.SpecialUsers import system
from DateTime import DateTime
from plone.dexterity.schema import portalTypeToSchemaName, splitSchemaName
from plone.dexterity.utils import createContent
from Products.CMFPlone.CatalogTool import DENIED_INTERFACES
from Products.CMFPlone.factory import addPloneSite
from Products.ZCatalog import Catalog as catalog_module
from Products.ZCTextIndex import Lexicon as lexicon_module
from zope.component.hooks import setSite
from zope.interface import providedBy

from plonebench.catalog import plan

spec_path, plan_path = sys.argv[1:3]
with open(spec_path) as file:
    spec = plan.CatalogSpec.from_json(json.load(file))
entries = plan.Entries(spec)
seconds = {}
# The seeded ids, from the creation of the site on.
catalog_module.randint = random.Random(f"{spec.seed}:rid").randint
lexicon_module.randrange = random.Random(f"{spec.seed}:wid").randrange
started = time.time()

newSecurityManager(None, system)
if plan.SITE_ID in app.objectIds():
    sys.exit("build_catalog: the storage already holds a site; build into a new store")
site = addPloneSite(
    app,
    plan.SITE_ID,
    title=entries.site_title(),
    extension_ids=(
        "plone.app.contenttypes:default",
        "plonetheme.barceloneta:default",
        "plone.restapi:default",
    ),
    setup_content=False,
)
setSite(site)
transaction.commit()
catalog = site.portal_catalog
for name, kind in plan.GENERIC_INDEXES.items():
    catalog.addIndex(name, kind)
for column in plan.COLUMNS:
    if column not in catalog._catalog.schema:
        catalog.addColumn(column)
transaction.commit()

inner = catalog._catalog
kinds = {name: index.meta_type for name, index in inner.indexes.items()}
if kinds != plan.INDEXES:
    differ = sorted(set(kinds.items()) ^ set(plan.INDEXES.items()))
    sys.exit(f"build_catalog: the indexes differ from the plan: {differ}")
if sorted(inner.schema) != sorted(plan.COLUMNS):
    differ = sorted(set(inner.schema) ^ set(plan.COLUMNS))
    sys.exit(f"build_catalog: the metadata columns differ from the plan: {differ}")
# The creation of the site catalogs the site root, at the time of the build.
# The plan gives the site root an entry of its own. The minimised cache
# drops the next ids that the catalog and the lexicons keep in volatile
# attributes.
for uid in list(inner.uids.keys()):
    inner.uncatalogObject(uid)
transaction.commit()
app._p_jar.cacheMinimize()
seconds["site"] = round(time.time() - started, 1)
print(f"build_catalog: site added in {seconds['site']} s", flush=True)


GENERATED = "plone.dexterity.schema.generated."


def stable(identifier):
    """The identifier of an interface, with the first date of the plan as
    the time of a generated schema interface."""
    if not identifier.startswith(GENERATED):
        return identifier
    _, portal_type, schema = splitSchemaName(identifier[len(GENERATED) :])
    name = portalTypeToSchemaName(
        portal_type, schema, prefix=plan.SITE_ID, suffix=str(plan.EPOCH * 60)
    )
    return GENERATED + name


def interfaces(obj):
    """The value of `object_provides`, as Plone's indexer gives it."""
    return tuple(
        stable(i.__identifier__)
        for i in providedBy(obj).flattened()
        if i.__identifier__ not in DENIED_INTERFACES
    )


# The interfaces of an object of each type. createContent makes an object
# that no container holds, and that the storage never sees.
PROVIDES = {"Plone Site": interfaces(site)}
for portal_type in plan.TYPES:
    if portal_type != "Plone Site":
        PROVIDES[portal_type] = interfaces(createContent(portal_type))


def date(value):
    return DateTime(value * 60, "UTC")


class Stub:
    """What the indexes and the columns read of one entry: an attribute for
    each value of the plan, and the physical path."""

    def __init__(self, values):
        state = self.__dict__
        for name, value in values.items():
            if name in plan.DATES:
                value = date(value)
            elif name == "object_provides":
                value = PROVIDES[value]
            state[name] = value
        self._physical = tuple(values["_path"].split("/"))

    def getPhysicalPath(self):
        return self._physical


jar = app._p_jar
tally = plan.Tally(entries)
building = time.time()
done = 0
for values in entries:
    inner.catalogObject(Stub(values), values["_path"])
    tally.add(values)
    done += 1
    if done % spec.batch == 0 or done == spec.entries:
        transaction.commit()
        jar.cacheMinimize()
        elapsed = time.time() - building
        peak = resource.getrusage(resource.RUSAGE_SELF).ru_maxrss
        peak_mb = peak / 1024 if sys.platform != "darwin" else peak / 1024 / 1024
        print(
            f"build_catalog: {done}/{spec.entries} entries in {elapsed:.0f} s, "
            f"{done / elapsed:.0f} entries/s, peak RSS {peak_mb:.0f} MB",
            flush=True,
        )
seconds["entries"] = round(time.time() - building, 1)
if len(inner) != spec.entries:
    sys.exit(f"build_catalog: the catalog holds {len(inner)} entries")

result = {
    "counts": tally.counts(),
    "queries": tally.query_set(),
    "seconds": seconds,
}
with open(plan_path, "w") as file:
    json.dump(result, file, indent=1)
print(
    f"build_catalog: {len(inner)} entries, {time.time() - started:.0f} s",
    flush=True,
)
transaction.abort()
app._p_jar.db().close()
