import io
import json
import sys
import unittest
import zipfile
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

from evidence import ARTIFACT, covers, find, qualified, record, skipped
from plan import select

REPO = "Govcraft/beankeeper-desktop"
TREE = "1" * 40
HEAD = "2" * 40


def archive(document) -> bytes:
    buffer = io.BytesIO()
    with zipfile.ZipFile(buffer, "w") as bundle:
        bundle.writestr("evidence.json", json.dumps(document))
    return buffer.getvalue()


class FakeApi:
    """Serves fixed API documents keyed by path prefix and records requests."""

    def __init__(self, evidence, head_tree=TREE, repository=REPO, conclusion="success"):
        self.evidence = evidence
        self.head_tree = head_tree
        self.repository = repository
        self.conclusion = conclusion
        self.paths = []

    def __call__(self, path, raw=False):
        self.paths.append(path)
        if path.startswith("actions/workflows/ci.yml/runs"):
            return {
                "workflow_runs": [
                    {
                        "id": 9,
                        "head_sha": HEAD,
                        "conclusion": self.conclusion,
                        "head_repository": {"full_name": self.repository},
                    }
                ]
            }
        if path == f"git/commits/{HEAD}":
            return {"tree": {"sha": self.head_tree}}
        if path == "actions/runs/9/artifacts":
            return {"artifacts": [{"id": 4, "name": ARTIFACT, "expired": False}]}
        if path == "actions/artifacts/4/zip":
            return archive(self.evidence)
        raise AssertionError(f"unexpected request {path}")


class CoverageTests(unittest.TestCase):
    def test_identical_tree_with_the_same_checks_is_covered(self):
        plan = select(["crates/beankeeper-desktop/src/ui.rs"])
        self.assertTrue(covers(record(plan, TREE, HEAD, REPO), plan, TREE, REPO))

    def test_broader_earlier_validation_covers_a_narrower_plan(self):
        broad = record(select([], full=True), TREE, HEAD, REPO)
        self.assertTrue(covers(broad, select(["install.ps1"]), TREE, REPO))

    def test_narrower_earlier_validation_does_not_cover(self):
        narrow = record(select(["install.ps1"]), TREE, HEAD, REPO)
        self.assertFalse(covers(narrow, select(["scripts/release.py"]), TREE, REPO))
        self.assertFalse(covers(narrow, select(["crates/x.rs"]), TREE, REPO))

    def test_a_different_tree_or_repository_is_never_covered(self):
        plan = select(["docs/setup.md"])
        evidence = record(plan, TREE, HEAD, REPO)
        self.assertFalse(covers(evidence, plan, "3" * 40, REPO))
        self.assertFalse(covers(evidence, plan, TREE, "someone/else"))

    def test_malformed_evidence_is_rejected(self):
        plan = select(["docs/setup.md"])
        good = record(plan, TREE, HEAD, REPO)
        for broken in (
            None,
            [],
            {**good, "version": 2},
            {**good, "sha": "short"},
            {**good, "selection": {**good["selection"], "rust": "yes"}},
            {**good, "selection": {**good["selection"], "packaging": "all"}},
        ):
            self.assertFalse(covers(broken, plan, TREE, REPO), broken)

    def test_skipped_plan_runs_no_check_jobs(self):
        plan = skipped(select([], full=True))
        self.assertFalse(plan["rust"] or plan["service_any"] or plan["packaging_any"])
        self.assertEqual(plan["service"], {"include": []})


class FindTests(unittest.TestCase):
    plan = select(["crates/beankeeper-desktop/src/ui.rs"])

    def test_returns_the_covering_pull_request_run(self):
        evidence = record(self.plan, TREE, HEAD, REPO)
        self.assertEqual(find(self.plan, TREE, REPO, FakeApi(evidence)), "9")

    def test_head_with_a_different_tree_is_skipped_before_downloading(self):
        api = FakeApi(record(self.plan, TREE, HEAD, REPO), head_tree="3" * 40)
        self.assertEqual(find(self.plan, TREE, REPO, api), "")
        self.assertNotIn("actions/runs/9/artifacts", api.paths)

    def test_fork_runs_are_never_trusted(self):
        api = FakeApi(record(self.plan, TREE, HEAD, REPO), repository="fork/beankeeper-desktop")
        self.assertEqual(find(self.plan, TREE, REPO, api), "")
        self.assertEqual(len(api.paths), 1)

    def test_insufficient_evidence_is_not_reused(self):
        narrow = record(select(["docs/setup.md"]), TREE, HEAD, REPO)
        self.assertEqual(find(self.plan, TREE, REPO, FakeApi(narrow)), "")


class QualifiedTests(unittest.TestCase):
    def runs(self, *runs):
        return lambda path: {"workflow_runs": list(runs)}

    def test_successful_nightly_or_manual_run_qualifies_the_commit(self):
        for event in ("schedule", "workflow_dispatch"):
            run = {"head_sha": HEAD, "conclusion": "success", "event": event}
            self.assertTrue(qualified(HEAD, self.runs(run)), event)

    def test_tag_runs_and_other_commits_do_not_qualify(self):
        tag = {"head_sha": HEAD, "conclusion": "success", "event": "push"}
        other = {"head_sha": TREE, "conclusion": "success", "event": "schedule"}
        self.assertFalse(qualified(HEAD, self.runs(tag, other)))

    def test_requires_a_full_sha(self):
        with self.assertRaises(ValueError):
            qualified("main", self.runs())


if __name__ == "__main__":
    unittest.main()
