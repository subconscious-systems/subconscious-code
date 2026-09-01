"""Run Subconscious Code as a native Harbor benchmark agent."""

from __future__ import annotations

import hashlib
import json
import os
import shlex
from pathlib import Path
from typing import Any, ClassVar

from harbor.agents.installed.base import (
    BaseInstalledAgent,
    EnvVar,
    with_prompt_template,
)
from harbor.environments.base import BaseEnvironment
from harbor.models.agent.context import AgentContext
from harbor.models.trial.paths import EnvironmentPaths

from .offline import OFFLINE_ROOT, OfflineRuntime


class SubconsciousCode(BaseInstalledAgent):
    """Harbor adapter for the headless `sc` coding-agent harness.

    Explicit artifacts always outrank an `sc` already in the task image: use a
    release binary, build the supplied source archive, reuse an existing binary,
    or finally build a pinned git revision. The agent run itself writes a
    privacy-safe benchmark report that Harbor converts into native token,
    cache, cost, and metadata fields.
    """

    SUPPORTS_ATIF: bool = True
    _OUTPUT_FILENAME = "subconscious-code.txt"
    _REPORT_FILENAME = "subconscious-code-report.json"
    _TRAJECTORY_FILENAME = "trajectory.json"
    _REMOTE_BINARY = "/tmp/subconscious-code-sc"
    _REMOTE_SOURCE_ARCHIVE = "/tmp/subconscious-code-source.tar.gz"
    _DEFAULT_REPOSITORY = (
        "https://github.com/subconscious-systems/subconscious-code.git"
    )

    ENV_VARS: ClassVar[list[EnvVar]] = [
        # GLM reasoning regularly exceeds the provider's implicit 4096-token
        # completion ceiling. Give benchmark turns enough room to finish one
        # thought while the core loop's no-progress guard bounds runaways.
        EnvVar("max_tokens", env="SC_MAX_TOKENS", type="int", default=8192),
        EnvVar("temperature", env="SC_TEMPERATURE", type="str"),
        EnvVar("max_iters", env="SC_MAX_ITERS", type="int"),
        EnvVar("timeout_ms", env="SC_TIMEOUT_MS", type="int", default=0),
        EnvVar("idle_timeout_ms", env="SC_IDLE_TIMEOUT_MS", type="int"),
        # Finish and publish an honest report before Harbor's 600s outer kill.
        EnvVar("turn_timeout_ms", env="SC_TURN_TIMEOUT_MS", type="int", default=540000),
        EnvVar("max_retries", env="SC_MAX_RETRIES", type="int"),
        EnvVar("request_gzip", env="SC_REQUEST_GZIP", type="bool"),
        # Benchmark runs should be hermetic by default. The sandbox confines
        # writes to declared workspace roots; network is a separate opt-in.
        EnvVar("sandbox", env="SC_SANDBOX", type="bool", default=True),
        EnvVar("sandbox_net", env="SC_SANDBOX_NET", type="bool", default=False),
    ]

    def __init__(
        self,
        *args: Any,
        binary_url: str | None = None,
        binary_sha256: str | None = None,
        binary_path: str | Path | None = None,
        source_archive: str | Path | None = None,
        repository: str = _DEFAULT_REPOSITORY,
        revision: str | None = None,
        reuse_existing: bool = True,
        offline: bool = True,
        offline_bundle: str | Path | None = None,
        offline_bundle_remote_path: str | None = None,
        offline_bundle_sha256: str | None = None,
        offline_control_plane_hosts: list[str] | None = None,
        **kwargs: Any,
    ) -> None:
        extra_env = kwargs.get("extra_env")
        self._offline = OfflineRuntime(
            enabled=offline,
            bundle=offline_bundle,
            bundle_remote_path=offline_bundle_remote_path,
            bundle_sha256=offline_bundle_sha256,
            control_plane_hosts=(
                offline_control_plane_hosts
                if offline_control_plane_hosts is not None
                else OfflineRuntime.control_plane_hosts_from_env(extra_env)
            ),
        )
        kwargs["extra_env"] = self._offline.merge_extra_env(extra_env)
        super().__init__(*args, **kwargs)
        self._binary_url = binary_url
        self._binary_sha256 = binary_sha256
        if self._offline.enabled and self._binary_url:
            raise ValueError("binary_url is unavailable in offline mode; use binary_path")
        if self._binary_url and not (
            self._binary_sha256 or os.environ.get("SC_HARBOR_BINARY_SHA256")
        ):
            raise ValueError("binary_sha256 is required when binary_url is set")
        self._binary_path = Path(binary_path).expanduser() if binary_path else None
        self._source_archive = (
            Path(source_archive).expanduser() if source_archive else None
        )
        self._repository = repository
        self._revision = revision
        self._reuse_existing = reuse_existing
        self._binary_path_sha256 = (
            self._sha256(self._binary_path)
            if self._binary_path is not None and self._binary_path.is_file()
            else None
        )
        self._source_sha256 = (
            self._sha256(self._source_archive)
            if self._source_archive is not None and self._source_archive.is_file()
            else None
        )

    @staticmethod
    def _sha256(path: Path) -> str:
        digest = hashlib.sha256()
        with path.open("rb") as artifact:
            for chunk in iter(lambda: artifact.read(1024 * 1024), b""):
                digest.update(chunk)
        return digest.hexdigest()

    @staticmethod
    def name() -> str:
        return "subconscious-code"

    def get_version_command(self) -> str | None:
        return 'export PATH="$HOME/.local/bin:$HOME/.cargo/bin:$PATH"; sc --version'

    def parse_version(self, stdout: str) -> str:
        text = stdout.strip().splitlines()[-1].strip()
        return text.removeprefix("sc ").strip()

    async def install(self, environment: BaseEnvironment) -> None:
        await self._offline.prepare(self, environment)

        # A directly uploaded binary needs no package-manager bootstrap.
        if not self._offline.enabled and self._binary_path is None:
            await self.exec_as_root(
                environment,
                command=(
                    "set -euo pipefail; "
                    "if command -v apt-get >/dev/null 2>&1; then "
                    "  apt-get update && "
                    "  DEBIAN_FRONTEND=noninteractive apt-get install -y "
                    "    ca-certificates curl git build-essential pkg-config; "
                    "elif command -v apk >/dev/null 2>&1; then "
                    "  apk add --no-cache ca-certificates curl git build-base pkgconf; "
                    "elif command -v dnf >/dev/null 2>&1; then "
                    "  dnf install -y ca-certificates curl git gcc gcc-c++ make pkgconf; "
                    "elif command -v yum >/dev/null 2>&1; then "
                    "  yum install -y ca-certificates curl git gcc gcc-c++ make pkgconfig; "
                    "fi"
                ),
            )

        if self._binary_path is not None:
            if not self._binary_path.is_file():
                raise ValueError(f"binary not found: {self._binary_path}")
            await environment.upload_file(
                str(self._binary_path.resolve()), self._REMOTE_BINARY
            )

        if self._source_archive is not None:
            if not self._source_archive.is_file():
                raise ValueError(f"source archive not found: {self._source_archive}")
            await environment.upload_file(
                str(self._source_archive.resolve()), self._REMOTE_SOURCE_ARCHIVE
            )

        install_env = {
            "SC_HARBOR_REUSE_EXISTING": "1" if self._reuse_existing else "0",
            "SC_HARBOR_BINARY_PATH": (
                self._REMOTE_BINARY if self._binary_path is not None else ""
            ),
            "SC_HARBOR_BINARY_URL": self._binary_url
            or (
                ""
                if self._offline.enabled
                else os.environ.get("SC_HARBOR_BINARY_URL", "")
            ),
            "SC_HARBOR_BINARY_SHA256": self._binary_sha256
            or os.environ.get("SC_HARBOR_BINARY_SHA256", ""),
            "SC_HARBOR_ARTIFACT_SHA256": self._binary_path_sha256
            or self._binary_sha256
            or os.environ.get("SC_HARBOR_ARTIFACT_SHA256", ""),
            "SC_HARBOR_SOURCE_SHA256": self._source_sha256
            or os.environ.get("SC_HARBOR_SOURCE_SHA256", ""),
            "SC_HARBOR_SOURCE_ARCHIVE": (
                self._REMOTE_SOURCE_ARCHIVE if self._source_archive is not None else ""
            ),
            "SC_HARBOR_REPOSITORY": os.environ.get(
                "SC_HARBOR_REPOSITORY", self._repository
            ),
            "SC_HARBOR_REVISION": self._revision
            or os.environ.get("SC_HARBOR_REVISION")
            or (f"v{self._version}" if self._version else "main"),
            "SC_HARBOR_OFFLINE": "1" if self._offline.enabled else "0",
            "SC_HARBOR_OFFLINE_ROOT": OFFLINE_ROOT,
        }
        await self.exec_as_agent(
            environment,
            command=self._install_command(),
            env=install_env,
        )

    @staticmethod
    def _install_command() -> str:
        return r'''set -euo pipefail
export PATH="$HOME/.local/bin:$HOME/.cargo/bin:$PATH"
mkdir -p "$HOME/.local/bin"
if [ -n "$SC_HARBOR_BINARY_PATH" ]; then
  printf '%s  %s\n' "$SC_HARBOR_ARTIFACT_SHA256" "$SC_HARBOR_BINARY_PATH" | sha256sum -c -
  install -m 0755 "$SC_HARBOR_BINARY_PATH" "$HOME/.local/bin/sc"
elif [ -n "$SC_HARBOR_BINARY_URL" ]; then
  work_dir="$(mktemp -d)"
  archive="$work_dir/download"
  curl -fL --retry 4 --retry-all-errors "$SC_HARBOR_BINARY_URL" -o "$archive"
  if [ -n "$SC_HARBOR_BINARY_SHA256" ]; then
    printf '%s  %s\n' "$SC_HARBOR_BINARY_SHA256" "$archive" | sha256sum -c -
  fi
  case "$SC_HARBOR_BINARY_URL" in
    *.tar.gz|*.tgz)
      tar -xzf "$archive" -C "$work_dir"
      source_bin="$(find "$work_dir" -type f -name sc -perm -u+x -print -quit)"
      ;;
    *)
      source_bin="$archive"
      chmod 0755 "$source_bin"
      ;;
  esac
  [ -n "$source_bin" ] && [ -f "$source_bin" ]
  install -m 0755 "$source_bin" "$HOME/.local/bin/sc"
elif [ -n "$SC_HARBOR_SOURCE_ARCHIVE" ]; then
  printf '%s  %s\n' "$SC_HARBOR_SOURCE_SHA256" "$SC_HARBOR_SOURCE_ARCHIVE" | sha256sum -c -
  if ! command -v cargo >/dev/null 2>&1; then
    if [ "$SC_HARBOR_OFFLINE" = 1 ]; then
      printf '%s\n' 'cargo is required to build the uploaded source archive offline' >&2
      exit 65
    else
      curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs |
        sh -s -- -y --profile minimal
      . "$HOME/.cargo/env"
    fi
  fi
  source_parent="$(mktemp -d)"
  tar -xzf "$SC_HARBOR_SOURCE_ARCHIVE" -C "$source_parent"
  source_dir="$source_parent/subconscious-code"
  [ -f "$source_dir/Cargo.toml" ]
  cargo install --locked --path "$source_dir/crates/rc-cli" \
    --root "$HOME/.local" --force
elif [ -x "$SC_HARBOR_OFFLINE_ROOT/payload/agents/sc" ]; then
  install -m 0755 "$SC_HARBOR_OFFLINE_ROOT/payload/agents/sc" "$HOME/.local/bin/sc"
elif [ "$SC_HARBOR_REUSE_EXISTING" = 1 ] && command -v sc >/dev/null 2>&1; then
  sc --version
else
  if [ "$SC_HARBOR_OFFLINE" = 1 ]; then
    printf '%s\n' 'sc is not prebaked and payload/agents/sc is absent from the offline bundle' >&2
    exit 65
  fi
  if ! command -v cargo >/dev/null 2>&1; then
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs |
      sh -s -- -y --profile minimal
    . "$HOME/.cargo/env"
  fi
  source_dir="$(mktemp -d)/subconscious-code"
  git clone "$SC_HARBOR_REPOSITORY" "$source_dir"
  git -C "$source_dir" checkout --detach "$SC_HARBOR_REVISION"
  cargo install --locked --path "$source_dir/crates/rc-cli" \
    --root "$HOME/.local" --force
fi
installed_sha="$(sha256sum "$(command -v sc)" | awk '{print $1}')"
mkdir -p "$HOME/.local/share/subconscious-code"
printf '{"schema_version":1,"binary_sha256":"%s","artifact_sha256":"%s","source_sha256":"%s","revision":"%s"}\n' \
  "$installed_sha" "$SC_HARBOR_ARTIFACT_SHA256" "$SC_HARBOR_SOURCE_SHA256" "$SC_HARBOR_REVISION" \
  > "$HOME/.local/share/subconscious-code/install-manifest.json"
sc --version'''

    @staticmethod
    def _normalized_model(model_name: str) -> str:
        return model_name.removeprefix("openai/")

    def _runtime_env(self) -> dict[str, str]:
        api_key = self._get_env("SC_API_KEY") or self._get_env("OPENAI_API_KEY")
        if not api_key:
            raise ValueError("SC_API_KEY or OPENAI_API_KEY is required")

        env = self.resolve_env_vars()
        env.update(
            {
                "SC_API_KEY": api_key,
                "SC_DANGEROUS": "1",
                "SC_HARBOR_ARTIFACT_SHA256": self._binary_path_sha256
                or self._binary_sha256
                or os.environ.get("SC_HARBOR_ARTIFACT_SHA256", ""),
                "SC_HARBOR_SOURCE_SHA256": self._source_sha256
                or os.environ.get("SC_HARBOR_SOURCE_SHA256", ""),
                "SC_HARBOR_REVISION": self._revision
                or os.environ.get("SC_HARBOR_REVISION", ""),
            }
        )
        base_url = (
            self._get_env("SC_BASE_URL")
            or self._get_env("OPENAI_BASE_URL")
            or self._get_env("OPENAI_API_BASE")
        )
        if base_url:
            env["SC_BASE_URL"] = base_url
        return env

    @with_prompt_template
    async def run(
        self,
        instruction: str,
        environment: BaseEnvironment,
        context: AgentContext,
    ) -> None:
        del context  # Harbor calls populate_context_post_run after syncing logs.
        if not self.model_name:
            raise ValueError("model_name is required")

        agent_dir = EnvironmentPaths.agent_dir.as_posix()
        output_path = (EnvironmentPaths.agent_dir / self._OUTPUT_FILENAME).as_posix()
        report_path = (EnvironmentPaths.agent_dir / self._REPORT_FILENAME).as_posix()
        trajectory_path = (
            EnvironmentPaths.agent_dir / self._TRAJECTORY_FILENAME
        ).as_posix()
        model = self._normalized_model(self.model_name)
        validate_report = shlex.quote(
            "import json,sys; "
            "report=json.load(open(sys.argv[1])); "
            "outcome=report.get('outcome'); "
            "known={'stop','length','iteration_limit','time_limit','cancelled','no_progress','incomplete'}; "
            "assert outcome in known, f'unknown subconscious-code outcome: {outcome!r}'; "
            "provenance=report.get('provenance') or {}; "
            "assert provenance.get('run_id')==sys.argv[3], 'stale benchmark report run id'; "
            "assert provenance.get('binary_sha256')==sys.argv[4], 'benchmark binary hash mismatch'; "
            "assert not sys.argv[5] or provenance.get('source_sha256')==sys.argv[5], 'benchmark source hash mismatch'; "
            "assert not sys.argv[6] or provenance.get('artifact_sha256')==sys.argv[6], 'benchmark artifact hash mismatch'; "
            "assert not sys.argv[7] or provenance.get('revision')==sys.argv[7], 'benchmark revision mismatch'; "
            "report['harness_process_exit_code']=int(sys.argv[2]); "
            "open(sys.argv[1]+'.next','w').write(json.dumps(report,indent=2)+'\\n'); "
            "__import__('os').replace(sys.argv[1]+'.next',sys.argv[1]); "
            "print(f'subconscious-code outcome: {outcome}', file=sys.stderr) "
            "if outcome not in {'stop','length'} else None"
        )
        await self.exec_as_agent(
            environment,
            command=(
                'set -euo pipefail; export PATH="$HOME/.local/bin:$HOME/.cargo/bin:$PATH"; '
                f"mkdir -p {shlex.quote(agent_dir)}; "
                f"rm -f {shlex.quote(output_path)} {shlex.quote(report_path)} "
                f"{shlex.quote(trajectory_path)} {shlex.quote(report_path + '.next')} "
                f"{shlex.quote(trajectory_path + '.next')} {shlex.quote((EnvironmentPaths.agent_dir / 'turns.jsonl').as_posix())}; "
                "run_id=\"$(date +%s%N)-$$\"; "
                "binary_sha=\"$(sha256sum \"$(command -v sc)\" | awk '{print $1}')\"; "
                "export SC_HARBOR_RUN_ID=\"$run_id\" SC_HARBOR_INSTALLED_BINARY_SHA256=\"$binary_sha\"; "
                "set +e; sc --dangerously-skip-permissions "
                f"--model {shlex.quote(model)} "
                f"--benchmark-report {shlex.quote(report_path)} "
                f"--benchmark-trajectory {shlex.quote(trajectory_path)} "
                f"--print={shlex.quote(instruction)} "
                f"2>&1 </dev/null | tee {shlex.quote(output_path)}; "
                "sc_status=${PIPESTATUS[0]}; set -e; "
                f"if [ ! -f {shlex.quote(report_path)} ]; then exit \"$sc_status\"; fi; "
                f"python3 -c {validate_report} {shlex.quote(report_path)} \"$sc_status\" \"$run_id\" \"$binary_sha\" "
                '"${SC_HARBOR_SOURCE_SHA256:-}" "${SC_HARBOR_ARTIFACT_SHA256:-}" "${SC_HARBOR_REVISION:-}"'
            ),
            env=self._runtime_env(),
        )

    def populate_context_post_run(self, context: AgentContext) -> None:
        path = self.logs_dir / self._REPORT_FILENAME
        if not path.is_file():
            return
        try:
            report = json.loads(path.read_text())
        except (OSError, json.JSONDecodeError):
            return

        usage = report.get("usage") or {}
        context.n_input_tokens = self._nonnegative_int(usage.get("input_tokens"))
        context.n_cache_tokens = self._nonnegative_int(
            usage.get("cached_input_tokens")
        )
        context.n_output_tokens = self._nonnegative_int(usage.get("output_tokens"))
        cost = report.get("cost_usd")
        if isinstance(cost, (int, float)) and not isinstance(cost, bool) and cost >= 0:
            context.cost_usd = float(cost)
        context.metadata = {
            "subconscious_code": report,
            "offline_runtime": self._offline.provenance(),
        }

    @staticmethod
    def _nonnegative_int(value: Any) -> int | None:
        if isinstance(value, bool) or not isinstance(value, int) or value < 0:
            return None
        return value
