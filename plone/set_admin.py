"""Make the admin user a manager of Zope, with its password.

`bin/instance run set_admin.py` runs this script in the instance, with the
storage of the scenario. The instance gives the Zope application as `app`.
ZOE_PLONE_ADMIN gives the admin user as user:password. Without it, the file
that ZOE_PLONE_ADMIN_FILE names holds that text. The password never goes
through a command line.

The script adds the user with the Manager role, or gives an existing user
the password and the role. Then it commits and closes the database.
"""

import os

import transaction


def admin():
    """Return the name and the password of the admin user."""
    text = os.environ.get("ZOE_PLONE_ADMIN")
    if not text:
        with open(os.environ["ZOE_PLONE_ADMIN_FILE"]) as file:
            text = file.read()
    name, password = text.strip().split(":", 1)
    return name, password


name, password = admin()
users = app.acl_users
if users.getUser(name) is None:
    users._doAddUser(name, password, ["Manager"], [])
elif users.meta_type == "Pluggable Auth Service":
    # A Plone site replaces the user folder of Zope with this one. Its
    # plugins keep the users and their roles.
    users.users.updateUserPassword(name, password)
    users.roles.assignRoleToPrincipal("Manager", name)
else:
    users._doChangeUser(name, password, ["Manager"], [])
transaction.commit()
print(f"set_admin.py: {name} is a manager of Zope")
app._p_jar.db().close()
