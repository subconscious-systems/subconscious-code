"""Build a deterministic, checksummed dependency bundle for E2B sandboxes."""

from __future__ import annotations

import argparse
import gzip
import hashlib
import io
import json
import tarfile
from pathlib import Path

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


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def collect_files(payload: Path) -> list[Path]:
    files: list[Path] = []
    for path in sorted(payload.rglob("*"), key=lambda item: item.as_posix()):
        relative = path.relative_to(payload)
        if relative.parts and relative.parts[0] not in ALLOWED_PAYLOAD_ROOTS:
            raise SystemExit(
                "refusing non-cache payload root "
                f"{relative.parts[0]!r}; allowed roots: "
                + ", ".join(sorted(ALLOWED_PAYLOAD_ROOTS))
            )
        if ".git" in relative.parts:
            raise SystemExit(f"refusing to bundle Git metadata: {relative}")
        if path.is_symlink():
            raise SystemExit(f"refusing to bundle symlink: {relative}")
        if path.is_dir():
            continue
        if not path.is_file():
            raise SystemExit(f"refusing to bundle non-regular file: {relative}")
        files.append(path)
    return files


def tar_info(name: str, *, size: int = 0, executable: bool = False) -> tarfile.TarInfo:
    info = tarfile.TarInfo(name)
    info.size = size
    info.mtime = 0
    info.uid = 0
    info.gid = 0
    info.uname = ""
    info.gname = ""
    info.mode = 0o755 if executable else 0o644
    return info


def main() -> None:
    parser = argparse.ArgumentParser(
        description=(
            "Package a prepared dependency cache. The input directory becomes "
            "payload/ in the archive; network downloads must happen before this step."
        )
    )
    parser.add_argument("payload", type=Path)
    parser.add_argument("output", type=Path)
    parser.add_argument(
        "--provenance",
        type=Path,
        help="optional JSON describing lockfiles, source revisions, and staging image",
    )
    args = parser.parse_args()

    payload = args.payload.resolve()
    output = args.output.resolve()
    if not payload.is_dir():
        raise SystemExit(f"payload directory not found: {payload}")
    if output == payload or payload in output.parents:
        raise SystemExit("output must be outside the payload directory")

    provenance = {}
    if args.provenance:
        provenance = json.loads(args.provenance.read_text())
        if not isinstance(provenance, dict):
            raise SystemExit("provenance JSON must be an object")

    files = collect_files(payload)
    manifest_files = {}
    for path in files:
        name = f"payload/{path.relative_to(payload).as_posix()}"
        manifest_files[name] = {
            "bytes": path.stat().st_size,
            "sha256": sha256(path),
        }
    manifest = {
        "schema_version": 1,
        "files": manifest_files,
        "provenance": provenance,
    }
    manifest_bytes = (json.dumps(manifest, indent=2, sort_keys=True) + "\n").encode()

    output.parent.mkdir(parents=True, exist_ok=True)
    with (
        output.open("wb") as raw,
        gzip.GzipFile(fileobj=raw, mode="wb", filename="", mtime=0) as zipped,
        tarfile.open(fileobj=zipped, mode="w", format=tarfile.PAX_FORMAT) as tar,
    ):
        tar.addfile(
            tar_info("manifest.json", size=len(manifest_bytes)),
            io.BytesIO(manifest_bytes),
        )
        for path in files:
            name = f"payload/{path.relative_to(payload).as_posix()}"
            executable = bool(path.stat().st_mode & 0o111)
            with path.open("rb") as stream:
                tar.addfile(
                    tar_info(
                        name,
                        size=path.stat().st_size,
                        executable=executable,
                    ),
                    stream,
                )

    print(json.dumps({"path": str(output), "bytes": output.stat().st_size, "sha256": sha256(output)}))


if __name__ == "__main__":
    main()
