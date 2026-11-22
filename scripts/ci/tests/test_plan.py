import subprocess
import sys
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

from catalog import ALL_TARGETS, REPRESENTATIVE_TARGETS, ROOT, SERVICE_RUNNERS
from plan import RULES, changed_paths, classify, select


def targets(plan):
    return {entry["target"] for entry in plan["packaging"]["include"]}


class SelectTests(unittest.TestCase):
    def test_documentation_runs_no_rust_or_packaging(self):
        plan = select(["docs/setup.md", "CHANGELOG.md", "README.md"])
        self.assertFalse(plan["rust"])
        self.assertFalse(plan["service_any"])
        self.assertFalse(plan["packaging_any"])
        self.assertFalse(plan["tooling"])

    def test_python_helper_tests_need_no_other_jobs(self):
        plan = select(["scripts/tests/test_install.py"])
        self.assertFalse(plan["rust"] or plan["service_any"] or plan["packaging_any"])

    def test_rust_source_runs_every_rust_check_without_packaging(self):
        plan = select(["crates/beankeeper-desktop/src/server.rs"])
        self.assertTrue(plan["rust"])
        self.assertEqual(
            [entry["family"] for entry in plan["service"]["include"]],
            list(SERVICE_RUNNERS),
        )
        self.assertFalse(plan["packaging_any"])

    def test_build_script_packages_every_target(self):
        plan = select(["crates/beankeeper-desktop/build.rs"])
        self.assertTrue(plan["rust"])
        self.assertEqual(targets(plan), ALL_TARGETS)

    def test_embedded_guide_runs_rust_and_both_install_guide_families(self):
        plan = select(["SKILL.md"])
        self.assertTrue(plan["rust"])
        self.assertEqual(targets(plan), {"x86_64-unknown-linux-gnu", "x86_64-pc-windows-msvc"})

    def test_installers_package_only_their_platforms(self):
        self.assertEqual(targets(select(["install.ps1"])), {"x86_64-pc-windows-msvc"})
        self.assertEqual(
            targets(select(["install.sh"])),
            {"x86_64-unknown-linux-gnu", "aarch64-apple-darwin"},
        )
        self.assertEqual(
            targets(select(["packaging/postinst"])),
            {"x86_64-unknown-linux-gnu", "aarch64-unknown-linux-gnu"},
        )
        self.assertEqual(
            targets(select(["scripts/sign-macos.sh"])),
            {"x86_64-apple-darwin", "aarch64-apple-darwin"},
        )
        self.assertFalse(select(["install.ps1"])["rust"])

    def test_service_test_runs_service_and_one_package_per_os(self):
        plan = select(["scripts/tests/service-lifecycle.py"])
        self.assertFalse(plan["rust"])
        self.assertTrue(plan["service_any"])
        self.assertEqual(targets(plan), REPRESENTATIVE_TARGETS)

    def test_ci_tooling_validates_rust_and_representative_packages(self):
        plan = select(["scripts/ci/plan.py"])
        self.assertTrue(plan["rust"] and plan["tooling"])
        self.assertEqual(targets(plan), REPRESENTATIVE_TARGETS)

    def test_ci_helper_tests_only_lint_and_run_python(self):
        plan = select(["scripts/ci/tests/test_plan.py"])
        self.assertTrue(plan["tooling"])
        self.assertFalse(plan["rust"] or plan["service_any"] or plan["packaging_any"])

    def test_unclassified_paths_select_everything(self):
        for path in (
            "new-top-level.toml",
            ".github/workflows/new.yml",
            "scripts/new.py",
        ):
            plan = select([path])
            self.assertTrue(plan["rust"] and plan["tooling"], path)
            self.assertEqual(targets(plan), ALL_TARGETS, path)

    def test_full_selects_everything_without_paths(self):
        plan = select([], full=True)
        self.assertTrue(plan["rust"] and plan["service_any"] and plan["tooling"])
        self.assertEqual(targets(plan), ALL_TARGETS)

    def test_union_is_the_strongest_requirement(self):
        plan = select(["docs/setup.md", "install.ps1", "crates/beankeeper-desktop/src/ui.rs"])
        self.assertTrue(plan["rust"])
        self.assertEqual(targets(plan), {"x86_64-pc-windows-msvc"})

    def test_every_tracked_file_is_classified_or_knowingly_full(self):
        tracked = subprocess.check_output(["git", "ls-files"], cwd=ROOT, text=True).split()
        unclassified = [
            path
            for path in tracked
            if classify(path)[1].startswith("unclassified") and not path.startswith(".github/")
        ]
        self.assertEqual(unclassified, [])

    def test_file_rules_never_name_directories(self):
        for pattern, _, _ in RULES:
            if not pattern.endswith("/"):
                self.assertFalse((ROOT / pattern).is_dir(), f"{pattern} needs a trailing slash")


class ChangedPathTests(unittest.TestCase):
    def test_unreliable_events_raise(self):
        sha = "a" * 40
        with self.assertRaises(ValueError):
            changed_paths("workflow_dispatch", sha, sha, False)
        with self.assertRaises(ValueError):
            changed_paths("push", sha, sha, True)
        with self.assertRaises(ValueError):
            changed_paths("push", "0" * 40, sha, False)
        with self.assertRaises(ValueError):
            changed_paths("push", "not-a-sha", sha, False)


if __name__ == "__main__":
    unittest.main()
