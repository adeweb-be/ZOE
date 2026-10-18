"""Write the switch `split_merges` of the merge policy into the store
(ADR 0048), and print the policy as a JSON line.

    run.py ZOPE_CONF split_merges.py MODE

The policy keeps its classes and its other switches. The script commits
before the instances of the run start.
"""

import json
import sys

import transaction
import zoe.policy

from plonebench.stack import split_merges_policy

mode = sys.argv[1]
connection = app._p_jar  # noqa: F821
zoe.policy.install(connection, split_merges_policy(zoe.policy.read(connection), mode))
transaction.commit()
print(json.dumps(zoe.policy.read(connection), sort_keys=True))
