"""The stacks under test: a storage server and the Zope instances on it.

Each stack is a storage server and a client section in `zope.conf`. The
runner writes `zope.conf` and `wsgi.ini` of each instance itself, from one
template, so every setting but the storage section is the same in every
stack. The eggs come from the buildout scenarios of `plone/`.

- `config`: the stacks, the settings and the files that a run needs;
- `templates`: `zope.conf`, `wsgi.ini` and `zeo.conf`;
- `processes`: the server, the instances and the scripts of a run.

The other modules use the names of the package, so a test can replace one
of them.
"""

from plonebench.stack.config import (
    FOLDER_CLASS,
    FOLDER_ENTRY,
    FOLDER_ORDERS,
    HERE,
    INZOPE,
    PLONE,
    PROBE,
    REPO,
    SPLIT_MERGES,
    STACKS,
    WORK,
    Settings,
    StackConfig,
    campaign_settings,
    egg_paths,
    folder_orders_policy,
    interpreter,
    missing_prerequisites,
    pin,
    pinning,
    profile_refusal,
    serve_options,
    split_merges_policy,
    split_merges_refusal,
    store_layout,
    zoe_binary,
    zope_serve_script,
)
from plonebench.stack.processes import (
    Stack,
    clean_environment,
    copy_store,
    http_ok,
    port_open,
    wait_for,
)
from plonebench.stack.templates import storage_section, wsgi_ini, zeo_conf, zope_conf

__all__ = [
    "FOLDER_CLASS",
    "FOLDER_ENTRY",
    "FOLDER_ORDERS",
    "HERE",
    "INZOPE",
    "PLONE",
    "PROBE",
    "REPO",
    "SPLIT_MERGES",
    "STACKS",
    "WORK",
    "Settings",
    "Stack",
    "StackConfig",
    "campaign_settings",
    "clean_environment",
    "copy_store",
    "egg_paths",
    "folder_orders_policy",
    "http_ok",
    "interpreter",
    "missing_prerequisites",
    "pin",
    "pinning",
    "port_open",
    "profile_refusal",
    "serve_options",
    "split_merges_policy",
    "split_merges_refusal",
    "storage_section",
    "store_layout",
    "wait_for",
    "wsgi_ini",
    "zeo_conf",
    "zoe_binary",
    "zope_conf",
    "zope_serve_script",
]
