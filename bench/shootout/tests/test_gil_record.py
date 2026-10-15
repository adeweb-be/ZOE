"""Unit tests of the GIL record: the startup module and its reader.

Each test starts a small Python process with the environment of a
zodbshootout run. A module named ZODB in `sys.modules` stands in for ZODB.
"""

import json
import subprocess
import sys
import sysconfig

from shootout import gil

FAKE_ZODB = "import sys, types; sys.modules['ZODB'] = types.ModuleType('ZODB')\n"
WARNING = (
    "import warnings\n"
    "warnings.warn(\n"
    "    \"The global interpreter lock (GIL) has been enabled to load module \"\n"
    "    \"'fake._ext', which has not declared that it can run safely without \"\n"
    "    \"the GIL.\",\n"
    "    RuntimeWarning,\n"
    ")\n"
)


def python(code, records, cwd):
    return subprocess.run(
        [sys.executable, "-c", code],
        env=gil.environ(records),
        cwd=cwd,
        capture_output=True,
        text=True,
        check=True,
        timeout=60,
    )


def records_in(directory):
    return [json.loads(path.read_text()) for path in directory.glob("*.json")]


def test_a_process_that_imported_zodb_writes_one_record(tmp_path):
    records = tmp_path / "gil"
    records.mkdir()
    python(FAKE_ZODB, records, tmp_path)
    [record] = records_in(records)
    assert set(record) == {"gil_enabled", "gil_enabled_by"}
    assert record["gil_enabled_by"] == []
    if not sysconfig.get_config_var("Py_GIL_DISABLED"):
        assert record["gil_enabled"] is True
    assert gil.read(records) == [record]


def test_a_process_without_zodb_writes_nothing(tmp_path):
    records = tmp_path / "gil"
    records.mkdir()
    python("pass", records, tmp_path)
    assert records_in(records) == []


def test_the_record_names_the_module_of_the_warning(tmp_path):
    records = tmp_path / "gil"
    records.mkdir()
    done = python(FAKE_ZODB + WARNING, records, tmp_path)
    [record] = records_in(records)
    assert record["gil_enabled_by"] == ["fake._ext"]
    # Python still shows the warning.
    assert "fake._ext" in done.stderr


def test_a_child_that_ends_with_os_exit_writes_its_own_record(tmp_path):
    records = tmp_path / "gil"
    records.mkdir()
    code = FAKE_ZODB + (
        "import os\n"
        "pid = os.fork()\n"
        "if pid == 0:\n"
        "    os._exit(0)\n"
        "os.waitpid(pid, 0)\n"
    )
    python(code, records, tmp_path)
    assert len(records_in(records)) == 2


def test_the_startup_directory_leaves_sys_path(tmp_path):
    records = tmp_path / "gil"
    records.mkdir()
    code = "import sys; print(repr(sys.path))"
    done = python(code, records, tmp_path)
    assert str(gil.STARTUP) not in done.stdout


def test_without_the_variable_the_startup_module_records_nothing(tmp_path):
    records = tmp_path / "gil"
    records.mkdir()
    env = gil.environ(records)
    del env[gil.VARIABLE]
    subprocess.run(
        [sys.executable, "-c", FAKE_ZODB],
        env=env,
        cwd=tmp_path,
        check=True,
        timeout=60,
    )
    assert records_in(records) == []


def test_pyperf_passes_the_variables_of_the_record_to_its_worker():
    assert set(gil.INHERIT) == {"PYTHONPATH", gil.VARIABLE}
