import argparse
import json
import os
import shutil
import subprocess
import tempfile
import time
from pathlib import Path

import requests
from playwright.sync_api import sync_playwright

parser = argparse.ArgumentParser(
    description="Exercise Desktop navigation in Chromium with a temporary demo ledger."
)
parser.add_argument(
    "--binary", required=True, type=Path, help="Path to the built Desktop bk binary"
)
parser.add_argument(
    "--chromium",
    default=shutil.which("chromium"),
    help="Optional Chromium executable; defaults to PATH or Playwright's bundled browser",
)
args = parser.parse_args()
binary = str(args.binary.resolve())
repo = Path(__file__).resolve().parent.parent
(repo / "target").mkdir(exist_ok=True)
with tempfile.TemporaryDirectory(dir=repo / "target") as directory:
    directory = Path(directory)
    secret = directory / "passphrase"
    secret.write_text("browser-navigation-integration-only")
    secret.chmod(0o600)
    db = directory / "books.db"
    env = {
        k: v
        for k, v in os.environ.items()
        if k
        not in [
            "BK_AGENT_TOKEN",
            "BEANKEEPER_DB",
            "BEANKEEPER_COMPANY",
            "BEANKEEPER_PASSPHRASE_CMD",
        ]
    }

    def cli(*args):
        result = subprocess.run(
            [
                binary,
                "--db",
                str(db),
                "--passphrase-file",
                str(secret),
                "--json",
                *args,
            ],
            env=env,
            capture_output=True,
            text=True,
            check=True,
        )
        return json.loads(result.stdout)

    cli("init", "--encrypt", "--demo")
    cli("key", "init")
    issued = cli(
        "agent",
        "issue",
        "--name",
        "browser_navigation",
        "--caps",
        "ui/navigate",
        "--generate-key",
    )["data"]
    token = issued["token"]
    company = cli("company", "list")["data"][0]["slug"]
    server = subprocess.Popen(
        [
            binary,
            "--db",
            str(db),
            "--passphrase-file",
            str(secret),
            "serve",
            "--bind",
            "127.0.0.1:0",
        ],
        env=env | {"BEANKEEPER_COMPANY": company},
        stdout=subprocess.DEVNULL,
        stderr=subprocess.PIPE,
        text=True,
    )
    try:
        launch = None
        while True:
            line = server.stderr.readline()
            if line.startswith("Pair this browser "):
                launch = line.split(": ", 1)[1].strip()
            if "listening on " in line:
                base = line.split("listening on ")[1].split()[0].rstrip("/")
                break
            if not line:
                raise RuntimeError("server did not start")
        http = requests.Session()
        http.trust_env = False
        headers = {
            "Authorization": "Bearer " + token,
            "Accept": "application/json, text/event-stream",
            "MCP-Protocol-Version": "2025-11-25",
        }

        def rpc(method, params):
            response = http.post(
                base + "/mcp",
                headers=headers,
                json={"jsonrpc": "2.0", "id": 1, "method": method, "params": params},
                timeout=5,
            )
            response.raise_for_status()
            if "Mcp-Session-Id" in response.headers:
                headers["Mcp-Session-Id"] = response.headers["Mcp-Session-Id"]
            body = response.text
            try:
                return json.loads(body)
            except json.JSONDecodeError:
                return next(
                    json.loads(line[5:])
                    for line in body.splitlines()
                    if line.startswith("data:") and '"id"' in line
                )

        rpc(
            "initialize",
            {
                "protocolVersion": "2025-11-25",
                "capabilities": {},
                "clientInfo": {"name": "chromium-check", "version": "1"},
            },
        )
        http.post(
            base + "/mcp",
            headers=headers,
            json={"jsonrpc": "2.0", "method": "notifications/initialized"},
            timeout=5,
        ).raise_for_status()

        def call(name, arguments=None):
            if arguments is None:
                arguments = {}
            result = rpc("tools/call", {"name": name, "arguments": arguments})
            if result.get("error") or result["result"].get("isError"):
                raise AssertionError(result)
            return result["result"]["structuredContent"]

        with sync_playwright() as p:
            browser = p.chromium.launch(executable_path=args.chromium, headless=True)
            context = browser.new_context()
            page = context.new_page()
            errors = []
            page.on("pageerror", lambda e: errors.append(str(e)))
            page.goto(launch)
            assert not context.cookies(), "GET paired the browser without confirmation"
            page.reload()
            page.get_by_role("button", name="Pair this browser", exact=True).click()
            unassociated = "Browser paired. Open your agent's connection link to enable navigation."
            page.get_by_text(unassociated, exact=True).wait_for()
            assert page.get_by_text("Agent navigation connected.", exact=True).count() == 0
            before = call("ui_status")
            assert before["paired"] is False and before["tabs"] == [], before
            for name, arguments in [
                ("ui_show_view", {"view_id": "accounts"}),
                ("ui_highlight", {"target": "main"}),
            ]:
                result = rpc("tools/call", {"name": name, "arguments": arguments})["result"]
                assert result["isError"] is True, result
                payload = result["structuredContent"]
                assert payload["code"] == "NO_CONNECTED_TAB", result
                assert payload["queued"] is False and payload["pairing_url"], result
            # Association must also update a tab whose stream is already open.
            waiting = context.new_page()
            waiting.goto(base)
            waiting.get_by_text(unassociated, exact=True).wait_for()
            invitation = before["pairing_url"]
            page.goto(invitation)
            page.get_by_role("button", name="Connect agent to this browser").click()
            page.get_by_text("Agent linked. Browser navigation ready.", exact=True).wait_for()
            waiting.get_by_text("Agent linked. Browser navigation ready.", exact=True).wait_for()
            assert waiting.locator("[data-agent-connection]").inner_text() == "Your agent is connected to this browser."
            waiting.close()

            def wait_status(count=1):
                for _ in range(50):
                    status = call("ui_status")
                    if len(status["tabs"]) == count:
                        return status
                    time.sleep(0.1)
                raise AssertionError(status)

            initial = wait_status()
            first = initial["tabs"][0]["tab_id"]
            assert call(
                "ui_show_view",
                {
                    "view_id": "transactions",
                    "state": {"company": company, "search": "rent & fees"},
                },
            )["queued"]
            page.wait_for_url("**/transactions?**")
            page.get_by_text("Agent linked. Browser navigation ready.", exact=True).wait_for()
            assert page.get_by_role(
                "heading", name="Transactions", exact=True
            ).is_visible()
            assert page.locator("input[name=search]").input_value() == "rent & fees"
            status = wait_status()
            assert status["tabs"][0]["tab_id"] == first, status
            assert "rent%20%26%20fees" in status["tabs"][0]["url"], status
            assert call("ui_highlight", {"target": "main"})["queued"]
            page.locator("main.agent-highlight").wait_for()
            page.get_by_text("Your agent highlighted main.", exact=True).wait_for()
            # Repeated heartbeat events must preserve the action acknowledgement.
            page.wait_for_timeout(750)
            assert page.get_by_text("Your agent highlighted main.", exact=True).is_visible()
            for _ in range(50):
                if call("ui_status")["tabs"][0]["highlight"] == "main":
                    break
                time.sleep(0.1)
            else:
                raise AssertionError("highlight not acknowledged")
            # A new page shares D3's cookie while retaining its own tab identity.
            second = context.new_page()
            second.goto(base + "/accounts?company=" + company)
            second.get_by_text("Agent linked. Browser navigation ready.", exact=True).wait_for()
            status = wait_status(2)
            ids = {tab["tab_id"] for tab in status["tabs"]}
            assert len(ids) == 2
            other = next(id for id in ids if id != first)
            assert call(
                "ui_show_view",
                {"view_id": "review", "state": {"company": company}, "tab_id": other},
            )["queued"]
            second.wait_for_url("**/review?**")
            second.get_by_text("Agent linked. Browser navigation ready.", exact=True).wait_for()
            assert page.get_by_role(
                "heading", name="Transactions", exact=True
            ).is_visible()
            # Error views retain the receiver and report the actual error title.
            assert call(
                "ui_show_view",
                {
                    "view_id": "transaction",
                    "state": {"company": company, "transaction_id": 99999999},
                    "tab_id": first,
                },
            )["queued"]
            page.wait_for_url("**/transactions/99999999?**")
            page.get_by_text("Agent linked. Browser navigation ready.", exact=True).wait_for()
            assert page.get_by_role(
                "heading", name="Unable to display this view"
            ).is_visible()
            status = wait_status(2)
            assert any("Unable to display" in tab["title"] for tab in status["tabs"]), (
                status
            )
            assert not errors, errors
            browser.close()
        print(
            "Chromium passed: unassociated browser status and navigation errors, live credential association, SSE navigation and filters, stable tab ID across loads, acknowledged highlight, two-tab isolation, and error-view reconnect."
        )
    finally:
        server.terminate()
        try:
            server.wait(timeout=5)
        except subprocess.TimeoutExpired:
            server.kill()
            server.wait()
