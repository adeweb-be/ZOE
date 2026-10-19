"""Import a GenericSetup profile into each Plone site of the store, as the
owner of a site does when it installs an add-on. Print the number of
principals whose properties moved, then the merge policy, as JSON lines.

    run.py ZOPE_CONF profile.py PROFILE

PROFILE is a profile id, such as `zoe.plone:default`. The import steps of
`zoe.plone:default` write the merge policy of the site (ADR 0046, section
9) and move the properties of the members and groups into records of
`zoe.plone`. The script commits before the instances of the run start.
"""

import json
import sys

import transaction
import zoe.policy
from AccessControl.SecurityManagement import newSecurityManager, noSecurityManager
from AccessControl.SpecialUsers import system
from Products.CMFCore.interfaces import ISiteRoot
from Products.CMFCore.utils import getToolByName
from zoe.plone import properties
from zope.component.hooks import setSite

(profile,) = sys.argv[1:]


def dicts(site):
    """Return the number of principals of `site` whose properties are a
    plain dict."""
    return sum(
        isinstance(value, dict)
        for sheet in properties.sheets(site)
        for value in getattr(sheet, "aq_base", sheet)._storage.values()
    )


newSecurityManager(None, system)
moved = {}
for site in app.objectValues():  # noqa: F821
    if ISiteRoot.providedBy(site):
        setSite(site)
        before = dicts(site)
        setup = getToolByName(site, "portal_setup")
        setup.runAllImportStepsFromProfile(f"profile-{profile}")
        transaction.commit()
        moved[site.getId()] = before - dicts(site)
setSite(None)
noSecurityManager()
connection = app._p_jar  # noqa: F821
print(json.dumps({"moved": moved}, sort_keys=True))
print(json.dumps(zoe.policy.read(connection), sort_keys=True))
