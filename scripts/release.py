#!/usr/bin/env python3
"""Build Desktop release assets. No credentials or third-party Python packages needed."""

import argparse
import base64
import hashlib
import io
import json
from concurrent.futures import ThreadPoolExecutor
from datetime import datetime, timezone
import os
from pathlib import Path, PurePosixPath
import re
import shutil
import subprocess
import tarfile
import tempfile
import tomllib
import urllib.request
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
LINUX_GLIBC_CEILING = (2, 35, 0)


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


def validate_release_changelog(release_version):
    changelog = (ROOT / "CHANGELOG.md").read_text(encoding="utf-8")
    headings = [line.rstrip() for line in changelog.splitlines() if line.startswith("## ")]
    if headings[:1] != ["## [Unreleased]"]:
        raise ValueError("CHANGELOG.md must begin its sections with ## [Unreleased]")
    expected_heading = f"## [{release_version}]"
    if len(headings) < 2 or headings[1].partition(" - ")[0] != expected_heading:
        raise ValueError(f"CHANGELOG.md must have {expected_heading} directly below Unreleased")


def version(tag=None, *, check_changelog=False):
    value = tomllib.loads((ROOT / "Cargo.toml").read_text())["workspace"]["package"]["version"]
    if not valid_version(value):
        raise ValueError("release packaging requires MAJOR.MINOR.PATCH or MAJOR.MINOR.PATCH-rc.N")
    if tag is not None and tag != f"{TAG_PREFIX}{value}":
        raise ValueError(f"tag must be {TAG_PREFIX}{value}, got {tag!r}")
    if tag is not None or check_changelog:
        validate_release_changelog(value)
    return value


def strict_json(data):
    """Reject ambiguous duplicate members at the untrusted JSON boundary."""
    def object_pairs(pairs):
        result = {}
        for key, value in pairs:
            if key in result:
                raise ValueError(f"duplicate JSON member: {key}")
            result[key] = value
        return result
    return json.loads(data, object_pairs_hook=object_pairs)


def base64url(value, size=None):
    if not isinstance(value, str) or not re.fullmatch(r"[A-Za-z0-9_-]+", value):
        raise ValueError("expected unpadded base64url")
    decoded = base64.b64decode(value + "=" * (-len(value) % 4), altchars=b"-_", validate=True)
    if base64.urlsafe_b64encode(decoded).decode().rstrip("=") != value:
        raise ValueError("noncanonical base64url")
    if size is not None and len(decoded) != size:
        raise ValueError(f"expected {size} decoded bytes")
    return decoded


def validate_release_revocations(release_version):
    """Stable tag preflight: authenticate the issuer export before any builds."""
    if is_prerelease(release_version):
        return
    path = ROOT / "packaging/revocations.json"
    try:
        with path.open("rb") as source:
            data = source.read(4 * 1024 * 1024 + 1)
        if not data or len(data) > 4 * 1024 * 1024:
            raise ValueError("snapshot must contain 1 byte through 4 MiB")
        envelope = strict_json(data)
        if not isinstance(envelope, dict) or set(envelope) != {"format", "key_id", "payload", "signature"}:
            raise ValueError("expected format, key_id, payload and signature")
        if envelope["format"] != "beankeeper-revocations-v1":
            raise ValueError("unsupported revocation format")
        kid = envelope["key_id"]
        if not isinstance(kid, str) or not re.fullmatch(r"[A-Za-z0-9_-]{1,64}", kid):
            raise ValueError("invalid issuer key ID")
        config = strict_json((ROOT / "packaging/license-embed.json").read_bytes())
        keys = strict_json(os.environ["BEANKEEPER_LICENSE_PUBLIC_KEYS_JSON"]) if os.environ.get(
            "BEANKEEPER_LICENSE_PUBLIC_KEYS_JSON") else config["public_keys"]
        if kid not in keys:
            raise ValueError("unknown issuer key ID")
        key = base64url(keys[kid], 32)
        signature = base64url(envelope["signature"], 64)
        payload = base64url(envelope["payload"])
        message = f"{envelope['format']}\n{kid}\n{envelope['payload']}".encode()
        with tempfile.TemporaryDirectory() as temporary:
            work = Path(temporary)
            # RFC 8410 SubjectPublicKeyInfo wrapper for an Ed25519 public key.
            (work / "key.der").write_bytes(bytes.fromhex("302a300506032b6570032100") + key)
            (work / "signature").write_bytes(signature)
            (work / "message").write_bytes(message)
            subprocess.run([
                "openssl", "pkeyutl", "-verify", "-pubin", "-keyform", "DER",
                "-inkey", str(work / "key.der"), "-rawin",
                "-in", str(work / "message"), "-sigfile", str(work / "signature"),
            ], check=True, capture_output=True)
        claims = strict_json(payload)
        if not isinstance(claims, dict) or set(claims) != {"generated_at", "revoked_license_ids"}:
            raise ValueError("expected generated_at and revoked_license_ids")
        timestamp = claims["generated_at"]
        if not isinstance(timestamp, str) or not re.fullmatch(
            r"[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}(?:\.[0-9]+)?(?:Z|[+-][0-9]{2}:[0-9]{2})", timestamp
        ):
            raise ValueError("expected RFC 3339 generation time")
        datetime.fromisoformat(timestamp.replace("Z", "+00:00"))
        ids = claims["revoked_license_ids"]
        if not isinstance(ids, list) or any(
            not isinstance(value, str) or not re.fullmatch(r"lic_[0-7][0123456789abcdefghjkmnpqrstvwxyz]{25}", value)
            for value in ids
        ):
            raise ValueError("expected canonical lic_ TypeIDs")
    except (OSError, ValueError, KeyError, TypeError, subprocess.CalledProcessError) as error:
        raise ValueError(
            f"stable release requires valid packaging/revocations.json from export-revocations: {error}"
        ) from error


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
    timestamp = int(os.environ.get("SOURCE_DATE_EPOCH", "0"))
    parents = {
        parent.as_posix()
        for path, _, _ in files
        for parent in PurePosixPath(path).parents
        if parent.parts
    }
    with tarfile.open(fileobj=data, mode="w:gz", format=tarfile.USTAR_FORMAT) as archive:
        for parent in sorted(parents):
            entry = tarfile.TarInfo(parent)
            entry.type = tarfile.DIRTYPE
            entry.mode = 0o755
            entry.mtime = timestamp
            archive.addfile(entry)
        for path, content, mode in files:
            entry = tarfile.TarInfo(path)
            entry.size = len(content)
            entry.mode = mode
            entry.mtime = timestamp
            archive.addfile(entry, io.BytesIO(content))
    return data.getvalue()


def glibc_version_text(version):
    parts = version if version[2] else version[:2]
    return ".".join(str(part) for part in parts)


def check_linux_abi(binary):
    result = subprocess.run(
        ["readelf", "--version-info", "--wide", str(binary)],
        check=True,
        capture_output=True,
        text=True,
        env={**os.environ, "LC_ALL": "C"},
    )
    _, _, needs = result.stdout.partition("Version needs section")
    names = re.findall(r"\bName:\s+(GLIBC_\S+)", needs)
    if not names:
        raise ValueError("Linux release binary has no measurable glibc version requirements")

    versions = []
    for name in names:
        match = re.fullmatch(r"GLIBC_(\d+)\.(\d+)(?:\.(\d+))?", name)
        if match is None:
            raise ValueError(f"Unexpected glibc version requirement: {name}")
        versions.append(tuple(int(part or 0) for part in match.groups()))
    required = max(versions)
    if required > LINUX_GLIBC_CEILING:
        raise ValueError(
            f"Linux binary requires glibc {glibc_version_text(required)}; "
            f"release ceiling is {glibc_version_text(LINUX_GLIBC_CEILING)}"
        )
    return glibc_version_text(required)


def debian(directory, release_version, target, output):
    # Standard Debian ar container, with root-owned control/data tar members.
    # Post-install setup is delegated to the invoking desktop user, never root.
    control = (
        "Package: beankeeper-desktop\n"
        f"Version: {debian_upstream_version(release_version)}\nArchitecture: {TARGETS[target]}\n"
        "Maintainer: Govcraft <roland@govcraft.ai>\n"
        "Section: utils\nPriority: optional\n"
        f"Depends: libc6 (>= {glibc_version_text(LINUX_GLIBC_CEILING)}), libgcc-s1\n"
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
    write_mcp_bundle(directory / executable, release_version, platform, output / f"{stem}.mcpb")


def universal_macos_bundle(output, release_version):
    """Combine the two tested, byte-for-byte Mach-O slices for a single Mac download."""
    with tempfile.TemporaryDirectory() as temporary:
        work = Path(temporary)
        slices = []
        for target in ("aarch64-apple-darwin", "x86_64-apple-darwin"):
            bundle = output / f"{name(release_version, target)}.mcpb"
            binary = work / target
            with zipfile.ZipFile(bundle) as archive:
                with archive.open("server/bk") as source, binary.open("xb") as destination:
                    shutil.copyfileobj(source, destination)
            slices.append(binary)
        universal = work / "bk"
        subprocess.run(["xcrun", "lipo", "-create", *slices, "-output", universal], check=True)
        subprocess.run(["xcrun", "lipo", universal, "-verify_arch", "arm64", "x86_64"], check=True)
        universal.chmod(0o755)
        reported = subprocess.check_output([universal, "--version"], text=True).strip()
        if reported != f"bk {release_version}":
            raise ValueError("universal macOS extension binary has the wrong version")
        write_mcp_bundle(
            universal, release_version, "darwin",
            output / f"bk-{release_version}-universal-apple-darwin.mcpb",
        )


def write_mcp_bundle(binary, release_version, platform, output):
    entry_point = f"server/{binary.name}"
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

    output.parent.mkdir(parents=True, exist_ok=True)
    with zipfile.ZipFile(output, "w") as archive:
        archive.writestr(
            zip_entry("manifest.json", 0o644),
            json.dumps(manifest, indent=2, sort_keys=True) + "\n",
        )
        with binary.open("rb") as source:
            with archive.open(zip_entry(entry_point, 0o755), "w") as packed:
                shutil.copyfileobj(source, packed)
        for path, source in (
            ("icon.png", packaging / "icon.png"),
            ("SKILL.md", ROOT / "SKILL.md"),
            ("LICENSE.md", ROOT / "LICENSE.md"),
        ):
            archive.writestr(zip_entry(path, 0o644), source.read_bytes())


def claude_plugin(output, release_version, marketplace):
    """Build the portable plugin and complete, ready-to-publish marketplace tree."""
    if not valid_version(release_version):
        raise ValueError("invalid plugin release version")
    source = ROOT / "packaging/claude-plugin"
    manifest = strict_json((source / ".claude-plugin/plugin.json").read_bytes())
    manifest["version"] = release_version
    mcp = strict_json((source / ".mcp.json").read_bytes())
    expected = {"mcpServers": {"beankeeper": {"command": "bk", "args": ["mcp", "extension"]}}}
    if manifest["name"] != "beankeeper" or mcp != expected:
        raise ValueError("plugin must connect to the installed bk mcp extension")
    catalog_source = ROOT / "packaging/claude-marketplace"
    catalog = strict_json((catalog_source / ".claude-plugin/marketplace.json").read_bytes())
    entries = catalog["plugins"]
    if len(entries) != 1 or entries[0]["name"] != manifest["name"] or entries[0]["source"] != "./plugins/beankeeper":
        raise ValueError("marketplace must point to the generated Beankeeper plugin")
    files = {
        ".claude-plugin/plugin.json": (json.dumps(manifest, indent=2) + "\n").encode(),
        ".mcp.json": (json.dumps(mcp, indent=2) + "\n").encode(),
        "skills/beankeeper/SKILL.md": (ROOT / "SKILL.md").read_bytes(),
        "README.md": (source / "README.md").read_bytes(),
        "LICENSE.md": (ROOT / "LICENSE.md").read_bytes(),
    }
    output.mkdir(parents=True, exist_ok=True)
    if marketplace.exists():
        shutil.rmtree(marketplace)
    shutil.copytree(catalog_source, marketplace)
    plugin = marketplace / "plugins/beankeeper"
    with zipfile.ZipFile(output / "claude-plugin.zip", "w", zipfile.ZIP_DEFLATED) as archive:
        for name, content in files.items():
            archive.writestr(name, content)
            destination = plugin / name
            destination.parent.mkdir(parents=True, exist_ok=True)
            destination.write_bytes(content)


COSIGN_ASSETS = {
    "LINUX_AMD64": "cosign-linux-amd64",
    "LINUX_ARM64": "cosign-linux-arm64",
    "DARWIN_AMD64": "cosign-darwin-amd64",
    "DARWIN_ARM64": "cosign-darwin-arm64",
    "WINDOWS_AMD64": "cosign-windows-amd64.exe",
}


def bootstrap_installers(output):
    """Generate installers and an audit record from the latest official cosign bytes."""
    headers = {"Accept": "application/vnd.github+json"}
    if token := os.environ.get("GITHUB_TOKEN"):
        headers["Authorization"] = f"Bearer {token}"
    request = urllib.request.Request(
        "https://api.github.com/repos/sigstore/cosign/releases/latest", headers=headers)
    with urllib.request.urlopen(request, timeout=60) as source:
        tag = strict_json(source.read())["tag_name"]
    if not isinstance(tag, str) or not re.fullmatch(r"v[0-9]+\.[0-9]+\.[0-9]+", tag):
        raise ValueError("expected a stable official cosign release")
    def hash_asset(item):
        key, asset = item
        url = f"https://github.com/sigstore/cosign/releases/download/{tag}/{asset}"
        print(f"Hashing {tag}/{asset}", flush=True)
        with urllib.request.urlopen(url, timeout=60) as source:
            return key, hashlib.file_digest(source, "sha256").hexdigest()

    with ThreadPoolExecutor(max_workers=len(COSIGN_ASSETS)) as executor:
        hashes = dict(executor.map(hash_asset, COSIGN_ASSETS.items()))
    render_bootstrap_installers(output, tag, hashes)


def render_bootstrap_installers(output, tag, hashes):
    """Rewrite generated assignments only after all bootstrap metadata validates."""
    if not re.fullmatch(r"v[0-9]+\.[0-9]+\.[0-9]+", tag) or set(hashes) != set(COSIGN_ASSETS):
        raise ValueError("invalid cosign bootstrap metadata")
    if any(not re.fullmatch(r"[a-f0-9]{64}", digest) for digest in hashes.values()):
        raise ValueError("invalid cosign bootstrap SHA-256")
    rendered = {}
    for name in ("install.sh", "install.ps1"):
        script = (ROOT / name).read_text(encoding="utf-8")
        values = {"VERSION": tag, **hashes}
        variables = ({"VERSION": "$CosignVersion", "WINDOWS_AMD64": "$CosignWindowsAmd64"}
                     if name.endswith("ps1") else {
                         key: f"COSIGN_{key}" for key in values if key != "WINDOWS_AMD64"})
        for key, variable in variables.items():
            separator = " = " if name.endswith("ps1") else "="
            pattern = rf"(?m)^({re.escape(variable + separator)})'[^']*'$"
            script, count = re.subn(pattern, lambda match: f"{match[1]}'{values[key]}'", script)
            if count != 1:
                raise ValueError(f"expected one generated {variable} assignment in {name}")
        if "@COSIGN_" in script:
            raise ValueError("unresolved cosign bootstrap placeholder")
        rendered[name] = script
    output.mkdir(parents=True, exist_ok=True)
    for name, script in rendered.items():
        (output / name).write_text(script, encoding="utf-8")
    (output / "cosign-bootstrap.json").write_text(json.dumps({
        "version": tag,
        "sha256": {COSIGN_ASSETS[key]: value for key, value in hashes.items()},
    }, indent=2, sort_keys=True) + "\n", encoding="utf-8")


def check_bootstrap_installers(output=None):
    """Verify committed source installers offline, then optionally stage them."""
    metadata = strict_json((ROOT / "cosign-bootstrap.json").read_bytes())
    hashes = metadata["sha256"]
    if set(hashes) != set(COSIGN_ASSETS.values()):
        raise ValueError("invalid cosign bootstrap assets")
    with tempfile.TemporaryDirectory() as temporary:
        rendered = Path(temporary)
        render_bootstrap_installers(rendered, metadata["version"], {
            key: hashes[asset] for key, asset in COSIGN_ASSETS.items()})
        for name in ("install.sh", "install.ps1", "cosign-bootstrap.json"):
            if (ROOT / name).read_text(encoding="utf-8") != (rendered / name).read_text(encoding="utf-8"):
                raise ValueError(f"{name} differs from cosign-bootstrap.json; run bootstrap-installers --write")
        if output is not None:
            output.mkdir(parents=True, exist_ok=True)
            for name in ("install.sh", "install.ps1", "cosign-bootstrap.json"):
                shutil.copy2(ROOT / name, output / name)


def checksums(output):
    assets = sorted(path for path in output.iterdir()
                    if path.name.endswith((".tar.gz", ".deb", ".pkg", ".zip", ".exe", ".mcpb", ".sh", ".ps1", "cosign-bootstrap.json")))
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
    parser.add_argument("command", choices=["check-bootstrap-installers", "bootstrap-installers", "version", "revocations", "stage", "installer", "archive", "mcpb", "mcpb-macos", "claude-plugin", "checksums", "update-manifest", "license-env", "linux-abi"])
    parser.add_argument("--write", action="store_true", help="refresh committed installers from the latest official cosign release")
    parser.add_argument("--tag")
    parser.add_argument(
        "--check-changelog", action="store_true",
        help="require Unreleased followed by a section for the workspace version",
    )
    parser.add_argument("--target", choices=TARGETS)
    parser.add_argument("--binary", type=Path)
    parser.add_argument("--work", type=Path, default=Path("staging"))
    parser.add_argument("--output", type=Path, default=Path("dist"))
    args = parser.parse_args()
    if args.command == "bootstrap-installers":
        if args.write:
            bootstrap_installers(ROOT)
        else:
            check_bootstrap_installers(args.output)
        return
    if args.command == "check-bootstrap-installers":
        check_bootstrap_installers()
        return
    if args.command == "license-env":
        write_license_env()
        return
    if args.command == "linux-abi":
        if args.binary is None:
            parser.error("--binary is required")
        try:
            required = check_linux_abi(args.binary)
        except (OSError, ValueError, subprocess.CalledProcessError) as error:
            parser.error(str(error))
        print(f"Linux binary requires glibc {required} or later.")
        return
    try:
        release_version = version(args.tag, check_changelog=args.check_changelog)
        if args.command == "revocations":
            validate_release_revocations(release_version)
            return
    except (OSError, ValueError) as error:
        parser.error(str(error))
    if args.command == "version":
        print(release_version)
    elif args.command == "update-manifest":
        update_manifest(args.output, release_version)
    elif args.command == "claude-plugin":
        claude_plugin(args.output, release_version, args.work / "claude-marketplace")
    elif args.command == "checksums":
        checksums(args.output)
    elif args.command == "mcpb-macos":
        universal_macos_bundle(args.output, release_version)
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
