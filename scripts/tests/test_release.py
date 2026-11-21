import hashlib
import os
import sys
import importlib.util
import io
import json
from pathlib import Path
import shutil
import subprocess
import tarfile
import tempfile
import unittest
from unittest.mock import patch
import zipfile

ROOT = Path(__file__).resolve().parents[2]
spec = importlib.util.spec_from_file_location("release", ROOT / "scripts/release.py")
release = importlib.util.module_from_spec(spec)
spec.loader.exec_module(release)


class PackagingTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.binary = self.root / "input-binary"
        self.binary.write_bytes(b"embedded UI and executable\n")
        self.output = self.root / "dist"
        self.output.mkdir()

    def test_tag_must_match_desktop_version(self):
        value = release.version()
        self.assertEqual(release.version(f"beankeeper-desktop-v{value}"), value)
        for tag in ("v0.1.0", "beankeeper-cli-v99.0.0", "main"):
            with self.assertRaises(ValueError):
                release.version(tag)


    def test_rc_tag_matches_and_debian_version_uses_tilde(self):
        self.assertTrue(release.valid_version("0.1.0-rc.1"))
        self.assertTrue(release.is_prerelease("0.1.0-rc.1"))
        self.assertFalse(release.is_prerelease("0.1.0"))
        self.assertEqual(release.debian_upstream_version("0.1.0-rc.1"), "0.1.0~rc.1")
        self.assertEqual(release.debian_upstream_version("0.1.0"), "0.1.0")
        with self.assertRaises(ValueError):
            release.version("beankeeper-desktop-v0.1.0")
        self.assertEqual(release.version("beankeeper-desktop-v0.1.0-rc.1"), "0.1.0-rc.1")
        directory = release.stage(self.binary, "0.1.0-rc.1", "x86_64-unknown-linux-gnu", self.root / "work")
        deb = self.output / "bk-0.1.0-rc.1-x86_64-unknown-linux-gnu.deb"
        release.debian(directory, "0.1.0-rc.1", "x86_64-unknown-linux-gnu", deb)
        with deb.open("rb") as data:
            self.assertEqual(data.read(8), b"!<arch>\n")
            members = {}
            while header := data.read(60):
                size = int(header[48:58])
                members[header[:16].decode().strip().rstrip("/")] = data.read(size)
                if size % 2:
                    data.read(1)
        with tarfile.open(fileobj=io.BytesIO(members["control.tar.gz"])) as control:
            metadata = control.extractfile("control").read().decode()
        self.assertIn("Version: 0.1.0~rc.1\n", metadata)
        self.assertNotIn("Version: 0.1.0-rc.1\n", metadata)

    def test_supported_archive_layouts_preserve_binary_and_license(self):
        for target in release.TARGETS:
            with self.subTest(target=target):
                directory = release.stage(self.binary, "0.1.0", target, self.root / "work")
                release.compress(directory, "0.1.0", target, self.output)
                stem = release.name("0.1.0", target)
                if "windows" in target:
                    with zipfile.ZipFile(self.output / f"{stem}.zip") as archive:
                        self.assertEqual(archive.read(f"{stem}/bk.exe"), self.binary.read_bytes())
                        self.assertIn(f"{stem}/LICENSE.md", archive.namelist())
                else:
                    with tarfile.open(self.output / f"{stem}.tar.gz") as archive:
                        self.assertEqual(archive.extractfile(f"{stem}/bk").read(), self.binary.read_bytes())
                        self.assertEqual(archive.getmember(f"{stem}/bk").mode, 0o755)
                        self.assertIn(f"{stem}/LICENSE.md", archive.getnames())

    def test_debian_architecture_payload_permissions_and_conflicts(self):
        for target in ("x86_64-unknown-linux-gnu", "aarch64-unknown-linux-gnu"):
            directory = release.stage(self.binary, "0.1.0", target, self.root / "work")
            release.installer(directory, "0.1.0", target, self.output)
            package = self.output / f"{directory.name}.deb"
            with package.open("rb") as data:
                self.assertEqual(data.read(8), b"!<arch>\n")
                members = {}
                while header := data.read(60):
                    self.assertEqual(header[58:], b"`\n")
                    size = int(header[48:58])
                    members[header[:16].decode().strip().rstrip("/")] = data.read(size)
                    if size % 2:
                        data.read(1)
            self.assertEqual(members["debian-binary"], b"2.0\n")
            with tarfile.open(fileobj=io.BytesIO(members["control.tar.gz"])) as control:
                metadata = control.extractfile("control").read().decode()
                self.assertIn(f"Architecture: {release.TARGETS[target]}\n", metadata)
                self.assertIn("Conflicts: beankeeper-cli, bk, bk-bin\n", metadata)
                self.assertEqual(control.getnames(), ["control"])
            with tarfile.open(fileobj=io.BytesIO(members["data.tar.gz"])) as payload:
                self.assertEqual(payload.extractfile("usr/bin/bk").read(), self.binary.read_bytes())
                self.assertEqual(payload.getmember("usr/bin/bk").mode, 0o755)
                self.assertEqual(payload.getmember("usr/bin/bk").uid, 0)
            if shutil.which("dpkg-deb"):
                subprocess.run(["dpkg-deb", "--info", str(package)], check=True, capture_output=True)

    def test_macos_installer_root_contains_binary_and_license(self):
        target = "aarch64-apple-darwin"
        directory = release.stage(self.binary, "0.1.0", target, self.root / "work")
        with patch.object(release.subprocess, "run") as run:
            release.installer(directory, "0.1.0", target, self.output)
        arguments = run.call_args.args[0]
        root = Path(arguments[arguments.index("--root") + 1])
        self.assertEqual((root / "usr/local/bin/bk").read_bytes(), self.binary.read_bytes())
        self.assertEqual((root / "usr/local/share/doc/beankeeper-desktop/LICENSE.md").read_bytes(),
                         (ROOT / "LICENSE.md").read_bytes())

    def test_checksums_cover_every_final_asset_and_exclude_bundles(self):
        for suffix in ("tar.gz", "deb", "pkg", "exe", "zip"):
            (self.output / f"bk-0.1.0-target.{suffix}").write_bytes(suffix.encode())
        (self.output / "bk-0.1.0-target.zip.sigstore").write_text("not an asset")
        release.checksums(self.output)
        manifest = (self.output / "checksums-sha256.txt").read_text().splitlines()
        self.assertEqual(len(manifest), 5)
        for line in manifest:
            digest, filename = line.split("  ")
            self.assertEqual(digest, hashlib.sha256((self.output / filename).read_bytes()).hexdigest())

    def test_update_manifest_hashes_matching_archives_and_uses_trusted_release_date(self):
        for target in release.TARGETS:
            directory = release.stage(self.binary, "0.1.0", target, self.root / "work")
            release.compress(directory, "0.1.0", target, self.output)
        release.update_manifest(self.output, "0.1.0", "2026-11-20T12:00:00Z")
        manifest = json.loads((self.output / "update-manifest.json").read_text())
        self.assertEqual(manifest["version"], "0.1.0")
        self.assertEqual(manifest["released_at"], "2026-11-20T12:00:00Z")
        self.assertEqual(set(manifest["artifacts"]), set(release.TARGETS))
        for target, artifact in manifest["artifacts"].items():
            extension = "zip" if "windows" in target else "tar.gz"
            archive = self.output / f"bk-0.1.0-{target}.{extension}"
            self.assertEqual(artifact["sha256"], hashlib.sha256(archive.read_bytes()).hexdigest())

    def test_missing_assets_and_invalid_names_are_rejected(self):
        with self.assertRaises(ValueError):
            release.checksums(self.output)
        for value, target in (("../../escape", next(iter(release.TARGETS))), ("0.1.0", "unrecognized")):
            with self.assertRaises(ValueError):
                release.name(value, target)

    def test_windows_version_info_is_numeric_for_stable_and_rc(self):
        self.assertEqual(release.windows_version_info("0.1.0"), "0.1.0.0")
        self.assertEqual(release.windows_version_info("0.1.0-rc.1"), "0.1.0.1")
        self.assertEqual(release.windows_version_info("1.2.3-rc.12"), "1.2.3.12")
        with self.assertRaises(ValueError):
            release.windows_version_info("0.1.0-beta.1")

    def test_windows_installer_passes_numeric_version_info(self):
        target = "x86_64-pc-windows-msvc"
        directory = release.stage(self.binary, "0.1.0-rc.1", target, self.root / "work")
        with patch.object(release.subprocess, "run") as run, patch.object(
            release.shutil, "which", return_value="ISCC.exe"
        ):
            release.installer(directory, "0.1.0-rc.1", target, self.output)
        arguments = run.call_args.args[0]
        self.assertIn("/DReleaseVersion=0.1.0-rc.1", arguments)
        self.assertIn("/DVersionInfoVersion=0.1.0.1", arguments)

    def test_license_embed_check_compares_trust_json_and_ignores_runtime_overrides(self):
        import importlib.util
        spec = importlib.util.spec_from_file_location(
            "check_license_embed", ROOT / "scripts/check-license-embed.py"
        )
        checker = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(checker)
        payload = {"service_url": "https://license.example.test", "public_keys": {"k": "pub"}}
        fake = self.root / "bk"
        fake.write_text(
            f"#!{sys.executable}\n"
            "import json, os, sys\n"
            "assert 'BEANKEEPER_LICENSE_SERVICE_URL' not in os.environ\n"
            "assert 'BEANKEEPER_LICENSE_PUBLIC_KEYS_JSON' not in os.environ\n"
            "assert 'BEANKEEPER_LICENSE_KEYS_FILE' not in os.environ\n"
            "json.dump(" + repr(payload) + ", sys.stdout)\n"
        )
        fake.chmod(0o755)
        env = {
            "BEANKEEPER_LICENSE_SERVICE_URL": "https://license.example.test",
            "BEANKEEPER_LICENSE_PUBLIC_KEYS_JSON": json.dumps(payload["public_keys"]),
            "BEANKEEPER_LICENSE_KEYS_FILE": "/tmp/should-not-be-used",
            "PATH": os.environ.get("PATH", "/usr/bin"),
        }
        checker.check(str(fake), env)
        env["BEANKEEPER_LICENSE_SERVICE_URL"] = "https://wrong.example"
        with self.assertRaises(SystemExit):
            checker.check(str(fake), env)




if __name__ == "__main__":
    unittest.main()
