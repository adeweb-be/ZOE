# Spike: who writes a Zope session on a Plone 6.1 site

Card ZOE-211, step 1; requirement F31, the session part. ADR 0043 (Context, and section A8) found that Plone 6.1 keeps nothing in a Zope session. This spike checks that finding on the requests of B5 and on the add-ons that a Plone 6.1 site installs.

**Verdict.** No path of Plone 6.1 and no maintained add-on of the target writes a Zope session on a standard site. A Plone 6.1 instance has no session container: plone.recipe.zope2instance mounts no temporary storage when Products.CMFPlone is in the eggs, and the upgrade to Plone 6.0 removes a broken `/temp_folder`. The only reader in the stack is `logoutUser` of PlonePAS. It asks for the session with `create=0`, so it reads a session only when the request has a browser id. PluggableAuthService keeps its CSRF tokens in a session, but plone.protect replaces these functions with no-ops when its ZCML loads. With a session container at the default path, 108 requests of the B5 operations wrote no session object, and the container was never made. Of 67 add-ons, one maintained add-on can use a Zope session: pas.plugins.oidc, through its property `use_session_data_manager`, which is off by default. With it on, a stock Plone 6.1 site has no container, so the login flow fails. Two other add-ons use a session, but their last releases are for Python 2 and Plone 4. The human deferred the session backend to ZOE-232 (#477).

## Question

Step 1 of the card asks:

1. Which code of Plone 6.1, or of a common add-on of a Plone 6.1 site, writes a Zope session (`REQUEST.SESSION`, `session_data_manager`, `temp_folder`) on a standard site?
2. Where does Plone 6.1 keep the data that a session kept before?
3. On the B5 fixture with the stock session machinery, does a request write a session?

## Method

- **Source.** The spike searched the 291 eggs of the zoe scenario (`target/plone/eggs/cp313`, Plone 6.1.5, with the test eggs of `plone/base.cfg`) for `.SESSION`, `SESSION`, `session_data_manager`, `getSessionData` and `temp_folder`, in Python files, page templates, ZCML and XML. A path below is relative to the egg directory.
- **Add-ons.** The spike took the last release of 67 add-ons from PyPI: form, shop, login, search, captcha, workflow, layout and export add-ons. It searched them in the same way. Each tests directory and test module stays out of the count. The add-ons and their versions are in the table of finding 4.
- **Requests.** A small fixture ran in the zoe scenario through `bin/instance run`: a Plone site with plone.app.contenttypes, plonetheme.barceloneta and plone.restapi, as `bench/plone/src/plonebench/inzope/build_site.py` adds; four users (a member, an editor, an importer and a manager); one section with three Documents, a News Item, a File and a Collection, and the folders of the import and of the inbox. A client in Python sent the operations of B5 (`bench/plone/src/plonebench/locustfile.py`) three times each, over HTTP to the instance: the anonymous views of a folder, a Document, a News Item, a File view and a download, a Collection, `@@search` and the REST `@search`; the login form and the REST `@login`; the edit form with a save and with a cancel; the add form with a save; a workflow transition and the anonymous view after it; a REST `PATCH`; a REST import; and the reindex of `SearchableText` through the ZMI. Three requests are not in B5: a logout, a group added through `@@usergroup-groupdetails`, and the ZMI page of the users of PAS, which renders a CSRF token of PAS. In all: 108 requests, 3 of each kind, 0 errors.
- **Two runs.** The `stock` run has no `/temp_folder`, as on a Plone 6.1 instance. Each access to a session then logs `External session data container '/temp_folder/session_data' not found.` (`Products.Sessions-6.0-py3.13.egg/Products/Sessions/SessionDataManager.py:297-330`), so the event log counts the accesses. The `temp` run adds `/temp_folder` as a plain Folder in the main storage. The session data manager then makes its stock `TransientObjectContainer` there at the first access (`SessionDataManager.py:333-345`), and a session write is a record of a class of Products.Transience in the store. After each run, a script read each transaction of the store after the fixture and counted its records by class.

## Findings

### 1. Plone 6.1 has no session container

- Products.Sessions 6.0 is a dependency of Products.PluggableAuthService. At the start of Zope it adds `browser_id_manager` and `session_data_manager` to the Zope root (`Products.Sessions-6.0-py3.13.egg/Products/Sessions/__init__.py:41-54`, `:97-100`). The manager names the container `/temp_folder/session_data`, and a hook before the traversal sets `request.SESSION` lazily (`SessionDataManager.py:442`). The spike found the hook on the root of the fixture: `(50, 'SessionDataManager')` in `__before_traverse__`.
- plone.recipe.zope2instance 8.0.1 writes no temporary storage when Products.CMFPlone is in the eggs and the part does not set `zodb-temporary-storage` (`plone.recipe.zope2instance-8.0.1-py3.13.egg/plone/recipe/zope2instance/recipe.py:694-717`). Its template for the temporary storage mounts `/temp_folder` with `Products.TemporaryFolder.TemporaryContainer` (`recipe.py:1294-1302`), and Products.TemporaryFolder is not an egg of Plone 6.1.
- The upgrade step `remove_temp_folder` of Plone 6.0 removes a broken `/temp_folder` and its mount point (`plone.app.upgrade-3.3.2-py3.13.egg/plone/app/upgrade/v60/alphas.py:24-45`, `v60/configure.zcml:32-33`).
- So `request.SESSION` is `None` on a Plone 6.1 site: the manager finds no container, and it can make one only in an existing `/temp_folder` (`SessionDataManager.py:315-326`).

### 2. The code of Plone 6.1 that reaches a session

The search of the 291 eggs gave these places, out of the tests:

| Code | What it does with a session | On Plone 6.1 |
| --- | --- | --- |
| `Products.PlonePAS-8.0.5-py3.13.egg/Products/PlonePAS/tools/membership.py:667-679`, `logoutUser` | `getSessionData(create=0)`, then `invalidate()` if a session exists | reads the container only when the request has a browser id (`SessionDataManager.py:137-146`). Plone sets no browser id, so the logout reads nothing |
| `Products.PluggableAuthService-3.0-py3.13.egg/Products/PluggableAuthService/utils.py:94-129`, `getCSRFToken`, `checkCSRFToken`, and `csrf_only` (`:153`) | keeps a CSRF token in the session under `_csrft_` | plone.protect replaces both functions with no-ops (`plone.protect-5.0.5-py3.13.egg/plone/protect/monkey.py:21-52`) when its ZCML loads (`plone/protect/configure.zcml:71`). The ZMI page of the users of PAS showed `value=""` in its field `csrf_token` |
| `Products.PluggableAuthService/plugins/SessionAuthHelper.py:66-103` | keeps the login and the password in the session | the plugin is not in the `acl_users` of a Plone site. The site of the fixture has 17 plugins, and none is a Session Auth Helper |
| `plone.recipe.zope2instance`, `plone.app.upgrade` | finding 1 | no container |

Products.CMFPlone, plone.app.users, plone.app.contenttypes, plone.restapi, plone.app.iterate, plone.app.discussion, plone.app.multilingual and the other eggs of Plone 6.1.5 hold no other access to a session.

### 3. Where Plone 6.1 keeps the data that a session kept before

| Data | Where | State on the server |
| --- | --- | --- |
| The login of a user | the cookie `__ac` of plone.session, a signed ticket (`plone/session/plugins/session.py:169-177`). The plugin `session` of the fixture has a timeout of 7,200 s and a refresh interval of 3,600 s | none: the cookie holds the ticket |
| A REST token | the plugin `jwt_auth` of plone.restapi, with `store_tokens` false on the fixture | none: the token is a signed JWT |
| The keys that sign the CSRF tokens | the `KeyManager` of plone.keyring, a persistent utility of the site and `_key_manager` of the Zope root (ADR 0043, Context) | persistent objects. They change only at a login, daily or weekly |
| The status messages | the cookie `statusmessages` of Products.statusmessages | none |
| The lock of an edit form | plone.locking, or the namespace `zoe.plone.locks` with the locking backend (ADR 0043) | the content object, or the ephemeral namespace |

### 4. Add-ons

Only three of the 67 add-ons use a Zope session:

| Add-on | Release | Plone | Use |
| --- | --- | --- | --- |
| pas.plugins.oidc | 2.0.0 | 6.0, 6.1 | `Session` (`pas/plugins/oidc/session.py:11-37`) keeps the `state`, the `nonce` and the PKCE verifier of the login flow. It uses a cookie `__ac_session` by default, and `session_data_manager.getSessionData(create=True)` when the property `use_session_data_manager` is true; its default is false (`pas/plugins/oidc/plugins.py:116`, `interfaces.py:45-50`). With the property on, a Plone 6.1 site has no container, `getSessionData` returns `None`, and the first `set` fails. So a site that turns the property on adds a session container itself |
| collective.z3cform.wizard | 1.4.8 | 4 (Python 2) | keeps the data of the steps of a wizard in `request.SESSION` (`collective/z3cform/wizard/wizard.py:83-180`) |
| collective.cart.core | 0.9.1 | 4 (Python 2) | keeps the cart in `getSessionData(create=True)` (`collective/cart/core/adapter/article.py:34`) |

These add-ons use no Zope session. The Plone versions are those of the classifiers of their last release:

- Forms: collective.easyform 5.0.2, collective.easyformplugin.createdx 1.0a3, collective.volto.formsupport 3.3.3, collective.formsupport.counter 1.0.4, Products.PloneFormGen 1.8.9 (Plone 5.2 and older).
- Shops: bda.plone.cart 2.0b1 and bda.plone.checkout 2.0b1, which keep the cart in a cookie.
- Login and users: pas.plugins.authomatic 2.0.0 (a cookie and the REST service), pas.plugins.ldap 1.8.4, pas.plugins.headers 2.0.0, pas.plugins.keycloakgroups 1.0.0b1, pas.plugins.memberpropertytogroup 3.0.0, collective.saml2 1.5, collective.shibboleth 1.3, collective.pwexpiry 1.0.0, collective.impersonate 3.0.0, collective.volto.otp 1.0.1, Products.membrane 8.0.0.
- Captchas and spam: plone.formwidget.recaptcha 3.0.0, plone.formwidget.hcaptcha 3.0.0, plone.formwidget.captcha 1.0.3, collective.recaptcha 2.1.2, collective.z3cform.norobots 3.0, collective.honeypot 5.0.0.
- The others: collective.cover 3.0.0, plone.app.mosaic 4.2.0, collective.collectionfilter 6.0.0, collective.taxonomy 3.1.7, collective.exportimport 1.16, collective.jsonify 1.6, collective.documentgenerator 4.0, collective.contact.core 1.43.3, Products.EasyNewsletter 6.0.4, collective.mailchimp 3.1.1, collective.solr 10.1.1, collective.elasticsearch 5.0.0, collective.searchandreplace 8.3.1, collective.bookmarks 1.1, collective.privacy 1.1.0b2, collective.cookiecuttr 2.0.1, collective.feedback 1.2.0, collective.anonfeedback 1.0, collective.behavior.banner 2.0, collective.messagesviewlet 0.23, collective.glossary 3.0.2, collective.lineage 4.0.0, collective.multilingual 2.0, collective.timestamp 1.5, collective.autopublishing 1.0.3, collective.limitfilesizepanel 3.0.3, collective.contentrules.mailtogroup 1.6.3, collective.tiles.collection 2.1.1, collective.workspace 3.0.0, collective.geo.leaflet 1.0.1, collective.sendaspdf 3.0, collective.MockMailHost 4.0.0, Products.Poi 4.0.3, Products.PloneKeywordManager 7.0.0, Products.CMFPlacefulWorkflow 4.0.0, plone.app.changeownership 1.0, plone.app.event 6.0.1, plone.app.discussion 6.0.1, plone.formwidget.namedfile 4.0.0, ftw.upgrade 3.3.1.

`collective/documentgenerator/browser/views.py:265` names a Folder `temp_folder` in a content folder; it is not the session folder.

The scenarios of `plone/` and the B5 fixture install only Plone, plone.app.contenttypes, plonetheme.barceloneta, plone.restapi and zoedb\[plone\].

### 5. No request of B5 writes a session

| Run | Requests | Errors | Accesses to a session (log lines) | Transactions after the fixture | Transactions with a record of Products.Transience | Container `/temp_folder/session_data` |
| --- | --- | --- | --- | --- | --- | --- |
| `stock`: no `/temp_folder` | 108 | 0 | 0 | 43 | 0 | none |
| `temp`: `/temp_folder` in the store | 108 | 0 | 0 | 40 | 0 | never made |

In the `temp` run, the manager makes the container at the first access to a session. It was never made, so no request of the run read or wrote `request.SESSION`. The `stock` run logged no access either. The logout reads no session because the client has no browser id cookie. The ZMI page of PAS renders its CSRF token through the no-op of plone.protect.

## What this means for F31

F31 names the ephemeral namespace for "sessions, CSRF keys, locks". ADR 0043 built the locks, kept the CSRF keys in plone.keyring (answer 12 of the human), and built no session data manager. This spike finds no code that would use one on a Plone 6.1 site of the target. A session backend helps only a site that adds a session container itself: for pas.plugins.oidc with `use_session_data_manager` on, or for code of its own. On such a site, a `/temp_folder` on a temporary storage keeps the sessions in the memory of each Zope process, so several processes behind a load balancer need sticky sessions. A session backend on the ephemeral namespace would share them through the store, and a restart would keep them.

The fixture check of the planner (card #433, first comment) shows that the stock `TransientObjectContainer` in a `zoe.Storage` already passes the 26 tests of the suite of Products.Sessions. So a site that adds a container on ZOE gets working sessions today. They are content writes: each session write is a record, and it stays until a pack.

The human decides between:

1. **Build it anyway** (steps 2 to 4 of the card): an ADR, then a session container on the namespace, opt-in. It costs the budget of the card, and a backend that no target site runs.
2. **Drop sessions from F31**: F31 keeps the locks and the registry. The text of F31 says that Plone 6.1 has no session container and that a site that adds one keeps the stock container of Products.Transience in the object graph.
3. **Defer**: F31 keeps its text of today ("no session data manager yet"); the card goes back to the backlog with this finding, and the acceptance test stays on the branch `plan/ZOE-211` until a site names a need.

Recommendation: 3, defer. No code of the target writes a session, and the one maintained add-on that can use one has a cookie mode by default. The acceptance test and this finding stay ready for the day that a site needs shared sessions.

Decision: the human deferred the session backend to the card ZOE-232 (#477), which keeps this finding and the acceptance test on the branch `plan/ZOE-211`.
