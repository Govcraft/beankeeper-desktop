import sys
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

from bump_core_release import (
    BumpError,
    apply_tag,
    changelog_line,
    current_tag,
    ensure_changelog,
    parse_pins,
    require_tag,
)

MANIFEST = """[dependencies]
axum = "1"
beankeeper = { git = "https://github.com/Govcraft/beankeeper", tag = "beankeeper-cli-v0.40.0" }
beankeeper-cli = { git = "https://github.com/Govcraft/beankeeper", tag = "beankeeper-cli-v0.40.0" }
chrono = "1"
"""

MAIN_MANIFEST = MANIFEST.replace('tag = "beankeeper-cli-v0.40.0"', 'branch = "main"')


class BumpCoreReleaseTests(unittest.TestCase):
    def test_require_tag_accepts_stable_cli_tags_only(self):
        self.assertEqual(require_tag("beankeeper-cli-v0.40.0"), "beankeeper-cli-v0.40.0")
        for tag in ("v0.40.0", "beankeeper-cli-v0.40", "beankeeper-cli-v0.40.0-rc.1", "main"):
            with self.subTest(tag=tag), self.assertRaises(BumpError):
                require_tag(tag)

    def test_current_tag_requires_matching_release_pins(self):
        self.assertEqual(current_tag(MANIFEST), "beankeeper-cli-v0.40.0")
        with self.assertRaises(BumpError):
            current_tag(MAIN_MANIFEST)
        disagreed = MANIFEST.replace(
            'beankeeper-cli = { git = "https://github.com/Govcraft/beankeeper", tag = "beankeeper-cli-v0.40.0" }',
            'beankeeper-cli = { git = "https://github.com/Govcraft/beankeeper", tag = "beankeeper-cli-v0.39.0" }',
        )
        with self.assertRaises(BumpError):
            current_tag(disagreed)

    def test_apply_tag_rewrites_both_pins(self):
        updated = apply_tag(MANIFEST, "beankeeper-cli-v0.41.0")
        self.assertEqual(current_tag(updated), "beankeeper-cli-v0.41.0")
        self.assertEqual(parse_pins(updated)["beankeeper"], "beankeeper-cli-v0.41.0")
        self.assertIn('tag = "beankeeper-cli-v0.41.0"', updated)
        self.assertNotIn("branch =", updated)
        # Already at the requested tag is a no-op rewrite that still parses.
        self.assertEqual(apply_tag(MANIFEST, "beankeeper-cli-v0.40.0"), MANIFEST)

    def test_cargo_generated_git_versions_are_normalized(self):
        cargo_add_manifest = MANIFEST.replace(
            'tag = "beankeeper-cli-v0.40.0" }',
            'tag = "beankeeper-cli-v0.40.0", version = "0.40.0" }',
        )
        self.assertEqual(current_tag(cargo_add_manifest), "beankeeper-cli-v0.40.0")
        self.assertEqual(apply_tag(cargo_add_manifest, "beankeeper-cli-v0.40.0"), MANIFEST)
        expected = MANIFEST.replace("beankeeper-cli-v0.40.0", "beankeeper-cli-v0.41.0")
        self.assertEqual(apply_tag(cargo_add_manifest, "beankeeper-cli-v0.41.0"), expected)

    def test_apply_tag_can_leave_main(self):
        updated = apply_tag(MAIN_MANIFEST, "beankeeper-cli-v0.40.0")
        self.assertEqual(current_tag(updated), "beankeeper-cli-v0.40.0")
        self.assertNotIn('branch = "main"', updated)

    def test_ensure_changelog_inserts_once_under_unreleased(self):
        changelog = "# Changelog\n\n## [Unreleased]\n\n## [1.0.3] - 2026-11-26\n\n- Older.\n"
        once = ensure_changelog(changelog, "beankeeper-cli-v0.40.0")
        self.assertIn(changelog_line("beankeeper-cli-v0.40.0"), once)
        self.assertEqual(once.count("beankeeper-cli-v0.40.0"), 1)
        self.assertEqual(ensure_changelog(once, "beankeeper-cli-v0.40.0"), once)
        self.assertTrue(
            once.index("## [Unreleased]")
            < once.index("beankeeper-cli-v0.40.0")
            < once.index("## [1.0.3]")
        )


if __name__ == "__main__":
    unittest.main()
