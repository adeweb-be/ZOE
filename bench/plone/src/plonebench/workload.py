"""The scenarios: which roles the simulated users take, and their targets.

Pure functions of the manifest and the user index, so the tests can check
them without Locust, and the report can name the mix of each run.
"""

from __future__ import annotations

import json
import os
from dataclasses import dataclass

from plonebench import corpus

# A role: the account it logs in with, and whether it goes through the
# rendered pages of Plone (`ui`) or through plone.restapi (`rest`).
ROLES = {
    "browse_anon": ("anonymous", "ui"),
    "browse_member": (corpus.MEMBER, "ui"),
    "search": (corpus.MEMBER, "ui"),
    "edit": (corpus.EDITOR, "ui"),
    "create_spread": (corpus.EDITOR, "ui"),
    "create_shared": (corpus.EDITOR, "ui"),
    "workflow": (corpus.EDITOR, "ui"),
    "edit_shared": (corpus.EDITOR, "ui"),
    "patch_fields": (corpus.EDITOR, "rest"),
    "patch_same": (corpus.EDITOR, "rest"),
    "warm": (corpus.EDITOR, "ui"),
    # The background roles: one user each, beside the mix.
    "reindex": (corpus.MAINTAINER, "ui"),
    "import": (corpus.IMPORTER, "rest"),
}

BACKGROUND = ("reindex", "import")


@dataclass(frozen=True)
class Scenario:
    name: str
    mix: dict[str, int]
    background: tuple[str, ...] = ()
    description: str = ""
    # True when the scenario writes. A write scenario starts from a fresh
    # copy of the fixture.
    writes: bool = True


SCENARIOS = {
    "browse": Scenario(
        "browse",
        {"browse_anon": 1, "browse_member": 1},
        description="rendered pages: folders, documents, news, files, breadcrumbs; "
        "anonymous and logged-in members",
        writes=False,
    ),
    "catalog": Scenario(
        "catalog",
        {"search": 1},
        description="@@search with text, filters, sorting and pages; collections; "
        "REST @search with metadata",
        writes=False,
    ),
    "edit": Scenario(
        "edit",
        {"edit": 1},
        description="the edit form of Plone on Documents, each user its own Documents",
    ),
    "create-spread": Scenario(
        "create-spread",
        {"create_spread": 1},
        description="the add form of Plone, each user in folders of its own section",
    ),
    "create-shared": Scenario(
        "create-shared",
        {"create_shared": 1},
        description="the add form of Plone, every user in the one folder bench/inbox",
    ),
    "workflow": Scenario(
        "workflow",
        {"workflow": 1},
        description="publish and retract through the workflow menu, with an "
        "anonymous visibility check",
    ),
    "mixed": Scenario(
        "mixed",
        {
            "browse_anon": 8,
            "browse_member": 3,
            "search": 4,
            "edit": 2,
            "create_spread": 1,
            "workflow": 1,
            "create_shared": 1,
        },
        description="a mix of interactive traffic, weights in the mix",
    ),
    "mixed-background": Scenario(
        "mixed-background",
        {
            "browse_anon": 8,
            "browse_member": 3,
            "search": 4,
            "edit": 2,
            "create_spread": 1,
            "workflow": 1,
            "create_shared": 1,
        },
        background=BACKGROUND,
        description="the mixed scenario, with one reindex of SearchableText in a loop "
        "and one REST import in a loop beside it",
    ),
    "contention-folder": Scenario(
        "contention-folder",
        {"edit_shared": 1},
        description="the edit form, each user its own Document, all Documents in one "
        "folder (shared folder and catalog structures)",
    ),
    "contention-fields": Scenario(
        "contention-fields",
        {"patch_fields": 1},
        description="REST PATCH: two users on each Document, one writes the title, "
        "the other the description",
    ),
    "contention-same": Scenario(
        "contention-same",
        {"patch_same": 1},
        description="REST PATCH: every user writes the title of one of the hot "
        "Documents (incompatible edits of one field)",
    ),
    "warmup": Scenario(
        "warmup",
        {"browse_anon": 2, "browse_member": 1, "search": 1, "warm": 1},
        description="the warmup: pages, searches, and the edit and add forms "
        "without a save",
        writes=False,
    ),
}


def assign_roles(mix: dict[str, int], count: int) -> list[str]:
    """Return the role of each of `count` users: a smooth weighted round
    robin, so any prefix of the list follows the weights as closely as a
    whole number of users can."""
    current = {role: 0 for role in mix}
    total = sum(mix.values())
    roles = []
    for _ in range(count):
        for role, weight in mix.items():
            current[role] += weight
        chosen = max(current, key=lambda role: (current[role], -list(mix).index(role)))
        current[chosen] -= total
        roles.append(chosen)
    return roles


def ordinal(roles: list[str], index: int) -> int:
    """Return the rank of user `index` among the users of its role."""
    role = roles[index]
    return sum(1 for r in roles[:index] if r == role)


@dataclass
class Targets:
    manifest: dict

    @classmethod
    def load(cls, path: str) -> Targets:
        with open(path) as file:
            return cls(json.load(file))

    @property
    def spec(self) -> corpus.SiteSpec:
        return corpus.SiteSpec.from_json(self.manifest["spec"])

    def items(self, **where) -> list[dict]:
        return [
            item
            for item in self.manifest["items"]
            if all(item.get(key) == value for key, value in where.items())
        ]

    def editor(self, rank: int) -> tuple[str, int]:
        """Return the account and the section of the editor of `rank`."""
        editors = self.manifest["users"][corpus.EDITOR]
        account = editors[rank % len(editors)]
        return account, corpus.editor_section(self.spec, editors.index(account))

    def browsable(self) -> list[dict]:
        """The items an anonymous visitor can read and that no scenario
        changes the state of."""
        return [
            item
            for item in self.manifest["items"]
            if item["pool"] != corpus.WORKFLOW
            and item["portal_type"] != "Collection"
            and (item["state"] in (corpus.PUBLISHED, None))
            and not item["path"].startswith(corpus.BENCH)
        ]

    def section_docs(self, section: int, pool: str) -> list[dict]:
        return [
            item
            for item in self.manifest["items"]
            if item["portal_type"] == "Document"
            and item["section"] == section
            and item["pool"] == pool
        ]

    def own_docs(self, rank: int, pool: str, users: int) -> list[dict]:
        """The Documents of `pool` that the editor of `rank` owns: those of
        its section, split among the users of the role in that section.
        Two ranks never share a Document while `users` is at least the
        number of users of the role."""
        _, section = self.editor(rank)
        sections = self.spec.sections
        per_section = max(1, -(-users // sections))
        docs = self.section_docs(section, pool)
        return docs[(rank // sections) % per_section :: per_section]

    def section_folders(self, section: int) -> list[str]:
        return [
            item["path"]
            for item in self.manifest["items"]
            if item["portal_type"] == "Folder" and item["section"] == section
        ]

    def shared_doc(self, rank: int) -> str:
        return f"{corpus.BENCH}/{corpus.SHARED}/doc-{rank % self.spec.shared:03d}"

    def pair_doc(self, rank: int) -> tuple[str, str]:
        """The Document and the field of a user of contention-fields: users
        2k and 2k+1 share Document k; the first writes the title, the
        second the description."""
        doc = f"{corpus.BENCH}/{corpus.SHARED}/doc-{(rank // 2) % self.spec.shared:03d}"
        return doc, ("title" if rank % 2 == 0 else "description")

    def hot_doc(self, rank: int) -> str:
        return f"{corpus.BENCH}/{corpus.HOT}/doc-{rank % self.spec.hot:03d}"


def steps_from_env(value: str | None) -> list[tuple[int, float]]:
    """Parse PLONEBENCH_STEPS: `users:seconds,users:seconds,...`."""
    if not value:
        return []
    steps = []
    for part in value.split(","):
        users, seconds = part.split(":")
        steps.append((int(users), float(seconds)))
    return steps


def env(name: str, default: str | None = None) -> str | None:
    return os.environ.get(f"PLONEBENCH_{name}", default)
