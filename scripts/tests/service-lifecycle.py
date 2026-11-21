#!/usr/bin/env python3
"""Exercise real per-user managers on disposable CI runners. Never run on an owner's machine."""

import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import signal
import socket
import subprocess
import sys
import tempfile
import time
import urllib.request
import urllib.error


def safe_log_tail(path):
    if not path.exists():
        return "no service log"
    content = path.read_text(encoding="utf-8", errors="replace")
    content = re.sub(r"([?&]token=)[^\s&]+", r"\1[redacted]", content)
    return content[-8000:]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binary", type=Path)
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
        # Keep client configs and LaunchAgent definitions away from runner defaults.
        # Linux systemd's own unit lookup uses the logged-in user's real home.
        if os.name == "nt":
            env["USERPROFILE"] = str(root / "home")
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
        finally:
            occupied.close()
            if installed:
                subprocess.run([str(binary), "service", "uninstall"], env=env,
                               capture_output=True, timeout=40)


if __name__ == "__main__":
    main()
