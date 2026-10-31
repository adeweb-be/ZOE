"""The commit and the tree that a result of the replay records, as every
harness records them (ADR 0054, section 4)."""

import os
import sys

import pytest

from replay import servers

HEAD = "a" * 40
# A git whose HEAD is HEAD, whose tracked files hold no change, and whose
# tree holds one untracked file.
FAKE_GIT = f"""\
import sys
args = sys.argv[1:]
if args[:1] == ["-C"]:
    args = args[2:]
if args[:2] == ["rev-parse", "HEAD"]:
    print("{HEAD}")
elif args[:2] == ["status", "--porcelain"]:
    if "--untracked-files=no" not in args:
        print("?? untracked.txt")
else:
    sys.exit(1)
"""


@pytest.fixture
def git(tmp_path, monkeypatch):
    """Put the fake git first on PATH."""
    bin_ = tmp_path / "bin"
    bin_.mkdir()
    script = tmp_path / "git.py"
    script.write_text(FAKE_GIT)
    (bin_ / "git").write_text(f'#!/bin/sh\nexec "{sys.executable}" "{script}" "$@"\n')
    (bin_ / "git").chmod(0o755)
    monkeypatch.setenv("PATH", f"{bin_}{os.pathsep}{os.environ['PATH']}")
    monkeypatch.delenv("ZOE_BENCH_COMMIT", raising=False)


def test_an_untracked_file_does_not_make_the_tree_dirty(git):
    found = servers.software()
    assert (found["commit"], found["dirty"]) == (HEAD, False)


def test_the_commit_is_zoe_bench_commit_and_another_tree_is_no_record_of_it(
    git, monkeypatch
):
    monkeypatch.setenv("ZOE_BENCH_COMMIT", "b" * 40)
    found = servers.software()
    assert (found["commit"], found["dirty"]) == ("b" * 40, None)

    monkeypatch.setenv("ZOE_BENCH_COMMIT", HEAD)
    found = servers.software()
    assert (found["commit"], found["dirty"]) == (HEAD, False)
