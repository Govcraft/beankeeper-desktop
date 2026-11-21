#!/usr/bin/env python3
"""Build Desktop release assets. No credentials or third-party Python packages needed."""

import argparse
import hashlib
import io
import json
from datetime import datetime, timezone
import os
from pathlib import Path
import re
import shutil
import subprocess
import tarfile
import tomllib
import zipfile

ROOT = Path(__file__).resolve().parents[1]
TARGETS = {
    "x86_64-unknown-linux-gnu": "amd64",
    "aarch64-unknown-linux-gnu": "arm64",
    "x86_64-apple-darwin": None,
    "aarch64-apple-darwin": None,
    "x86_64-pc-windows-msvc": None,
}
TAG_PREFIX = "beankeeper-desktop-v"


def version(tag=None):
    value = tomllib.loads((ROOT / "Cargo.toml").read_text())["workspace"]["package"]["version"]
    if not re.fullmatch(r"\d+\.\d+\.\d+", value):
        raise ValueError("release packaging requires a numeric MAJOR.MINOR.PATCH version")
    if tag is not None and tag != f"{TAG_PREFIX}{value}":
        raise ValueError(f"tag must be {TAG_PREFIX}{value}, got {tag!r}")
    return value


def name(release_version, target):
    if target not in TARGETS or not re.fullmatch(r"\d+\.\d+\.\d+", release_version):
        raise ValueError("unsupported target or invalid release version")
    return f"bk-{release_version}-{target}"


def stage(binary, release_version, target, work):
    directory = work / name(release_version, target)
    if directory.exists():
        shutil.rmtree(directory)
    directory.mkdir(parents=True)
    executable = directory / ("bk.exe" if "windows" in target else "bk")
    shutil.copy2(binary, executable)
    executable.chmod(0o755)
    for document in ("README.md", "LICENSE.md"):
        shutil.copy2(ROOT / document, directory / document)
    return directory


def tar_bytes(files):
    data = io.BytesIO()
    with tarfile.open(fileobj=data, mode="w:gz", format=tarfile.USTAR_FORMAT) as archive:
        for path, content, mode in files:
            entry = tarfile.TarInfo(path)
            entry.size = len(content)
            entry.mode = mode
            entry.mtime = int(os.environ.get("SOURCE_DATE_EPOCH", "0"))
            archive.addfile(entry, io.BytesIO(content))
    return data.getvalue()


def debian(directory, release_version, target, output):
    # Standard Debian ar container, with root-owned control/data tar members.
    # No maintainer scripts, services, ledger access or post-install execution.
    control = (
        "Package: beankeeper-desktop\n"
        f"Version: {release_version}\nArchitecture: {TARGETS[target]}\n"
        "Maintainer: Govcraft <roland@govcraft.ai>\n"
        "Section: utils\nPriority: optional\n"
        "Depends: libc6 (>= 2.39), libgcc-s1\n"
        "Conflicts: beankeeper-cli, bk, bk-bin\n"
        "Homepage: https://github.com/Govcraft/beankeeper-desktop\n"
        "Description: Beankeeper Desktop local browser workspace and MCP server\n"
        " Source-available Desktop host with embedded UI.\n"
    ).encode()
    files = [("usr/bin/bk", (directory / "bk").read_bytes(), 0o755)]
    for document in ("README.md", "LICENSE.md"):
        files.append((f"usr/share/doc/beankeeper-desktop/{document}",
                      (directory / document).read_bytes(), 0o644))
    members = [("debian-binary", b"2.0\n"),
               ("control.tar.gz", tar_bytes([("control", control, 0o644)])),
               ("data.tar.gz", tar_bytes(files))]
    with output.open("wb") as archive:
        archive.write(b"!<arch>\n")
        for filename, content in members:
            header = f"{filename + '/':<16}{0:<12}{0:<6}{0:<6}{'100644':<8}{len(content):<10}`\n"
            archive.write(header.encode("ascii"))
            archive.write(content)
            if len(content) % 2:
                archive.write(b"\n")


def installer(directory, release_version, target, output):
    stem = name(release_version, target)
    output.mkdir(parents=True, exist_ok=True)
    if "linux" in target:
        debian(directory, release_version, target, output / f"{stem}.deb")
    elif "darwin" in target:
        root = directory.parent / f"pkg-root-{target}"
        (root / "usr/local/bin").mkdir(parents=True, exist_ok=True)
        shutil.copy2(directory / "bk", root / "usr/local/bin/bk")
        documents = root / "usr/local/share/doc/beankeeper-desktop"
        documents.mkdir(parents=True, exist_ok=True)
        for document in ("README.md", "LICENSE.md"):
            shutil.copy2(directory / document, documents / document)
        subprocess.run(["pkgbuild", "--root", str(root), "--identifier",
                        "ai.govcraft.beankeeper-desktop", "--version", release_version,
                        "--install-location", "/", str(output / f"{stem}.pkg")], check=True)
    else:
        compiler = shutil.which("ISCC.exe") or str(
            Path(os.environ.get("ProgramFiles(x86)", "C:/Program Files (x86)"))
            / "Inno Setup 6/ISCC.exe")
        subprocess.run([compiler, f"/DReleaseVersion={release_version}",
                        f"/DSourceDir={directory.resolve()}",
                        f"/DOutputDir={output.resolve()}", f"/DArtifactName={stem}",
                        str(ROOT / "packaging/windows.iss")], check=True)


def compress(directory, release_version, target, output):
    stem = name(release_version, target)
    output.mkdir(parents=True, exist_ok=True)
    if "windows" in target:
        with zipfile.ZipFile(output / f"{stem}.zip", "w", zipfile.ZIP_DEFLATED) as archive:
            for path in sorted(directory.iterdir()):
                archive.write(path, f"{stem}/{path.name}")
    else:
        with tarfile.open(output / f"{stem}.tar.gz", "w:gz") as archive:
            archive.add(directory, arcname=stem)


def checksums(output):
    assets = sorted(path for path in output.iterdir()
                    if path.name.endswith((".tar.gz", ".deb", ".pkg", ".zip", ".exe")))
    if not assets:
        raise ValueError("no release assets found")
    lines = []
    for path in assets:
        with path.open("rb") as data:
            digest = hashlib.file_digest(data, "sha256").hexdigest()
        lines.append(f"{digest}  {path.name}\n")
    (output / "checksums-sha256.txt").write_text("".join(lines), encoding="utf-8")


def update_manifest(output, release_version, released_at=None):
    """Emit exact JSON bytes before Sigstore signing, using the archive layout."""
    if released_at is None:
        released_at = subprocess.check_output(
            ["git", "show", "-s", "--format=%ct", "HEAD"], cwd=ROOT, text=True).strip()
        released_at = datetime.fromtimestamp(int(released_at), timezone.utc).isoformat().replace("+00:00", "Z")
    artifacts = {}
    for target in TARGETS:
        extension = "zip" if "windows" in target else "tar.gz"
        archive = output / f"{name(release_version, target)}.{extension}"
        if archive.exists():
            artifacts[target] = {"sha256": hashlib.sha256(archive.read_bytes()).hexdigest()}
    if not artifacts:
        raise ValueError("no update archives found")
    manifest = {"schema_version": 1, "version": release_version,
                "released_at": released_at, "artifacts": artifacts}
    (output / "update-manifest.json").write_text(
        json.dumps(manifest, sort_keys=True, separators=(",", ":")) + "\n", encoding="utf-8")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("command", choices=["version", "stage", "installer", "archive", "checksums", "update-manifest"])
    parser.add_argument("--tag")
    parser.add_argument("--target", choices=TARGETS)
    parser.add_argument("--binary", type=Path)
    parser.add_argument("--work", type=Path, default=Path("staging"))
    parser.add_argument("--output", type=Path, default=Path("dist"))
    args = parser.parse_args()
    release_version = version(args.tag)
    if args.command == "version":
        print(release_version)
    elif args.command == "update-manifest":
        update_manifest(args.output, release_version)
    elif args.command == "checksums":
        checksums(args.output)
    else:
        if args.target is None:
            parser.error("--target is required")
        directory = args.work / name(release_version, args.target)
        if args.command == "stage":
            if args.binary is None:
                parser.error("--binary is required")
            stage(args.binary, release_version, args.target, args.work)
        elif args.command == "installer":
            installer(directory, release_version, args.target, args.output)
        else:
            compress(directory, release_version, args.target, args.output)


if __name__ == "__main__":
    main()
