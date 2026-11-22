"""Identify, warm, trim and prune the compiler caches that only main writes.

A cache identity hashes what decides dependency artifacts (manifests, the lock
file, the compiler and the warm recipe) but never workspace source, so editing
code reuses the cache while a dependency or compiler change rotates it.
Validation jobs restore caches and never save them; the maintenance workflow
warms only identities missing from main and prunes the ones they replace.
"""

import argparse
import hashlib
import json
import os
import shutil
import subprocess
import urllib.error
import urllib.request
from pathlib import Path

from catalog import ROOT, Family, families, toolchain

PREFIX = "ci-v1-"
LEGACY_PREFIX = "v0-rust-"
MAIN = "refs/heads/main"
UPLOAD_LIMIT = 4 * 1024**3
PACKAGE = "beankeeper-desktop"
# Build settings every job sharing these caches must export identically.
CARGO_ENV = {
    "CARGO_INCREMENTAL": "0",
    "CARGO_PROFILE_DEV_DEBUG": "0",
    "CARGO_PROFILE_TEST_DEBUG": "0",
}
COMMON_INPUTS = (
    "Cargo.lock",
    "Cargo.toml",
    "rust-toolchain.toml",
    ".cargo/config.toml",
)
RELEASE_INPUTS = (".github/actions/release-build/action.yml",)
LINUX_RELEASE_INPUTS = ("packaging/linux-release.Dockerfile",)


def warm_commands(family: Family) -> list[list[str]]:
    """Compile exactly the dependency graphs validation jobs build."""
    if family.kind == "msrv":
        return [["cargo", "check", "--workspace", "--all-targets", "--locked"]]
    if family.kind == "release":
        return []  # The shared release-build action compiles release families.
    commands = []
    if family.name == "dev-linux":
        commands += [
            ["cargo", "clippy", "--workspace", "--all-targets", "--locked"],
            # Nextest and doctests compile the same test-profile graph.
            ["cargo", "test", "--workspace", "--locked", "--no-run"],
        ]
    # Dev-dependency features differ, so the service binary graph is separate.
    return [*commands, ["cargo", "build", "--locked"]]


def inputs(family: Family, root: Path = ROOT) -> list[str]:
    paths = set(COMMON_INPUTS)
    paths.update(p.relative_to(root).as_posix() for p in root.glob("crates/*/Cargo.toml"))
    if family.kind == "release":
        paths.update(RELEASE_INPUTS)
        if "linux" in family.target:
            paths.update(LINUX_RELEASE_INPUTS)
    return sorted(paths)


def identity(family: Family, root: Path = ROOT) -> tuple[str, str]:
    """Return the exact key and the family/compiler restore prefix."""
    compiler = toolchain(family, root)
    digest = hashlib.sha256()
    recipe = {
        "family": family.name,
        "runner": family.runner,
        "target": family.target,
        "commands": warm_commands(family),
        "env": CARGO_ENV,
    }
    digest.update(json.dumps(recipe, sort_keys=True).encode() + b"\0")
    for name in inputs(family, root):
        path = root / name
        if path.is_file():
            contents = path.read_bytes().replace(b"\r\n", b"\n")
            digest.update(name.encode() + b"\0" + contents + b"\0")
    prefix = f"{PREFIX}{family.name}-{compiler}-"
    return prefix + digest.hexdigest()[:32], prefix


def current_keys(root: Path = ROOT) -> dict[str, str]:
    return {name: identity(family, root)[0] for name, family in families().items()}


def missing(entries: list[dict], keys: dict[str, str]) -> list[str]:
    present = {entry["key"] for entry in entries if entry.get("ref") == MAIN}
    return [name for name, key in keys.items() if key not in present]


def obsolete(entries: list[dict], keys: dict[str, str]) -> list[int]:
    """Main caches a present, current identity supersedes, plus legacy caches."""
    present = {entry["key"] for entry in entries if entry.get("ref") == MAIN}
    # Only rotate a family once its replacement exists, so restores never go cold.
    superseded = tuple(f"{PREFIX}{name}-" for name, key in keys.items() if key in present)
    selected = []
    for entry in entries:
        key = entry.get("key", "")
        if entry.get("ref") != MAIN or key in keys.values():
            continue
        if key.startswith(LEGACY_PREFIX) or (superseded and key.startswith(superseded)):
            selected.append(entry["id"])
    return selected


def profile_dir(family: Family, target: Path) -> Path:
    if family.kind == "release":
        return target / family.target / "release"
    return target / "debug"


def clean_command(family: Family) -> list[str]:
    command = ["cargo", "clean", "--package", PACKAGE]
    if family.kind == "release":
        command += ["--release", "--target", family.target]
    return command


def trim(family: Family, target: Path, run=subprocess.run) -> int:
    """Keep dependency artifacts only: no workspace crate, executables or scratch."""
    run(clean_command(family), check=True)
    profile = profile_dir(family, target)
    for relative in ("incremental", "examples"):
        shutil.rmtree(profile / relative, ignore_errors=True)
    shutil.rmtree(target / "doc", ignore_errors=True)
    if profile.is_dir():
        for path in profile.iterdir():
            if path.is_file():  # Uplifted executables and their dep-info.
                path.unlink()
    deps = profile / "deps"
    if deps.is_dir():
        for path in deps.iterdir():
            if path.is_file() and path.suffix in {"", ".exe", ".pdb"}:
                path.unlink()
    return sum(p.stat().st_size for p in target.rglob("*") if p.is_file())


def api(path: str, method: str = "GET"):
    url = f"{os.environ['GITHUB_API_URL']}/repos/{os.environ['GITHUB_REPOSITORY']}/{path}"
    request = urllib.request.Request(
        url,
        method=method,
        headers={
            "Authorization": f"Bearer {os.environ['GH_TOKEN']}",
            "Accept": "application/vnd.github+json",
        },
    )
    try:
        with urllib.request.urlopen(request, timeout=30) as response:
            return json.load(response) if method == "GET" else None
    except urllib.error.HTTPError as error:
        if method == "DELETE" and error.code == 404:
            return None
        raise


def inventory(request=api) -> list[dict]:
    entries, page = [], 1
    while True:
        batch = request(f"actions/caches?per_page=100&page={page}&ref={MAIN}")["actions_caches"]
        entries.extend(batch)
        if len(batch) < 100:
            return entries
        page += 1


def warm_matrix(names: list[str], keys: dict[str, str]) -> dict:
    catalog = families()
    return {
        "include": [
            {
                "family": name,
                "os": catalog[name].runner,
                "kind": catalog[name].kind,
                "target": catalog[name].target,
                "key": keys[name],
            }
            for name in names
        ]
    }


def write_output(values: dict[str, str]) -> None:
    with open(os.environ["GITHUB_OUTPUT"], "a", encoding="utf-8") as stream:
        stream.writelines(f"{name}={value}\n" for name, value in values.items())


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("command", choices=["setup", "plan", "warm", "trim", "prune"])
    parser.add_argument("--family", choices=sorted(families()))
    args = parser.parse_args()
    if args.command in {"setup", "warm", "trim"} and not args.family:
        parser.error(f"{args.command} requires --family")
    family = families()[args.family] if args.family else None
    if args.command == "setup":
        key, prefix = identity(family)
        write_output(
            {
                "key": key,
                "restore-prefix": prefix,
                "toolchain": toolchain(family),
                "targets": family.target,
                "components": ",".join(family.components),
            }
        )
    elif args.command == "plan":
        keys = current_keys()
        names = missing(inventory(), keys)
        print(f"Missing main caches: {names or 'none'}")
        write_output(
            {
                "matrix": json.dumps(warm_matrix(names, keys), separators=(",", ":")),
                "missing": str(bool(names)).lower(),
            }
        )
    elif args.command == "warm":
        for command in warm_commands(family):
            print(json.dumps(command), flush=True)
            subprocess.run(command, check=True)
    elif args.command == "trim":
        size = trim(family, Path(os.environ.get("CARGO_TARGET_DIR", "target")))
        print(f"Compiler cache footprint: {size / 1024**2:.1f} MiB")
        write_output({"save": str(size <= UPLOAD_LIMIT).lower()})
    else:
        for cache_id in obsolete(inventory(), current_keys()):
            api(f"actions/caches/{cache_id}", "DELETE")
            print(f"Removed superseded cache {cache_id}")


if __name__ == "__main__":
    main()
