import hashlib
import json
import os
import re
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest
import zipfile

from test_release import ROOT, release


DOWNLOAD_BASE = "https://beankeeper.app/download"
GITHUB_DOWNLOAD_BASE = "https://github.com/Govcraft/beankeeper-desktop/releases/download"


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
        self.stage_shell_fixture("0.1.0")
        self.cosign()
        self.bundles()
        for command in ("sh", "awk", "cat", "tar", "install", "mktemp", "mkdir", "mv", "rm", "gzip", "chmod", "sha256sum", "shasum", "grep"):
            if location := shutil.which(command):
                (self.bin / command).symlink_to(location)
        # Exercise release discovery without exposing jq from the host.
        (self.bin / "python3").symlink_to(sys.executable)
        self.script("uname", 'print("Linux" if "-s" in sys.argv else "x86_64")')
        self.script("curl", '''
import shutil
from urllib.parse import urlparse
args = sys.argv[1:]
url = next(arg for arg in args if arg.startswith("https://"))
fixtures = Path(os.environ["FIXTURES"])
with (fixtures / "requested-urls").open("a") as requests:
    print(url, file=requests)
if url == "https://api.github.com/repos/Govcraft/beankeeper-desktop/releases?per_page=100":
    log = Path(os.environ["FIXTURES"]) / "release-requests"
    count = len(log.read_text().splitlines()) if log.exists() else 0
    with log.open("a") as requests:
        if "-H" in args:
            assert args[args.index("-H") + 1] == "@-"
            requests.write(sys.stdin.read())
        else:
            requests.write("anonymous\\n")
    statuses = os.environ.get("RELEASE_STATUSES", "200").split(",")
    status = statuses[min(count, len(statuses) - 1)]
    Path(args[args.index("-D") + 1]).write_text(os.environ.get("RELEASE_HEADERS", ""))
    Path(args[args.index("-o") + 1]).write_text(os.environ["FIXTURE_RELEASES"] if status == "200" else "{}")
    print(status, end="")
    sys.exit(0)
assert "-H" not in args, "release downloads must not receive the API token"
base = os.environ.get("BK_DOWNLOAD_BASE", "https://beankeeper.app/download").rstrip("/")
if url == base + "/latest/version.txt":
    if os.environ.get("VERSION_UNAVAILABLE"):
        sys.exit(22)
    print(os.environ.get("VERSION_BODY", os.environ["FIXTURE_VERSION"] + "\\n"), end="")
    sys.exit(0)
assert args[0] == "-q" and args[args.index("--proto-redir") + 1] == "=https"
version = os.environ["FIXTURE_VERSION"]
if base == "https://github.com/Govcraft/beankeeper-desktop/releases/download":
    version = "beankeeper-desktop-v" + version
if url.startswith("https://github.com/sigstore/cosign/releases/download/"):
    if os.environ.get("BOOTSTRAP_UNAVAILABLE"): sys.exit(22)
else:
    assert url.startswith(f"{base}/{version}/"), url
path = fixtures / Path(urlparse(url).path).name
if not path.exists():
    if "-w" in args: print(os.environ.get("BUNDLE_STATUS", "404"), end="")
    sys.exit(22)
shutil.copyfile(path, args[args.index("-o") + 1])
if "-w" in args: print("200", end="")
''')
        self.env = {**os.environ, "PATH": str(self.bin), "BK_VERSION": "0.1.0", "FIXTURE_VERSION": "0.1.0",
                    "BK_INSTALL": str(self.destination), "FIXTURES": str(self.fixtures),
                    "TMPDIR": str(self.root), "BK_TEST_LOG": str(self.root / "service-actions")}
        for name in ("BK_DOWNLOAD_BASE", "GH_TOKEN", "GITHUB_TOKEN"):
            self.env.pop(name, None)
        self.script("date", 'print("1000")')
        self.script("sleep", 'with (Path(os.environ["FIXTURES"]) / "release-delays").open("a") as log:\n'
                             '    print(sys.argv[1], file=log)')

    def stage_shell_fixture(self, version):
        target = "x86_64-unknown-linux-gnu"
        self.stem = f"bk-{version}-{target}"
        binary = self.root / "bk"
        binary.write_text(
            f'#!/bin/sh\nif [ "$1" = --version ]; then printf "bk {version}\\n"; '
            'else printf "%s\\n" "$*" >> "$BK_TEST_LOG"; fi\n'
        )
        directory = release.stage(binary, version, target, self.root / "work")
        release.compress(directory, version, target, self.fixtures)
        release.checksums(self.fixtures)

    def script(self, name, code):
        path = self.bin / name
        path.write_text(f"#!{sys.executable}\nimport os, sys\nfrom pathlib import Path\n" + code + "\n")
        path.chmod(0o755)

    def run_installer(self):
        return subprocess.run(["/bin/sh", str(getattr(self, "installer", ROOT / "install.sh"))], env=self.env,
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

    def assert_release_requests(self, base, version, *, discover):
        archive = f"{self.stem}.tar.gz"
        self.assertEqual((self.fixtures / "requested-urls").read_text().splitlines(),
                         ([f"{base}/latest/version.txt"] if discover else []) +
                         [f"{base}/{version}/{name}" for name in
                          (archive, "checksums-sha256.txt", f"{archive}.sigstore", "checksums-sha256.txt.sigstore")])
        self.assertEqual((self.fixtures / "verified").read_text().splitlines(),
                         [archive, "checksums-sha256.txt"])

    def test_default_discovers_version_and_downloads_signed_assets_without_json_reader(self):
        self.env.pop("BK_VERSION")
        self.env["GH_TOKEN"] = "do-not-send"
        (self.bin / "python3").unlink()
        result = self.run_installer()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assert_release_requests(DOWNLOAD_BASE, "0.1.0", discover=True)

    def test_override_uses_the_same_mirror_layout_and_trust_identity(self):
        self.env.pop("BK_VERSION")
        mirror = "https://mirror.example/releases"
        self.env.update(BK_DOWNLOAD_BASE=f"{mirror}/", VERSION_BODY="0.1.0\r\n")
        result = self.run_installer()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assert_release_requests(mirror, "0.1.0", discover=True)

    def test_explicit_version_bypasses_discovery(self):
        self.env["VERSION_UNAVAILABLE"] = "1"
        result = self.run_installer()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assert_release_requests(DOWNLOAD_BASE, "0.1.0", discover=False)

    def test_invalid_base_is_rejected_before_network_access(self):
        for base in ("http://mirror.example", "https:///download", "https://mirror.example/?query"):
            with self.subTest(base=base):
                self.env["BK_DOWNLOAD_BASE"] = base
                result = self.run_installer()
                self.assertNotEqual(result.returncode, 0)
                self.assertIn("BK_DOWNLOAD_BASE must be an HTTPS URL", result.stderr)
                self.assertFalse((self.fixtures / "requested-urls").exists())

    def test_unavailable_or_invalid_version_metadata_does_not_fall_back(self):
        self.env.pop("BK_VERSION")
        requests = self.fixtures / "requested-urls"
        for body in (None, "", "0.1.0\n0.2.0\n", "../main", "0.1.0 "):
            with self.subTest(body=body):
                self.env.pop("VERSION_UNAVAILABLE", None)
                if body is None:
                    self.env["VERSION_UNAVAILABLE"] = "1"
                    message = "could not read the latest release version"
                else:
                    self.env["VERSION_BODY"] = body
                    message = "invalid version"
                result = self.run_installer()
                self.assertNotEqual(result.returncode, 0)
                self.assertIn(message, result.stderr)
                self.assertEqual(requests.read_text().splitlines(), [f"{DOWNLOAD_BASE}/latest/version.txt"])
                self.assertFalse((self.destination / "bk").exists())
                requests.unlink()

    def test_missing_cosign_stops_even_with_legacy_fallback_settings(self):
        (self.bin / "cosign").unlink()
        self.env.update(BK_REQUIRE_SIGNATURES="0", BK_VERIFY_ATTESTATION="1")
        result = self.run_installer()
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(result.stderr, "Could not securely prepare release verification; retry the installer.\n")
        self.assertFalse((self.destination / "bk").exists())

    def bootstrap_fixture(self, os_name="Linux", arch="x86_64", fail_signature=False):
        self.cosign(fail=fail_signature)
        verifier = (self.bin / "cosign").read_bytes()
        (self.bin / "cosign").unlink()
        for asset in release.COSIGN_ASSETS.values():
            (self.fixtures / asset).write_bytes(verifier)
        output = self.root / "generated"
        release.render_bootstrap_installers(output, "v3.0.0", {
            key: hashlib.sha256(verifier).hexdigest() for key in release.COSIGN_ASSETS
        })
        self.installer = output / "install.sh"
        self.script("uname", f'print({os_name!r} if "-s" in sys.argv else {arch!r})')

    def test_bootstrap_verifies_both_signatures_for_every_posix_platform_and_cleans_up(self):
        for os_name, arch, asset in (
            ("Linux", "x86_64", "cosign-linux-amd64"),
            ("Linux", "aarch64", "cosign-linux-arm64"),
            ("Darwin", "x86_64", "cosign-darwin-amd64"),
            ("Darwin", "arm64", "cosign-darwin-arm64"),
        ):
            with self.subTest(os=os_name, arch=arch):
                self.bootstrap_fixture(os_name, arch)
                # The fixture archive stays Linux; only bootstrap platform selection changes.
                script = self.installer.read_text().replace('    resolve_download_base',
                    '    TARGET="x86_64-unknown-linux-gnu"\n    resolve_download_base', 1)
                self.installer.write_text(script)
                self.env.update(BK_REQUIRE_SIGNATURES="1", BK_NO_SERVICE="1")
                result = self.run_installer()
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertIn("https://github.com/sigstore/cosign/releases/download/v3.0.0/" + asset,
                              (self.fixtures / "requested-urls").read_text())
                self.assertEqual((self.fixtures / "verified").read_text().splitlines(),
                                 [f"{self.stem}.tar.gz", "checksums-sha256.txt"])
                (self.fixtures / "verified").unlink()
                self.assertFalse(any(self.root.glob("tmp.*")))
                self.assertFalse((self.bin / "cosign").exists())

    def test_bootstrap_failure_preserves_previous_install_and_never_executes_verifier(self):
        for failure in ("tampered", "network"):
            with self.subTest(failure=failure):
                self.bootstrap_fixture()
                (self.destination / "bk").write_text("previous binary")
                if failure == "tampered":
                    (self.fixtures / "cosign-linux-amd64").write_text("untrusted")
                else:
                    self.env["BOOTSTRAP_UNAVAILABLE"] = "1"
                result = self.run_installer()
                self.assertNotEqual(result.returncode, 0)
                self.assertEqual(result.stderr, "Could not securely prepare release verification; retry the installer.\n")
                self.assertEqual((self.destination / "bk").read_text(), "previous binary")
                self.assertFalse((self.fixtures / "verified").exists())
                self.assertFalse((self.root / "service-actions").exists())
                self.assertFalse(any(self.root.glob("tmp.*")))

    def test_bootstrapped_signature_failure_never_installs(self):
        self.bootstrap_fixture(fail_signature=True)
        result = self.run_installer()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("Sigstore verification failed", result.stderr)
        self.assertFalse((self.destination / "bk").exists())

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

    def test_missing_bundle_preserves_installed_binary(self):
        installed = self.destination / "bk"
        installed.write_text("previous version")
        self.env["BK_REQUIRE_SIGNATURES"] = "0"
        for name in (f"{self.stem}.tar.gz", "checksums-sha256.txt"):
            with self.subTest(artifact=name):
                (self.fixtures / f"{name}.sigstore").unlink()
                result = self.run_installer()
                self.assertNotEqual(result.returncode, 0)
                self.assertIn(f"required Sigstore bundle missing for {name} (HTTP 404)", result.stderr)
                self.assertEqual(installed.read_text(), "previous version")
                self.bundles()

    def test_agent_install_rejects_older_binary_before_replacing_installed_copy(self):
        binary = self.root / "bk"
        binary.write_text('#!/bin/sh\n[ "$*" = "service install --help" ]\n')
        directory = release.stage(binary, "0.1.0", "x86_64-unknown-linux-gnu", self.root / "old-work")
        release.compress(directory, "0.1.0", "x86_64-unknown-linux-gnu", self.fixtures)
        release.checksums(self.fixtures)
        self.cosign()
        self.bundles()
        self.env.pop("BK_VERSION")
        self.env["BK_REQUIRE_AGENT_SETUP"] = "1"
        installed = self.destination / "bk"
        installed.write_text("previous executable")

        result = self.run_installer()

        self.assertNotEqual(result.returncode, 0)
        self.assertIn("lacks managed service or agent registration commands", result.stderr)
        self.assertEqual(installed.read_text(), "previous executable")

    def test_guided_discovery_refuses_when_no_release_includes_both_signature_bundles(self):
        self.use_discovery(BK_REQUIRE_AGENT_SETUP="1", FIXTURE_RELEASES=json.dumps([self.release_entry(
            "0.2.0-rc.1", "x86_64-unknown-linux-gnu", missing="checksums-sha256.txt.sigstore",
        )]))

        result = self.run_installer()

        self.assertNotEqual(result.returncode, 0)
        self.assertIn("no published release with signature bundles found", result.stderr)
        self.assertFalse((self.destination / "bk").exists())

    def use_discovery(self, **overrides):
        self.env.pop("BK_VERSION", None)
        self.env.update({"BK_DOWNLOAD_BASE": GITHUB_DOWNLOAD_BASE + "/",
                         "FIXTURE_RELEASES": json.dumps([self.release_entry("0.1.0", "x86_64-unknown-linux-gnu")]),
                         **overrides})

    def test_discovery_rejects_malformed_metadata(self):
        for body in ('[{"draft":false}', '{"message":"not a release list"}', '[] []'):
            with self.subTest(body=body):
                self.use_discovery(FIXTURE_RELEASES=body)
                result = self.run_installer()
                self.assertNotEqual(result.returncode, 0)
                self.assertIn("could not read published release metadata", result.stderr)
                self.assertFalse((self.destination / "bk").exists())

    def test_discovery_explains_missing_json_reader_and_explicit_version_still_works(self):
        (self.bin / "python3").unlink()
        self.use_discovery()
        result = self.run_installer()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("release discovery needs python3 or jq", result.stderr)
        self.assertIn("set BK_VERSION", result.stderr)
        self.assertFalse((self.destination / "bk").exists())
        self.env["BK_VERSION"] = "0.1.0"
        result = self.run_installer()
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_authenticates_release_discovery_using_either_token(self):
        self.use_discovery(GITHUB_TOKEN="fallback", GH_TOKEN="preferred")
        result = self.run_installer()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.env["GH_TOKEN"] = ""
        result = self.run_installer()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual((self.fixtures / "release-requests").read_text().splitlines(),
                         ["Authorization: Bearer preferred", "Authorization: Bearer fallback"])

    def test_retries_anonymous_403_and_429_with_backoff(self):
        self.use_discovery(RELEASE_STATUSES="403,429,200", RELEASE_HEADERS="Retry-After: 75\n")
        result = self.run_installer()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual((self.fixtures / "release-requests").read_text().splitlines(), ["anonymous"] * 3)
        self.assertEqual((self.fixtures / "release-delays").read_text().splitlines(), ["75", "120"])

    def test_persistent_rate_limit_suggests_an_explicit_signed_version(self):
        self.use_discovery(RELEASE_STATUSES="429")
        result = self.run_installer()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("GitHub rate limit (HTTP 429) while listing releases", result.stderr)
        self.assertIn("Set BK_VERSION to a signed release", result.stderr)
        self.assertEqual((self.fixtures / "release-delays").read_text().splitlines(), ["60", "120"])
        self.assertFalse((self.destination / "bk").exists())

    def test_exhausted_quota_fails_without_retrying_before_a_distant_reset(self):
        self.use_discovery(RELEASE_STATUSES="403", RELEASE_HEADERS="X-RateLimit-Remaining: 0\nX-RateLimit-Reset: 1600\n")
        result = self.run_installer()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("GitHub rate limit (HTTP 403) while listing releases", result.stderr)
        self.assertIn("BK_VERSION", result.stderr)
        self.assertEqual((self.fixtures / "release-requests").read_text(), "anonymous\n")
        self.assertFalse((self.fixtures / "release-delays").exists())

    def assert_regular_release_selection(self, version, releases):
        self.stage_shell_fixture(version)
        self.cosign()
        self.bundles()
        self.use_discovery(BK_REQUIRE_AGENT_SETUP="0", FIXTURE_VERSION=version,
                           FIXTURE_RELEASES=json.dumps(releases))

        result = self.run_installer()

        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn(f"installed bk v{version}", result.stdout)
        self.assertEqual((self.fixtures / "verified").read_text().splitlines(),
                         [f"{self.stem}.tar.gz", "checksums-sha256.txt"])

    def test_regular_discovery_uses_newest_signed_rc_when_no_stable_exists(self):
        target = "x86_64-unknown-linux-gnu"
        self.assert_regular_release_selection("0.2.0-rc.10", [
            self.release_entry("0.2.0-rc.11", target, missing="checksums-sha256.txt.sigstore"),
            self.release_entry("0.2.0-rc.9", target),
            self.release_entry("0.2.0-rc.10", target),
        ])

    def test_regular_discovery_prefers_signed_stable_over_newer_rc(self):
        target = "x86_64-unknown-linux-gnu"
        self.assert_regular_release_selection("0.1.0", [
            self.release_entry("0.2.0-rc.10", target),
            self.release_entry("0.0.9", target),
            self.release_entry("0.1.0", target),
        ])

    def test_release_selection_matches_across_available_json_readers(self):
        target = "x86_64-unknown-linux-gnu"
        version = "0.10.0"
        corrupted = self.release_entry("0.99.0", target, missing="checksums-sha256.txt.sigstore")
        corrupted["assets"].append({"name": "junk checksums-sha256.txt.sigstore"})
        corrupted["assets"].append({"name": "checksums-sha256.txt.sigstore\n"})
        candidates = [
            self.release_entry("0.9.0", target),
            self.release_entry("0.101.0", target, tag_name="beankeeper-desktop-v0.101.0\n"),
            self.release_entry("0.100.0", target, draft=True),
            self.release_entry("0.100.0", target, prerelease=True),
            self.release_entry("0.100.0", "aarch64-unknown-linux-gnu"),
            corrupted, self.release_entry(version, target),
        ]
        if jq := shutil.which("jq"):
            (self.bin / "jq").symlink_to(jq)
            self.assert_regular_release_selection(version, candidates)
            (self.bin / "jq").unlink()
            (self.fixtures / "verified").unlink()
        self.assert_regular_release_selection(version, candidates)

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

    def test_skill_commands_install_verified_binary_and_start_service(self):
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
if "https://beankeeper.app/download/latest/install.sh" in args:
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
                result = subprocess.run(
                    ["/bin/sh", str(script)],
                    env={**self.env, "HOME": str(fake_home), "BK_TEST_CALLS": str(expected),
                         "FIXTURE_VERSION": version,
                         "BK_TEST_INSTALLER": str(ROOT / "install.sh"), "BK_TEST_FIXTURE_CURL": str(fixture_curl)},
                    capture_output=True, text=True,
                )
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertTrue((fake_home / ".local/bin/bk").exists())
                self.assertEqual(expected.read_text().splitlines(), [
                    "service install --no-agents", "service status --json",
                ])
                expected.unlink()

                bundles = self.root / "guide-bundles"
                release.mcp_bundle(directory, version, target, bundles)
                with zipfile.ZipFile(bundles / f"{directory.name}.mcpb") as archive:
                    guide = archive.read("SKILL.md").decode("utf-8")
                for instruction in (
                    "## Recover from a rejected Beankeeper tool call",
                    "Correct the call and retry it in the same conversation.",
                    "the error code and message so they can report it.",
                    "in the Beankeeper app, on a screen, or in a terminal.",
                    "into a description to work around a rejected call.",
                    "## Annotating or tagging an entry",
                    "`txn_meta_set` with `company`, exactly one of `id` or `reference`",
                    '`values: {"status":"pending"}`',
                    '`values: ["status=pending"]`',
                    '`txn_meta_unset` with the same selector and `keys: ["status"]`',
                    "`bk txn meta set ID status=pending`",
                    "`status: pending`",
                    "`flag: review`",
                    "Reserved keys are `correlate`, `proposal_id`, `reverses`, `reversed_by`,",
                    "`applies_to` and `beankeeper_source`.",
                    "### Pending card holds",
                    '`status: "pending"`',
                    "`txn_settle_hold`",
                    "`bk txn settle-hold --id 42 --amount 43.88 --date 2026-03-04 --reference posted-1`",
                    "settle the hold instead of posting a second entry.",
                    "which restores the hold.",
                    "Rename accounts with the",
                    "rename tool, never create-and-transfer.",
                    "`account_rename` with `company`, the",
                ):
                    self.assertIn(instruction, guide)

    def test_invalid_bundle_and_non_404_download_errors_fail_closed(self):
        self.cosign(fail=True)
        self.bundles()
        result = self.run_installer()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("Sigstore verification failed", result.stderr)
        self.assertFalse((self.destination / "bk").exists())
        self.cosign()
        (self.fixtures / "checksums-sha256.txt.sigstore").unlink()
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


@unittest.skipUnless(shutil.which("pwsh") or shutil.which("powershell"), "PowerShell required")
class WindowsBootstrapTests(unittest.TestCase):
    def test_bootstrap_validates_before_returning_executable_and_fails_closed(self):
        with tempfile.TemporaryDirectory() as temporary:
            work = Path(temporary)
            payload = b"fixture verifier, never executed"
            release.render_bootstrap_installers(work, "v3.0.0", {
                key: hashlib.sha256(payload).hexdigest() for key in release.COSIGN_ASSETS
            })
            (work / "payload").write_bytes(payload)
            test = work / "test.ps1"
            test.write_text(r'''
$ErrorActionPreference = 'Stop'
. (Join-Path $PSScriptRoot 'install.ps1')
function Get-Command { return $null }
function Get-ReleaseFile($Url, $Destination) {
    if ($Url -cne 'https://github.com/sigstore/cosign/releases/download/v3.0.0/cosign-windows-amd64.exe') {
        throw 'Unexpected URL'
    }
    if ($script:FailNetwork) { throw 'Network unavailable' }
    Copy-Item (Join-Path $PSScriptRoot 'payload') $Destination -Force
}
$expected = Join-Path $PSScriptRoot 'cosign.exe'
if ((Get-Cosign $PSScriptRoot) -cne $expected) { throw 'Wrong executable' }
foreach ($failure in @('tamper', 'network')) {
    if ($failure -eq 'tamper') { Set-Content (Join-Path $PSScriptRoot 'payload') 'tampered' }
    $script:FailNetwork = $failure -eq 'network'
    $failed = $false
    try { Get-Cosign $PSScriptRoot } catch {
        if ($_.Exception.Message -cne 'Could not securely prepare release verification; retry the installer.') { throw }
        $failed = $true
    }
    if (-not $failed) { throw 'Bootstrap accepted untrusted verifier' }
}
''')
            result = subprocess.run([shutil.which("pwsh") or shutil.which("powershell"),
                                     "-NoProfile", "-File", str(test)], capture_output=True, text=True)
            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)


if __name__ == "__main__":
    unittest.main()
