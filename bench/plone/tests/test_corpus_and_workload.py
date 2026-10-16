"""The plan of the site and the roles of the users are deterministic, and
the users of the scenarios without deliberate contention never share a
Document."""

from plonebench import corpus, workload


def manifest_of(spec: corpus.SiteSpec) -> dict:
    plan = corpus.plan_site(spec)
    return {
        "spec": spec.to_json(),
        "site": "Plone",
        "items": [
            {
                "path": i.path,
                "portal_type": i.portal_type,
                "title": i.title,
                "state": i.state
                if i.portal_type in ("Document", "News Item")
                else "published",
                "pool": i.pool,
                "section": i.section,
                "size": i.size,
                "versions": 1,
                "workflow_history": 1,
            }
            for i in plan.items
        ],
        "users": corpus.user_ids(spec),
        "search_terms": corpus.search_terms(plan),
        "subjects": plan.words[50:90],
    }


def test_the_same_spec_gives_the_same_plan():
    a = corpus.plan_site(corpus.PRESETS["small"])
    b = corpus.plan_site(corpus.PRESETS["small"])
    assert [(i.path, i.title, i.text, i.state) for i in a.items] == [
        (i.path, i.title, i.text, i.state) for i in b.items
    ]
    assert corpus.file_bytes(a.spec, "x", 100) == corpus.file_bytes(a.spec, "x", 100)


def test_another_seed_gives_another_text():
    a = corpus.plan_site(corpus.SiteSpec(seed=1))
    b = corpus.plan_site(corpus.SiteSpec(seed=2))
    assert [i.path for i in a.items] == [i.path for i in b.items]
    assert [i.title for i in a.items] != [i.title for i in b.items]


def test_the_shape_follows_the_spec():
    spec = corpus.SiteSpec(
        sections=2, depth=1, fanout=2, documents=3, news=1, files=1, hot=2, shared=4
    )
    counts = corpus.plan_site(spec).counts()
    folders_per_section = 1 + 2
    assert counts["Document"] == 2 * folders_per_section * 3 + 2 + 4
    assert counts["News Item"] == 2 * folders_per_section
    assert counts["File"] == 2 * folders_per_section
    assert counts["Collection"] == 2
    # Sections, their subfolders, bench and its four folders.
    assert counts["Folder"] == 2 * folders_per_section + 1 + 4


def test_the_presets_grow():
    small = sum(corpus.plan_site(corpus.PRESETS["small"]).counts().values())
    medium = sum(corpus.plan_site(corpus.PRESETS["medium"]).counts().values())
    assert medium > 5 * small


def test_roles_follow_the_weights_on_every_prefix():
    mix = {"a": 3, "b": 1}
    roles = workload.assign_roles(mix, 8)
    assert roles.count("a") == 6 and roles.count("b") == 2
    assert roles[:4].count("a") == 3


def test_roles_of_one_weight():
    assert workload.assign_roles({"edit": 1}, 3) == ["edit", "edit", "edit"]


def test_edit_users_never_share_a_document():
    spec = corpus.PRESETS["small"]
    targets = workload.Targets(manifest_of(spec))
    users = 16
    owned = [
        {d["path"] for d in targets.own_docs(rank, corpus.EDIT, users)}
        for rank in range(users)
    ]
    for i in range(users):
        assert owned[i], f"user {i} owns no Document"
        for j in range(i + 1, users):
            assert not owned[i] & owned[j]


def test_an_editor_owns_documents_of_its_own_section():
    spec = corpus.PRESETS["small"]
    targets = workload.Targets(manifest_of(spec))
    for rank in range(8):
        _, section = targets.editor(rank)
        for doc in targets.own_docs(rank, corpus.WORKFLOW, 8):
            assert doc["section"] == section


def test_field_pairs_write_one_field_each():
    targets = workload.Targets(manifest_of(corpus.PRESETS["small"]))
    pairs = [targets.pair_doc(rank) for rank in range(16)]
    assert len(set(pairs)) == 16
    assert pairs[0][0] == pairs[1][0] and {pairs[0][1], pairs[1][1]} == {
        "title",
        "description",
    }


def test_browsing_never_reads_the_workflow_pool():
    targets = workload.Targets(manifest_of(corpus.PRESETS["small"]))
    assert all(i["pool"] != corpus.WORKFLOW for i in targets.browsable())
    assert all(i["state"] == "published" for i in targets.browsable())


def test_steps():
    assert workload.steps_from_env("1:60,4:30") == [(1, 60.0), (4, 30.0)]
    assert workload.steps_from_env("") == []
