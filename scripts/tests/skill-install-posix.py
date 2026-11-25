#!/usr/bin/env python3
"""Execute the POSIX skill guide with fixture or published downloads."""

import argparse
from collections import Counter
import hashlib
import json
import os
from pathlib import Path
import platform
import re
import shutil
import subprocess
import sys
import tempfile

ROOT = Path(__file__).resolve().parents[2]
PROJECT = "Govcraft/beankeeper-desktop"
DOWNLOAD_BASE = "https://beankeeper.app/download"
INSTALLER_URL = f"{DOWNLOAD_BASE}/latest/install.sh"
OIDC_ISSUER = "https://token.actions.githubusercontent.com"
UNIT = "beankeeper-desktop.service"
LAUNCH_AGENT = "ai.govcraft.beankeeper-desktop"


def fixture_tool(mode, arguments):
    """Answer only the exact artifact and signature requests expected from the guide."""
    version = os.environ["BK_SKILL_TEST_VERSION"]
    archive = os.environ["BK_SKILL_TEST_ARCHIVE"]
    fixtures = Path(os.environ["BK_SKILL_TEST_FIXTURES"])
    allowed = {archive, "checksums-sha256.txt"}
    release_files = allowed | {f"{file}.sigstore" for file in allowed}
    if mode == "fixture-curl":
        urls = [argument for argument in arguments if argument.startswith("https://")]
        if len(urls) != 1:
            raise ValueError("expected one fixture URL")
        url = urls[0]
        if url == INSTALLER_URL:
            source = ROOT / "install.sh"
        elif url == f"{DOWNLOAD_BASE}/latest/version.txt":
            assert "-H" not in arguments, "mirror discovery must be anonymous"
            print(version)
            return
        else:
            base = f"{DOWNLOAD_BASE}/{version}/"
            if not url.startswith(base):
                raise ValueError(f"unexpected fixture URL {url}")
            name = url.removeprefix(base)
            if name not in release_files:
                raise ValueError(f"unexpected fixture file {name}")
            source = fixtures / name
        shutil.copyfile(source, arguments[arguments.index("-o") + 1])
        if "-w" in arguments:
            print("200", end="")
        return

    if not arguments or arguments[0] != "verify-blob":
        raise ValueError("expected cosign verify-blob")
    artifact = Path(arguments[-1]).name
    parameters = dict(zip(arguments[1:-1:2], arguments[2:-1:2], strict=True))
    bundle = Path(parameters["--bundle"])
    expected = f"https://github.com/{PROJECT}/.github/workflows/release.yml@refs/tags/beankeeper-desktop-v{version}"
    if (
        artifact not in allowed or bundle.name != f"{artifact}.sigstore" or not bundle.is_file()
        or parameters["--certificate-identity"] != expected
        or parameters["--certificate-oidc-issuer"] != OIDC_ISSUER
    ):
        raise ValueError("fixture verifier received unexpected identity or bundle")
    with (fixtures / "verified.txt").open("a", encoding="utf-8") as output:
        print(artifact, file=output)


def command(arguments, env, *, check=True, stdin=None):
    result = subprocess.run(
        [str(arg) for arg in arguments], env=env, capture_output=True,
        text=True, encoding="utf-8", timeout=120, input=stdin,
    )
    if check and result.returncode:
        raise RuntimeError(f"{' '.join(map(str, arguments))} failed:\n{result.stderr}")
    return result


def check_claude_code_connection(home, binary, env):
    config = json.loads((home / ".claude.json").read_text(encoding="utf-8"))
    server = config["mcpServers"]["beankeeper"]
    assert Path(server["command"]).resolve() == binary.resolve()
    requests = [
        {"id": 1, "method": "initialize", "params": {
            "protocolVersion": "2025-11-25", "capabilities": {},
            "clientInfo": {"name": "skill-install-ci", "version": "1"},
        }},
        {"method": "notifications/initialized"},
        {"id": 2, "method": "tools/call", "params": {"name": "company_list", "arguments": {}}},
    ]
    messages = "".join(json.dumps({"jsonrpc": "2.0", **request}) + "\n" for request in requests)
    result = command([server["command"], *server["args"]], env, stdin=messages)
    replies = {reply["id"]: reply["result"] for reply in map(json.loads, result.stdout.splitlines())}
    assert replies[1]["serverInfo"]["name"] == "beankeeper"
    assert not replies[2].get("isError", False), replies[2]


def native_config(env, sandbox):
    if sys.platform == "darwin":
        domain = f"gui/{os.getuid()}"
        command(["launchctl", "print", domain], env)
        existing = command(["launchctl", "print", f"{domain}/{LAUNCH_AGENT}"], env, check=False)
        if existing.returncode == 0:
            raise RuntimeError("an existing Beankeeper service owns the login registration")
        return sandbox / "config"

    # systemd links its unit in the logged-in user's own search path even
    # though the documented installation block uses an isolated fixture home.
    config = Path(env.get("XDG_CONFIG_HOME") or Path.home() / ".config")
    if not config.is_absolute():
        raise RuntimeError("the systemd user configuration path must be absolute")
    unit = config / "systemd/user" / UNIT
    existing = command(
        ["systemctl", "--user", "show", "--property=LoadState", "--value", UNIT],
        env, check=False,
    )
    if unit.exists() or unit.is_symlink() or existing.stdout.strip() != "not-found":
        raise RuntimeError("a clean systemd user bus with no Beankeeper unit is required")
    if any((config / name).exists() for name in ("Claude", "Code")):
        raise RuntimeError("the runner's desktop config must be unused by local agents")
    return config


def fixture_path(sandbox, original_path):
    directory = sandbox / "commands"
    directory.mkdir()
    required = ("sh", "uname", "awk", "cat", "tar", "install", "mktemp", "mkdir", "mv", "rm", "gzip", "grep", "id")
    manager = "launchctl" if sys.platform == "darwin" else "systemctl"
    reader = "osascript" if sys.platform == "darwin" else "python3"
    for name in (*required, manager, reader):
        path = shutil.which(name, path=original_path)
        if path is None:
            raise RuntimeError(f"the fixture needs {name}")
        (directory / name).symlink_to(path)
    for name in ("sha256sum", "shasum"):
        if path := shutil.which(name, path=original_path):
            (directory / name).symlink_to(path)
    for name in ("curl", "cosign"):
        shim = directory / name
        shim.write_text(
            f'#!/bin/sh\nexec "$BK_SKILL_TEST_PYTHON" "$BK_SKILL_TEST_DRIVER" fixture-{name} "$@"\n',
            encoding="utf-8",
        )
        shim.chmod(0o755)
    return directory


def prepare_fixture(target, artifacts, sandbox, env):
    version = subprocess.check_output(
        [sys.executable, str(ROOT / "scripts/release.py"), "version"], text=True,
    ).strip()
    archive = f"bk-{version}-{target}.tar.gz"
    fixtures = sandbox / "fixtures"
    fixtures.mkdir()
    staged_archive = fixtures / archive
    shutil.copyfile(artifacts / archive, staged_archive)
    with staged_archive.open("rb") as source:
        digest = hashlib.file_digest(source, "sha256").hexdigest()
    (fixtures / "checksums-sha256.txt").write_text(f"{digest}  {archive}\n", encoding="utf-8")
    for name in (archive, "checksums-sha256.txt"):
        (fixtures / f"{name}.sigstore").write_text("fixture bundle\n", encoding="utf-8")
    env.update(
        PATH=str(fixture_path(sandbox, env.get("PATH", os.defpath))),
        BK_SKILL_TEST_VERSION=version, BK_SKILL_TEST_ARCHIVE=archive, BK_SKILL_TEST_FIXTURES=str(fixtures),
        BK_SKILL_TEST_PYTHON=sys.executable, BK_SKILL_TEST_DRIVER=str(Path(__file__).resolve()),
    )
    return version, archive, fixtures


def exercise(target, artifacts, published):
    block = re.search(r"```sh\n(.*?)```", (ROOT / "SKILL.md").read_text(encoding="utf-8"), re.DOTALL)
    if block is None:
        raise RuntimeError("missing the Linux and macOS installation block in SKILL.md")

    with tempfile.TemporaryDirectory(prefix="bk skill café-") as temporary:
        sandbox = Path(temporary).resolve()
        home = sandbox / "profile"
        codex = home / ".codex"
        codex.mkdir(parents=True)
        (home / ".claude").mkdir()
        env = dict(os.environ)
        config = native_config(env, sandbox)
        for name in ("BEANKEEPER_DB", "BEANKEEPER_PASSPHRASE_CMD", "BK_AGENT_TOKEN", "BK_VERSION", "BK_DOWNLOAD_BASE"):
            env.pop(name, None)
        env.update(
            HOME=str(home), XDG_CONFIG_HOME=str(config), XDG_DATA_HOME=str(sandbox / "xdg-data"),
            CODEX_HOME=str(codex), CLAUDE_CONFIG_DIR=str(home / ".claude"),
            COPILOT_HOME=str(home / ".copilot"), GROK_HOME=str(home / ".grok"),
            BEANKEEPER_SERVICE_DATA_DIR=str(sandbox / "data"),
            BEANKEEPER_DESKTOP_STATE_DIR=str(sandbox / "state"),
            BEANKEEPER_SERVICE_LOG_DIR=str(sandbox / "logs"), BEANKEEPER_NO_UPDATE_CHECK="1",
        )
        if not published:
            env.pop("GH_TOKEN", None)
            env.pop("GITHUB_TOKEN", None)
            version, archive, fixtures = prepare_fixture(target, artifacts, sandbox, env)
        script = sandbox / "guide.sh"
        guide = block.group(1)
        if published:
            # Exercise the checkout before it reaches the guide's public bootstrap URL.
            # The installer runs in a child shell with real curl and cosign.
            env.update(BK_SKILL_TEST_INSTALLER_URL=INSTALLER_URL, BK_SKILL_TEST_INSTALLER=str(ROOT / "install.sh"))
            guide = '''\
curl() {
    if [ "$#" -eq 4 ] && [ "$1" = -fsSL ] &&
        [ "$2" = "$BK_SKILL_TEST_INSTALLER_URL" ] && [ "$3" = -o ]; then
        cp "$BK_SKILL_TEST_INSTALLER" "$4"
    else
        command curl "$@"
    fi
}
''' + guide
        script.write_text(guide, encoding="utf-8")
        binary = home / ".local/bin/bk"
        books = sandbox / "data/books.db"
        recovery = books.with_suffix(".recovery.txt")
        try:
            result = command(["/bin/sh", script], env)
            if published:
                version = command([binary, "--version"], env).stdout.strip().removeprefix("bk ")
            else:
                if result.stdout.count("Encrypted ledger: ") != 1:
                    raise RuntimeError("the guide must provision the service once")
                verified = (fixtures / "verified.txt").read_text(encoding="utf-8").splitlines()
                if Counter(verified) != Counter([archive, "checksums-sha256.txt"]):
                    raise RuntimeError("the guide did not verify both artifact identities exactly once")

            status = json.loads(command([binary, "service", "status", "--json"], env).stdout)
            if not (status["installed"] and status["running"] and status["version"] == version):
                raise RuntimeError("the guide did not start the native service")
            if Path(status["books"]).resolve() != books.resolve() or not books.is_file() or not recovery.is_file():
                raise RuntimeError("the first-run service did not create the isolated books and recovery kit")
            if recovery.stat().st_mode & 0o077:
                raise RuntimeError("the recovery kit must be private to the owner")
            doctor = json.loads(command([binary, "agents", "doctor", "codex", "--json"], env).stdout)
            if doctor["data"]["agents"][0]["state"] != "configured" or doctor["data"]["service"]["state"] != "ready":
                raise RuntimeError("the guide did not configure a usable agent connection")
            check_claude_code_connection(home, binary, env)
        finally:
            if binary.is_file():
                command([binary, "service", "uninstall"], env)
        if not (books.is_file() and recovery.is_file()):
            raise RuntimeError("service uninstall must preserve the books and recovery kit")
        print(f"Skill guide installed bk {version} for {target} and passed the native MCP check")


def main():
    if len(sys.argv) > 1 and sys.argv[1] in ("fixture-curl", "fixture-cosign"):
        fixture_tool(sys.argv[1], sys.argv[2:])
        return
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--target", required=True)
    source = parser.add_mutually_exclusive_group()
    source.add_argument("--artifacts", type=Path, default=ROOT / "dist")
    source.add_argument("--published", action="store_true", help="use real downloads and Sigstore verification")
    args = parser.parse_args()
    runner_os = {"linux": "Linux", "darwin": "macOS"}.get(sys.platform)
    if os.environ.get("GITHUB_ACTIONS") != "true" or not runner_os or os.environ.get("RUNNER_OS") != runner_os:
        parser.error("requires a disposable Linux or macOS GitHub Actions runner")
    machine = {"x86_64": "x86_64", "arm64": "aarch64", "aarch64": "aarch64"}.get(platform.machine())
    os_target = "apple-darwin" if sys.platform == "darwin" else "unknown-linux-gnu"
    if args.target != f"{machine}-{os_target}":
        parser.error("the release target must match the native runner")
    exercise(args.target, args.artifacts, args.published)


if __name__ == "__main__":
    main()
