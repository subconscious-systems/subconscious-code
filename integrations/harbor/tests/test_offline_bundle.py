from __future__ import annotations

import hashlib
import json
import subprocess
import sys
from pathlib import Path

from subconscious_harbor import offline
from subconscious_harbor.offline import read_bundle_manifest

BUILDER = Path(__file__).parents[1] / "build-offline-bundle.py"


def build(payload: Path, output: Path) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        [sys.executable, str(BUILDER), str(payload), str(output)],
        check=False,
        capture_output=True,
        text=True,
    )


def test_builder_is_deterministic_and_manifests_every_file(tmp_path):
    payload = tmp_path / "payload"
    (payload / "agents").mkdir(parents=True)
    (payload / "agents" / "sc").write_bytes(b"binary")
    (payload / "python" / "wheels").mkdir(parents=True)
    wheel = payload / "python" / "wheels" / "mini_swe_agent-1-py3-none-any.whl"
    wheel.write_bytes(b"wheel")
    first = tmp_path / "first.tar.gz"
    second = tmp_path / "second.tar.gz"

    assert build(payload, first).returncode == 0
    assert build(payload, second).returncode == 0

    assert hashlib.sha256(first.read_bytes()).digest() == hashlib.sha256(
        second.read_bytes()
    ).digest()
    manifest = read_bundle_manifest(first)
    assert manifest["files"] == {
        "payload/agents/sc": {
            "bytes": 6,
            "sha256": hashlib.sha256(b"binary").hexdigest(),
        },
        "payload/python/wheels/mini_swe_agent-1-py3-none-any.whl": {
            "bytes": 5,
            "sha256": hashlib.sha256(b"wheel").hexdigest(),
        },
    }
    result = json.loads(build(payload, tmp_path / "third.tar.gz").stdout)
    assert len(result["sha256"]) == 64


def test_builder_rejects_git_metadata(tmp_path):
    payload = tmp_path / "payload"
    (payload / "agents" / ".git").mkdir(parents=True)
    (payload / "agents" / ".git" / "config").write_text("upstream data")

    result = build(payload, tmp_path / "bundle.tar.gz")

    assert result.returncode != 0
    assert "refusing to bundle Git metadata" in result.stderr


def test_runtime_verifies_and_extracts_built_bundle(tmp_path, monkeypatch):
    payload = tmp_path / "payload"
    (payload / "agents").mkdir(parents=True)
    agent_binary = payload / "agents" / "sc"
    agent_binary.write_bytes(b"verified binary")
    agent_binary.chmod(0o755)
    bundle = tmp_path / "bundle.tar.gz"
    assert build(payload, bundle).returncode == 0

    root = tmp_path / "runtime"
    monkeypatch.setattr(offline, "OFFLINE_ROOT", str(root))
    command = offline.OfflineRuntime._extract_command(
        str(bundle), hashlib.sha256(bundle.read_bytes()).hexdigest()
    )
    result = subprocess.run(
        ["bash", "-c", command],
        check=False,
        capture_output=True,
        text=True,
    )

    assert result.returncode == 0, result.stderr
    assert (root / "payload" / "agents" / "sc").read_bytes() == b"verified binary"
    assert (root / "payload" / "agents" / "sc").stat().st_mode & 0o111
