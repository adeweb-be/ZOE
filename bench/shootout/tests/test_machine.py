"""The machine of a result: one Mac files its results under one name, the
model of its CPU, as the Plone benchmark records it."""

import subprocess
import sys

from shootout import environment

MODEL = "Apple M2 Pro"


def test_a_mac_files_its_results_under_the_model_of_its_cpu(tmp_path, monkeypatch):
    run = subprocess.run

    def sysctl(command, *args, **kwargs):
        if command[:3] == ["sysctl", "-n", "machdep.cpu.brand_string"]:
            return subprocess.CompletedProcess(command, 0, MODEL + "\n", "")
        return run(command, *args, **kwargs)

    monkeypatch.setattr(subprocess, "run", sysctl)
    monkeypatch.setattr(sys, "platform", "darwin")
    monkeypatch.delenv("ZOE_BENCH_MACHINE", raising=False)

    found = environment.machine(tmp_path)

    assert found["cpu"] == MODEL
    assert found["name"] == "apple-m2-pro"
