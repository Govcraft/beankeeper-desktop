"""Reuse completed validation instead of repeating it on identical inputs.

``select`` runs in CI's planning job. On a main push it looks for a successful
same-repository pull request run that validated exactly this source tree with
at least the checks main's plan needs, and if one exists it skips every check
job. ``record`` writes that evidence after ci-gate passes. ``qualified`` lets
the nightly release qualification skip a commit an earlier run qualified.
"""

import argparse
import io
import json
import os
import re
import subprocess
import urllib.error
import urllib.parse
import urllib.request
import zipfile
from pathlib import Path

ARTIFACT = "ci-evidence"
VERSION = 1
MAX_RESPONSE = 1024**2
MAX_RECORD = 64 * 1024
SHA = re.compile(r"[0-9a-f]{40}")


def selection(plan: dict) -> dict:
    """The checks a plan ran, in a form a later plan can compare against."""
    return {
        "rust": plan["rust"] is True,
        "service": sorted(entry["family"] for entry in plan["service"]["include"]),
        "packaging": sorted(entry["target"] for entry in plan["packaging"]["include"]),
        "tooling": plan["tooling"] is True,
    }


def record(plan: dict, tree: str, sha: str, repository: str) -> dict:
    return {
        "version": VERSION,
        "repository": repository,
        "tree": tree,
        "sha": sha,
        "selection": selection(plan),
    }


def covers(evidence: object, plan: dict, tree: str, repository: str) -> bool:
    """True when evidence validated this exact tree with every check plan needs."""
    if not isinstance(evidence, dict) or evidence.get("version") != VERSION:
        return False
    have = evidence.get("selection")
    if not (
        evidence.get("repository") == repository
        and evidence.get("tree") == tree
        and isinstance(evidence.get("sha"), str)
        and SHA.fullmatch(evidence["sha"])
        and isinstance(have, dict)
        and isinstance(have.get("rust"), bool)
        and isinstance(have.get("tooling"), bool)
        and all(
            isinstance(have.get(key), list) and all(isinstance(v, str) for v in have[key])
            for key in ("service", "packaging")
        )
    ):
        return False
    need = selection(plan)
    return (
        (have["rust"] or not need["rust"])
        and (have["tooling"] or not need["tooling"])
        and set(need["service"]) <= set(have["service"])
        and set(need["packaging"]) <= set(have["packaging"])
    )


def skipped(plan: dict) -> dict:
    """The plan with every check job deselected; tooling runs in planning itself."""
    return {
        **plan,
        "rust": False,
        "service": {"include": []},
        "service_any": False,
        "packaging": {"include": []},
        "packaging_any": False,
    }


class ArtifactRedirect(urllib.request.HTTPRedirectHandler):
    """Signed artifact URLs on another host must not receive the GitHub token."""

    def redirect_request(self, req, fp, code, msg, headers, newurl):
        redirected = super().redirect_request(req, fp, code, msg, headers, newurl)
        if (
            redirected
            and urllib.parse.urlsplit(req.full_url).netloc != urllib.parse.urlsplit(newurl).netloc
        ):
            redirected.remove_header("Authorization")
        return redirected


def api(path: str, raw: bool = False):
    # Paths come from fixed templates and API-typed ids, never from artifacts.
    url = f"{os.environ['GITHUB_API_URL']}/repos/{os.environ['GITHUB_REPOSITORY']}/{path}"
    request = urllib.request.Request(
        url,
        headers={
            "Authorization": f"Bearer {os.environ['GH_TOKEN']}",
            "Accept": "application/vnd.github+json",
        },
    )
    with urllib.request.build_opener(ArtifactRedirect()).open(request, timeout=20) as response:
        data = response.read(MAX_RESPONSE + 1)
    if len(data) > MAX_RESPONSE:
        raise ValueError(f"Oversized response from {path}")
    return data if raw else json.loads(data)


def read_record(archive_bytes: bytes) -> object:
    with zipfile.ZipFile(io.BytesIO(archive_bytes)) as archive:
        if archive.getinfo("evidence.json").file_size > MAX_RECORD:
            raise ValueError("Oversized evidence record")
        return json.loads(archive.read("evidence.json"))


def find(plan: dict, tree: str, repository: str, request=api) -> str:
    """Return the id of a pull request run whose evidence covers plan, or ""."""
    runs = request("actions/workflows/ci.yml/runs?event=pull_request&status=success&per_page=50")[
        "workflow_runs"
    ]
    for run in runs:
        if (
            run.get("conclusion") != "success"
            or (run.get("head_repository") or {}).get("full_name") != repository
            or not SHA.fullmatch(str(run.get("head_sha", "")))
            or not isinstance(run.get("id"), int)
        ):
            continue
        # The pull request head itself must already be this tree, not just its merge.
        if request(f"git/commits/{run['head_sha']}")["tree"]["sha"] != tree:
            continue
        artifacts = request(f"actions/runs/{run['id']}/artifacts")["artifacts"]
        artifact = next(
            (a for a in artifacts if a.get("name") == ARTIFACT and not a.get("expired")), None
        )
        if artifact is None or not isinstance(artifact.get("id"), int):
            continue
        evidence = read_record(request(f"actions/artifacts/{artifact['id']}/zip", raw=True))
        if covers(evidence, plan, tree, repository):
            return str(run["id"])
    return ""


def qualified(sha: str, request=api) -> bool:
    """Whether a scheduled or manual release run already packaged this commit."""
    if not SHA.fullmatch(sha):
        raise ValueError("Qualification requires a full commit SHA")
    runs = request(f"actions/workflows/release.yml/runs?head_sha={sha}&status=success")[
        "workflow_runs"
    ]
    return any(
        run.get("head_sha") == sha
        and run.get("conclusion") == "success"
        and run.get("event") in {"schedule", "workflow_dispatch"}
        for run in runs
    )


def git(*args: str) -> str:
    return subprocess.check_output(["git", *args], text=True).strip()


def write_outputs(values: dict[str, object]) -> None:
    with open(os.environ["GITHUB_OUTPUT"], "a", encoding="utf-8") as stream:
        stream.writelines(
            f"{name}={value if isinstance(value, str) else json.dumps(value, separators=(',', ':'))}\n"
            for name, value in values.items()
        )


def select_command() -> None:
    plan = json.loads(Path("ci-plan.json").read_text())
    reused = ""
    if (
        os.environ.get("GITHUB_EVENT_NAME") == "push"
        and os.environ.get("GITHUB_REF") == "refs/heads/main"
        and os.environ.get("PUSH_FORCED") != "true"
    ):
        try:
            reused = find(plan, git("rev-parse", "HEAD^{tree}"), os.environ["GITHUB_REPOSITORY"])
        except (
            urllib.error.URLError,
            ValueError,
            KeyError,
            TypeError,
            zipfile.BadZipFile,
        ) as error:
            print(f"No reusable evidence ({error}); validating this commit.")
    if reused:
        print(f"Pull request run {reused} already validated this exact tree; skipping checks.")
        plan = skipped(plan)
    write_outputs(
        {
            **{
                name: plan[name]
                for name in ("rust", "service", "service_any", "packaging", "packaging_any")
            },
            "reused": reused,
        }
    )


def record_command() -> None:
    sha = git("rev-parse", "HEAD")
    if sha != os.environ.get("GITHUB_SHA"):
        raise ValueError(
            f"Checked out {sha}, but this run validated {os.environ.get('GITHUB_SHA')}"
        )
    plan = json.loads(Path("ci-plan.json").read_text())
    evidence = record(plan, git("rev-parse", "HEAD^{tree}"), sha, os.environ["GITHUB_REPOSITORY"])
    Path("evidence.json").write_text(json.dumps(evidence, indent=2) + "\n")
    print(json.dumps(evidence, indent=2))


def qualified_command() -> None:
    sha = os.environ["GITHUB_SHA"]
    needed = True
    if os.environ.get("GITHUB_EVENT_NAME") == "schedule":
        try:
            needed = not qualified(sha)
        except (urllib.error.URLError, ValueError, KeyError) as error:
            print(f"Could not check earlier qualification ({error}); qualifying.")
    if not needed:
        print(f"{sha} was already qualified by an earlier release run; skipping packaging.")
    write_outputs({"needed": str(needed).lower()})


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("command", choices=["select", "record", "qualified"])
    command = parser.parse_args().command
    {"select": select_command, "record": record_command, "qualified": qualified_command}[command]()


if __name__ == "__main__":
    main()
