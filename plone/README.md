# Plone instances

The Plone instances that the suites and the benchmarks run against. zc.buildout builds them; `just plone` is the entry point. One configuration per scenario:

| File | Storage |
| --- | --- |
| `filestorage.cfg` | FileStorage, the baseline |
| `zeo.cfg` | a ZEO 6 server and a ZEO client, the baseline |
| `zoe.cfg` | the in-process `<zoe>` storage |
| `zeoclient.cfg` | a ZEO client of `zoe serve` |

`just plone <scenario>` builds a scenario and starts its instance in the foreground. `just plone <scenario> build` only builds it. `just plone <scenario> check` runs `set_admin.py` and `create_site.py` in the instance, which create the admin user and a Plone site. Then it starts the instance, and `read_site.py` reads the page of the site over HTTP. A new mark in the title of the site tells that the page comes from this instance. The instances listen on 127.0.0.1, on the port `ZOE_PLONE_PORT`, 8080 by default.

No file of the repository holds a password. `ZOE_PLONE_ADMIN` gives the admin user of the instances as `user:password`. Without it, `just plone` writes a random password once to `target/plone/admin`, which only the user can read, and prints that path. The password never goes on a command line or into a file of the build: buildout gets only the user name, with an empty password, so zope2instance writes no `inituser` file. Before the instance starts, `set_admin.py` runs in it and gives that user the Manager role and the password. `set_admin.py`, `create_site.py` and `read_site.py` read the admin user from `ZOE_PLONE_ADMIN`, or else from the file that `ZOE_PLONE_ADMIN_FILE` names.

`versions.cfg` pins the Plone 6.1 release, and `requirements.txt` gives zc.buildout and its tools at the versions of that release. The build goes to `target/plone`: the scenarios share the eggs and the downloads, and each scenario has its own directory for its scripts, parts and data. `just plone` builds the `zoedb` wheel, installs it into `target/plone/zoedb` and links it as a develop egg, so each build takes the current code.

`templates/zope.conf.in` carries the `<zoe>` stanza. plone.recipe.zope2instance cannot write it, so `zoe.cfg` renders the template with collective.recipe.template into the instance. The instances run on CPython 3.13, the newest version that Plone 6.1 supports.

The first build needs the network. A later build of the same scenario does not.
