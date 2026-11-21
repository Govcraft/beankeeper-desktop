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


# Numeric X.Y.Z, or an -rc.N prerelease that must match Cargo.toml and the git tag.
VERSION_PATTERN = r"\d+\.\d+\.\d+(?:-rc\.\d+)?"


def valid_version(value):
    return bool(re.fullmatch(VERSION_PATTERN, value))


def is_prerelease(release_version):
    return bool(re.fullmatch(r"\d+\.\d+\.\d+-rc\.\d+", release_version))


def windows_version_info(release_version):
    """Inno Setup VersionInfoVersion must be numeric X.Y.Z.W, not 0.1.0-rc.1."""
    if not valid_version(release_version):
        raise ValueError("unsupported target or invalid release version")
    if is_prerelease(release_version):
        core, rc = release_version.split("-rc.", 1)
        return f"{core}.{int(rc)}"
    return f"{release_version}.0"


def debian_upstream_version(release_version):
    """Debian would treat 0.1.0-rc.1 as revision rc.1; use 0.1.0~rc.1 so it sorts before 0.1.0."""
    if not valid_version(release_version):
        raise ValueError("unsupported target or invalid release version")
    return release_version.replace("-rc.", "~rc.", 1)


def version(tag=None):
    value = tomllib.loads((ROOT / "Cargo.toml").read_text())["workspace"]["package"]["version"]
    if not valid_version(value):
        raise ValueError("release packaging requires MAJOR.MINOR.PATCH or MAJOR.MINOR.PATCH-rc.N")
    if tag is not None and tag != f"{TAG_PREFIX}{value}":
        raise ValueError(f"tag must be {TAG_PREFIX}{value}, got {tag!r}")
    return value


def name(release_version, target):
    if target not in TARGETS or not valid_version(release_version):
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
    # Post-install setup is delegated to the invoking desktop user, never root.
    control = (
        "Package: beankeeper-desktop\n"
        f"Version: {debian_upstream_version(release_version)}\nArchitecture: {TARGETS[target]}\n"
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
               ("control.tar.gz", tar_bytes([("control", control, 0o644),
                                                ("postinst", (ROOT / "packaging/postinst").read_bytes(), 0o755)])),
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
        hooks = directory.parent / f"pkg-scripts-{target}"
        hooks.mkdir(parents=True, exist_ok=True)
        shutil.copy2(ROOT / "packaging/postinstall", hooks / "postinstall")
        subprocess.run(["pkgbuild", "--root", str(root), "--scripts", str(hooks), "--identifier",
                        "ai.govcraft.beankeeper-desktop", "--version", release_version,
                        "--install-location", "/", str(output / f"{stem}.pkg")], check=True)
    else:
        compiler = shutil.which("ISCC.exe") or str(
            Path(os.environ.get("ProgramFiles(x86)", "C:/Program Files (x86)"))
            / "Inno Setup 6/ISCC.exe")
        subprocess.run([compiler, f"/DReleaseVersion={release_version}",
                        f"/DVersionInfoVersion={windows_version_info(release_version)}",
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


def mcp_bundle(directory, release_version, target, output):
    """Package the staged (and, where configured, OS-signed) native executable."""
    stem = name(release_version, target)
    platform = "win32" if "windows" in target else "darwin" if "darwin" in target else "linux"
    executable = "bk.exe" if platform == "win32" else "bk"
    entry_point = f"server/{executable}"
    packaging = ROOT / "packaging/claude"
    manifest = json.loads((packaging / "manifest.json").read_text(encoding="utf-8"))
    manifest.update(
        version=release_version,
        compatibility={"platforms": [platform]},
        server={
            "type": "binary",
            "entry_point": entry_point,
            "mcp_config": {"command": "${__dirname}/" + entry_point, "args": ["mcp", "extension"]},
        },
    )

    def zip_entry(path, mode):
        entry = zipfile.ZipInfo(path)
        entry.create_system = 3  # Encode Unix modes on all release runners.
        entry.external_attr = (0o100000 | mode) << 16
        entry.compress_type = zipfile.ZIP_DEFLATED
        return entry

    output.mkdir(parents=True, exist_ok=True)
    with zipfile.ZipFile(output / f"{stem}.mcpb", "w") as archive:
        archive.writestr(
            zip_entry("manifest.json", 0o644),
            json.dumps(manifest, indent=2, sort_keys=True) + "\n",
        )
        with (directory / executable).open("rb") as source:
            with archive.open(zip_entry(entry_point, 0o755), "w") as packed:
                shutil.copyfileobj(source, packed)
        for path, source in (
            ("icon.png", packaging / "icon.png"),
            ("SKILL.md", ROOT / "SKILL.md"),
            ("LICENSE.md", directory / "LICENSE.md"),
        ):
            archive.writestr(zip_entry(path, 0o644), source.read_bytes())


def checksums(output):
    assets = sorted(path for path in output.iterdir()
                    if path.name.endswith((".tar.gz", ".deb", ".pkg", ".zip", ".exe", ".mcpb")))
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



def write_license_env():
    """Emit GITHUB_ENV assignments for build-time license embedding.

    Committed packaging/license-embed.json is the default. Repository Actions
    variables BEANKEEPER_LICENSE_PUBLIC_KEYS_JSON and BEANKEEPER_LICENSE_SERVICE_URL
    override that file when set. The public key is not secret.
    """
    config = json.loads((ROOT / "packaging/license-embed.json").read_text(encoding="utf-8"))
    keys = os.environ.get("BEANKEEPER_LICENSE_PUBLIC_KEYS_JSON") or json.dumps(
        config["public_keys"], separators=(",", ":"), sort_keys=True)
    url = os.environ.get("BEANKEEPER_LICENSE_SERVICE_URL") or config["service_url"]
    if not keys.strip() or not url.strip():
        raise ValueError("license embed requires public keys and a service URL")
    dest = os.environ.get("GITHUB_ENV")
    body = (
        "BEANKEEPER_LICENSE_PUBLIC_KEYS_JSON<<EOF\n"
        f"{keys}\nEOF\n"
        "BEANKEEPER_LICENSE_SERVICE_URL<<EOF\n"
        f"{url}\nEOF\n"
    )
    if dest:
        with open(dest, "a", encoding="utf-8") as handle:
            handle.write(body)
    else:
        print(body, end="")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("command", choices=["version", "stage", "installer", "archive", "mcpb", "checksums", "update-manifest", "license-env"])
    parser.add_argument("--tag")
    parser.add_argument("--target", choices=TARGETS)
    parser.add_argument("--binary", type=Path)
    parser.add_argument("--work", type=Path, default=Path("staging"))
    parser.add_argument("--output", type=Path, default=Path("dist"))
    args = parser.parse_args()
    if args.command == "license-env":
        write_license_env()
        return
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
        elif args.command == "mcpb":
            mcp_bundle(directory, release_version, args.target, args.output)
        else:
            compress(directory, release_version, args.target, args.output)


if __name__ == "__main__":
    main()
