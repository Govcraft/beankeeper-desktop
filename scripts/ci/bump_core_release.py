"""Point Desktop's beankeeper git dependencies at a CLI release tag.

The two crates must share one tag of the form beankeeper-cli-vX.Y.Z. This
script rewrites the manifests only; the caller runs `cargo update` to refresh
Cargo.lock. It does not publish crates, run bk, or bump the Desktop version.
"""

from __future__ import annotations

import argparse
import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
MANIFEST = ROOT / "crates" / "beankeeper-desktop" / "Cargo.toml"
CHANGELOG = ROOT / "CHANGELOG.md"
TAG_RE = re.compile(r"^beankeeper-cli-v(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$")
DEP_RE = re.compile(
    r'^(beankeeper(?:-cli)?) = \{ git = "https://github.com/Govcraft/beankeeper", '
    r'(?:branch = "main"|tag = "(beankeeper-cli-v[0-9]+\.[0-9]+\.[0-9]+)")'
    r'(?:, version = "[^"]+")? \}\n',
    re.MULTILINE,
)
CRATES = ("beankeeper", "beankeeper-cli")


class BumpError(ValueError):
    pass


def require_tag(tag: str) -> str:
    if not TAG_RE.fullmatch(tag):
        raise BumpError(f"tag {tag!r} is not a stable beankeeper-cli-vX.Y.Z release")
    return tag


def parse_pins(manifest: str) -> dict[str, str]:
    found = {name: None for name in CRATES}
    for match in DEP_RE.finditer(manifest):
        name, tag = match.group(1), match.group(2)
        if name not in found:
            continue
        found[name] = tag or "main"
    missing = [name for name, tag in found.items() if tag is None]
    if missing:
        raise BumpError(f"could not find git pins for {', '.join(missing)}")
    return found  # type: ignore[return-value]


def current_tag(manifest: str) -> str:
    pins = parse_pins(manifest)
    tags = set(pins.values())
    if len(tags) != 1:
        raise BumpError(f"beankeeper pins disagree: {pins}")
    tag = tags.pop()
    if tag == "main":
        raise BumpError("dependencies still track branch main")
    return require_tag(tag)


def apply_tag(manifest: str, tag: str) -> str:
    require_tag(tag)
    parse_pins(manifest)

    def replace(match: re.Match[str]) -> str:
        name = match.group(1)
        return f'{name} = {{ git = "https://github.com/Govcraft/beankeeper", tag = "{tag}" }}\n'

    updated, count = DEP_RE.subn(replace, manifest)
    if count != 2:
        raise BumpError(f"expected to rewrite two pins, rewrote {count}")
    parse_pins(updated)  # confirm both still parse
    current_tag(updated)
    if current_tag(updated) != tag:
        raise BumpError("rewrite did not pin the requested tag")
    return updated


def changelog_line(tag: str) -> str:
    require_tag(tag)
    return f"- Pin the ledger library and CLI to the {tag} release tag.\n"


def ensure_changelog(changelog: str, tag: str) -> str:
    require_tag(tag)
    if (
        tag in changelog.split("## [", 1)[-1].split("## [", 1)[0]
        or tag in changelog.split("## [Unreleased]", 1)[1].split("## [", 1)[0]
    ):
        return changelog
    heading = "## [Unreleased]\n"
    if heading not in changelog:
        raise BumpError("CHANGELOG.md needs a ## [Unreleased] heading")
    return changelog.replace(heading, heading + "\n" + changelog_line(tag), 1)


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest="command", required=True)

    sub.add_parser("current", help="print the tag currently pinned in Cargo.toml")

    apply = sub.add_parser("apply", help="rewrite Cargo.toml and CHANGELOG.md to a tag")
    apply.add_argument("--tag", required=True)
    apply.add_argument("--manifest", type=Path, default=MANIFEST)
    apply.add_argument("--changelog", type=Path, default=CHANGELOG)

    args = parser.parse_args(argv)
    try:
        if args.command == "current":
            print(current_tag(MANIFEST.read_text()))
            return 0
        tag = require_tag(args.tag)
        manifest_path: Path = args.manifest
        changelog_path: Path = args.changelog
        written_manifest = apply_tag(manifest_path.read_text(), tag)
        written_log = ensure_changelog(changelog_path.read_text(), tag)
        if (
            written_manifest == manifest_path.read_text()
            and written_log == changelog_path.read_text()
        ):
            print(f"already pinned to {tag}")
            return 0
        manifest_path.write_text(written_manifest)
        changelog_path.write_text(written_log)
        print(f"pinned {tag}")
        return 0
    except (OSError, BumpError) as error:
        print(f"bump-core-release: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main())
