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
        binary.write_text('#!/bin/sh\nprintf "bk 0.1.0\\n"\n')
        directory = release.stage(binary, "0.1.0", "x86_64-unknown-linux-gnu", self.root / "work")
        release.compress(directory, "0.1.0", "x86_64-unknown-linux-gnu", self.fixtures)
        release.checksums(self.fixtures)
        for command in ("sh", "awk", "tar", "install", "mktemp", "mkdir", "rm", "gzip", "sha256sum", "shasum", "grep"):
            if location := shutil.which(command):
                (self.bin / command).symlink_to(location)
        self.script("uname", 'print("Linux" if "-s" in sys.argv else "x86_64")')
        self.script("curl", '''
import shutil
from urllib.parse import urlparse
args = sys.argv[1:]
url = next(arg for arg in args if arg.startswith("https://"))
assert url.startswith("https://github.com/Govcraft/beankeeper-desktop/releases/download/beankeeper-desktop-v0.1.0/"), url
path = Path(os.environ["FIXTURES"]) / Path(urlparse(url).path).name
if not path.exists():
    if "-w" in args: print(os.environ.get("BUNDLE_STATUS", "404"), end="")
    sys.exit(22)
shutil.copyfile(path, args[args.index("-o") + 1])
if "-w" in args: print("200", end="")
''')
        self.env = {**os.environ, "PATH": str(self.bin), "BK_VERSION": "0.1.0",
                    "BK_INSTALL": str(self.destination), "FIXTURES": str(self.fixtures),
                    "TMPDIR": str(self.root)}

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
assert args[args.index("--certificate-identity") + 1] == "https://github.com/Govcraft/beankeeper-desktop/.github/workflows/release.yml@refs/tags/beankeeper-desktop-v0.1.0"
assert args[args.index("--certificate-oidc-issuer") + 1] == "https://token.actions.githubusercontent.com"
with open(os.environ["FIXTURES"] + "/verified", "a") as log: log.write(Path(args[-1]).name + "\\n")
sys.exit({1 if fail else 0})
''')

    def bundles(self):
        for name in (f"{self.stem}.tar.gz", "checksums-sha256.txt"):
            (self.fixtures / f"{name}.sigstore").write_text("fixture bundle")

    def test_no_cosign_checks_checksum_and_installs(self):
        result = self.run_installer()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("cosign not found", result.stderr)
        self.assertTrue((self.destination / "bk").exists())

    def test_tampered_archive_never_replaces_installed_binary(self):
        (self.destination / "bk").write_text("previous version")
        (self.fixtures / f"{self.stem}.tar.gz").write_bytes(b"tampered")
        result = self.run_installer()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("checksum mismatch", result.stderr)
        self.assertEqual((self.destination / "bk").read_text(), "previous version")

    def test_invalid_version_is_rejected_before_downloading(self):
        for value in ("../main", "0.1", "0.1.0.1", "0.1.0-beta", "0.1.0-rc", "v0.1.0-rc.1"):
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

    def test_skill_commands_install_verified_binary_and_connect(self):
        match = re.search(r"```sh\n(.*?)```", (ROOT / "SKILL.md").read_text(), re.DOTALL)
        self.assertIsNotNone(match, "missing Linux/macOS command block")
        script = self.root / "skill.sh"
        script.write_text(match.group(1))

        expected = self.root / "calls"
        binary = self.root / "bk"
        binary.write_text(
            '#!/bin/sh\n'
            'if [ "$*" = "--version" ]; then printf "bk 0.1.0\\n"; exit 0; fi\n'
            'printf "%s\\n" "$*" >> "$BK_TEST_CALLS"\n'
        )
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
        for platform, target in [
            ("Linux", "x86_64-unknown-linux-gnu"),
            ("Darwin", "aarch64-apple-darwin"),
        ]:
            with self.subTest(platform=platform):
                fake_home = self.root / platform
                fake_home.mkdir()
                self.stem = f"bk-0.1.0-{target}"
                for path in self.fixtures.iterdir():
                    path.unlink()
                directory = release.stage(binary, "0.1.0", target, self.root / "guide-work")
                release.compress(directory, "0.1.0", target, self.fixtures)
                release.checksums(self.fixtures)
                self.bundles()
                self.cosign()
                arch = "aarch64" if platform == "Darwin" else "x86_64"
                self.script("uname", f'print({platform!r} if "-s" in sys.argv else {arch!r})')
                result = subprocess.run(
                    ["/bin/sh", str(script)],
                    env={**self.env, "HOME": str(fake_home), "BK_TEST_CALLS": str(expected),
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
