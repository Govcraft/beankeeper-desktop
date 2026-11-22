"""Runners, compilers and cache families shared by planning and cache maintenance."""

import re
import tomllib
from dataclasses import dataclass
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]

# Release targets and the pinned image each one is packaged on.
TARGET_RUNNERS = {
    "x86_64-unknown-linux-gnu": "ubuntu-24.04",
    "aarch64-unknown-linux-gnu": "ubuntu-24.04-arm",
    "x86_64-apple-darwin": "macos-15-intel",
    "aarch64-apple-darwin": "macos-15",
    "x86_64-pc-windows-msvc": "windows-2022",
}
LINUX_TARGETS = frozenset(t for t in TARGET_RUNNERS if "linux" in t)
MACOS_TARGETS = frozenset(t for t in TARGET_RUNNERS if "apple" in t)
WINDOWS_TARGETS = frozenset(t for t in TARGET_RUNNERS if "windows" in t)
# One target per operating system covers OS-specific packaging behaviour.
REPRESENTATIVE_TARGETS = frozenset(
    {"x86_64-unknown-linux-gnu", "aarch64-apple-darwin", "x86_64-pc-windows-msvc"}
)
POSIX_TARGETS = frozenset({"x86_64-unknown-linux-gnu", "aarch64-apple-darwin"})
ALL_TARGETS = frozenset(TARGET_RUNNERS)

# Native background-service checks: cache family -> pinned image.
SERVICE_RUNNERS = {
    "dev-linux": "ubuntu-24.04",
    "dev-macos": "macos-15",
    "dev-windows": "windows-2025",
}


@dataclass(frozen=True)
class Family:
    """One reusable compiler cache: a compiler, runner image and build recipe."""

    name: str
    runner: str
    kind: str  # "dev", "msrv" or "release"
    target: str = ""
    components: tuple[str, ...] = ()


def families() -> dict[str, Family]:
    result = {
        "dev-linux": Family(
            "dev-linux", SERVICE_RUNNERS["dev-linux"], "dev", "", ("clippy", "rustfmt")
        ),
        "dev-macos": Family("dev-macos", SERVICE_RUNNERS["dev-macos"], "dev"),
        "dev-windows": Family("dev-windows", SERVICE_RUNNERS["dev-windows"], "dev"),
        "msrv-linux": Family("msrv-linux", "ubuntu-24.04", "msrv"),
    }
    for target, runner in TARGET_RUNNERS.items():
        name = f"release-{target}"
        result[name] = Family(name, runner, "release", target)
    return result


def stable_toolchain(root: Path = ROOT) -> str:
    channel = tomllib.loads((root / "rust-toolchain.toml").read_text())["toolchain"]["channel"]
    if not re.fullmatch(r"[0-9]+\.[0-9]+\.[0-9]+", channel):
        raise ValueError(f"rust-toolchain.toml must pin an exact release: {channel}")
    return channel


def msrv_toolchain(root: Path = ROOT) -> str:
    version = tomllib.loads((root / "Cargo.toml").read_text())["workspace"]["package"][
        "rust-version"
    ]
    if re.fullmatch(r"[0-9]+\.[0-9]+", version):
        version += ".0"
    if not re.fullmatch(r"[0-9]+\.[0-9]+\.[0-9]+", version):
        raise ValueError(f"Invalid workspace rust-version: {version}")
    return version


def toolchain(family: Family, root: Path = ROOT) -> str:
    return msrv_toolchain(root) if family.kind == "msrv" else stable_toolchain(root)
