import os
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
        for command in ("sh", "awk", "tar", "install", "mktemp", "rm", "gzip", "sha256sum", "shasum", "grep"):
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
        for value in ("../main", "0.1", "0.1.0.1", "0.1.0-beta"):
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
