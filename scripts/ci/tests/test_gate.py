import sys
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

from gate import expectations, verify


def results(rust="true", service="true", packaging="false", **overrides):
    outcome = {
        "changes": {
            "result": "success",
            "outputs": {
                "rust": rust,
                "service_any": service,
                "packaging_any": packaging,
            },
        },
        "stable": {"result": "success" if rust == "true" else "skipped"},
        "msrv": {"result": "success" if rust == "true" else "skipped"},
        "service": {"result": "success" if service == "true" else "skipped"},
        "packaging": {"result": "success" if packaging == "true" else "skipped"},
    }
    for name, result in overrides.items():
        outcome[name] = {**outcome[name], "result": result}
    return outcome


class GateTests(unittest.TestCase):
    def check(self, outcome):
        verify(outcome, expectations(outcome))

    def test_planned_successes_and_skips_pass(self):
        self.check(results())
        self.check(results(rust="false", service="false", packaging="true"))

    def test_failure_or_cancellation_fails(self):
        for result in ("failure", "cancelled"):
            with self.assertRaisesRegex(ValueError, "stable"):
                self.check(results(stable=result))

    def test_unplanned_skip_fails(self):
        outcome = results(service="true")
        outcome["service"]["result"] = "skipped"
        with self.assertRaisesRegex(ValueError, "service: expected success"):
            self.check(outcome)

    def test_unplanned_run_fails(self):
        outcome = results(packaging="false")
        outcome["packaging"]["result"] = "success"
        with self.assertRaisesRegex(ValueError, "packaging: expected skipped"):
            self.check(outcome)

    def test_failed_planning_fails_closed(self):
        outcome = results()
        outcome["changes"] = {"result": "failure", "outputs": {}}
        with self.assertRaisesRegex(ValueError, "plan output"):
            self.check(outcome)

    def test_unexpected_dependencies_fail(self):
        outcome = results()
        outcome["extra"] = {"result": "success"}
        with self.assertRaisesRegex(ValueError, "differ"):
            self.check(outcome)


if __name__ == "__main__":
    unittest.main()
