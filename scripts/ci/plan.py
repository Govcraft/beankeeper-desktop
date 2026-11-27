"""Select CI checks conservatively from a reliable Git change set.

Every path is matched against an ordered rule table. A path no rule recognises,
a diff that cannot be computed, a manual dispatch or ``[full-ci]`` in the pull
request title or body selects every check. Python helper tests always run in
the planning job, so rules only decide the Rust, service and packaging jobs.
"""

import argparse
import json
import os
import re
import subprocess
from dataclasses import dataclass, field
from pathlib import Path

from catalog import (
    ALL_TARGETS,
    LINUX_TARGETS,
    MACOS_TARGETS,
    POSIX_TARGETS,
    REPRESENTATIVE_TARGETS,
    SERVICE_RUNNERS,
    TARGET_RUNNERS,
    WINDOWS_TARGETS,
)


@dataclass(frozen=True)
class Checks:
    rust: bool = False
    service: bool = False
    packaging: frozenset[str] = field(default_factory=frozenset)
    tooling: bool = False

    def __or__(self, other: "Checks") -> "Checks":
        return Checks(
            self.rust or other.rust,
            self.service or other.service,
            self.packaging | other.packaging,
            self.tooling or other.tooling,
        )


NOTHING = Checks()
RUST = Checks(rust=True, service=True)
EVERYTHING = Checks(True, True, ALL_TARGETS, True)
CI_TOOLING = Checks(True, True, REPRESENTATIVE_TARGETS, True)


def packaging(targets: frozenset[str], tooling: bool = False) -> Checks:
    return Checks(packaging=targets, tooling=tooling)


# Ordered (pattern, checks, reason). A pattern ending in "/" matches a directory.
RULES: tuple[tuple[str, Checks, str], ...] = (
    ("docs/", NOTHING, "documentation"),
    ("ruff.toml", Checks(tooling=True), "CI helper lint configuration"),
    ("CHANGELOG.md", NOTHING, "documentation"),
    # Copied verbatim into archives; nightly qualification packages them.
    ("README.md", NOTHING, "documentation"),
    ("LICENSE.md", NOTHING, "documentation"),
    (".gitignore", NOTHING, "repository metadata"),
    (".remember/", NOTHING, "session notes"),
    ("scripts/test-navigation-browser.py", NOTHING, "manual browser helper"),
    ("scripts/tests/test_install.py", NOTHING, "Python helper test"),
    ("scripts/tests/test_release.py", NOTHING, "Python helper test"),
    # Embedded by include_str! and exercised by the install-guide tests.
    (
        "SKILL.md",
        RUST | packaging(frozenset({"x86_64-unknown-linux-gnu"}) | WINDOWS_TARGETS),
        "embedded agent guide",
    ),
    # License trust and the Windows stack reservation reach every release binary.
    (
        "crates/beankeeper-desktop/build.rs",
        RUST | packaging(ALL_TARGETS),
        "build script",
    ),
    (
        "packaging/license-embed.json",
        RUST | packaging(ALL_TARGETS),
        "embedded license trust",
    ),
    (
        "packaging/revocations.json",
        RUST | packaging(ALL_TARGETS),
        "embedded signed license revocations",
    ),
    ("crates/", RUST, "Rust source"),
    ("ui/", RUST, "embedded UI"),
    ("Cargo.toml", RUST, "workspace manifest"),
    ("Cargo.lock", RUST, "locked dependencies"),
    (".cargo/", RUST, "Cargo configuration"),
    (".clippy.toml", RUST, "lint configuration"),
    ("rust-toolchain.toml", RUST | packaging(REPRESENTATIVE_TARGETS), "compiler pin"),
    (
        "scripts/tests/service-lifecycle.py",
        Checks(service=True, packaging=REPRESENTATIVE_TARGETS),
        "service and packaged-extension test",
    ),
    (
        "scripts/tests/prepare-native-keychain.sh",
        Checks(service=True, packaging=REPRESENTATIVE_TARGETS),
        "native credential store setup",
    ),
    ("packaging/claude/", packaging(REPRESENTATIVE_TARGETS), "Claude extension bundle"),
    ("install.sh", packaging(POSIX_TARGETS), "POSIX installer"),
    (
        "cosign-bootstrap.json",
        packaging(ALL_TARGETS),
        "installer verifier bootstrap metadata and offline consistency checks",
    ),
    (
        "scripts/tests/skill-install-posix.py",
        packaging(POSIX_TARGETS),
        "POSIX install guide test",
    ),
    ("install.ps1", packaging(WINDOWS_TARGETS), "Windows installer"),
    ("packaging/windows.iss", packaging(WINDOWS_TARGETS), "Windows installer"),
    (
        "scripts/tests/install-windows.ps1",
        packaging(WINDOWS_TARGETS),
        "Windows installer test",
    ),
    (
        "scripts/tests/skill-install-windows.ps1",
        packaging(WINDOWS_TARGETS),
        "Windows install guide test",
    ),
    ("scripts/sign-macos.sh", packaging(MACOS_TARGETS), "macOS signing"),
    ("packaging/postinstall", packaging(MACOS_TARGETS), "macOS package script"),
    (
        "packaging/linux-release.Dockerfile",
        packaging(LINUX_TARGETS),
        "Linux build image",
    ),
    ("packaging/postinst", packaging(LINUX_TARGETS), "Debian package script"),
    ("scripts/release.py", packaging(ALL_TARGETS), "release packaging"),
    ("scripts/check-license-embed.py", packaging(ALL_TARGETS), "release smoke test"),
    (
        ".github/workflows/packaging.yml",
        packaging(ALL_TARGETS, True),
        "packaging workflow",
    ),
    (".github/workflows/release.yml", packaging(ALL_TARGETS, True), "release workflow"),
    (".github/actions/release-build/", packaging(ALL_TARGETS, True), "release build"),
    (
        ".github/workflows/cache-maintenance.yml",
        Checks(tooling=True),
        "cache maintenance workflow",
    ),
    (
        ".github/workflows/live-install.yml",
        Checks(tooling=True),
        "published installation workflow",
    ),
    ("scripts/ci/tests/", Checks(tooling=True), "CI helper tests"),
    ("scripts/ci/", CI_TOOLING, "CI planning, gate and cache identity"),
    (".github/actions/", CI_TOOLING, "shared CI action"),
    (".github/workflows/ci.yml", CI_TOOLING, "CI workflow"),
)


def matches(pattern: str, path: str) -> bool:
    return path.startswith(pattern) if pattern.endswith("/") else path == pattern


def classify(path: str) -> tuple[Checks, str]:
    for pattern, checks, reason in RULES:
        if matches(pattern, path):
            return checks, reason
    return EVERYTHING, "unclassified path, selecting every check"


def select(paths: list[str], full: bool = False) -> dict[str, object]:
    """Union the checks every changed path requires into an output plan."""
    if full:
        checks, reasons = EVERYTHING, ["Every check requested"]
    else:
        checks, reasons = NOTHING, []
        for path in paths:
            needed, reason = classify(path)
            checks |= needed
            reasons.append(f"{path}: {reason}")
    service = [
        {"family": family, "os": runner}
        for family, runner in SERVICE_RUNNERS.items()
        if checks.service
    ]
    targets = [
        {"target": target, "os": runner}
        for target, runner in TARGET_RUNNERS.items()
        if target in checks.packaging
    ]
    return {
        "rust": checks.rust,
        "service": {"include": service},
        "service_any": bool(service),
        "packaging": {"include": targets},
        "packaging_any": bool(targets),
        "tooling": checks.tooling,
        "reasons": reasons,
    }


def changed_paths(event: str, base: str, head: str, forced: bool) -> list[str]:
    if event not in {"pull_request", "push"} or forced:
        raise ValueError(f"No reliable incremental diff for event {event!r}")
    for name, sha in (("base", base), ("head", head)):
        if not re.fullmatch(r"[0-9a-f]{40}", sha) or sha == "0" * 40:
            raise ValueError(f"Missing or invalid {name} commit")
    if event == "pull_request":
        base = subprocess.check_output(["git", "merge-base", base, head], text=True).strip()
    output = subprocess.check_output(
        ["git", "diff", "--name-only", "--no-renames", "-z", base, head]
    )
    return [p.decode("utf-8", errors="surrogateescape") for p in output.split(b"\0") if p]


def write_outputs(plan: dict[str, object], stream) -> None:
    for name in (
        "rust",
        "service",
        "service_any",
        "packaging",
        "packaging_any",
        "tooling",
    ):
        stream.write(f"{name}={json.dumps(plan[name], separators=(',', ':'))}\n")


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--full", action="store_true", help="select every check")
    parser.add_argument("--paths", nargs="*", help="classify these paths instead of Git")
    args = parser.parse_args()
    request = os.environ.get("PR_TITLE", "") + "\n" + os.environ.get("PR_BODY", "")
    full = args.full or "[full-ci]" in request
    paths: list[str] = []
    if args.paths is not None:
        paths = args.paths
    elif not full:
        try:
            paths = changed_paths(
                os.environ.get("EVENT", ""),
                os.environ.get("BASE_SHA", ""),
                os.environ.get("HEAD_SHA", ""),
                os.environ.get("PUSH_FORCED") == "true",
            )
        except (ValueError, subprocess.CalledProcessError) as error:
            print(f"Incremental selection unavailable ({error}); selecting everything.")
            full = True
    plan = select(paths, full)
    Path("ci-plan.json").write_text(json.dumps(plan, indent=2) + "\n")
    print(json.dumps(plan, indent=2))
    if output := os.environ.get("GITHUB_OUTPUT"):
        with open(output, "a", encoding="utf-8") as stream:
            write_outputs(plan, stream)


if __name__ == "__main__":
    main()
