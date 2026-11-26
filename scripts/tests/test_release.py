import hashlib
import importlib.util
import io
import json
import os
import shutil
import struct
import subprocess
import sys
import tarfile
import tempfile
import unittest
import zipfile
from pathlib import Path
from unittest.mock import patch

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

    def test_pr_keeps_version_and_adds_unreleased_notes(self):
        (self.root / "Cargo.toml").write_text('[workspace.package]\nversion = "1.2.3"\n')
        changelog = self.root / "CHANGELOG.md"
        previous = "## [1.2.3] - 2026-11-22\n\n- A shipped change.\n"
        with patch.object(release, "ROOT", self.root):
            for notes in ("", "- A change for the next release.\n\n"):
                with self.subTest(notes=notes):
                    changelog.write_text(f"# Changelog\n\n## [Unreleased]\n\n{notes}{previous}")
                    self.assertEqual(release.version(check_changelog=True), "1.2.3")

    def test_release_pr_and_tag_use_the_same_version_section(self):
        (self.root / "Cargo.toml").write_text('[workspace.package]\nversion = "1.3.0-rc.1"\n')
        changelog = self.root / "CHANGELOG.md"
        changelog.write_text(
            "# Changelog\n\n## [Unreleased]\n\n"
            "## [1.3.0-rc.1] - 2026-11-23\n\n- A change for the release.\n\n"
            "## [1.2.3] - 2026-11-22\n"
        )
        with patch.object(release, "ROOT", self.root):
            self.assertEqual(release.version(check_changelog=True), "1.3.0-rc.1")
            self.assertEqual(release.version("beankeeper-desktop-v1.3.0-rc.1"), "1.3.0-rc.1")
            for tag in ("beankeeper-desktop-v1.3.0", "v1.3.0-rc.1", "beankeeper-cli-v1.3.0-rc.1", "main"):
                with self.subTest(tag=tag), self.assertRaisesRegex(
                    ValueError, r"tag must be beankeeper-desktop-v1\.3\.0-rc\.1"
                ):
                    release.version(tag)

    def test_pr_and_tag_reject_missing_or_stale_changelog_headings(self):
        (self.root / "Cargo.toml").write_text('[workspace.package]\nversion = "1.2.3"\n')
        changelog = self.root / "CHANGELOG.md"
        for headings, error in (
            ("## [1.2.3]\n## [Unreleased]", r"must begin its sections with ## \[Unreleased\]"),
            ("## [Unreleased]\n", r"must have ## \[1.2.3\] directly below Unreleased"),
            (
                "## [Unreleased]\n## [1.2.4]\n## [1.2.3]",
                r"must have ## \[1.2.3\] directly below Unreleased",
            ),
            ("## [Unreleased]\n## [1.2.30]", r"must have ## \[1.2.3\] directly below Unreleased"),
        ):
            changelog.write_text(f"# Changelog\n\n{headings}\n")
            with self.subTest(headings=headings), patch.object(release, "ROOT", self.root):
                with self.assertRaisesRegex(ValueError, error):
                    release.version(check_changelog=True)
                with self.assertRaisesRegex(ValueError, error):
                    release.version("beankeeper-desktop-v1.2.3")
                self.assertEqual(release.version(), "1.2.3")

    def test_debian_prerelease_version_uses_tilde(self):
        self.assertTrue(release.valid_version("0.1.0-rc.1"))
        self.assertTrue(release.is_prerelease("0.1.0-rc.1"))
        self.assertFalse(release.is_prerelease("0.1.0"))
        self.assertEqual(release.debian_upstream_version("0.1.0-rc.1"), "0.1.0~rc.1")
        self.assertEqual(release.debian_upstream_version("0.1.0"), "0.1.0")
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

    def test_linux_abi_gate_accepts_the_ceiling_and_ignores_exported_versions(self):
        symbols = """Version definition section '.gnu.version_d':
            Name: GLIBC_2.39
            Version needs section '.gnu.version_r' contains 1 entry:
            Name: GLIBC_2.9 Flags: none
            Name: GLIBC_2.35 Flags: none
        """
        with patch.object(release.subprocess, "run", return_value=subprocess.CompletedProcess(
            args=["readelf"], returncode=0, stdout=symbols
        )) as readelf:
            self.assertEqual(release.check_linux_abi(self.binary), "2.35")
            self.assertEqual(
                readelf.call_args.args[0],
                ["readelf", "--version-info", "--wide", str(self.binary)],
            )

    def test_linux_abi_gate_rejects_incompatible_or_unmeasurable_requirements(self):
        for version, error in (
            ("GLIBC_2.39", "requires glibc 2.39"),
            ("GLIBC_2.35.1", "requires glibc 2.35.1"),
            ("GLIBC_ABI_DT_RELR", "Unexpected glibc version requirement"),
            ("GLIBC_PRIVATE", "Unexpected glibc version requirement"),
            ("GCC_3.0", "no measurable glibc version requirements"),
        ):
            with self.subTest(version=version):
                symbols = f"Version needs section '.gnu.version_r':\n Name: {version} Flags: none"
                with patch.object(release.subprocess, "run", return_value=subprocess.CompletedProcess(
                    args=["readelf"], returncode=0, stdout=symbols
                )), self.assertRaisesRegex(ValueError, error):
                    release.check_linux_abi(self.binary)

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
                self.assertIn("Depends: libc6 (>= 2.35), libgcc-s1\n", metadata)
                self.assertIn("Conflicts: beankeeper-cli, bk, bk-bin\n", metadata)
                self.assertEqual(control.getnames(), ["control", "postinst"])
                self.assertEqual(control.getmember("postinst").mode, 0o755)
                hook = control.extractfile("postinst").read().decode()
                self.assertIn('BK_NO_SERVICE', hook)
                self.assertIn('runuser -u "$owner"', hook)
                self.assertIn('run_as_owner service install --no-agents', hook)
                self.assertIn('run_as_owner agents connect --all', hook)
                self.assertIn('BK_NO_AGENTS', hook)
            with tarfile.open(fileobj=io.BytesIO(members["data.tar.gz"])) as payload:
                self.assertEqual(payload.extractfile("usr/bin/bk").read(), self.binary.read_bytes())
                self.assertEqual(payload.getmember("usr/bin/bk").mode, 0o755)
                self.assertEqual(payload.getmember("usr/bin/bk").uid, 0)
                for member in ("usr/bin", "usr/share/doc/beankeeper-desktop"):
                    directory = payload.getmember(member)
                    self.assertTrue(directory.isdir())
                    self.assertEqual(directory.mode, 0o755)
                    self.assertEqual(directory.uid, 0)
            if shutil.which("dpkg-deb"):
                subprocess.run(["dpkg-deb", "--info", str(package)], check=True, capture_output=True)

    def test_macos_installer_root_contains_binary_and_license(self):
        target = "aarch64-apple-darwin"
        directory = release.stage(self.binary, "0.1.0", target, self.root / "work")
        with patch.object(release.subprocess, "run") as run:
            release.installer(directory, "0.1.0", target, self.output)
        arguments = run.call_args.args[0]
        root = Path(arguments[arguments.index("--root") + 1])
        hooks = Path(arguments[arguments.index("--scripts") + 1])
        self.assertIn('run_as_owner service install --no-agents', (hooks / 'postinstall').read_text())
        self.assertIn('run_as_owner agents connect --all', (hooks / 'postinstall').read_text())
        self.assertIn('BK_NO_AGENTS', (hooks / 'postinstall').read_text())
        self.assertIn('BK_NO_SERVICE', (hooks / 'postinstall').read_text())
        self.assertEqual((root / "usr/local/bin/bk").read_bytes(), self.binary.read_bytes())
        self.assertEqual((root / "usr/local/share/doc/beankeeper-desktop/LICENSE.md").read_bytes(),
                         (ROOT / "LICENSE.md").read_bytes())

    def test_mcp_bundle_matches_each_native_release_target(self):
        for target in release.TARGETS:
            with self.subTest(target=target):
                directory = release.stage(self.binary, "0.1.0-rc.1", target, self.root / "work")
                release.mcp_bundle(directory, "0.1.0-rc.1", target, self.output)
                with zipfile.ZipFile(self.output / f"{directory.name}.mcpb") as archive:
                    manifest = json.loads(archive.read("manifest.json"))
                    platform = (
                        "win32" if "windows" in target else "darwin" if "darwin" in target else "linux"
                    )
                    binary = "server/bk.exe" if platform == "win32" else "server/bk"
                    self.assertEqual(manifest["version"], "0.1.0-rc.1")
                    self.assertEqual(manifest["compatibility"]["platforms"], [platform])
                    self.assertEqual(manifest["server"]["type"], "binary")
                    self.assertEqual(manifest["server"]["entry_point"], binary)
                    self.assertEqual(
                        manifest["server"]["mcp_config"],
                        {"command": "${__dirname}/" + binary, "args": ["mcp", "extension"]},
                    )
                    member = archive.getinfo(binary)
                    self.assertEqual(member.create_system, 3)
                    self.assertEqual((member.external_attr >> 16) & 0o777, 0o755)
                    self.assertEqual(archive.read(member), self.binary.read_bytes())
                    self.assertEqual(archive.read("SKILL.md"), (ROOT / "SKILL.md").read_bytes())
                    self.assertEqual(archive.read("LICENSE.md"), (ROOT / "LICENSE.md").read_bytes())
                    icon = archive.read(manifest["icon"])
                    self.assertEqual(icon[:8], b"\x89PNG\r\n\x1a\n")
                    self.assertEqual(icon[8:16], b"\x00\x00\x00\rIHDR")
                    width, height = struct.unpack(">II", icon[16:24])
                    self.assertGreaterEqual(width, 256)
                    self.assertGreaterEqual(height, 256)
                    # Pin the reviewed two-bean render, not whatever source file
                    # happens to be present when the bundle is built.
                    self.assertEqual(
                        hashlib.sha256(icon).hexdigest(),
                        "df800be58153b2c68c71f25466dec67fc358607d993352a9641550b2393a1da6",
                    )

    def test_universal_macos_bundle_uses_both_packaged_slices(self):
        release_version = "1.2.3"
        targets = ("aarch64-apple-darwin", "x86_64-apple-darwin")
        for target in targets:
            self.binary.write_bytes(target.encode())
            staged = release.stage(self.binary, release_version, target, self.root / "work")
            release.mcp_bundle(staged, release_version, target, self.output)

        def lipo(arguments, *, check):
            self.assertTrue(check)
            if "-create" in arguments:
                self.assertEqual([path.read_bytes() for path in arguments[3:5]],
                                 [target.encode() for target in targets])
                arguments[-1].write_bytes(b"fat Mach-O fixture")

        with (patch.object(release.subprocess, "run", side_effect=lipo) as run,
              patch.object(release.subprocess, "check_output", return_value="bk 1.2.3\n")):
            release.universal_macos_bundle(self.output, release_version)
            self.assertEqual(run.call_count, 2)
            universal = run.call_args_list[0].args[0][-1]
            run.assert_called_with(
                ["xcrun", "lipo", universal, "-verify_arch", "arm64", "x86_64"], check=True,
            )

        filename = "bk-1.2.3-universal-apple-darwin.mcpb"
        with zipfile.ZipFile(self.output / filename) as archive:
            manifest = json.loads(archive.read("manifest.json"))
            self.assertEqual(manifest["compatibility"]["platforms"], ["darwin"])
            self.assertEqual(manifest["server"]["entry_point"], "server/bk")
            self.assertEqual(archive.read("server/bk"), b"fat Mach-O fixture")
        release.checksums(self.output)
        self.assertIn(f"  {filename}\n", (self.output / "checksums-sha256.txt").read_text())

    def test_skill_teaches_owner_reply_shape(self):
        guide = (ROOT / "SKILL.md").read_text(encoding="utf-8")
        _, heading, rest = guide.partition("\n## How to talk to the owner\n")
        self.assertTrue(heading, "missing owner voice section")
        section = rest.split("\n## ", 1)[0]
        for phrase in (
            "Lead with the answer or outcome",
            "After a change",
            "receipt number",
            "You could ask:",
            "Leave out “As an AI…,” “Anything else?”",
        ):
            with self.subTest(phrase=phrase):
                self.assertIn(phrase, section)

    def test_skill_teaches_financial_chart_semantics(self):
        guide = (ROOT / "SKILL.md").read_text(encoding="utf-8")
        _, heading, rest = guide.partition("\n### Color financial charts by meaning\n")
        self.assertTrue(heading, "missing financial chart guidance")
        section = rest.split("\nCall `ui_get_link`", 1)[0]
        for requirement in (
            "never by a value's sign",
            "Liabilities and debt must use a neutral or warning color",
            "Never use green or the same positive/cash color",
            "Net worth combines assets and debt",
            "Negative net worth must use a neutral",
            "color is never the only distinction",
            "Do not negate debt",
            "Do not infer roles from a display name or amount sign",
            "If the chart client cannot",
            "table instead of a",
            "\u2212$500.00",
            "**fictional**",
        ):
            with self.subTest(requirement=requirement):
                self.assertIn(requirement, " ".join(section.split()))

    def test_checksums_cover_every_final_asset_and_exclude_bundles(self):
        for suffix in ("tar.gz", "deb", "pkg", "exe", "zip", "mcpb"):
            (self.output / f"bk-0.1.0-target.{suffix}").write_bytes(suffix.encode())
        (self.output / "bk-0.1.0-target.zip.sigstore").write_text("not an asset")
        release.checksums(self.output)
        manifest = (self.output / "checksums-sha256.txt").read_text().splitlines()
        self.assertEqual(len(manifest), 6)
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
