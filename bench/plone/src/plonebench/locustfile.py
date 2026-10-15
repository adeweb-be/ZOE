"""The Locust workloads of the Plone end-to-end benchmark.

The runner starts Locust with this file and sets these variables:

- PLONEBENCH_MANIFEST: the manifest of the fixture;
- PLONEBENCH_CREDENTIALS: the file that holds the password of the accounts;
- PLONEBENCH_HOSTS: the URLs of the Zope instances, comma separated. A user
  stays on one instance, as behind a sticky load balancer;
- PLONEBENCH_SCENARIO: a scenario of `workload.SCENARIOS`;
- PLONEBENCH_LEDGER: the ledger file of this Locust process;
- PLONEBENCH_STEPS: `users:seconds,...`, the steps of the closed workload;
- PLONEBENCH_THINK: `min,max` seconds between two operations of a user,
  0 by default;
- PLONEBENCH_CLIENT_RETRIES: the retries of the client after a failed write,
  0 by default. A retry is part of its operation: the latency of the
  operation runs from its first request to its final outcome.

Each simulated user has its own HTTP session and its own account. A
logical operation is one or more requests; Locust records each request
under a stable name, and each operation as the request type `OP`.
"""

from __future__ import annotations

import itertools
import json
import random
import time
import uuid

import gevent
from locust import HttpUser, LoadTestShape, events, task

from plonebench import corpus, forms, workload
from plonebench.ledger import AMBIGUOUS, FAILED, OK, PENDING, REJECTED, Ledger, Record

SCENARIO = workload.SCENARIOS[workload.env("SCENARIO", "browse")]
TARGETS = workload.Targets.load(workload.env("MANIFEST"))
with open(workload.env("CREDENTIALS")) as _file:
    PASSWORD = json.load(_file)["password"]
HOSTS = workload.env("HOSTS", "http://127.0.0.1:18080").split(",")
STEPS = workload.steps_from_env(workload.env("STEPS"))
THINK = tuple(float(x) for x in workload.env("THINK", "0,0").split(","))
CLIENT_RETRIES = int(workload.env("CLIENT_RETRIES", "0"))
SEED = int(workload.env("SEED", "1"))
SITE = TARGETS.manifest["site"]
MAX_USERS = max([users for users, _ in STEPS] or [1])
ROLES = workload.assign_roles(SCENARIO.mix, MAX_USERS)
ROLE_COUNTS = {role: ROLES.count(role) for role in SCENARIO.mix}
LEDGER = Ledger(workload.env("LEDGER", "ledger.jsonl"))
SEARCH_TERMS = TARGETS.manifest["search_terms"]
SUBJECTS = TARGETS.manifest["subjects"]
BROWSABLE = TARGETS.browsable()
FOLDERS = [i for i in BROWSABLE if i["portal_type"] == "Folder"]
DOCS = [i for i in BROWSABLE if i["portal_type"] in ("Document", "News Item")]
FILES = [i for i in BROWSABLE if i["portal_type"] == "File"]
COLLECTIONS = TARGETS.items(portal_type="Collection")
# Titles carry an operation code made of words that the site already
# holds. A new word in each save would make concurrent saves collide on
# the word ids of the lexicons, which real edits do far less often.
CODE_WORDS = corpus.code_words(corpus.plan_site(TARGETS.spec))

_counter = itertools.count()
_state = {"step": 0}
RETRYABLE = {409, 500, 502, 503, 504}


def _words(rng: random.Random, count: int) -> str:
    vocabulary = SEARCH_TERMS + SUBJECTS
    return " ".join(rng.choice(vocabulary) for _ in range(count))


class Attempt:
    """The requests of one attempt of a logical operation."""

    def __init__(self, user: PloneUser, op: str, number: int, kind: str, start: float):
        self.user = user
        self.op = op
        self.number = number
        self.kind = kind
        self.start = start
        self.statuses: list[int] = []
        self.ambiguous = False

    def intent(self, target: str, values: dict) -> None:
        """Record what a write will write, before its request goes out."""
        user = self.user
        LEDGER.write(
            Record(
                op=self.op,
                kind=self.kind,
                user=user.index,
                role=user.role,
                account=user.account,
                instance=user.instance,
                step=_state["step"],
                start=self.start,
                end=time.time(),
                outcome=PENDING,
                attempts=self.number,
                target=target,
                values=values,
            )
        )

    def request(self, method: str, path: str, name: str, **kwargs):
        headers = dict(kwargs.pop("headers", {}) or {})
        headers.update(
            {
                "X-Bench-Op": self.op,
                "X-Bench-Attempt": str(self.number),
                "X-Bench-Name": name,
            }
        )
        url = (
            path if path.startswith("http") else f"{self.user.base}/{path.lstrip('/')}"
        )
        with self.user.client.request(
            method, url, name=name, headers=headers, catch_response=True, **kwargs
        ) as response:
            status = response.status_code or 0
            self.statuses.append(status)
            if status == 0:
                self.ambiguous = True
                response.failure(f"no response: {response.error!r}")
            elif status >= 400:
                response.failure(f"HTTP {status}")
            else:
                response.success()
            return response


class OpResult:
    def __init__(self, outcome: str, target=None, values=None, detail=None):
        self.outcome = outcome
        self.target = target
        self.values = values or {}
        self.detail = detail


class PloneUser(HttpUser):
    host = HOSTS[0]
    # The roles decide the work; the closed loop has no think time unless
    # PLONEBENCH_THINK sets one.

    def wait_time(self):
        low, high = THINK
        return random.uniform(low, high) if high > 0 else 0

    def on_start(self):
        self.index = next(_counter)
        extra = len(SCENARIO.background)
        if self.index < extra:
            # The first users that the shape starts are the background
            # users, one for each background role.
            self.role = SCENARIO.background[self.index]
            self.rank = 0
        else:
            mix_index = (self.index - extra) % len(ROLES)
            self.role = ROLES[mix_index]
            self.rank = workload.ordinal(ROLES, mix_index)
        self.instance = self.index % len(HOSTS)
        self.base = f"{HOSTS[self.instance]}/{SITE}"
        self.rng = random.Random(SEED * 1_000_003 + self.index)
        self.account = None
        self.token = None
        self.users_of_role = ROLE_COUNTS.get(self.role, 1)
        kind, api = workload.ROLES[self.role]
        if kind == corpus.EDITOR:
            self.account, self.section = TARGETS.editor(self.rank)
        elif kind == corpus.MEMBER:
            members = TARGETS.manifest["users"][corpus.MEMBER]
            self.account = members[self.rank % len(members)]
        elif kind in (corpus.MAINTAINER, corpus.IMPORTER):
            self.account = TARGETS.manifest["users"][kind][0]
        if self.account is not None:
            if api == "rest":
                self.operation("login-rest", self.login_rest)
            else:
                self.operation("login", self.login_form)
        self.prepare()

    # Logical operations.

    def operation(self, kind: str, body, *args, retry: bool = False):
        """Run `body(attempt, *args)` as one logical operation, with the
        retries of the client for a write, and record it."""
        op = uuid.uuid4().hex
        start = time.time()
        attempts = 0
        statuses: list[int] = []
        result = None
        retries = CLIENT_RETRIES if retry else 0
        while True:
            attempts += 1
            attempt = Attempt(self, op, attempts, kind, start)
            try:
                result = body(attempt, *args)
            except Exception as error:  # a parse error or an unexpected page
                result = OpResult(REJECTED, detail=f"{type(error).__name__}: {error}")
            statuses.extend(attempt.statuses)
            if attempt.ambiguous and result.outcome != OK:
                result.outcome = AMBIGUOUS
            if result.outcome == FAILED and attempts <= retries:
                if attempt.statuses and attempt.statuses[-1] in RETRYABLE:
                    gevent.sleep(0.05 * 2 ** (attempts - 1))
                    continue
            break
        end = time.time()
        LEDGER.write(
            Record(
                op=op,
                kind=kind,
                user=self.index,
                role=self.role,
                account=self.account,
                instance=self.instance,
                step=_state["step"],
                start=start,
                end=end,
                outcome=result.outcome,
                attempts=attempts,
                requests=len(statuses),
                statuses=statuses,
                target=result.target,
                values=result.values,
                detail=result.detail,
            )
        )
        events.request.fire(
            request_type="OP",
            name=kind,
            response_time=(end - start) * 1000.0,
            response_length=0,
            exception=None if result.outcome == OK else Exception(result.outcome),
            context={},
        )
        return result

    @staticmethod
    def judge(response, ok: bool, target=None, values=None, detail=None) -> OpResult:
        status = response.status_code or 0
        if status == 0:
            return OpResult(AMBIGUOUS, target, values, detail or "no response")
        if status >= 400:
            return OpResult(FAILED, target, values, detail or f"HTTP {status}")
        return OpResult(OK if ok else REJECTED, target, values, detail)

    # Logins.

    def login_form(self, attempt: Attempt) -> OpResult:
        page = attempt.request("GET", "login", "GET login form")
        form = forms.parse(page.text).form(id="LoginForm")
        response = attempt.request(
            "POST",
            form.action,
            "POST login",
            data=form.fields(
                __ac_name=self.account,
                __ac_password=PASSWORD,
                **{"buttons.login": "Log in"},
            ),
        )
        ok = "__ac" in self.client.cookies.keys() and not forms.is_login_form(
            response.text
        )
        return self.judge(response, ok, target=self.account)

    def login_rest(self, attempt: Attempt) -> OpResult:
        response = attempt.request(
            "POST",
            "@login",
            "POST @login",
            json={"login": self.account, "password": PASSWORD},
            headers={"Accept": "application/json"},
        )
        token = None
        if response.status_code == 200:
            token = response.json().get("token")
        self.token = token
        return self.judge(response, token is not None, target=self.account)

    def rest_headers(self) -> dict:
        return {
            "Accept": "application/json",
            "Content-Type": "application/json",
            "Authorization": f"Bearer {self.token}",
        }

    # The preparation of each role.

    def prepare(self):
        if self.role in ("edit", "workflow"):
            pool = corpus.EDIT if self.role == "edit" else corpus.WORKFLOW
            self.docs = TARGETS.own_docs(self.rank, pool, self.users_of_role)
        elif self.role == "create_spread":
            self.folders = TARGETS.section_folders(self.section)
        elif self.role == "warm":
            self.docs = TARGETS.section_docs(self.section, corpus.EDIT)
            self.folders = TARGETS.section_folders(self.section)
        self.states = {}

    @task
    def work(self):
        getattr(self, f"role_{self.role}")()

    # Browsing.

    def view(
        self, attempt: Attempt, item: dict, name: str, suffix: str = ""
    ) -> OpResult:
        response = attempt.request("GET", item["path"] + suffix, name)
        text = response.text if response.status_code == 200 else ""
        canonical = forms.canonical(text) or ""
        ok = canonical.endswith("/" + item["path"]) and not forms.is_login_form(text)
        return self.judge(response, ok, target=item["path"])

    def download(self, attempt: Attempt, item: dict) -> OpResult:
        response = attempt.request(
            "GET", f"{item['path']}/@@download/file", "GET file download"
        )
        ok = response.status_code == 200 and len(response.content) == item["size"]
        return self.judge(response, ok, target=item["path"])

    def browse(self):
        folder = self.rng.choice(FOLDERS)
        self.operation("view-folder", self.view, folder, "GET folder")
        children = [i for i in DOCS if i["path"].startswith(folder["path"] + "/")]
        doc = self.rng.choice(children or DOCS)
        name = "GET news item" if doc["portal_type"] == "News Item" else "GET document"
        self.operation(
            "view-" + doc["portal_type"].lower().replace(" ", "-"), self.view, doc, name
        )
        # Up the breadcrumbs, to the parent of the item.
        parent_path = doc["path"].rpartition("/")[0]
        parent = next((i for i in FOLDERS if i["path"] == parent_path), None)
        if parent is not None:
            self.operation("view-folder", self.view, parent, "GET folder")
        if FILES and self.rng.random() < 0.2:
            item = self.rng.choice(FILES)
            # The view of a File; its own URL sends the file itself.
            self.operation("view-file", self.view, item, "GET file view", "/view")
            self.operation("download-file", self.download, item)

    def role_browse_anon(self):
        self.browse()

    def role_browse_member(self):
        self.browse()

    # Catalog.

    def search_page(self, attempt: Attempt, params: dict, name: str) -> OpResult:
        response = attempt.request("GET", "@@search", name, params=params)
        text = response.text if response.status_code == 200 else ""
        ok = 'id="search-results' in text
        return self.judge(response, ok, target=name)

    def collection(self, attempt: Attempt, item: dict) -> OpResult:
        response = attempt.request("GET", item["path"], "GET collection")
        text = response.text if response.status_code == 200 else ""
        ok = forms.canonical(text) is not None and not forms.is_login_form(text)
        return self.judge(response, ok, target=item["path"])

    def rest_search(self, attempt: Attempt, term: str) -> OpResult:
        response = attempt.request(
            "GET",
            "@search",
            "REST GET @search",
            params=[
                ("SearchableText", term),
                ("metadata_fields", "modified"),
                ("metadata_fields", "review_state"),
                ("b_size", "25"),
            ],
            headers={"Accept": "application/json"},
        )
        ok = response.status_code == 200 and "items_total" in response.json()
        return self.judge(response, ok, target="@search")

    def role_search(self):
        term = self.rng.choice(SEARCH_TERMS)
        choice = self.rng.random()
        if choice < 0.35:
            self.operation(
                "search",
                self.search_page,
                {"SearchableText": term},
                "GET @@search text",
            )
        elif choice < 0.55:
            params = {
                "SearchableText": term,
                "portal_type:list": "Document",
                "sort_on": "modified",
                "sort_order": "reverse",
            }
            self.operation(
                "search-filtered", self.search_page, params, "GET @@search filtered"
            )
        elif choice < 0.65:
            params = {
                "Subject:list": self.rng.choice(SUBJECTS),
                "sort_on": "sortable_title",
            }
            self.operation(
                "search-subject", self.search_page, params, "GET @@search subject"
            )
        elif choice < 0.75:
            params = {"SearchableText": term, "b_start:int": "10"}
            self.operation(
                "search-page-2", self.search_page, params, "GET @@search page 2"
            )
        elif choice < 0.9:
            self.operation("collection", self.collection, self.rng.choice(COLLECTIONS))
        else:
            self.operation("rest-search", self.rest_search, term)

    # Edits through the edit form.

    def edit_form(self, attempt: Attempt, path: str) -> OpResult:
        page = attempt.request("GET", f"{path}/@@edit", "GET edit form")
        form = forms.parse(page.text).form_with("form.buttons.save")
        if form is None:
            return self.judge(page, False, target=path, detail="no edit form")
        title = f"Edit {corpus.op_code(attempt.op, CODE_WORDS)} " + _words(self.rng, 3)
        description = "Edited " + _words(self.rng, 8)
        attempt.intent(path, {"title": title, "description": description})
        response = attempt.request(
            "POST",
            form.action,
            "POST edit form",
            data=form.fields(
                **{
                    "form.widgets.IDublinCore.title": title,
                    "form.widgets.IDublinCore.description": description,
                    "form.buttons.save": "Save",
                }
            ),
        )
        ok = response.url.rstrip("/").endswith("/" + path) and any(
            "Changes saved" in m for m in forms.status_messages(response.text)
        )
        return self.judge(
            response,
            ok,
            target=path,
            values={"title": title, "description": description},
        )

    def role_edit(self):
        if not self.docs:
            gevent.sleep(1)
            return
        doc = self.rng.choice(self.docs)
        self.operation("edit", self.edit_form, doc["path"], retry=True)

    def role_edit_shared(self):
        self.operation(
            "edit", self.edit_form, TARGETS.shared_doc(self.rank), retry=True
        )

    # Creation through the add form.

    def add_form(self, attempt: Attempt, folder: str) -> OpResult:
        page = attempt.request("GET", f"{folder}/++add++Document", "GET add form")
        form = forms.parse(page.text).form_with("form.buttons.save")
        if form is None:
            return self.judge(page, False, target=folder, detail="no add form")
        title = f"New {corpus.op_code(attempt.op, CODE_WORDS)} " + _words(self.rng, 3)
        attempt.intent(folder, {"title": title, "folder": folder})
        response = attempt.request(
            "POST",
            form.action,
            "POST add form",
            data=form.fields(
                **{
                    "form.widgets.IDublinCore.title": title,
                    "form.widgets.IDublinCore.description": "Created "
                    + _words(self.rng, 8),
                    "form.widgets.IRichTextBehavior.text": "<p>"
                    + _words(self.rng, 60)
                    + "</p>",
                    "form.buttons.save": "Save",
                }
            ),
        )
        created = response.url.split(f"/{SITE}/", 1)[-1]
        created = created.removesuffix("/view")
        ok = created.startswith(folder + "/") and any(
            "Item created" in m for m in forms.status_messages(response.text)
        )
        return self.judge(
            response,
            ok,
            target=created if ok else folder,
            values={"title": title, "folder": folder},
        )

    def role_create_spread(self):
        self.operation(
            "create", self.add_form, self.rng.choice(self.folders), retry=True
        )

    def role_create_shared(self):
        self.operation(
            "create", self.add_form, f"{corpus.BENCH}/{corpus.INBOX}", retry=True
        )

    # Workflow.

    def transition(self, attempt: Attempt, path: str) -> OpResult:
        page = attempt.request("GET", path, "GET document")
        links = forms.parse(page.text).links_containing("workflow_action=")
        actions = {
            link.split("workflow_action=")[1].split("&")[0]: link for link in links
        }
        for action, state in (  # noqa: B007 (both are used after the loop)
            ("publish", corpus.PUBLISHED),
            ("retract", corpus.PRIVATE),
        ):
            if action in actions:
                break
        else:
            return self.judge(
                page, False, target=path, detail=f"no transition in {sorted(actions)}"
            )
        attempt.intent(path, {"action": action, "state": state})
        response = attempt.request("GET", actions[action], "GET content_status_modify")
        ok = any(
            "Item state changed" in m for m in forms.status_messages(response.text)
        )
        return self.judge(
            response, ok, target=path, values={"action": action, "state": state}
        )

    def visibility(self, attempt: Attempt, path: str, state: str) -> OpResult:
        # A request without the cookies of the user: an anonymous visitor.
        response = self.client.request(
            "GET",
            f"{self.base}/{path}",
            name="GET document anonymous",
            headers={
                "X-Bench-Op": attempt.op,
                "X-Bench-Name": "GET document anonymous",
            },
            cookies={},
            allow_redirects=True,
        )
        attempt.statuses.append(response.status_code or 0)
        text = response.text or ""
        visible = response.status_code == 200 and not forms.is_login_form(text)
        expected = state == corpus.PUBLISHED
        return OpResult(
            OK if visible == expected else REJECTED,
            target=path,
            values={"state": state, "visible": visible},
        )

    def role_workflow(self):
        if not self.docs:
            gevent.sleep(1)
            return
        doc = self.rng.choice(self.docs)
        result = self.operation("workflow", self.transition, doc["path"], retry=True)
        if result.outcome == OK and self.rng.random() < 0.5:
            # Anonymous requests share no session: use a fresh client.
            saved = self.client.cookies.copy()
            self.client.cookies.clear()
            try:
                self.operation(
                    "visibility", self.visibility, doc["path"], result.values["state"]
                )
            finally:
                self.client.cookies.update(saved)

    # REST edits.

    def patch(self, attempt: Attempt, path: str, field: str) -> OpResult:
        value = f"Patch {corpus.op_code(attempt.op, CODE_WORDS)} " + _words(self.rng, 4)
        attempt.intent(path, {field: value})
        response = attempt.request(
            "PATCH",
            path,
            f"REST PATCH {field}",
            json={field: value},
            headers=self.rest_headers(),
        )
        return self.judge(
            response, response.status_code == 204, target=path, values={field: value}
        )

    def role_patch_fields(self):
        path, field = TARGETS.pair_doc(self.rank)
        self.operation("patch-" + field, self.patch, path, field, retry=True)

    def role_patch_same(self):
        self.operation(
            "patch-title", self.patch, TARGETS.hot_doc(self.rank), "title", retry=True
        )

    # Warmup: the forms without a save.

    def role_warm(self):
        doc = self.rng.choice(self.docs)
        folder = self.rng.choice(self.folders)

        def forms_only(attempt: Attempt) -> OpResult:
            first = attempt.request("GET", f"{doc['path']}/@@edit", "GET edit form")
            # The edit form locks the item, and only a save or a cancel
            # releases the lock. A lock that stays makes the edit form of
            # another editor show the locked item. So cancel, as a browser
            # user does.
            form = forms.parse(first.text).form_with("form.buttons.cancel")
            cancelled = form is not None
            if cancelled:
                cancel = attempt.request(
                    "POST",
                    form.action,
                    "POST edit form cancel",
                    data=form.fields(**{"form.buttons.cancel": "Cancel"}),
                )
                cancelled = cancel.status_code == 200 and any(
                    "Edit cancelled" in m for m in forms.status_messages(cancel.text)
                )
            second = attempt.request("GET", f"{folder}/++add++Document", "GET add form")
            ok = first.status_code == 200 and cancelled and second.status_code == 200
            return self.judge(second, ok, target=doc["path"])

        self.operation("warm-forms", forms_only)

    # Background.

    def reindex(self, attempt: Attempt) -> OpResult:
        # The ZMI pages hold no token of plone.protect; a page of Plone does.
        # Without a valid token, plone.protect aborts the transaction and
        # the ZMI still redirects with its message.
        page = attempt.request("GET", "@@search", "GET @@search token")
        token = forms.authenticator(page.text)
        if not token:
            return self.judge(page, False, target="SearchableText", detail="no token")
        response = attempt.request(
            "POST",
            "portal_catalog/manage_reindexIndex",
            "POST reindex SearchableText",
            data={"ids:list": "SearchableText", "_authenticator": token},
        )
        ok = (
            "Reindexing%20Performed" in response.url
            or "Reindexing Performed" in response.url
        )
        return self.judge(response, ok, target="SearchableText")

    def role_reindex(self):
        self.operation("reindex", self.reindex, retry=True)
        gevent.sleep(5)

    def import_item(self, attempt: Attempt) -> OpResult:
        title = f"Import {corpus.op_code(attempt.op, CODE_WORDS)} " + _words(
            self.rng, 3
        )
        attempt.intent(
            f"{corpus.BENCH}/{corpus.IMPORT}",
            {"title": title, "folder": f"{corpus.BENCH}/{corpus.IMPORT}"},
        )
        response = attempt.request(
            "POST",
            f"{corpus.BENCH}/{corpus.IMPORT}",
            "REST POST import",
            json={
                "@type": "Document",
                "title": title,
                "description": "Imported " + _words(self.rng, 8),
                "text": {
                    "content-type": "text/html",
                    "data": "<p>" + _words(self.rng, 80) + "</p>",
                },
            },
            headers=self.rest_headers(),
        )
        created = None
        if response.status_code == 201:
            created = response.json()["@id"].split(f"/{SITE}/", 1)[-1]
        return self.judge(
            response,
            created is not None,
            target=created or f"{corpus.BENCH}/{corpus.IMPORT}",
            values={"title": title, "folder": f"{corpus.BENCH}/{corpus.IMPORT}"},
        )

    def role_import(self):
        self.operation("import", self.import_item, retry=True)


class Steps(LoadTestShape):
    """The steps of the closed workload: each step holds a number of users
    for some seconds. The users of a step stay in the next one. The
    background users of a scenario come on top of the users of the step."""

    use_common_options = False

    def tick(self):
        if not STEPS:
            return None
        elapsed = self.get_run_time()
        total = 0.0
        for index, (users, seconds) in enumerate(STEPS):
            total += seconds
            if elapsed < total:
                _state["step"] = index
                extra = len(SCENARIO.background)
                return users + extra, max(users + extra, 1) * 4.0
        return None


@events.quitting.add_listener
def _close(environment, **kwargs):
    LEDGER.close()
