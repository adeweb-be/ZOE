"""Acceptance test of the Tier 1 deployment files (N9, N10, N15).

`deploy/tier1/` holds the Kubernetes manifests of one `zoe serve` as a
StatefulSet, as the specification lists them:

- a `StatefulSet` with `replicas: 1`, `podManagementPolicy: OrderedReady`
  and a `volumeClaimTemplate` with a storage class, running the image
  `ghcr.io/adeweb-be/zoe` with `serve`, its configuration from a
  `ConfigMap` through `ZOE_CONFIG`, a readiness probe on `/readyz` and a
  liveness probe on `/healthz` of the admin port, and a
  `terminationGracePeriodSeconds` above its `shutdown-timeout`;
- a `Service` on the ZEO port and a second one on the admin port, and a
  `ServiceMonitor` that scrapes `/metrics` there;
- a `PodDisruptionBudget` with `maxUnavailable: 0`;
- a `CronJob` that runs `zoe backup --admin` against the server, then
  restores the dumps into a scratch directory with `zoe restore` and runs
  `zoe verify` on the copy. It waits for phase 7: the phase 2 manifests
  ship no backup CronJob.

Every object has the label `app.kubernetes.io/name: zoe`. Beside them,
`deploy/systemd/zoe.service` and `deploy/supervisor/zoe.conf` run the
binary on a host, and `deploy/grafana/zoe.json` is the dashboard of the
metrics. No file holds a credential, a host name or a site name.

Run it with `just deploy check`.
"""

import json
import re
from pathlib import Path

import pytest
import yaml

DEPLOY = Path(__file__).resolve().parents[1]
TIER1 = DEPLOY / "tier1"
NAME = {"app.kubernetes.io/name": "zoe"}


def objects():
    found = []
    for path in sorted(TIER1.glob("*.yaml")):
        found.extend(doc for doc in yaml.safe_load_all(path.read_text()) if doc)
    return found


def one(kind):
    matches = [obj for obj in objects() if obj["kind"] == kind]
    assert len(matches) >= 1, f"no {kind}"
    return matches


def seconds(value):
    """Seconds of a value such as 30, '30', '30s' or '2m'."""
    match = re.fullmatch(r"(\d+)([sm]?)", str(value))
    assert match, value
    return int(match[1]) * (60 if match[2] == "m" else 1)


def server_container():
    (statefulset,) = one("StatefulSet")
    containers = statefulset["spec"]["template"]["spec"]["containers"]
    return statefulset, next(
        c for c in containers if c["image"].startswith("ghcr.io/adeweb-be/zoe")
    )


def test_every_object_has_the_name_label():
    for obj in objects():
        labels = obj["metadata"].get("labels", {})
        assert labels.get("app.kubernetes.io/name") == "zoe", obj["metadata"]["name"]


def test_the_statefulset():
    statefulset, container = server_container()
    spec = statefulset["spec"]
    assert spec["replicas"] == 1
    assert spec["podManagementPolicy"] == "OrderedReady"
    (claim,) = spec["volumeClaimTemplates"]
    assert claim["spec"].get("storageClassName")
    assert "serve" in container.get("args", []) + container.get("command", [])
    env = {e["name"]: e for e in container.get("env", [])}
    assert "ZOE_CONFIG" in env

    ports = {p["name"]: p["containerPort"] for p in container["ports"]}
    assert set(ports) >= {"zeo", "admin"}
    assert container["readinessProbe"]["httpGet"]["path"] == "/readyz"
    assert container["livenessProbe"]["httpGet"]["path"] == "/healthz"
    for probe in ("readinessProbe", "livenessProbe"):
        assert container[probe]["httpGet"]["port"] in ("admin", ports["admin"])


def test_the_grace_period_is_above_the_shutdown_timeout():
    statefulset, _ = server_container()
    grace = statefulset["spec"]["template"]["spec"]["terminationGracePeriodSeconds"]
    (configmap,) = [
        c for c in one("ConfigMap") if any(k.endswith(".toml") for k in c["data"])
    ]
    config = next(v for k, v in configmap["data"].items() if k.endswith(".toml"))
    match = re.search(r'^shutdown-timeout\s*=\s*"?(\w+)"?', config, re.MULTILINE)
    timeout = seconds(match[1]) if match else 30
    assert grace > timeout


def test_the_services_and_the_service_monitor():
    services = one("Service")
    ports = {p["name"] for s in services for p in s["spec"]["ports"]}
    assert {"zeo", "admin"} <= ports
    assert len(services) >= 2
    (monitor,) = one("ServiceMonitor")
    assert monitor["apiVersion"].startswith("monitoring.coreos.com/")
    endpoints = monitor["spec"]["endpoints"]
    assert any(
        e.get("path", "/metrics") == "/metrics" and e["port"] == "admin"
        for e in endpoints
    )


def test_the_disruption_budget_waits_for_a_manual_window():
    (budget,) = one("PodDisruptionBudget")
    assert budget["spec"]["maxUnavailable"] == 0


@pytest.mark.xfail(
    reason=(
        "the phase 2 manifests ship no backup CronJob: it comes with the "
        "backup of a store that the server holds open, in phase 7"
    ),
    strict=True,
)
def test_the_backup_job_backs_up_restores_and_verifies():
    (job,) = one("CronJob")
    pod = job["spec"]["jobTemplate"]["spec"]["template"]["spec"]
    script = json.dumps(pod)
    for step in ("backup", "--admin", "restore", "verify"):
        assert step in script, step


def test_the_host_units_stop_the_server_cleanly():
    unit = (DEPLOY / "systemd" / "zoe.service").read_text()
    assert re.search(r"^ExecStart=.*zoe serve", unit, re.MULTILINE)
    assert re.search(r"^KillSignal=SIGTERM", unit, re.MULTILINE)
    stop = re.search(r"^TimeoutStopSec=(\w+)", unit, re.MULTILINE)
    assert stop and seconds(stop[1]) > 30
    assert re.search(r"^User=zoe", unit, re.MULTILINE)
    assert re.search(r"^Restart=on-failure", unit, re.MULTILINE)

    conf = (DEPLOY / "supervisor" / "zoe.conf").read_text()
    assert "[program:zoe]" in conf
    assert re.search(r"^command=.*zoe serve", conf, re.MULTILINE)
    assert re.search(r"^stopsignal=TERM", conf, re.MULTILINE)
    wait = re.search(r"^stopwaitsecs=(\d+)", conf, re.MULTILINE)
    assert wait and int(wait[1]) > 30


def test_the_dashboard_plots_the_metrics_of_the_specification():
    dashboard = json.loads((DEPLOY / "grafana" / "zoe.json").read_text())
    text = json.dumps(dashboard)
    for metric in (
        "zoe_finish_latency_seconds",
        "zoe_fsync_batch_size",
        "zoe_lock_wait_seconds",
        "zoe_conflicts_resolved_total",
        "zoe_invalidation_queue_depth",
    ):
        assert metric in text, metric


def test_no_file_holds_a_secret():
    for path in DEPLOY.rglob("*"):
        if path.is_file() and path.suffix in {".yaml", ".service", ".conf", ".json"}:
            text = path.read_text().lower()
            for word in ("password:", "secret:", "token:", "-----begin"):
                assert word not in text, f"{path}: {word}"
