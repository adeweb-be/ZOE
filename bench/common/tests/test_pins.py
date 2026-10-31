"""The check of `just check` that the harnesses pin the forks of python/."""

import shutil

from benchcommon import pins
from benchcommon.servers import REPO


def test_the_harnesses_pin_the_forks_of_the_workspace():
    assert pins.differences() == []


def test_a_harness_with_another_pin_is_named(tmp_path):
    for path in (pins.WORKSPACE, *pins.HARNESSES):
        (tmp_path / path).parent.mkdir(parents=True)
        shutil.copy(REPO / path, tmp_path / path)
    harness = tmp_path / pins.HARNESSES[1]
    text = harness.read_text()
    rev = pins.sources(harness)["BTrees"]["rev"]
    harness.write_text(text.replace(rev, "0" * 40))

    (line,) = pins.differences(tmp_path)
    assert line.startswith(f"{pins.HARNESSES[1]}: BTrees is ")
    assert "0" * 40 in line
