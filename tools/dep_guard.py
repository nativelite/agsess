#!/usr/bin/env python3
"""nativelite zero-dependency guard for a Rust crate (stdlib Python only).

Fails (exit 1) if the crate declares any THIRD-PARTY dependency. For Rust this
is authoritative: a crate cannot use code it does not declare in Cargo.toml, so
checking every dependency table is a complete guarantee — no source scan needed.

agsess is an *app-variant* crate: it is allowed to depend on other nativelite
org crates (here, `json`) but on nothing from crates.io. So the rule is not
"empty" but "every dependency is an org crate", identified by a
``package = "nativelite-*"`` registry dep (with a version: the crates.io
publish form) or a git dep on ``github.com/nativelite/``. A foreign crates.io
name or git URL is a third-party dependency and fails.

``[dev-dependencies]`` must still be empty: nativelite tests use the built-in
``#[test]`` harness with hand-authored vectors, which needs nothing external.
"""
from __future__ import annotations

import sys
import tomllib
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
# Dev-deps must be empty; runtime/build deps may be org crates (checked below).
EMPTY_TABLES = ("dev-dependencies",)
ORG_TABLES = ("dependencies", "build-dependencies")
ORG_GIT_PREFIX = "https://github.com/nativelite/"


def is_org_dep(spec: object) -> bool:
    """True iff `spec` is a nativelite org crate: a ``package = "nativelite-*"``
    registry dep (with a version, the publish form) or a git dep on the org."""
    if not isinstance(spec, dict):
        return False  # a bare version string is a crates.io dep
    git = spec.get("git")
    if isinstance(git, str) and git.startswith(ORG_GIT_PREFIX):
        return True
    pkg = spec.get("package") or ""
    return pkg.startswith("nativelite-") and bool(spec.get("version"))


def check_deps(table_name: str, deps: object, problems: list[str]) -> None:
    if not isinstance(deps, dict):
        return
    for name, spec in deps.items():
        if not is_org_dep(spec):
            problems.append(
                f"[{table_name}] {name!r} is not a nativelite org crate "
                f"(must be git = \"{ORG_GIT_PREFIX}...\"); found: {spec!r}"
            )


def check_manifest() -> list[str]:
    data = tomllib.loads((ROOT / "Cargo.toml").read_text(encoding="utf-8"))
    problems: list[str] = []
    for table in EMPTY_TABLES:
        deps = data.get(table)
        if deps:
            problems.append(f"[{table}] must be empty, found: {sorted(deps)}")
    for table in ORG_TABLES:
        check_deps(table, data.get(table), problems)
    # Also cover target-specific dependency tables.
    for tname, target in (data.get("target") or {}).items():
        for table in EMPTY_TABLES:
            if target.get(table):
                problems.append(
                    f"[target.{tname}.{table}] must be empty, "
                    f"found: {sorted(target[table])}"
                )
        for table in ORG_TABLES:
            check_deps(f"target.{tname}.{table}", target.get(table), problems)
    return problems


def main() -> int:
    problems = check_manifest()
    if problems:
        print("Dependency guard FAILED:")
        for p in problems:
            print(f"  - {p}")
        return 1
    print("Dependency guard OK: nativelite org crates only, no third-party deps.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
