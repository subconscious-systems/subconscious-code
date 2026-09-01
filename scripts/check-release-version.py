#!/usr/bin/env python3
"""Fail a release when its tag and publishable package versions diverge."""

from __future__ import annotations

import argparse
import re
import tomllib
from pathlib import Path


REPOSITORY_ROOT = Path(__file__).resolve().parents[1]
SEMVER = re.compile(r"[0-9]+\.[0-9]+\.[0-9]+(?:-[0-9A-Za-z.-]+)?")


def project_version(path: Path, table: str) -> str:
    with path.open("rb") as stream:
        document = tomllib.load(stream)
    value = document
    for part in table.split("."):
        value = value[part]
    if not isinstance(value, str):
        raise SystemExit(f"{path}: {table} must be a string")
    return value


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument(
        "--tag",
        help="release tag to validate (for example v0.1.0); omit for a local parity check",
    )
    args = parser.parse_args()

    rust_version = project_version(
        REPOSITORY_ROOT / "Cargo.toml", "workspace.package.version"
    )
    harbor_version = project_version(
        REPOSITORY_ROOT / "integrations/harbor/pyproject.toml", "project.version"
    )
    if rust_version != harbor_version:
        raise SystemExit(
            "release version mismatch: "
            f"Rust workspace is {rust_version}, Harbor adapter is {harbor_version}"
        )
    if not SEMVER.fullmatch(rust_version):
        raise SystemExit(f"release version is not supported SemVer: {rust_version}")

    expected_tag = f"v{rust_version}"
    if args.tag and args.tag != expected_tag:
        raise SystemExit(
            f"release tag mismatch: expected {expected_tag}, got {args.tag}"
        )

    changelog = (REPOSITORY_ROOT / "CHANGELOG.md").read_text()
    if f"## [{rust_version}]" not in changelog:
        raise SystemExit(f"CHANGELOG.md has no [{rust_version}] release section")

    print(f"release versions agree: {expected_tag}")


if __name__ == "__main__":
    main()
