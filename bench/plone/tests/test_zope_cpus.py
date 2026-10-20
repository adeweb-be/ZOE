"""Unit tests of the clock and the throttle time of the Zope CPUs: the
reads of sysfs, the values of the steps, the line of the log of a run,
the line `done` of the showcase and the runs of the B5 summary. The tests
run no benchmark."""

import json
import pathlib
import sys

import jsonschema
import pytest

from plonebench import report, runner, sampler, showcase, summary

RUNS = pathlib.Path(__file__).parent / "data" / "runs"
ZEO = RUNS / "20260101T000000Z-edit-zeo-r0"
ZOE = RUNS / "20260101T000500Z-edit-zoe-native-qi-kv-r0"


def write(path, text):
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(text)


@pytest.fixture
def sysfs(tmp_path, monkeypatch):
    """A fake /sys/devices/system/cpu."""
    root = tmp_path / "cpu"
    monkeypatch.setattr(sampler, "SYS_CPU", root)
    return root


def test_the_cpus_of_taskset_lists():
    assert sampler.cpu_numbers(("1", "3,4", "6-8", "3")) == (1, 3, 4, 6, 7, 8)
    # A part of another form gives no CPU.
    assert sampler.cpu_numbers(("0-7:2", "9")) == (9,)
    assert sampler.cpu_numbers(()) == ()


def test_the_sampler_reads_the_clock_and_the_throttle_time_of_each_cpu(sysfs):
    write(sysfs / "cpu1" / "cpufreq" / "cpuinfo_avg_freq", "4500000\n")
    write(sysfs / "cpu1" / "cpufreq" / "scaling_cur_freq", "4400000\n")
    write(sysfs / "cpu1" / "thermal_throttle" / "core_throttle_total_time_ms", "17\n")
    # No measured clock: the clock of the governor.
    write(sysfs / "cpu3" / "cpufreq" / "scaling_cur_freq", "4200000\n")
    # A measured clock that does not read.
    write(sysfs / "cpu6" / "cpufreq" / "cpuinfo_avg_freq", "")
    write(sysfs / "cpu6" / "cpufreq" / "scaling_cur_freq", "800000\n")

    assert sampler.read_cpus((1, 3, 6, 8)) == {
        "clock_mhz": {"1": 4500.0, "3": 4200.0, "6": 800.0},
        "throttle_ms": {"1": 17},
    }


@pytest.mark.skipif(sys.platform == "darwin", reason="macOS has no sysfs")
def test_a_sample_holds_the_zope_cpus_only_when_the_sampler_has_them(sysfs, tmp_path):
    write(sysfs / "cpu1" / "cpufreq" / "cpuinfo_avg_freq", "4500000\n")
    with_cpus = sampler.Sampler(tmp_path / "a.jsonl", None, zope_cpus=(1,))
    assert with_cpus.sample()["zope_cpus"]["clock_mhz"] == {"1": 4500.0}
    assert "zope_cpus" not in sampler.Sampler(tmp_path / "b.jsonl", None).sample()


def sample(t, clock, throttle):
    """A sample of two Zope CPUs."""
    return {
        "t": t,
        "cpu": {"user": t, "idle": 0},
        "disk": None,
        "processes": {},
        "zope_cpus": {
            "clock_mhz": {"1": clock, "3": clock + 100},
            "throttle_ms": {"1": throttle, "3": 2 * throttle},
        },
    }


def test_the_throttle_time_is_the_change_in_the_steps_summed_over_the_cpus():
    # One sample a second. From t = 4, the core of CPU 1 is throttled
    # 100 ms a second, and the core of CPU 3 200 ms a second. The samples
    # outside the two windows, such as the trim, read 400 and 500 MHz.
    windows = [(2, 5), (7, 10)]
    samples = [
        sample(
            t,
            4000 if any(a <= t <= b for a, b in windows) else 400,
            max(0, t - 4) * 100,
        )
        for t in range(11)
    ]

    clock, throttle = sampler.zope_cpus(samples, windows)

    # The mean of 4000 and 4100 MHz: the samples outside the windows do
    # not count.
    assert clock == 4050.0
    # From t = 4 to 5: 300 ms. From t = 7 to 10: 900 ms.
    assert throttle == 1.2

    step = sampler.summarize(samples, 7, 10)
    assert step["zope_clock_mhz"] == 4050.0
    assert step["zope_throttle_seconds"] == 0.9


def test_a_machine_without_the_files_gives_null_never_0():
    samples = [
        {"t": t, "cpu": {"user": t}, "disk": None, "processes": {}} for t in (0, 1, 2)
    ]
    assert sampler.zope_cpus(samples, [(0, 2)]) == (None, None)
    step = sampler.summarize(samples, 0, 2)
    assert "zope_clock_mhz" not in step
    assert "zope_throttle_seconds" not in step

    # A clock without the throttle file.
    for item in samples:
        item["zope_cpus"] = {"clock_mhz": {"1": 3000.0}, "throttle_ms": {}}
    assert sampler.zope_cpus(samples, [(0, 2)]) == (3000.0, None)


def test_the_log_of_a_run_gives_the_clock_and_the_throttle_time():
    result = {"stack": {"settings": {"zope_cpus": ["1", "3", "6", "8"]}}}
    line = runner.zope_cpus_line(
        {**result, "zope_clock_mhz": 4321.4, "zope_throttle_seconds": 2.5}
    )
    assert line == (
        "zope CPUs 1;3;6;8: clock mean 4321 MHz, throttle 2.5 s during the steps"
    )
    unknown = runner.zope_cpus_line(
        {**result, "zope_clock_mhz": None, "zope_throttle_seconds": None}
    )
    assert "MHz" not in unknown
    assert "clock unknown" in unknown and "throttle unknown" in unknown


def test_the_line_done_gives_the_clock_and_the_throttle_time():
    run = {"id": "b5-xlarge-edit-zeo", "kind": "b5"}
    record = {"verdict": "pass", "zope_clock_mhz": 4321.4}
    assert showcase._told(run, {**record, "zope_throttle_seconds": 0.0}) == (
        ": verification pass, zope clock 4321 MHz, zope throttle 0.0 s"
    )
    # A run without the throttle file, and a run of a machine without
    # either.
    assert showcase._told(run, {**record, "zope_throttle_seconds": None}) == (
        ": verification pass, zope clock 4321 MHz"
    )
    assert showcase._told(run, {"verdict": "pass"}) == ": verification pass"


def test_the_summary_records_the_throttle_time_of_each_run(monkeypatch):
    monkeypatch.setenv("ZOE_BENCH_MACHINE", "reference")
    zeo, zoe = report.load([ZEO, ZOE])
    zeo["zope_throttle_seconds"] = 1.5

    document = summary.document([zeo, zoe])

    throttle = {run["stack"]: run["zope_throttle_seconds"] for run in document["runs"]}
    assert throttle == {"zeo": 1.5, "zoe-native-qi-kv": None}
    # No run recorded the clock: the runs keep the keys of today.
    assert all("zope_clock_mhz" not in run for run in document["runs"])

    # A throttle time is never below 0.
    document["runs"][0]["zope_throttle_seconds"] = -0.5
    with pytest.raises(jsonschema.ValidationError):
        jsonschema.validate(document, json.loads(summary.SCHEMA.read_text()))
