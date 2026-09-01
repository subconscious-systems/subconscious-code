#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd "$script_dir/../.." && pwd)"
out_dir="${1:-$script_dir/dist}"
uv_cache_dir="${UV_CACHE_DIR:-/tmp/subconscious-code-harbor-uv-cache}"

mkdir -p "$out_dir"
# Reusing `dist/` must never carry a wheel/source bundle from an earlier
# worktree into a new manifest. These are the exact artifacts this script owns.
rm -f \
  "$out_dir/subconscious-code-source.tar.gz" \
  "$out_dir"/subconscious_code_harbor-*.whl \
  "$out_dir"/subconscious_code_harbor-*.tar.gz \
  "$out_dir/SHA256SUMS" \
  "$out_dir/MANIFEST.json"
# Benchmark trajectories and result tables are repository evidence, not
# runtime inputs. Keeping them out of the release source payload avoids
# shipping more than a thousand traces (and tens of megabytes) with Harbor.
COPYFILE_DISABLE=1 tar \
  --no-xattrs \
  --exclude='subconscious-code/.git' \
  --exclude='subconscious-code/target' \
  --exclude='*/target' \
  --exclude='subconscious-code/benchmark-results' \
  --exclude='subconscious-code/trial-results' \
  --exclude='subconscious-code/swebench*' \
  --exclude='subconscious-code/working-cli-plan.md' \
  --exclude='subconscious-code/improvement.md' \
  --exclude='subconscious-code/integrations/harbor/dist' \
  --exclude='*/.venv' \
  --exclude='*/.DS_Store' \
  --exclude='*/__pycache__' \
  --exclude='*/.pytest_cache' \
  --exclude='*/.ruff_cache' \
  -C "$(dirname "$repo_root")" \
  -czf "$out_dir/subconscious-code-source.tar.gz" \
  subconscious-code

UV_CACHE_DIR="$uv_cache_dir" uv build "$script_dir" --out-dir "$out_dir"

if command -v sha256sum >/dev/null 2>&1; then
  (
    cd "$out_dir"
    sha256sum subconscious-code-source.tar.gz \
      subconscious_code_harbor-*.whl subconscious_code_harbor-*.tar.gz \
      > SHA256SUMS
  )
else
  (
    cd "$out_dir"
    shasum -a 256 subconscious-code-source.tar.gz \
      subconscious_code_harbor-*.whl subconscious_code_harbor-*.tar.gz \
      > SHA256SUMS
  )
fi

python3 - "$out_dir" <<'PY'
import hashlib
import json
import pathlib
import sys

root = pathlib.Path(sys.argv[1])
artifacts = {}
for path in sorted(root.iterdir()):
    if path.is_file() and path.name not in {".gitignore", "MANIFEST.json"}:
        artifacts[path.name] = {
            "sha256": hashlib.sha256(path.read_bytes()).hexdigest(),
            "bytes": path.stat().st_size,
        }
(root / "MANIFEST.json").write_text(
    json.dumps({"schema_version": 1, "artifacts": artifacts}, indent=2) + "\n"
)
PY
