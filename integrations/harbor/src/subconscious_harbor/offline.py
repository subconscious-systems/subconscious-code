"""Shared fail-closed offline runtime for Harbor benchmark agents."""

from __future__ import annotations

import hashlib
import json
import shlex
from pathlib import Path
from typing import Any
from urllib.parse import urlparse

from harbor.environments.base import BaseEnvironment

OFFLINE_ROOT = "/tmp/subconscious-offline"
REMOTE_BUNDLE = "/tmp/subconscious-offline-bundle.tar.gz"
ALLOWED_PAYLOAD_ROOTS = {
    "agents",
    "cargo",
    "deno",
    "go",
    "npm",
    "python",
    "toolchains",
    "uv",
}

_FORBIDDEN_HOSTS = {
    "github.com",
    "api.github.com",
    "raw.githubusercontent.com",
    "objects.githubusercontent.com",
    "codeload.github.com",
    "pypi.org",
    "files.pythonhosted.org",
    "registry.npmjs.org",
    "registry.yarnpkg.com",
    "crates.io",
    "index.crates.io",
    "static.crates.io",
    "proxy.golang.org",
    "sum.golang.org",
    "deno.land",
    "jsr.io",
}


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as artifact:
        for chunk in iter(lambda: artifact.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


class OfflineRuntime:
    """Upload and activate an immutable dependency bundle without internet I/O.

    The sandbox network policy remains the primary isolation boundary. Package
    manager flags and Git protocol policy provide defense in depth and make an
    accidental dependency miss fail immediately instead of trying the network.
    """

    def __init__(
        self,
        *,
        enabled: bool = True,
        bundle: str | Path | None = None,
        bundle_remote_path: str | None = None,
        bundle_sha256: str | None = None,
        control_plane_hosts: list[str] | tuple[str, ...] | None = None,
    ) -> None:
        if bundle is not None and bundle_remote_path is not None:
            raise ValueError(
                "offline_bundle and offline_bundle_remote_path are mutually exclusive"
            )

        self.enabled = enabled
        self.bundle = Path(bundle).expanduser() if bundle is not None else None
        self.bundle_remote_path = bundle_remote_path
        self.control_plane_hosts = {
            self._normalize_host(host) for host in (control_plane_hosts or [])
        }

        if self.bundle is not None:
            if not self.bundle.is_file():
                raise ValueError(f"offline bundle not found: {self.bundle}")
            actual = sha256_file(self.bundle)
            if bundle_sha256 is not None and actual != bundle_sha256.lower():
                raise ValueError(
                    f"offline bundle SHA-256 mismatch: expected {bundle_sha256}, got {actual}"
                )
            self.bundle_sha256 = actual
        else:
            self.bundle_sha256 = bundle_sha256.lower() if bundle_sha256 else None

        if self.bundle_remote_path is not None and self.bundle_sha256 is None:
            raise ValueError(
                "offline_bundle_sha256 is required with offline_bundle_remote_path"
            )

    @property
    def environment(self) -> dict[str, str]:
        if not self.enabled:
            return {}
        root = OFFLINE_ROOT
        return {
            "SC_HARBOR_OFFLINE": "1",
            "SC_HARBOR_OFFLINE_ROOT": root,
            "GIT_CONFIG_NOSYSTEM": "1",
            "GIT_CONFIG_GLOBAL": f"{root}/gitconfig",
            "PIP_NO_INDEX": "1",
            "PIP_DISABLE_PIP_VERSION_CHECK": "1",
            "PIP_FIND_LINKS": f"{root}/payload/python/wheels",
            "UV_OFFLINE": "1",
            "UV_NO_MANAGED_PYTHON": "1",
            "UV_CACHE_DIR": f"{root}/payload/uv/cache",
            "npm_config_offline": "true",
            "npm_config_audit": "false",
            "npm_config_fund": "false",
            "npm_config_update_notifier": "false",
            "npm_config_cache": f"{root}/payload/npm/cache",
            "YARN_ENABLE_NETWORK": "0",
            "YARN_ENABLE_TELEMETRY": "0",
            "CARGO_NET_OFFLINE": "true",
            "CARGO_HOME": f"{root}/payload/cargo",
            "RUSTUP_DIST_SERVER": "file:///nonexistent/offline-rustup-dist",
            "RUSTUP_UPDATE_ROOT": "file:///nonexistent/offline-rustup-update",
            "GOPROXY": "off",
            "GOSUMDB": "off",
            "GOMODCACHE": f"{root}/payload/go/pkg/mod",
            "DENO_DIR": f"{root}/payload/deno",
            "DENO_NO_UPDATE_CHECK": "1",
            "HF_HUB_OFFLINE": "1",
            "TRANSFORMERS_OFFLINE": "1",
        }

    @staticmethod
    def _normalize_host(host: str) -> str:
        value = host.strip().lower().rstrip(".")
        if "://" in value:
            parsed = urlparse(value)
            value = parsed.hostname or value
        return value.removeprefix("*.")

    @classmethod
    def control_plane_hosts_from_env(cls, env: dict[str, str] | None) -> list[str]:
        source = env or {}
        hosts: list[str] = []
        for key in (
            "SC_BASE_URL",
            "OPENAI_BASE_URL",
            "OPENAI_API_BASE",
            "ANTHROPIC_BASE_URL",
            "MSWEA_API_BASE",
        ):
            value = source.get(key)
            if value and urlparse(value).hostname:
                hosts.append(urlparse(value).hostname or "")
        return hosts

    def merge_extra_env(self, extra_env: dict[str, str] | None) -> dict[str, str]:
        merged = dict(extra_env or {})
        # The offline controls intentionally win over caller-provided values.
        merged.update(self.environment)
        return merged

    @staticmethod
    def _network_mode(environment: BaseEnvironment) -> tuple[str | None, list[str]]:
        try:
            policy = environment.network_policy
        except (AttributeError, RuntimeError):
            return None, []
        mode = getattr(policy, "network_mode", None)
        mode_text = getattr(mode, "value", mode)
        hosts = list(getattr(policy, "allowed_hosts", None) or [])
        return str(mode_text) if mode_text is not None else None, hosts

    def validate_network_policy(self, environment: BaseEnvironment) -> None:
        if not self.enabled:
            return
        mode, hosts = self._network_mode(environment)
        if mode not in {"no-network", "allowlist"}:
            raise RuntimeError(
                "offline benchmark agents require environment.network_mode to be "
                "'no-network' or 'allowlist'; public or unknown networking is rejected"
            )
        if mode != "allowlist":
            return

        rejected: list[str] = []
        for raw_host in hosts:
            host = raw_host.strip().lower().rstrip(".")
            unwrapped = host.removeprefix("*.")
            if (
                host in {"*", "0.0.0.0/0", "::/0"}
                or unwrapped in _FORBIDDEN_HOSTS
                or any(unwrapped.endswith(f".{item}") for item in _FORBIDDEN_HOSTS)
            ):
                rejected.append(raw_host)
        if rejected:
            raise RuntimeError(
                "offline network allowlist contains source/package hosts: "
                + ", ".join(sorted(rejected))
            )
        unapproved = [
            raw_host
            for raw_host in hosts
            if self._normalize_host(raw_host) not in self.control_plane_hosts
        ]
        if unapproved:
            raise RuntimeError(
                "offline network allowlist contains hosts not declared as model/metrics "
                "control plane: "
                + ", ".join(sorted(unapproved))
            )

    @staticmethod
    def _policy_command() -> str:
        code = r'''
from pathlib import Path
root = Path("/tmp/subconscious-offline")
root.mkdir(mode=0o700, parents=True, exist_ok=True)
for relative in (
    "payload/python/wheels", "payload/uv/cache", "payload/npm/cache",
    "payload/cargo", "payload/go/pkg/mod", "payload/deno",
):
    (root / relative).mkdir(mode=0o700, parents=True, exist_ok=True)
(root / "gitconfig").write_text("""[protocol \"http\"]
    allow = never
[protocol \"https\"]
    allow = never
[protocol \"git\"]
    allow = never
[protocol \"ssh\"]
    allow = never
[protocol \"file\"]
    allow = always
""")
'''.strip()
        return f"python3 -c {shlex.quote(code)}"

    @staticmethod
    def _extract_command(archive: str, expected_sha256: str) -> str:
        code = r'''
import hashlib
import json
import os
from pathlib import Path, PurePosixPath
import shutil
import sys
import tarfile

allowed_payload_roots = __ALLOWED_PAYLOAD_ROOTS__

archive = Path(sys.argv[1])
root = Path(sys.argv[2])
expected = sys.argv[3]
digest = hashlib.sha256()
with archive.open("rb") as stream:
    for chunk in iter(lambda: stream.read(1024 * 1024), b""):
        digest.update(chunk)
actual = digest.hexdigest()
if actual != expected:
    raise SystemExit(f"offline bundle SHA-256 mismatch: expected {expected}, got {actual}")

with tarfile.open(archive, "r:*") as bundle:
    members = bundle.getmembers()
    for member in members:
        path = PurePosixPath(member.name)
        if (
            path.is_absolute()
            or ".." in path.parts
            or not path.parts
            or path.parts[0] not in {"manifest.json", "payload"}
            or (
                path.parts[0] == "payload"
                and len(path.parts) > 1
                and path.parts[1] not in allowed_payload_roots
            )
            or member.issym()
            or member.islnk()
            or not (member.isfile() or member.isdir())
        ):
            raise SystemExit(f"unsafe offline bundle member: {member.name!r}")
    staging = root.with_name(root.name + ".next")
    if staging.exists():
        shutil.rmtree(staging)
    staging.mkdir(mode=0o700, parents=True)
    for member in members:
        target = staging.joinpath(*PurePosixPath(member.name).parts)
        if member.isdir():
            target.mkdir(mode=0o700, parents=True, exist_ok=True)
            continue
        target.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
        source = bundle.extractfile(member)
        if source is None:
            raise SystemExit(f"unable to read offline bundle member: {member.name!r}")
        with source, target.open("wb") as destination:
            shutil.copyfileobj(source, destination)
        target.chmod(member.mode & 0o777)

manifest_path = staging / "manifest.json"
manifest = json.loads(manifest_path.read_text())
if manifest.get("schema_version") != 1:
    raise SystemExit("unsupported offline bundle manifest schema")
expected_files = manifest.get("files")
if not isinstance(expected_files, dict):
    raise SystemExit("offline bundle manifest files must be an object")
actual_files = {
    item.relative_to(staging).as_posix()
    for item in (staging / "payload").rglob("*")
    if item.is_file()
}
if actual_files != set(expected_files):
    raise SystemExit("offline bundle contents do not match its manifest")
for relative, metadata in expected_files.items():
    path = staging / relative
    data_hash = hashlib.sha256()
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            data_hash.update(chunk)
    if path.stat().st_size != metadata.get("bytes") or data_hash.hexdigest() != metadata.get("sha256"):
        raise SystemExit(f"offline bundle file checksum mismatch: {relative}")
if root.exists():
    shutil.rmtree(root)
os.replace(staging, root)
'''.strip().replace(
            "__ALLOWED_PAYLOAD_ROOTS__", repr(sorted(ALLOWED_PAYLOAD_ROOTS))
        )
        return " ".join(
            [
                "python3 -c",
                shlex.quote(code),
                shlex.quote(archive),
                shlex.quote(OFFLINE_ROOT),
                shlex.quote(expected_sha256),
            ]
        )

    async def prepare(self, agent: Any, environment: BaseEnvironment) -> None:
        if not self.enabled:
            return
        self.validate_network_policy(environment)

        archive: str | None = None
        if self.bundle is not None:
            await environment.upload_file(str(self.bundle.resolve()), REMOTE_BUNDLE)
            archive = REMOTE_BUNDLE
        elif self.bundle_remote_path is not None:
            archive = self.bundle_remote_path

        if archive is not None:
            assert self.bundle_sha256 is not None
            await agent.exec_as_agent(
                environment,
                command=self._extract_command(archive, self.bundle_sha256),
                env=self.environment,
            )
        await agent.exec_as_agent(
            environment,
            command=self._policy_command(),
            env=self.environment,
        )

    def provenance(self) -> dict[str, Any]:
        return {
            "enabled": self.enabled,
            "bundle_sha256": self.bundle_sha256,
            "control_plane_hosts": sorted(self.control_plane_hosts),
            "bundle_source": (
                "uploaded"
                if self.bundle is not None
                else "prebaked"
                if self.bundle_remote_path is not None
                else "none"
            ),
        }


def read_bundle_manifest(path: Path) -> dict[str, Any]:
    """Small public helper used by tests and staging automation."""
    import tarfile

    with tarfile.open(path, "r:*") as bundle:
        manifest = bundle.extractfile("manifest.json")
        if manifest is None:
            raise ValueError("offline bundle is missing manifest.json")
        return json.load(manifest)
