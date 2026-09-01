"""Offline Harbor adapter for mini-swe-agent."""

from __future__ import annotations

import shlex
from pathlib import Path
from typing import Any

from harbor.agents.installed.mini_swe_agent import MiniSweAgent as HarborMiniSweAgent
from harbor.environments.base import BaseEnvironment
from harbor.models.agent.context import AgentContext

from .offline import OFFLINE_ROOT, OfflineRuntime


class OfflineMiniSweAgent(HarborMiniSweAgent):
    """Run mini-swe-agent using only a prebaked install or uploaded wheelhouse."""

    def __init__(
        self,
        *args: Any,
        offline: bool = True,
        offline_bundle: str | Path | None = None,
        offline_bundle_remote_path: str | None = None,
        offline_bundle_sha256: str | None = None,
        offline_control_plane_hosts: list[str] | None = None,
        reuse_existing: bool = True,
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
        self._reuse_existing = reuse_existing
        super().__init__(*args, **kwargs)

    def get_version_command(self) -> str | None:
        return (
            '. "$HOME/.local/bin/env" 2>/dev/null || true; '
            "python3 -c \"import importlib.metadata as m; "
            "print(m.version('mini-swe-agent'))\""
        )

    async def install(self, environment: BaseEnvironment) -> None:
        if not self._offline.enabled:
            await super().install(environment)
            return

        await self._offline.prepare(self, environment)
        version_spec = f"=={self._version}" if self._version else ""
        env = self._offline.environment | {
            "MSWEA_HARBOR_REUSE_EXISTING": "1" if self._reuse_existing else "0",
            "MSWEA_HARBOR_PACKAGE_SPEC": f"mini-swe-agent{version_spec}",
        }
        await self.exec_as_agent(
            environment,
            command=self._install_command(),
            env=env,
        )

    @staticmethod
    def _install_command() -> str:
        wheelhouse = shlex.quote(f"{OFFLINE_ROOT}/payload/python/wheels")
        return f'''set -euo pipefail
mkdir -p "$HOME/.local/bin" "$HOME/.local/share"
cat > "$HOME/.local/bin/env" <<'MSWEA_ENV'
export PATH="$HOME/.local/bin:$PATH"
MSWEA_ENV
if [ "$MSWEA_HARBOR_REUSE_EXISTING" = 1 ] && command -v mini-swe-agent >/dev/null 2>&1; then
  mini-swe-agent --help >/dev/null
elif find {wheelhouse} -maxdepth 1 -type f -name 'mini_swe_agent-*.whl' -print -quit | grep -q .; then
  command -v python3 >/dev/null 2>&1
  python3 -m venv "$HOME/.local/share/mini-swe-agent-venv"
  "$HOME/.local/share/mini-swe-agent-venv/bin/python" -m pip install \
    --no-index --find-links {wheelhouse} "$MSWEA_HARBOR_PACKAGE_SPEC"
  ln -sfn "$HOME/.local/share/mini-swe-agent-venv/bin/mini-swe-agent" \
    "$HOME/.local/bin/mini-swe-agent"
else
  printf '%s\n' 'mini-swe-agent is not prebaked and its wheelhouse is absent from the offline bundle' >&2
  exit 65
fi
. "$HOME/.local/bin/env"
mini-swe-agent --help >/dev/null'''

    def populate_context_post_run(self, context: AgentContext) -> None:
        super().populate_context_post_run(context)
        metadata = dict(context.metadata or {})
        metadata["offline_runtime"] = self._offline.provenance()
        context.metadata = metadata
