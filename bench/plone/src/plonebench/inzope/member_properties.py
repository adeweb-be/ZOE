"""Move the properties of the members and groups of each Plone site of the
store into records of `zoe.plone`, and write the tier 2 entry of their
class into the merge policy (ADR 0046, section 9). Print the number of
principals that moved, then the policy, as JSON lines.

    run.py ZOPE_CONF member_properties.py

The import step of the profile `zoe.plone:default` does the same move. The
script does only this part of the profile, so the other entries and
switches of the policy stay as the fixture and the options of the run give
them. The policy keeps its classes and its switches. The script commits
before the instances of the run start.
"""

import json

import transaction
import zoe.policy
from Products.CMFCore.interfaces import ISiteRoot
from zoe.plone import properties
from zope.component.hooks import setSite

moved = {}
for site in app.objectValues():  # noqa: F821
    if ISiteRoot.providedBy(site):
        setSite(site)
        moved[site.getId()] = sum(
            properties.move_properties(sheet) for sheet in properties.sheets(site)
        )
setSite(None)
connection = app._p_jar  # noqa: F821
policy = {
    key: value
    for key, value in (zoe.policy.read(connection) or {}).items()
    if key != "version"
}
policy["classes"] = {**policy.get("classes", {}), **properties.policy()["classes"]}
zoe.policy.install(connection, policy)
transaction.commit()
print(json.dumps({"moved": moved}, sort_keys=True))
print(json.dumps(zoe.policy.read(connection), sort_keys=True))
