#!/usr/bin/env python3
"""Exercise real per-user managers on disposable CI runners. Never run on an owner's machine."""

import argparse
from contextlib import contextmanager
import filecmp
import hashlib
import json
import os
from pathlib import Path
from queue import Queue
import re
import shutil
import signal
import socket
import stat
import subprocess
import sys
import tempfile
from threading import Thread
import time
import urllib.request
import urllib.error
import zipfile


def safe_log_tail(path):
    if not path.exists():
        return "no service log"
    content = path.read_text(encoding="utf-8", errors="replace")
    content = re.sub(r"([?&]token=)[^\s&]+", r"\1[redacted]", content)
    return content[-8000:]


def assert_minimal_linux_status(binary, root, first):
    import pwd

    # Keep the live, systemd-managed fixture isolated while allowing the client
    # to discover it through default paths under a home of its own.
    home = root / "minimal launcher home"
    definition = Path(first["service_definition"])
    unit = home / ".config/systemd/user" / definition.name
    unit.parent.mkdir(parents=True)
    unit.symlink_to(definition)
    state = home / ".local/state/beankeeper-desktop"
    state.parent.mkdir(parents=True)
    state.symlink_to(root / "state", target_is_directory=True)
    account = pwd.getpwuid(os.getuid())
    result = subprocess.run(
        ["env", "-i", f"HOME={home}", f"LOGNAME={account.pw_name}",
         f"USER={account.pw_name}", f"SHELL={account.pw_shell}", f"PATH={os.environ['PATH']}",
         str(binary), "service", "status", "--json"],
        capture_output=True, text=True, encoding="utf-8", timeout=30,
    )
    assert result.returncode == 0, "minimal-environment service status failed"
    status = json.loads(result.stdout)
    assert status["installed"] and status["running"], "minimal environment lost the live service"
    assert status["pid"] == first["pid"]
    assert status["mcp_endpoint"] == first["mcp_endpoint"]
    print("Linux status found the live service with only HOME, LOGNAME, USER, SHELL and PATH")


def extension_launcher(binary, cache, bundle):
    if bundle is None:
        launcher = cache / binary.name
        shutil.copy2(binary, launcher)
        return launcher, ("mcp", "extension"), None

    with zipfile.ZipFile(bundle) as archive:
        manifest = json.loads(archive.read("manifest.json"))
        server = manifest["server"]
        entry = server["entry_point"]
        assert server["type"] == "binary" and entry == f"server/{binary.name}"
        config = server["mcp_config"]
        assert config == {"command": "${__dirname}/" + entry, "args": ["mcp", "extension"]}
        member = archive.getinfo(entry)
        mode = member.external_attr >> 16
        assert stat.S_ISREG(mode) and mode & 0o111
        launcher = cache / entry
        launcher.parent.mkdir()
        with archive.open(member) as source, launcher.open("xb") as destination:
            shutil.copyfileobj(source, destination)
        launcher.chmod(mode & 0o777)
        assert filecmp.cmp(launcher, binary, shallow=False)
    return launcher, tuple(config["args"]), manifest


@contextmanager
def extension_session(launcher, arguments, env):
    """Keep the same stdio session open while setup runs and tools arrive."""
    with tempfile.TemporaryFile(mode="w+", encoding="utf-8") as errors:
        with subprocess.Popen(
            [str(launcher), *arguments], env=env, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
            stderr=errors, text=True, encoding="utf-8",
        ) as process:
            replies = Queue()

            def read():
                try:
                    for line in process.stdout:
                        replies.put(json.loads(line))
                except ValueError as error:
                    replies.put(error)
                finally:
                    replies.put(None)

            reader = Thread(target=read, daemon=True)
            reader.start()
            next_id = 0
            notifications = []

            def request(method, params, timeout=10):
                nonlocal next_id
                next_id += 1
                frame = {"jsonrpc": "2.0", "id": next_id, "method": method, "params": params}
                process.stdin.write(json.dumps(frame) + "\n")
                process.stdin.flush()
                deadline = time.monotonic() + timeout
                while True:
                    reply = replies.get(timeout=max(0, deadline - time.monotonic()))
                    assert isinstance(reply, dict), "extension stopped or sent non-MCP stdout"
                    if "id" in reply:
                        assert reply["id"] == next_id and "error" not in reply
                        return reply["result"]
                    notifications.append(reply["method"])

            def tool(name, arguments=None, timeout=10):
                return request("tools/call", {"name": name, "arguments": arguments or {}}, timeout)

            try:
                initialized = request("initialize", {
                    "protocolVersion": "2025-11-25", "capabilities": {},
                    "clientInfo": {"name": "extension-ci", "version": "1"},
                })
                assert initialized["serverInfo"]["name"] == "beankeeper"
                assert initialized["capabilities"]["tools"]["listChanged"]
                assert "setup_status" in initialized["instructions"]
                process.stdin.write('{"jsonrpc":"2.0","method":"notifications/initialized"}\n')
                yield request, tool, notifications
                process.stdin.close()
                assert process.wait(timeout=10) == 0, "extension exited unsuccessfully"
            finally:
                if process.poll() is None:
                    process.kill()
                reader.join(timeout=10)


def setup_in_extension(launcher, arguments, env, definition):
    with extension_session(launcher, arguments, env) as (request, tool, notifications):
        tools = {entry["name"] for entry in request("tools/list", {})["tools"]}
        assert tools == {"setup", "setup_status", "doctor"}
        assert tool("setup_status")["structuredContent"]["state"] == "not_started"
        rejected = tool("setup", {"url": "unexpected", "command": "unexpected"})
        assert rejected["isError"] and rejected["structuredContent"]["code"] == "SERVICE_ARGUMENTS"
        assert not definition.exists() and not definition.is_symlink()
        assert tool("setup")["structuredContent"]["state"] == "running"

        deadline = time.monotonic() + 100
        while True:
            progress = tool("setup_status")["structuredContent"]
            if progress["state"] != "running":
                break
            assert time.monotonic() < deadline, "local service setup never became ready"
            time.sleep(0.2)
        assert progress["state"] == "ready", progress
        assert progress["registration"] == {"checked": []}, progress
        assert not progress["claude_restart_required"]
        assert "notifications/tools/list_changed" in notifications
        assert tool("setup")["structuredContent"]["state"] == "ready"
        assert tool("doctor")["structuredContent"]["state"] == "ready"
        tools = {entry["name"] for entry in request("tools/list", {})["tools"]}
        assert {"company_list", "setup", "setup_status", "doctor", "demo_start"} <= tools

        # Set up my books uses the existing onboarding tool on this connection.
        ledger = tool("setup_status")["structuredContent"]
        expected = {key: ledger.get(key) for key in ("state", "next_step", "companies", "accounts")}
        assert expected == {
            "state": "installed", "next_step": "list_accounts", "companies": 1, "accounts": 0,
        }, expected

        # Create demo books uses the existing isolated practice ledger in the same chat.
        practice = tool("demo_start", timeout=30)["structuredContent"]
        assert practice["state"] == "active"
        companies = tool("company_list")
        assert companies["_meta"]["beankeeper/ledgerTarget"] == practice["target"]
        assert tool("demo_remove")["structuredContent"]["state"] == "removed"
        assert tool("setup_status")["structuredContent"]["companies"] == ledger["companies"]


def exercise_extension(binary, root, base_env, bundle=None):
    """Exercise the MCPB launch command with a fresh native service registration."""
    parent = root / "fresh extension"
    cache = parent / "Claude cache"
    cache.mkdir(parents=True)
    launcher, arguments, manifest = extension_launcher(binary, cache, bundle)
    env = {
        **base_env,
        "BEANKEEPER_SERVICE_DATA_DIR": str(parent / "data"),
        "BEANKEEPER_DESKTOP_STATE_DIR": str(parent / "state"),
        "BEANKEEPER_SERVICE_LOG_DIR": str(parent / "logs"),
        "CODEX_HOME": str(parent / "codex"),
    }
    if sys.platform == "darwin":
        # Use the login session that prepared the native keychain. The ledger,
        # service state and logs still live under the test's temporary root.
        env["HOME"] = str(Path.home())
    home = Path(env["USERPROFILE" if os.name == "nt" else "HOME"])
    if manifest is not None and os.name == "nt":
        local_data = parent / "Local app data"
        env["LOCALAPPDATA"] = str(local_data)
        env.pop("CLAUDE_USER_DATA_DIR", None)
        claude_directory = local_data / "Claude-Data" / "claude_pzs8sxrjxfjjc"
    elif manifest is not None:
        claude_directory = parent / "Claude profile"
        env["CLAUDE_USER_DATA_DIR"] = str(claude_directory)
    elif os.name == "nt":
        claude_directory = Path(env.get("APPDATA") or home / "AppData/Roaming") / "Claude"
    else:
        support = (home / "Library/Application Support" if sys.platform == "darwin"
                   else Path(env.get("XDG_CONFIG_HOME") or home / ".config"))
        claude_directory = support / "Claude"
    claude_desktop = claude_directory / "claude_desktop_config.json"
    claude_desktop.parent.mkdir(parents=True, exist_ok=True)
    # Even a detected Claude profile without a registry entry must be skipped.
    # Never replace pre-existing configuration (native smoke also runs without a bundle).
    had_claude_config = claude_desktop.exists()
    if not had_claude_config:
        claude_desktop.write_text('{"mcpServers":{}}', encoding="utf-8")
    before = claude_desktop.read_bytes()
    codex_home = Path(env["CODEX_HOME"])
    codex_home.mkdir()

    def run(executable, *arguments, **kwargs):
        return subprocess.run(
            [str(executable), *arguments], env=kwargs.pop("env", env), capture_output=True,
            text=True, encoding="utf-8", timeout=120, **kwargs,
        )

    if sys.platform == "darwin":
        keychain = run("/usr/bin/security", "default-keychain", "-d", "user")
        assert keychain.returncode == 0, keychain.stderr
        assert Path(keychain.stdout.strip().strip('"')).resolve() == Path(env["BK_TEST_KEYCHAIN"]).resolve()

    baseline = run(binary, "service", "status", "--json")
    assert baseline.returncode == 0, baseline.stderr
    definition = Path(json.loads(baseline.stdout)["service_definition"])
    assert not definition.exists() and not definition.is_symlink(), "service must start unregistered"

    try:
        setup_in_extension(launcher, arguments, env, definition)

        # Startup points outside the extension cache, allowing the host app to
        # replace or remove the bundle while the service and books are intact.
        launcher.unlink()
        installed = parent / "state" / "claude-extension" / launcher.name
        assert installed.is_file()
        output = run(installed, "service", "status", "--json")
        assert output.returncode == 0, output.stderr
        status = json.loads(output.stdout)
        assert status["installed"] and status["running"]
        books = Path(status["books"])
        assert books.with_suffix(".recovery.txt").exists()
        assert not books.with_suffix(".passphrase").exists()
        metadata = json.loads(books.with_suffix(".desktop.json").read_text(encoding="utf-8"))
        assert metadata["secret_store_file"] is None
        assert not books.read_bytes().startswith(b"SQLite format 3")
        assert claude_desktop.read_bytes() == before
        codex_config = (codex_home / "config.toml").read_text(encoding="utf-8")
        assert installed.name in codex_config and "claude-extension" in codex_config
        if manifest is not None:
            assert status["version"] == manifest["version"]
            for action in ("stop", "start"):
                result = run(installed, "service", action)
                assert result.returncode == 0, result.stderr + safe_log_tail(parent / "logs/service.log")
            restarted = run(installed, "service", "status", "--json")
            assert restarted.returncode == 0, restarted.stderr
            status = json.loads(restarted.stdout)
            assert status["installed"] and status["running"]
            exercise_claude_doctor(run, installed, claude_directory, manifest, status)
            print("Packaged Claude extension first launch, setup_status, and doctor registry fixture passed")
            return

        # Exercise the same authenticated tool through the public doctor command.
        agent_env = env.copy()
        if sys.platform.startswith("linux"):
            agent_env["XDG_CONFIG_HOME"] = env.get("XDG_CONFIG_HOME", str(home / ".config"))
            agent_env["HOME"] = str(parent / "agent-home")
        connected = run(installed, "agents", "connect", "codex", "--json", env=agent_env)
        assert connected.returncode == 0, connected.stdout + connected.stderr
        doctor = run(installed, "agents", "doctor", "codex", "--json", env=agent_env)
        assert doctor.returncode == 0, doctor.stdout + doctor.stderr
        diagnostic = json.loads(doctor.stdout)
        assert diagnostic["data"]["agents"][0]["state"] == "configured"
        assert diagnostic["data"]["service"]["state"] == "ready"
        assert diagnostic["data"]["service"]["endpoint"] == status["mcp_endpoint"]

        # An explicitly stopped service must stay stopped on the next launch.
        stopped = run(installed, "service", "stop")
        assert stopped.returncode == 0, stopped.stderr
        with extension_session(installed, ("mcp", "extension"), env) as (_, tool, _):
            unavailable = tool("doctor")
            assert unavailable["isError"]
            assert unavailable["structuredContent"]["code"] in ("SERVICE_NOT_READY", "SESSION_ENV_UNAVAILABLE")
        doctor = run(installed, "agents", "doctor", "codex", "--json", env=agent_env)
        assert doctor.returncode != 0, doctor.stdout + doctor.stderr
        assert json.loads(doctor.stdout)["data"]["service"]["error"]["code"] == "SERVICE_NOT_READY"
        disconnected = run(installed, "agents", "disconnect", "codex", "--json", env=agent_env)
        assert disconnected.returncode == 0, disconnected.stdout + disconnected.stderr
        print("Claude extension first launch, MCP tools, durable executable and persistent stop passed")
    finally:
        run(binary, "service", "uninstall")
        if not had_claude_config:
            claude_desktop.unlink()


def exercise_claude_doctor(run, binary, directory, manifest, service):
    directory.mkdir(parents=True, exist_ok=True)
    registry = directory / "extensions-installations.json"
    extension_id = "local.mcpb.govcraft.beankeeper"
    installations = {"extensions": {extension_id: {"id": extension_id, "manifest": manifest}}}
    registry.write_text(json.dumps(installations), encoding="utf-8")
    settings = directory / "Claude Extensions Settings" / f"{extension_id}.json"
    settings.parent.mkdir()
    settings.write_text(json.dumps({"isEnabled": True}), encoding="utf-8")

    output = run(binary, "agents", "doctor", "claude-desktop", "--json")
    assert output.returncode == 0, output.stdout + output.stderr
    diagnostic = json.loads(output.stdout)
    assert diagnostic["ok"]
    assert diagnostic["data"]["agents"][0]["state"] == "extension_managed"
    assert diagnostic["data"]["service"]["state"] == "ready"
    assert diagnostic["data"]["service"]["endpoint"] == service["mcp_endpoint"]

    settings.write_text(json.dumps({"isEnabled": False}), encoding="utf-8")
    output = run(binary, "agents", "doctor", "claude-desktop", "--json")
    assert output.returncode != 0, output.stdout + output.stderr
    diagnostic = json.loads(output.stdout)
    assert not diagnostic["ok"]
    assert diagnostic["data"]["agents"][0]["state"] == "extension_disabled"
    assert diagnostic["data"]["agents"][0]["error"]["code"] == "AGENT_EXTENSION_DISABLED"
    assert diagnostic["data"]["service"]["state"] == "ready"


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binary", type=Path)
    parser.add_argument("--extension", type=Path, help="exercise the packaged Claude extension only")
    args = parser.parse_args()
    if os.environ.get("CI") != "true":
        parser.error("requires CI=true and a disposable runner")
    with tempfile.TemporaryDirectory(prefix="bk service café 's owner’s-") as temporary:
        root = Path(temporary)
        env = dict(os.environ)
        env.update(BEANKEEPER_SERVICE_DATA_DIR=str(root / "data"),
                   BEANKEEPER_DESKTOP_STATE_DIR=str(root / "state"),
                   BEANKEEPER_SERVICE_LOG_DIR=str(root / "logs"),
                   BEANKEEPER_NO_UPDATE_CHECK="1")
        for key in ("BEANKEEPER_DB", "BEANKEEPER_PASSPHRASE_CMD", "BK_AGENT_TOKEN"):
            env.pop(key, None)
        if sys.platform.startswith("linux"):
            # Simulate SSH, tmux and GUI launchers that never inherited these
            # variables. CI prepares the real user bus and Secret Service first.
            env.pop("XDG_RUNTIME_DIR", None)
            env.pop("DBUS_SESSION_BUS_ADDRESS", None)
        # Keep client configs and LaunchAgent definitions away from runner defaults.
        # Linux systemd's own unit lookup uses the logged-in user's real home.
        if os.name == "nt":
            env["USERPROFILE"] = str(root / "home")
            env["APPDATA"] = str(root / "home/AppData/Roaming")
        elif sys.platform == "darwin":
            env["HOME"] = str(root / "home")
        else:
            env["HOME"] = str(Path.home())
        (root / "home").mkdir()
        binary = root / ("bk.exe" if os.name == "nt" else "bk")
        shutil.copy2(args.binary.resolve(), binary)
        if os.name == "nt":
            # Verify the actual PE header, not merely the build-script text.
            image = binary.read_bytes()
            optional = int.from_bytes(image[0x3c:0x40], "little") + 24
            assert int.from_bytes(image[optional:optional + 2], "little") == 0x20b
            reserve = int.from_bytes(image[optional + 72:optional + 80], "little")
            assert reserve >= 8 * 1024 * 1024, f"Windows main stack reserve is only {reserve} bytes"
        if args.extension is not None:
            exercise_extension(binary, root, env, args.extension)
            return
        occupied = socket.socket()
        occupied.bind(("127.0.0.1", 8750))
        occupied.listen()
        installed = False

        def command(*arguments):
            result = subprocess.run([str(binary), *arguments], env=env, text=True,
                                    capture_output=True, encoding="utf-8", timeout=100)
            if result.returncode:
                log = root / "logs/service.log"
                # Failure logs must never include recovery or credential files.
                diagnostics = safe_log_tail(log)
                raise AssertionError(f"{' '.join(arguments)} failed:\n{result.stderr}\n{diagnostics}")
            return result.stdout

        def status():
            return json.loads(command("service", "status", "--json"))

        def diagnostics():
            log = root / "logs/service.log"
            details = safe_log_tail(log)
            if os.name == "nt":
                task = subprocess.run(["powershell.exe", "-NoProfile", "-NonInteractive", "-Command",
                    "$sid = [Security.Principal.WindowsIdentity]::GetCurrent().User.Value; "
                    "$name = 'Beankeeper-Desktop-' + $sid; "
                    "Get-ScheduledTask -TaskName $name | Select-Object State | ConvertTo-Json -Compress; "
                    "Get-ScheduledTaskInfo -TaskName $name | Select-Object LastTaskResult,LastRunTime,NextRunTime | ConvertTo-Json -Compress"],
                    capture_output=True, encoding="utf-8", errors="replace", timeout=15)
                details += "\nTask Scheduler: " + task.stdout + task.stderr
            return details

        def windows_task_diagnostics():
            script = """
                $sid = [Security.Principal.WindowsIdentity]::GetCurrent().User.Value
                $name = "Beankeeper-Desktop-$sid"
                Get-ScheduledTask -TaskName $name | Select-Object State | Format-List
                Get-ScheduledTaskInfo -TaskName $name |
                    Select-Object LastRunTime, NextRunTime, LastTaskResult, NumberOfMissedRuns |
                    Format-List
                Get-WinEvent -FilterHashtable @{
                    LogName = 'Microsoft-Windows-TaskScheduler/Operational'
                    StartTime = (Get-Date).AddMinutes(-10)
                } -ErrorAction SilentlyContinue |
                    Where-Object { $_.Message -like "*$name*" } |
                    Select-Object -First 20 TimeCreated, Id, Message | Format-List
            """
            result = subprocess.run(
                ["powershell.exe", "-NoProfile", "-NonInteractive", "-Command", script],
                env=env, capture_output=True, text=True, encoding="utf-8",
                errors="replace", timeout=15,
            )
            return result.stdout + result.stderr

        def wait(predicate, seconds=100):
            deadline = time.monotonic() + seconds
            state = None
            while time.monotonic() < deadline:
                state = status()
                if predicate(state):
                    return state
                time.sleep(0.5)
            details = windows_task_diagnostics() if os.name == "nt" else ""
            log = root / "logs/service.log"
            if log.exists():
                details += safe_log_tail(log)
            raise AssertionError(f"service did not reach expected state: {state}\n{details}")

        try:
            if os.name == "nt":
                # Hosted runners may leave Task Scheduler history disabled.
                subprocess.run(["wevtutil.exe", "sl", "Microsoft-Windows-TaskScheduler/Operational",
                                "/e:true"], capture_output=True, timeout=15)
            assert not status()["installed"]
            # Native keychains are not available on all hosted runners. The explicit
            # file seam exercises protected Windows DACLs as well as Unix modes.
            installed = True  # Also clean up partially successful installations.
            command("service", "install", "--file-store")
            first = wait(lambda s: s["running"])
            if sys.platform.startswith("linux"):
                assert_minimal_linux_status(binary, root, first)
            assert first["mcp_endpoint"].startswith("http://127.0.0.1:")
            assert ":8750/" not in first["mcp_endpoint"], "occupied port was not avoided"
            version = command("--version").strip().removeprefix("bk ")
            assert first["version"] == version
            client = urllib.request.build_opener(urllib.request.ProxyHandler({}))
            with client.open(first["desktop_url"] + "health", timeout=5) as response:
                assert response.read() == b"beankeeper-desktop-v1"
            books = Path(first["books"])
            assert books.exists() and not books.read_bytes().startswith(b"SQLite format 3")
            recovery = books.with_suffix(".recovery.txt")
            owner = books.with_suffix(".owner-token")
            agent = books.with_suffix(".agent-token")
            fingerprint = lambda path: hashlib.sha256(path.read_bytes()).hexdigest()
            credentials = {path: fingerprint(path) for path in (recovery, owner, agent)}
            if os.name != "nt":
                for path in (*credentials, books.with_suffix(".passphrase")):
                    assert path.stat().st_mode & 0o077 == 0
            else:
                for path in (*credentials, books.with_suffix(".passphrase")):
                    checked = subprocess.run(["powershell.exe", "-NoProfile", "-NonInteractive", "-Command",
                        "$acl = Get-Acl -LiteralPath $env:BK_TEST_CREDENTIAL; "
                        "$owner = $acl.GetOwner([Security.Principal.SecurityIdentifier]).Value; "
                        "$user = [Security.Principal.WindowsIdentity]::GetCurrent().User.Value; "
                        "if ($owner -ne $user) { throw 'Credential owner is not the current user' }"],
                        env={**env, "BK_TEST_CREDENTIAL": str(path)}, capture_output=True, text=True, encoding="utf-8", errors="replace", timeout=15)
                    assert checked.returncode == 0, checked.stderr
            # Add recognizable ledger content through the public ledger CLI, then
            # verify both data and credentials survive replacement and reinstall.
            added = subprocess.run([str(binary), "--db", str(books), "company", "create",
                                    "service-persistence", "Service persistence"],
                                   env={**env, "BK_AGENT_TOKEN": owner.read_text().strip()},
                                   capture_output=True, text=True, encoding="utf-8", timeout=30)
            assert added.returncode == 0, added.stderr
            if os.name == "nt":
                subprocess.run(["taskkill", "/F", "/PID", str(first["pid"])], check=True, capture_output=True)
            else:
                os.kill(first["pid"], signal.SIGKILL)
            restarted = wait(lambda s: s["running"] and s["pid"] != first["pid"])
            assert restarted["version"] == version
            command("service", "stop")
            command("service", "stop")  # Persistent stop is idempotent.
            stopped = status()
            assert stopped["installed"] and not stopped["running"]
            time.sleep(3)
            assert not status()["running"], "stop allowed an automatic restart"
            try:
                with client.open(restarted["desktop_url"] + "health", timeout=2):
                    raise AssertionError("stopped host is still accepting requests")
            except urllib.error.HTTPError as error:
                raise AssertionError("stopped host is still accepting requests") from error
            except (urllib.error.URLError, ConnectionError, TimeoutError):
                pass
            # Replace the installed executable as an installer does. Reinstallation
            # must register the new inode/path and reuse existing books and secrets.
            replacement = root / ("replacement.exe" if os.name == "nt" else "replacement")
            shutil.copy2(args.binary.resolve(), replacement)
            os.replace(replacement, binary)
            command("service", "install", "--file-store")
            assert status()["running"]
            for path, digest in credentials.items():
                assert fingerprint(path) == digest, f"reinstall rotated {path.name}"
            listed = subprocess.run([str(binary), "--db", str(books), "company", "list"],
                                    env={**env, "BK_AGENT_TOKEN": agent.read_text().strip()},
                                    capture_output=True, text=True, encoding="utf-8", timeout=30)
            assert listed.returncode == 0 and "service-persistence" in listed.stdout, listed.stderr
            command("service", "stop")
            command("service", "start")
            assert status()["running"]
            command("service", "uninstall")
            installed = False
            assert not status()["installed"] and not status()["running"]
            assert books.exists()
            for path, digest in credentials.items():
                assert fingerprint(path) == digest
            print("Service install, fallback, health, crash restart, persistent stop, upgrade, data preservation and uninstall passed")
            exercise_extension(binary, root, env)
        finally:
            occupied.close()
            if installed:
                subprocess.run([str(binary), "service", "uninstall"], env=env,
                               capture_output=True, timeout=40)


if __name__ == "__main__":
    main()
