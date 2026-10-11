"""Create a Plone site in the instance, as the admin user.

`bin/instance run create_site.py MARK` runs this script in the instance,
with the storage of the scenario, after set_admin.py. The instance gives the
Zope application as `app`. ZOE_PLONE_ADMIN gives the admin user as
user:password. Without it, the file that ZOE_PLONE_ADMIN_FILE names holds
that text.

As the admin user, the script removes the site of an earlier run, adds a
new site, commits and closes the database. The title of the site holds
MARK, so a reader can tell this site from another one.
"""

import os
import sys

import transaction
from AccessControl.SecurityManagement import newSecurityManager
from Products.CMFPlone.factory import addPloneSite

SITE_ID = "Plone"
# The instance puts the arguments of the script after its path.
TITLE = f"Check site {sys.argv[-1]}"


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

if SITE_ID in app.objectIds():
    app.manage_delObjects([SITE_ID])
    transaction.commit()
addPloneSite(
    app,
    SITE_ID,
    title=TITLE,
    extension_ids=(
        "plone.app.contenttypes:default",
        "plonetheme.barceloneta:default",
    ),
    setup_content=True,
)
transaction.commit()
print(f"create_site: {name} created the Plone site {SITE_ID}, {app[SITE_ID].title!r}")
app._p_jar.db().close()
