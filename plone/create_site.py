"""Create a Plone site in the instance, as the admin user.

`bin/instance run create_site.py MARK [MOUNT]` runs this script in the
instance, with the storage of the scenario, after set_admin.py. The instance
gives the Zope application as `app`. ZOE_PLONE_ADMIN gives the admin user as
user:password. Without it, the file that ZOE_PLONE_ADMIN_FILE names holds
that text.

As the admin user, the script removes the site of an earlier run, adds a
new site, commits and closes the databases. The title of the site holds
MARK, so a reader can tell this site from another one.

MOUNT is a mount point of zope.conf, such as /content. With MOUNT, the
script adds the mount point when it is missing, and adds the site in the
folder of the mount point, so the site goes to the mounted database.
Products.ZODBMountPoint adds the mount point.
"""

import os
import sys

import transaction
from AccessControl.SecurityManagement import newSecurityManager
from Products.CMFPlone.factory import addPloneSite

SITE_ID = "Plone"
# The instance runs the script with exec. sys.argv holds the path of the
# script, then the arguments of the script.
SCRIPT = sys._getframe().f_code.co_filename
MARK, *MOUNT = sys.argv[sys.argv.index(SCRIPT) + 1 :]
TITLE = f"Check site {MARK}"


def admin():
    """Return the name and the password of the admin user."""
    text = os.environ.get("ZOE_PLONE_ADMIN")
    if not text:
        with open(os.environ["ZOE_PLONE_ADMIN_FILE"]) as file:
            text = file.read()
    name, password = text.strip().split(":", 1)
    return name, password


name, _ = admin()
users = app.acl_users
newSecurityManager(None, users.getUser(name).__of__(users))

folder = app
if MOUNT:
    from Products.ZODBMountPoint.MountedObject import manage_addMounts

    path = MOUNT[0]
    if path.strip("/") not in app.objectIds():
        manage_addMounts(app, (path,))
        transaction.commit()
    folder = app.unrestrictedTraverse(path)

if SITE_ID in folder.objectIds():
    folder.manage_delObjects([SITE_ID])
    transaction.commit()
addPloneSite(
    folder,
    SITE_ID,
    title=TITLE,
    extension_ids=(
        "plone.app.contenttypes:default",
        "plonetheme.barceloneta:default",
    ),
    setup_content=True,
)
transaction.commit()
site = folder[SITE_ID]
print(f"create_site: {name} created {'/'.join(site.getPhysicalPath())}, {site.title!r}")
# A mount point opens a database of its own. Close each database.
for db in app._p_jar.db().databases.values():
    db.close()
