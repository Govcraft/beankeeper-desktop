"""Reject failures, cancellations and unexpected skips behind the single required check."""

import json
import os


def expectations(results: dict) -> dict[str, bool]:
    """Map each gated job to whether the plan required it to run."""
    outputs = results.get("changes", {}).get("outputs", {})
    flags = {}
    for key in ("rust", "service_any", "packaging_any"):
        value = outputs.get(key)
        if value not in {"true", "false"}:
            raise ValueError(f"Invalid or missing plan output {key!r}: {value!r}")
        flags[key] = value == "true"
    return {
        "changes": True,
        "stable": flags["rust"],
        "msrv": flags["rust"],
        "upstream-main": flags["rust"],
        "service": flags["service_any"],
        "packaging": flags["packaging_any"],
    }


def verify(results: dict, expected: dict[str, bool]) -> None:
    if results.keys() != expected.keys():
        raise ValueError(
            f"Gate dependencies {sorted(results)} differ from expected {sorted(expected)}"
        )
    for name, required in expected.items():
        received = results[name].get("result")
        wanted = "success" if required else "skipped"
        if received != wanted:
            raise ValueError(f"{name}: expected {wanted}, received {received}")


def main() -> None:
    results = json.loads(os.environ["RESULTS"])
    verify(results, expectations(results))
    print("Every selected check succeeded; every skip was planned.")


if __name__ == "__main__":
    main()
