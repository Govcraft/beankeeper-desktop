"""Keep workflows, shared actions and the Python catalog from drifting apart."""

import re
import sys
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

from cache import CARGO_ENV
from catalog import ROOT, families, msrv_toolchain
from gate import expectations

GITHUB = ROOT / ".github"
COMPILING_WORKFLOWS = ("ci.yml", "packaging.yml", "cache-maintenance.yml")


def text(relative: str) -> str:
    return (GITHUB / relative).read_text()


def cache_paths(action: str) -> str:
    match = re.search(r"path: \|\n((?: {10}\S.*\n)+)", text(f"actions/{action}/action.yml"))
    if match is None:
        raise AssertionError(f"{action} has no cache path block")
    return match[1]


class WorkflowConsistencyTests(unittest.TestCase):
    def test_only_the_shared_action_installs_compilers(self):
        for path in GITHUB.rglob("*.yml"):
            contents = path.read_text()
            self.assertNotIn("Swatinem/rust-cache", contents, path)
            if path != GITHUB / "actions/rust-setup/action.yml":
                self.assertNotIn("dtolnay/rust-toolchain", contents, path)

    def test_restore_and_save_use_identical_cache_paths(self):
        self.assertEqual(cache_paths("rust-setup"), cache_paths("rust-cache-save"))

    def test_compiling_workflows_export_the_cache_build_settings(self):
        for name in COMPILING_WORKFLOWS:
            contents = text(f"workflows/{name}")
            for variable, value in CARGO_ENV.items():
                self.assertIn(f"  {variable}: '{value}'\n", contents, name)

    def test_gate_needs_exactly_the_gated_jobs(self):
        needs = re.search(r"  ci-gate:\n(?:.*\n)*?    needs: \[(.*)\]", text("workflows/ci.yml"))
        outputs = {"rust": "true", "service_any": "true", "packaging_any": "true"}
        expected = expectations({"changes": {"outputs": outputs}})
        self.assertEqual({n.strip() for n in needs[1].split(",")}, set(expected))

    def test_ci_jobs_run_on_their_cache_family_images(self):
        contents = text("workflows/ci.yml")
        for job, family in (("stable", "dev-linux"), ("msrv", "msrv-linux")):
            block = re.search(rf"\n  {job}:\n((?:    .*\n|\n)*)", contents)[1]
            self.assertIn(f"runs-on: {families()[family].runner}\n", block, job)
            self.assertIn(f"family: {family}\n", block, job)

    def test_third_party_actions_are_pinned_to_immutable_revisions(self):
        for path in GITHUB.rglob("*.yml"):
            for reference in re.findall(r"uses: (\S+)", path.read_text()):
                if reference.startswith(("./", "actions/")):
                    continue  # Local actions and GitHub's own actions.
                if reference.startswith("docker://"):
                    self.assertRegex(reference, r"@sha256:[0-9a-f]{64}$", path)
                else:
                    self.assertRegex(reference, r"@[0-9a-f]{40}$", path)

    def test_msrv_job_name_matches_the_workspace(self):
        major_minor = msrv_toolchain().rsplit(".", 1)[0]
        self.assertIn(f"name: MSRV / Rust {major_minor}\n", text("workflows/ci.yml"))


if __name__ == "__main__":
    unittest.main()
