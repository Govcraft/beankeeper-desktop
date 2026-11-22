import json
import os
import re
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest

from test_release import ROOT, release


@unittest.skipIf(os.name == "nt", "POSIX shell installer")
class ShellInstallerTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.fixtures = self.root / "fixtures"
        self.fixtures.mkdir()
        self.bin = self.root / "bin"
        self.bin.mkdir()
        self.destination = self.root / "installed"
        self.destination.mkdir()
        self.stem = "bk-0.1.0-x86_64-unknown-linux-gnu"
        binary = self.root / "bk"
        binary.write_text('#!/bin/sh\nif [ "$1" = --version ]; then printf "bk 0.1.0\\n"; else printf "%s\\n" "$*" >> "$BK_TEST_LOG"; fi\n')
        directory = release.stage(binary, "0.1.0", "x86_64-unknown-linux-gnu", self.root / "work")
        release.compress(directory, "0.1.0", "x86_64-unknown-linux-gnu", self.fixtures)
        release.checksums(self.fixtures)
        for command in ("sh", "awk", "tar", "install", "mktemp", "mkdir", "mv", "rm", "gzip", "sha256sum", "shasum", "grep", "jq"):
            if location := shutil.which(command):
                (self.bin / command).symlink_to(location)
        self.script("uname", 'print("Linux" if "-s" in sys.argv else "x86_64")')
        self.script("curl", '''
import shutil
from urllib.parse import urlparse
args = sys.argv[1:]
url = next(arg for arg in args if arg.startswith("https://"))
if url == "https://api.github.com/repos/Govcraft/beankeeper-desktop/releases?per_page=100":
    print(os.environ["FIXTURE_RELEASES"])
    sys.exit(0)
assert url.startswith("https://github.com/Govcraft/beankeeper-desktop/releases/download/beankeeper-desktop-v" + os.environ["FIXTURE_VERSION"] + "/"), url
path = Path(os.environ["FIXTURES"]) / Path(urlparse(url).path).name
if not path.exists():
    if "-w" in args: print(os.environ.get("BUNDLE_STATUS", "404"), end="")
    sys.exit(22)
shutil.copyfile(path, args[args.index("-o") + 1])
if "-w" in args: print("200", end="")
''')
        self.env = {**os.environ, "PATH": str(self.bin), "BK_VERSION": "0.1.0", "FIXTURE_VERSION": "0.1.0",
                    "BK_INSTALL": str(self.destination), "FIXTURES": str(self.fixtures),
                    "TMPDIR": str(self.root), "BK_TEST_LOG": str(self.root / "service-actions")}

    def script(self, name, code):
        path = self.bin / name
        path.write_text(f"#!{sys.executable}\nimport os, sys\nfrom pathlib import Path\n" + code + "\n")
        path.chmod(0o755)

    def run_installer(self):
        return subprocess.run(["/bin/sh", str(ROOT / "install.sh")], env=self.env,
                              capture_output=True, text=True)

    def cosign(self, fail=False):
        self.script("cosign", f'''
args = sys.argv[1:]
assert args[0] == "verify-blob"
assert args[args.index("--certificate-identity") + 1] == "https://github.com/Govcraft/beankeeper-desktop/.github/workflows/release.yml@refs/tags/beankeeper-desktop-v" + os.environ["FIXTURE_VERSION"]
assert args[args.index("--certificate-oidc-issuer") + 1] == "https://token.actions.githubusercontent.com"
with open(os.environ["FIXTURES"] + "/verified", "a") as log: log.write(Path(args[-1]).name + "\\n")
sys.exit({1 if fail else 0})
''')

    def bundles(self):
        for name in (f"{self.stem}.tar.gz", "checksums-sha256.txt"):
            (self.fixtures / f"{name}.sigstore").write_text("fixture bundle")

    @staticmethod
    def release_entry(version, target, *, missing=None, **overrides):
        archive = f"bk-{version}-{target}.tar.gz"
        names = (archive, f"{archive}.sigstore", "checksums-sha256.txt", "checksums-sha256.txt.sigstore")
        return {"tag_name": f"beankeeper-desktop-v{version}", "draft": False,
                "prerelease": "-rc." in version,
                "assets": [{"name": name} for name in names if name != missing], **overrides}

    def test_no_cosign_checks_checksum_and_installs(self):
        result = self.run_installer()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("cosign not found", result.stderr)
        self.assertTrue((self.destination / "bk").exists())

    def test_service_is_installed_by_default_and_reinstall_stops_before_upgrading(self):
        result = self.run_installer()
        self.assertEqual(result.returncode, 0, result.stderr)
        actions = self.root / "service-actions"
        self.assertEqual(actions.read_text().splitlines(), ["service install --no-agents", "agents connect --all"])
        result = self.run_installer()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(actions.read_text().splitlines(),
                         ["service install --no-agents", "agents connect --all", "service --help",
                          "service stop", "service install --no-agents", "agents connect --all"])

    def test_service_install_has_environment_and_flag_opt_outs(self):
        self.env["BK_NO_SERVICE"] = "1"
        result = self.run_installer()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertFalse((self.root / "service-actions").exists())
        (self.destination / "bk").unlink()
        self.env.pop("BK_NO_SERVICE")
        result = subprocess.run(["/bin/sh", str(ROOT / "install.sh"), "--no-service"],
                                env=self.env, capture_output=True, text=True)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertFalse((self.root / "service-actions").exists())

    def test_agent_opt_out_keeps_background_hosting(self):
        self.env["BK_NO_AGENTS"] = "1"
        result = self.run_installer()
        self.assertEqual(result.returncode, 0, result.stderr)
        actions = self.root / "service-actions"
        self.assertEqual(actions.read_text().splitlines(), ["service install --no-agents"])
        (self.destination / "bk").unlink()
        actions.unlink()
        self.env.pop("BK_NO_AGENTS")
        result = subprocess.run(["/bin/sh", str(ROOT / "install.sh"), "--no-agents"],
                                env=self.env, capture_output=True, text=True)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(actions.read_text().splitlines(), ["service install --no-agents"])

    def test_tampered_archive_never_replaces_installed_binary(self):
        (self.destination / "bk").write_text("previous version")
        (self.fixtures / f"{self.stem}.tar.gz").write_bytes(b"tampered")
        result = self.run_installer()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("checksum mismatch", result.stderr)
        self.assertEqual((self.destination / "bk").read_text(), "previous version")

    def test_invalid_version_is_rejected_before_downloading(self):
        for value in ("../main", "0.1", "0.1.0.1", "0.1.0-beta", "0.1.0-rc", "v0.1.0-rc.1", "0.1.0\n../main"):
            with self.subTest(value=value):
                self.env["BK_VERSION"] = value
                result = self.run_installer()
                self.assertNotEqual(result.returncode, 0)
                self.assertIn("invalid version", result.stderr)
                self.assertFalse((self.destination / "bk").exists())

    def test_missing_checksum_fails_before_installation(self):
        (self.fixtures / "checksums-sha256.txt").write_text("")
        result = self.run_installer()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("no checksum found", result.stderr)
        self.assertFalse((self.destination / "bk").exists())

    def test_present_bundles_verify_both_with_exact_release_identity(self):
        self.cosign()
        self.bundles()
        result = self.run_installer()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual((self.fixtures / "verified").read_text().splitlines(),
                         [f"{self.stem}.tar.gz", "checksums-sha256.txt"])

    def test_legacy_404_falls_back_to_checksum(self):
        self.cosign()
        result = self.run_installer()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stderr.count("HTTP 404"), 2)

    def test_agent_install_requires_cosign_and_both_bundles(self):
        self.env["BK_REQUIRE_SIGNATURES"] = "1"
        missing_cosign = self.run_installer()
        self.assertNotEqual(missing_cosign.returncode, 0)
        self.assertIn("cosign is required", missing_cosign.stderr)
        self.assertFalse((self.destination / "bk").exists())

        self.cosign()
        (self.fixtures / f"{self.stem}.tar.gz.sigstore").write_text("fixture bundle")
        missing_checksum_bundle = self.run_installer()
        self.assertNotEqual(missing_checksum_bundle.returncode, 0)
        self.assertIn("required Sigstore bundle missing for checksums-sha256.txt", missing_checksum_bundle.stderr)
        self.assertFalse((self.destination / "bk").exists())

        self.bundles()
        verified = self.run_installer()
        self.assertEqual(verified.returncode, 0, verified.stderr)
        self.assertTrue((self.destination / "bk").is_file())

    def test_agent_install_rejects_older_binary_before_replacing_installed_copy(self):
        binary = self.root / "bk"
        binary.write_text('#!/bin/sh\n[ "$*" = "service install --help" ]\n')
        directory = release.stage(binary, "0.1.0", "x86_64-unknown-linux-gnu", self.root / "old-work")
        release.compress(directory, "0.1.0", "x86_64-unknown-linux-gnu", self.fixtures)
        release.checksums(self.fixtures)
        self.cosign()
        self.bundles()
        self.env.pop("BK_VERSION")
        self.env.update(BK_REQUIRE_SIGNATURES="1", BK_REQUIRE_AGENT_SETUP="1",
                        FIXTURE_RELEASES=json.dumps([self.release_entry("0.1.0", "x86_64-unknown-linux-gnu")]))
        installed = self.destination / "bk"
        installed.write_text("previous executable")

        result = self.run_installer()

        self.assertNotEqual(result.returncode, 0)
        self.assertIn("lacks managed service or agent registration commands", result.stderr)
        self.assertEqual(installed.read_text(), "previous executable")

    def test_guided_discovery_refuses_when_no_release_includes_both_signature_bundles(self):
        self.env.pop("BK_VERSION")
        self.env.update(BK_REQUIRE_SIGNATURES="1", BK_REQUIRE_AGENT_SETUP="1",
                        FIXTURE_RELEASES=json.dumps([self.release_entry(
                            "0.2.0-rc.1", "x86_64-unknown-linux-gnu", missing="checksums-sha256.txt.sigstore",
                        )]))

        result = self.run_installer()

        self.assertNotEqual(result.returncode, 0)
        self.assertIn("no published release with signature bundles found", result.stderr)
        self.assertFalse((self.destination / "bk").exists())

    def test_install_documentation_has_no_hard_coded_desktop_release(self):
        for path, heading in (
            ("SKILL.md", "Install and set up Beankeeper on this machine"),
            ("README.md", "Release packages"),
            ("docs/RELEASE.md", "Checksum-verifying command-line installers"),
        ):
            with self.subTest(path=path):
                _, separator, rest = (ROOT / path).read_text().partition(f"\n## {heading}\n")
                self.assertTrue(separator, f"missing installation section: {heading}")
                self.assertNotRegex(rest.split("\n## ", 1)[0], r"\b(?:v)?[0-9]+\.[0-9]+\.[0-9]+\b")

    def test_skill_commands_install_verified_binary_and_connect(self):
        match = re.search(r"```sh\n(.*?)```", (ROOT / "SKILL.md").read_text(), re.DOTALL)
        self.assertIsNotNone(match, "missing Linux/macOS command block")
        script = self.root / "skill.sh"
        script.write_text(match.group(1))

        expected = self.root / "calls"
        binary = self.root / "bk"
        self.env.pop("BK_VERSION")
        fixture_curl = self.bin / "curl-fixture"
        (self.bin / "curl").rename(fixture_curl)
        self.script("curl", """
import shutil
args = sys.argv[1:]
if "https://raw.githubusercontent.com/Govcraft/beankeeper-desktop/main/install.sh" in args:
    shutil.copyfile(os.environ["BK_TEST_INSTALLER"], args[args.index("-o") + 1])
else:
    os.execv(os.environ["BK_TEST_FIXTURE_CURL"], [os.environ["BK_TEST_FIXTURE_CURL"], *args])
""")
        for platform, target, version in [
            ("Linux", "x86_64-unknown-linux-gnu", "0.2.0-rc.10"),
            ("Darwin", "aarch64-apple-darwin", "0.2.0"),
        ]:
            with self.subTest(platform=platform):
                fake_home = self.root / platform
                fake_home.mkdir()
                self.stem = f"bk-{version}-{target}"
                binary.write_text(
                    '#!/bin/sh\n'
                    f'if [ "$*" = "--version" ]; then printf "bk {version}\\n"; exit 0; fi\n'
                    'if [ "${3:-}" = --help ]; then exit 0; fi\n'
                    'printf "%s\\n" "$*" >> "$BK_TEST_CALLS"\n'
                )
                for path in self.fixtures.iterdir():
                    path.unlink()
                directory = release.stage(binary, version, target, self.root / "guide-work")
                release.compress(directory, version, target, self.fixtures)
                release.checksums(self.fixtures)
                self.bundles()
                self.cosign()
                arch = "aarch64" if platform == "Darwin" else "x86_64"
                self.script("uname", f'print({platform!r} if "-s" in sys.argv else {arch!r})')
                candidates = [
                    self.release_entry("0.3.0-rc.1", target, draft=True),
                    self.release_entry("0.3.0", target, missing=f"bk-0.3.0-{target}.tar.gz"),
                    self.release_entry("0.4.0-rc.1", target, prerelease=False),
                    self.release_entry(version, target),
                    self.release_entry("0.2.0-rc.9", target),
                    self.release_entry("0.1.0", target),
                ]
                if platform == "Darwin":
                    candidates.append(self.release_entry("0.2.0-rc.10", target))
                result = subprocess.run(
                    ["/bin/sh", str(script)],
                    env={**self.env, "HOME": str(fake_home), "BK_TEST_CALLS": str(expected),
                         "FIXTURE_VERSION": version, "FIXTURE_RELEASES": json.dumps(candidates),
                         "BK_TEST_INSTALLER": str(ROOT / "install.sh"), "BK_TEST_FIXTURE_CURL": str(fixture_curl)},
                    capture_output=True, text=True,
                )
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertTrue((fake_home / ".local/bin/bk").exists())
                self.assertEqual(expected.read_text().splitlines(), [
                    "service install --no-agents", "agents connect --all --json", "service status --json",
                ])
                expected.unlink()

    def test_invalid_bundle_and_non_404_download_errors_fail_closed(self):
        self.cosign(fail=True)
        self.bundles()
        result = self.run_installer()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("Sigstore verification failed", result.stderr)
        self.assertFalse((self.destination / "bk").exists())
        for path in self.fixtures.glob("*.sigstore"):
            path.unlink()
        self.env["BUNDLE_STATUS"] = "503"
        result = self.run_installer()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("could not download Sigstore bundle", result.stderr)

    def test_apple_signing_skip_requires_no_macos_tools(self):
        for enabled in ("false", "true"):
            env = {**self.env, "ENABLE_APPLE_SIGNING": enabled,
                   "APPLE_CERTIFICATES_P12_BASE64": ""}
            result = subprocess.run(["/bin/bash", str(ROOT / "scripts/sign-macos.sh")],
                                    env=env, capture_output=True, text=True)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertIn("Apple signing/notarization skipped", result.stdout)


if __name__ == "__main__":
    unittest.main()
