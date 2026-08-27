from __future__ import annotations

import hashlib
import json
from pathlib import Path

import pytest
from harbor.environments.base import BaseEnvironment, ExecResult
from harbor.models.agent.context import AgentContext

from subconscious_harbor.agent import SubconsciousCode


class FakeEnvironment(BaseEnvironment):
    def __init__(self) -> None:
        self.calls: list[dict] = []
        self.uploads: list[tuple[str, str]] = []

    @property
    def type(self):
        return "docker"

    def _validate_definition(self):
        pass

    async def exec(self, command, cwd=None, env=None, timeout_sec=None, user=None):
        self.calls.append(
            {"command": command, "cwd": cwd, "env": dict(env or {}), "user": user}
        )
        return ExecResult(stdout="", stderr="", return_code=0)

    async def start(self, *args, **kwargs):
        pass

    async def stop(self, *args, **kwargs):
        pass

    async def upload_file(self, source, destination, *args, **kwargs):
        self.uploads.append((str(source), str(destination)))

    async def download_file(self, *args, **kwargs):
        pass

    async def upload_dir(self, *args, **kwargs):
        pass

    async def download_dir(self, *args, **kwargs):
        pass

    async def is_dir(self, *args, **kwargs):
        return True

    async def is_file(self, *args, **kwargs):
        return True


def make_agent(logs_dir: Path, **kwargs) -> SubconsciousCode:
    return SubconsciousCode(
        logs_dir=logs_dir,
        model_name="openai/subconscious/glm-5.2",
        extra_env={
            "OPENAI_API_KEY": "test-key",
            "OPENAI_API_BASE": "https://api.subconscious.dev/v1",
        },
        **kwargs,
    )


@pytest.mark.asyncio
async def test_run_forwards_model_endpoint_and_report(tmp_path):
    agent = make_agent(tmp_path, max_tokens=4096, request_gzip=True)
    environment = FakeEnvironment()

    await agent.run("fix the issue", environment, AgentContext())

    call = environment.calls[-1]
    assert "sc --dangerously-skip-permissions" in call["command"]
    assert "--model subconscious/glm-5.2" in call["command"]
    assert "--benchmark-report /logs/agent/subconscious-code-report.json" in call["command"]
    assert "--benchmark-trajectory /logs/agent/trajectory.json" in call["command"]
    assert agent.SUPPORTS_ATIF is True
    assert call["env"]["SC_API_KEY"] == "test-key"
    assert call["env"]["SC_BASE_URL"] == "https://api.subconscious.dev/v1"
    assert call["env"]["SC_MAX_TOKENS"] == "4096"
    assert call["env"]["SC_REQUEST_GZIP"] == "true"
    assert call["env"]["SC_SANDBOX"] == "true"
    assert call["env"]["SC_SANDBOX_NET"] == "false"
    assert "no_progress" in call["command"]
    assert "incomplete" in call["command"]
    assert "PIPESTATUS[0]" in call["command"]
    assert "harness_process_exit_code" in call["command"]
    assert "rm -f /logs/agent/subconscious-code.txt" in call["command"]
    assert "SC_HARBOR_RUN_ID" in call["command"]
    assert "SC_HARBOR_INSTALLED_BINARY_SHA256" in call["command"]
    assert "stale benchmark report run id" in call["command"]
    assert "benchmark source hash mismatch" in call["command"]
    assert "benchmark artifact hash mismatch" in call["command"]
    assert "benchmark revision mismatch" in call["command"]


@pytest.mark.asyncio
async def test_prompt_is_shell_quoted(tmp_path):
    agent = make_agent(tmp_path)
    environment = FakeEnvironment()

    await agent.run("don't expand $HOME; `touch /tmp/nope`", environment, AgentContext())

    command = environment.calls[-1]["command"]
    assert "'don'\"'\"'t expand $HOME; `touch /tmp/nope`'" in command
    assert environment.calls[-1]["env"]["SC_MAX_TOKENS"] == "8192"
    assert environment.calls[-1]["env"]["SC_TURN_TIMEOUT_MS"] == "540000"


def test_report_populates_native_harbor_metrics(tmp_path):
    report = {
        "schema_version": 1,
        "usage": {
            "input_tokens": 120,
            "cached_input_tokens": 80,
            "output_tokens": 30,
            "total_tokens": 150,
        },
        "cost_usd": 0.0123,
        "request_count": 4,
    }
    (tmp_path / "subconscious-code-report.json").write_text(json.dumps(report))
    agent = make_agent(tmp_path)
    context = AgentContext()

    agent.populate_context_post_run(context)

    assert context.n_input_tokens == 120
    assert context.n_cache_tokens == 80
    assert context.n_output_tokens == 30
    assert context.cost_usd == 0.0123
    assert context.metadata == {"subconscious_code": report}


def test_explicit_install_artifacts_outrank_prebaked_binary(tmp_path):
    agent = make_agent(
        tmp_path,
        binary_url="https://example.test/sc-x86_64-unknown-linux-gnu.tar.gz",
        binary_sha256="a" * 64,
    )
    command = agent._install_command()

    assert "SC_HARBOR_REUSE_EXISTING" in command
    assert "SC_HARBOR_BINARY_SHA256" in command
    assert "cargo install --locked" in command
    assert command.index('if [ -n "$SC_HARBOR_BINARY_URL" ]') < command.index(
        'elif [ -n "$SC_HARBOR_SOURCE_ARCHIVE" ]'
    )
    assert command.index('elif [ -n "$SC_HARBOR_SOURCE_ARCHIVE" ]') < command.index(
        'elif [ "$SC_HARBOR_REUSE_EXISTING" = 1 ]'
    )
    assert 'sc --version\nelse' in command


def test_remote_binary_requires_a_checksum(tmp_path):
    with pytest.raises(ValueError, match="binary_sha256 is required"):
        make_agent(tmp_path, binary_url="https://example.test/sc")


@pytest.mark.asyncio
async def test_direct_binary_upload_skips_package_manager(tmp_path):
    binary = tmp_path / "sc"
    binary.write_bytes(b"static-binary")
    agent = make_agent(tmp_path, binary_path=binary)
    environment = FakeEnvironment()

    await agent.install(environment)

    assert environment.uploads == [(str(binary.resolve()), "/tmp/subconscious-code-sc")]
    assert len(environment.calls) == 1, "direct binary should skip root package setup"
    install_call = environment.calls[0]
    assert install_call["env"]["SC_HARBOR_BINARY_PATH"] == "/tmp/subconscious-code-sc"
    assert install_call["env"]["SC_HARBOR_ARTIFACT_SHA256"] == hashlib.sha256(
        b"static-binary"
    ).hexdigest()
    assert 'install -m 0755 "$SC_HARBOR_BINARY_PATH"' in install_call["command"]


@pytest.mark.asyncio
async def test_install_uploads_exact_source_bundle(tmp_path):
    source_archive = tmp_path / "subconscious-code-source.tar.gz"
    source_archive.write_bytes(b"archive")
    agent = make_agent(tmp_path, source_archive=source_archive)
    environment = FakeEnvironment()

    await agent.install(environment)

    assert environment.uploads == [
        (str(source_archive.resolve()), "/tmp/subconscious-code-source.tar.gz")
    ]
    install_call = environment.calls[-1]
    assert (
        install_call["env"]["SC_HARBOR_SOURCE_ARCHIVE"]
        == "/tmp/subconscious-code-source.tar.gz"
    )
    assert install_call["env"]["SC_HARBOR_SOURCE_SHA256"] == hashlib.sha256(
        b"archive"
    ).hexdigest()
    assert 'tar -xzf "$SC_HARBOR_SOURCE_ARCHIVE"' in install_call["command"]
