#!/usr/bin/env python3
"""Keep the six published Huggr crates on one exact version."""

from __future__ import annotations

import argparse
import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
CRATES = (
    "huggr-core",
    "huggr-replay",
    "huggr-host",
    "huggr-providers",
    "huggr-agent",
    "huggr-toolkit",
)
VERSION_RE = re.compile(r"^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$")


def workspace_version(root: Path = ROOT) -> str:
    section = ""
    for line in (root / "Cargo.toml").read_text().splitlines():
        if line.startswith("["):
            section = line
        elif section == "[workspace.package]":
            match = re.fullmatch(r'version = "([^"]+)"', line)
            if match:
                return match.group(1)
    raise ValueError("Cargo.toml has no [workspace.package].version")


def next_version(current: str, bump: str) -> str:
    match = VERSION_RE.fullmatch(current)
    if not match:
        raise ValueError(f"unsupported release version `{current}`; expected MAJOR.MINOR.PATCH")
    major, minor, patch = (int(value) for value in match.groups())
    if bump == "none":
        return current
    if bump == "major":
        return f"{major + 1}.0.0"
    if bump == "minor":
        return f"{major}.{minor + 1}.0"
    if bump == "patch":
        return f"{major}.{minor}.{patch + 1}"
    raise ValueError(f"unknown bump `{bump}`")


def set_version(version: str, root: Path = ROOT) -> None:
    if not VERSION_RE.fullmatch(version):
        raise ValueError(f"invalid release version `{version}`; expected MAJOR.MINOR.PATCH")
    path = root / "Cargo.toml"
    lines = path.read_text().splitlines(keepends=True)
    section = ""
    package_updated = False
    dependencies_updated: set[str] = set()
    rendered: list[str] = []
    dependency_re = re.compile(
        r'^(huggr-(?:core|host|providers|replay|agent|toolkit)) = \{ version = "=[^"]+",(.*)$'
    )
    for line in lines:
        stripped = line.rstrip("\n")
        if stripped.startswith("["):
            section = stripped
        if section == "[workspace.package]" and re.fullmatch(r'version = "[^"]+"', stripped):
            line = f'version = "{version}"\n'
            package_updated = True
        elif section == "[workspace.dependencies]":
            match = dependency_re.fullmatch(stripped)
            if match:
                name, rest = match.groups()
                line = f'{name} = {{ version = "={version}",{rest}\n'
                dependencies_updated.add(name)
        rendered.append(line)
    missing = set(CRATES) - dependencies_updated
    if not package_updated or missing:
        detail = ", ".join(sorted(missing)) if missing else "workspace version"
        raise ValueError(f"release version fields are missing: {detail}")
    path.write_text("".join(rendered))


def check(root: Path = ROOT) -> list[str]:
    errors: list[str] = []
    version = workspace_version(root)
    if not VERSION_RE.fullmatch(version):
        errors.append(f"workspace version `{version}` is not MAJOR.MINOR.PATCH")
    cargo = (root / "Cargo.toml").read_text()
    for name in CRATES:
        expected = f'{name} = {{ version = "={version}", path = "crates/{name}" }}'
        if expected not in cargo:
            errors.append(f"workspace dependency `{name}` is not pinned to ={version}")
        manifest = (root / "crates" / name / "Cargo.toml").read_text()
        if "version.workspace = true" not in manifest:
            errors.append(f"{name} does not inherit the workspace version")
        if 'publish = ["crates-io"]' not in manifest:
            errors.append(f"{name} is not restricted to crates.io publication")

    sync_pairs = (
        ("examples/huglet-weather/Cargo.toml", "crates/huggr-toolkit/assets/weather-template/Cargo.toml.txt"),
        ("examples/huglet-weather/README.md", "crates/huggr-toolkit/assets/weather-template/README.md"),
        ("examples/huglet-weather/SYSTEM.md", "crates/huggr-toolkit/assets/weather-template/SYSTEM.md"),
        ("examples/huglet-weather/huggr.toml", "crates/huggr-toolkit/assets/weather-template/huggr.toml"),
        ("examples/huglet-weather/src/lib.rs", "crates/huggr-toolkit/assets/weather-template/lib.rs"),
        ("bindings/python/python/huggr_agents/_types.py", "crates/huggr-toolkit/assets/python/_types.py"),
    )
    for source, packaged in sync_pairs:
        if (root / source).read_bytes() != (root / packaged).read_bytes():
            errors.append(f"packaged asset `{packaged}` is out of sync with `{source}`")
    return errors


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    subparsers = parser.add_subparsers(dest="command", required=True)
    subparsers.add_parser("current")
    next_parser = subparsers.add_parser("next")
    next_parser.add_argument("bump", choices=("none", "patch", "minor", "major"))
    set_parser = subparsers.add_parser("set")
    set_parser.add_argument("version")
    subparsers.add_parser("check")
    subparsers.add_parser("crates")
    args = parser.parse_args()
    try:
        if args.command == "current":
            print(workspace_version())
        elif args.command == "next":
            print(next_version(workspace_version(), args.bump))
        elif args.command == "set":
            set_version(args.version)
            errors = check()
            if errors:
                raise ValueError("; ".join(errors))
            print(args.version)
        elif args.command == "check":
            errors = check()
            if errors:
                for error in errors:
                    print(f"error: {error}", file=sys.stderr)
                return 1
            print(workspace_version())
        elif args.command == "crates":
            print("\n".join(CRATES))
    except (OSError, ValueError) as error:
        print(f"error: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
