"""Create Documents in threads, and count the ConflictErrors by class.

`bin/instance run creators.py THREADS ITEMS SEED` runs this script in the
instance, with the storage of the scenario, after set_admin.py and
create_site.py. The instance gives the Zope application as `app`.
ZOE_PLONE_ADMIN gives the admin user as user:password. Without it, the file
that ZOE_PLONE_ADMIN_FILE names holds that text.

As the admin user, the script sets `_uid_counter` of `portal_uidgenerator`
of the site to SEED and commits: the site has given ids up to SEED. Then it
adds the folder `creators` and gives it a history id, so the load does not
write the folder for its history id. Then it runs THREADS threads, each
with its own connection. Each thread creates ITEMS Documents in the folder,
one transaction for each Document, and retries a transaction on
`ConflictError` until it commits, as Zope does. CMFEditions gives each new
Document a history id from `portal_uidgenerator`.

The last line of stdout is a JSON report:

    {"created": <Documents in the folder>,
     "seed": <_uid_counter of the tool before the load>,
     "history_ids": [<history id of each Document>],
     "raised": {"<module.Class>": <ConflictErrors raised on that class>}}

A thread that fails with another error makes the script exit with 1.
"""

import collections
import json
import os
import sys
import threading
import traceback

import transaction
from AccessControl.SecurityManagement import newSecurityManager, noSecurityManager
from Acquisition import aq_base
from Products.CMFCore.utils import getToolByName
from Testing.makerequest import makerequest
from ZODB.POSException import ConflictError
from zope.component.hooks import setSite
from zope.globalrequest import setRequest

SITE_ID = "Plone"
FOLDER_ID = "creators"
# The instance runs the script with exec. sys.argv holds the path of the
# script, then the arguments of the script.
SCRIPT = sys._getframe().f_code.co_filename
ARGS = sys.argv[sys.argv.index(SCRIPT) + 1 :]
if len(ARGS) != 3:
    sys.exit("usage: bin/instance run creators.py THREADS ITEMS SEED")
THREADS, ITEMS, SEED = (int(arg) for arg in ARGS)


def admin():
    """Return the name and the password of the admin user."""
    text = os.environ.get("ZOE_PLONE_ADMIN")
    if not text:
        with open(os.environ["ZOE_PLONE_ADMIN_FILE"]) as file:
            text = file.read()
    name, password = text.strip().split(":", 1)
    return name, password


def enter(application, name):
    """Log in as `name` and enter the site, in this thread.

    Return the site.
    """
    root = makerequest(application)
    setRequest(root.REQUEST)
    users = root.acl_users
    newSecurityManager(None, users.getUser(name).__of__(users))
    site = root[SITE_ID]
    setSite(site)
    return site


def leave():
    """Leave the site and log out, in this thread."""
    setSite(None)
    setRequest(None)
    noSecurityManager()


def counter_of(tool):
    """Return `_uid_counter` of the tool as an int."""
    counter = aq_base(tool)._uid_counter
    return counter if isinstance(counter, int) else counter()


name, _ = admin()
db = app._p_jar.db()

# Prepare the site in the main thread.
site = enter(aq_base(app), name)
tool = getToolByName(site, "portal_uidgenerator")
aq_base(tool)._uid_counter = SEED
transaction.commit()
seed = counter_of(tool)
site.invokeFactory("Folder", FOLDER_ID, title="Creators")
getToolByName(site, "portal_historyidhandler").register(site[FOLDER_ID])
transaction.commit()

raised = collections.Counter()
failures = []
lock = threading.Lock()


def creator(index):
    """Create ITEMS Documents, one transaction for each."""
    connection = db.open()
    try:
        folder = enter(connection.root()["Application"], name)[FOLDER_ID]
        for item in range(ITEMS):
            while True:
                try:
                    transaction.begin()
                    folder.invokeFactory(
                        "Document",
                        f"doc-{index}-{item}",
                        title=f"Document {index} {item}",
                    )
                    transaction.commit()
                    break
                except ConflictError as error:
                    transaction.abort()
                    with lock:
                        raised[error.class_name or "unknown"] += 1
    except BaseException:
        with lock:
            failures.append(traceback.format_exc())
    finally:
        transaction.abort()
        leave()
        connection.close()


threads = [threading.Thread(target=creator, args=(i,)) for i in range(THREADS)]
for thread in threads:
    thread.start()
for thread in threads:
    thread.join()

for failure in failures:
    print(failure, file=sys.stderr)

# See the commits of the threads.
transaction.begin()
folder = site[FOLDER_ID]
handler = getToolByName(site, "portal_historyidhandler")
documents = [folder[id_] for id_ in folder.objectIds() if id_.startswith("doc-")]
report = {
    "created": len(documents),
    "seed": seed,
    "history_ids": [handler.queryUid(document) for document in documents],
    "raised": dict(raised),
}
transaction.abort()
leave()
print(json.dumps(report), flush=True)
for database in db.databases.values():
    database.close()
if failures:
    sys.exit(1)
