"""Check the merge policy of zoe.plone and its append-only workflow history
in a Plone site, with the storage of the scenario.

`bin/instance run history.py THREADS` runs this script in the instance,
after set_admin.py and create_site.py. The instance gives the Zope
application as `app`. ZOE_PLONE_ADMIN gives the admin user as
user:password. Without it, the file that ZOE_PLONE_ADMIN_FILE names holds
that text.

As the admin user, the script:

1. adds the Document `before`, whose history is the stock one;
2. imports the profile `zoe.plone:default`, whose import step derives the
   merge policy of the site and writes it into the store;
3. gives the type Document the behaviour `zoe.plone.history`, and runs the
   upgrade step, which moves the history of `before`;
4. adds the Document `after`, which gets a `WorkflowHistory` when it is
   added;
5. runs THREADS threads, each with its own connection, that read `after`
   from one snapshot, add one status to its history through the workflow
   tool, and commit in turn, without a retry;
6. writes a policy with the Document class on, and lets two connections
   change the title and the description of `after` from one snapshot, each
   with `notifyModified`, and commit in turn.

The last line of stdout is a JSON report:

    {"classes": <the classes of the derived policy>,
     "document": <the entry of the Document class>,
     "event": <the entry of the Event class>,
     "moved": <the objects that the upgrade step moved>,
     "before": <the class of the history of before>,
     "before_entries": <the statuses of before>,
     "after": <the class of the history of after>,
     "committed": <the transitions that committed>,
     "conflicts": <the transitions that raised ConflictError>,
     "entries": <the statuses of after>,
     "fields": <the outcome of the two changes of fields>,
     "title": ..., "description": ...}
"""

import json
import os
import sys
import threading

import transaction
from AccessControl.SecurityManagement import newSecurityManager, noSecurityManager
from Acquisition import aq_base
from DateTime import DateTime
from Products.CMFCore.utils import getToolByName
from Testing.makerequest import makerequest
from ZODB.POSException import ConflictError
from zope.component.hooks import setSite
from zope.globalrequest import setRequest

import zoe.policy
from zoe.plone import history, merges

SITE_ID = "Plone"
WORKFLOW = "simple_publication_workflow"
DOCUMENT = "plone.app.contenttypes.content.Document"
EVENT = "plone.app.contenttypes.content.Event"
SCRIPT = sys._getframe().f_code.co_filename
ARGS = sys.argv[sys.argv.index(SCRIPT) + 1 :]
if len(ARGS) != 1:
    sys.exit("usage: bin/instance run history.py THREADS")
THREADS = int(ARGS[0])


def admin():
    """Return the name of the admin user."""
    text = os.environ.get("ZOE_PLONE_ADMIN")
    if not text:
        with open(os.environ["ZOE_PLONE_ADMIN_FILE"]) as file:
            text = file.read()
    return text.strip().split(":", 1)[0]


def enter(application, name):
    """Log in as `name` and enter the site, in this thread. Return the
    site."""
    root = makerequest(application)
    setRequest(root.REQUEST)
    users = root.acl_users
    newSecurityManager(None, users.getUser(name).__of__(users))
    site = root[SITE_ID]
    setSite(site)
    return site


def leave():
    setSite(None)
    setRequest(None)
    noSecurityManager()


def class_of(obj):
    history_object = aq_base(obj).workflow_history
    return f"{type(history_object).__module__}.{type(history_object).__name__}"


name = admin()
db = app._p_jar.db()  # noqa: F821
site = enter(aq_base(app), name)  # noqa: F821

# 1. A Document with the stock history.
site.invokeFactory("Document", "before", title="Before")
transaction.commit()

# 2. The profile writes the derived policy.
setup = getToolByName(site, "portal_setup")
setup.runAllImportStepsFromProfile("profile-zoe.plone:default")
transaction.commit()
derived = zoe.policy.read(site._p_jar)

# 3. The behaviour, then the upgrade step.
fti = getToolByName(site, "portal_types")["Document"]
fti.behaviors = (*fti.behaviors, "zoe.plone.history")
transaction.commit()
moved = history.upgrade(setup)
transaction.commit()

# 4. A new Document gets the history of the behaviour.
site.invokeFactory("Document", "after", title="After", description="one")
transaction.commit()
leave()

# 5. Transitions from one snapshot.
connections = []
for _ in range(THREADS):
    manager = transaction.TransactionManager()
    connections.append((manager, db.open(transaction_manager=manager)))
for manager, connection in connections:
    manager.begin()
    connection.cacheMinimize()
statuses = [
    {
        "action": "comment",
        "actor": name,
        "comments": f"transition {number}",
        "review_state": "private",
        "time": DateTime(),
    }
    for number in range(THREADS)
]
results = [None] * THREADS
barrier = threading.Barrier(THREADS)


def transition(number):
    manager, connection = connections[number]
    root = connection.root()["Application"]
    site = enter(root, name)
    try:
        tool = getToolByName(site, "portal_workflow")
        tool.setStatusOf(WORKFLOW, site["after"], statuses[number])
        barrier.wait(60)
        # Commit in the order of the numbers.
        while results[:number].count(None):
            threading.Event().wait(0.01)
        try:
            manager.commit()
            results[number] = "committed"
        except ConflictError:
            manager.abort()
            results[number] = "conflict"
    finally:
        leave()


threads = [threading.Thread(target=transition, args=(n,)) for n in range(THREADS)]
for thread in threads:
    thread.start()
for thread in threads:
    thread.join(120)
for _, connection in connections:
    connection.close()

# 6. Two changes of two fields of one Document.
site = enter(aq_base(app), name)  # noqa: F821
policy = merges.derive(site, {"classes": {DOCUMENT: {"tier2": "on"}}})
zoe.policy.install(site._p_jar, policy)
transaction.commit()
leave()
pair = []
for _ in range(2):
    manager = transaction.TransactionManager()
    pair.append((manager, db.open(transaction_manager=manager)))
for manager, connection in pair:
    manager.begin()
    connection.cacheMinimize()
changes = [("title", "After, edited"), ("description", "two")]
for (manager, connection), (field, value) in zip(pair, changes, strict=True):
    document = enter(connection.root()["Application"], name)["after"]
    setattr(aq_base(document), field, value)
    document.notifyModified()
    leave()
outcomes = []
for manager, _ in pair:
    try:
        manager.commit()
        outcomes.append("committed")
    except ConflictError:
        manager.abort()
        outcomes.append("conflict")
for _, connection in pair:
    connection.close()

transaction.begin()
site = enter(aq_base(app), name)  # noqa: F821
before = site["before"]
after = site["after"]
report = {
    "classes": sorted(derived["classes"]),
    "document": derived["classes"].get(DOCUMENT),
    "event": derived["classes"].get(EVENT),
    "moved": moved,
    "before": class_of(before),
    "before_entries": len(aq_base(before).workflow_history[WORKFLOW]),
    "after": class_of(after),
    "committed": results.count("committed"),
    "conflicts": results.count("conflict"),
    "entries": len(aq_base(after).workflow_history[WORKFLOW]),
    "fields": outcomes,
    "title": after.title,
    "description": after.description,
}
leave()
print(json.dumps(report))
