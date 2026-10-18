"""Write the switches `split_merges` (ADR 0048) and `folder_orders`
(ADR 0052) of the merge policy into the store, and print the policy as a
JSON line.

    run.py ZOPE_CONF split_merges.py SPLIT_MERGES [FOLDER_ORDERS]

Each argument is a mode, or `-` to leave its switch as it is. The switch
`folder_orders` comes with the tier 2 entry of the class of the folders.
The policy keeps its classes and its other switches. The script commits
before the instances of the run start.
"""

import json
import sys

import transaction
import zoe.policy

from plonebench.stack import folder_orders_policy, split_merges_policy

modes = sys.argv[1:] + ["-"]
split, folder = modes[0], modes[1]
connection = app._p_jar  # noqa: F821
policy = zoe.policy.read(connection)
if split != "-":
    policy = split_merges_policy(policy, split)
if folder != "-":
    policy = folder_orders_policy(policy, folder)
if policy is not None:
    policy = {key: value for key, value in policy.items() if key != "version"}
    zoe.policy.install(connection, policy)
transaction.commit()
print(json.dumps(zoe.policy.read(connection), sort_keys=True))
