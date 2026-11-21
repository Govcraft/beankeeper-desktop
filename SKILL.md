---
name: beankeeper-desktop
description: Work with the user's encrypted local Beankeeper Desktop workspace over authenticated MCP.
---

# Beankeeper Desktop

## Install and set up Beankeeper on this machine

Run `bk service status --json` in the owner's logged-in user session. If
`bk` is missing from PATH, check the per-user install location first.
That is `$HOME/.local/bin/bk` on Linux and macOS, and
`$env:LOCALAPPDATA/Programs/Beankeeper Desktop/bk.exe` on Windows.
If `running` is true and the Beankeeper MCP tools respond, go straight to
setup in conversation. If `installed` is true and `running` is false, run
`bk service start` and check again. For a running service with no agent
connection, run `bk agents connect --all --json` and reload the client.
If this installation is missing or predates the service and registration
commands, install a signed Desktop release for this operating system and
processor using the commands below.

Use [Sigstore's cosign installation instructions](https://docs.sigstore.dev/cosign/system_config/installation/)
if `cosign` is missing. Install it through the owner's trusted package channel
or stop and explain what is missing. Guided setup requires Desktop `0.1.0-rc.12`
or a newer signed release. The commands select `0.1.0-rc.12` by default. If that
release is still pending, report it and stop. To select a newer release, set
`BK_VERSION` to its version without the tag prefix.

### Linux and macOS

Run this from the desktop login, with the regular user account. The installer
detects Linux versus macOS and x86-64 versus Arm64.

```sh
(
    set -eu
    export BK_VERSION="${BK_VERSION:-0.1.0-rc.12}"
    bk_bin="$HOME/.local/bin"
    bk_download="$(mktemp -d)"
    trap 'rm -rf "$bk_download"' EXIT
    mkdir -p "$bk_bin"
    curl -fsSL https://raw.githubusercontent.com/Govcraft/beankeeper-desktop/main/install.sh -o "$bk_download/install.sh"
    BK_REQUIRE_SIGNATURES=1 BK_REQUIRE_AGENT_SETUP=1 BK_INSTALL="$bk_bin" BK_NO_SERVICE=1 \
        sh "$bk_download/install.sh"
    "$bk_bin/bk" service install --no-agents
    "$bk_bin/bk" agents connect --all --json
    "$bk_bin/bk" service status --json
)
```

### Windows

Run this in PowerShell 5.1 or later as the logged-in user. The installer checks
for x64 Windows before downloading the release.

```powershell
& {
    $ErrorActionPreference = 'Stop'
    $bkVersion = if ($env:BK_VERSION) { $env:BK_VERSION } else { '0.1.0-rc.12' }
    $bkDir = Join-Path $env:LOCALAPPDATA 'Programs\Beankeeper Desktop'
    $bkSetup = Join-Path ([IO.Path]::GetTempPath()) ("bk-install-" + [guid]::NewGuid() + ".ps1")
    try {
        Invoke-WebRequest -UseBasicParsing -Uri 'https://raw.githubusercontent.com/Govcraft/beankeeper-desktop/main/install.ps1' -OutFile $bkSetup
        & $bkSetup -Version $bkVersion -RequireSignatures -RequireAgentSetup -NoService -InstallDir $bkDir
        $bk = Join-Path $bkDir 'bk.exe'
        & $bk service install --no-agents
        if ($LASTEXITCODE -ne 0) { throw 'Beankeeper service installation failed' }
        & $bk agents connect --all --json
        if ($LASTEXITCODE -ne 0) { throw 'Beankeeper agent registration failed' }
        & $bk service status --json
        if ($LASTEXITCODE -ne 0) { throw 'Could not read Beankeeper service status' }
    } finally {
        Remove-Item -LiteralPath $bkSetup -Force -ErrorAction SilentlyContinue
    }
}
```

Both installers require SHA-256 and Sigstore verification of the release
archive and checksum list, using the exact Govcraft release workflow and tag
and GitHub Actions OIDC issuer. If verification fails, stop before executing the
downloaded binary. Explain what failed, such as a missing signed release,
unavailable cosign, or a checksum mismatch. If the service or registration
fails, report the status and the specific step. Repair that step before
resuming installation.

Explain where the private recovery kit was created on first installation. Ask
the owner to open the kit directly and keep an offline copy. Only the owner
handles the key. Use the installed location or restart the agent client when it
needs to reload its connection. Confirm with the read-only `company_list` MCP
tool, then call `setup_status` to begin conversational onboarding. Tell
the owner that the books are encrypted locally, give the exact books path from
`bk service status --json`, and explain that `bk agents disconnect` and
`bk service uninstall` remove the connection and background service while
preserving the books. The binary can then be removed from its install directory.

If you cannot run commands on the owner's machine, give them the installation
instructions for their platform or the Beankeeper extension for Claude Desktop
when available. A browser-only agent needs a supported local connection to use
the books.

## Manage background hosting

Installers normally configure encrypted local books and background hosting.
Use `service_status` on the separate `beankeeper-service` MCP connection to check
readiness. `service_start`, `service_stop`, `service_install` and
`service_uninstall` manage the per-user host, even when its HTTP server is stopped.
These local tools have OS-user service authority, separate from ledger grants;
use them only when the owner's request authorizes that lifecycle action.
They accept no arguments and return named errors or structured status with a
plain-language message, running version, MCP endpoint and optional Desktop link.
Answer in plain words first. Offer the Desktop view only when useful.

The ledger MCP entry runs `bk mcp bridge --service`, discovering the current
loopback port and using the private agent-token file. If unavailable, use
`service_status` to diagnose and `service_start` when authorized. Reconnect the
ledger MCP client after restarting hosting. CLI equivalents are
`bk service install | uninstall | start | stop | status --json`.
Uninstall removes hosting while preserving all books and credentials.
See [background hosting](docs/service.md) for storage and recovery details.

## Connect a local agent

Installers register Beankeeper with detected clients by default. Run
`bk agents detect --json` to see which clients are available. To repair the
connection, use `bk agents connect --all` or select one client, such as
`bk agents connect codex`. Restart the client to load the updated configuration.
Use `bk agents doctor --json` to check the managed configuration and make a
read-only tool call through the local service.

Supported clients are Claude Desktop, Claude Code, Codex CLI, Cursor, Gemini
CLI, VS Code with Copilot, and Grok Build. An installed Beankeeper extension
manages its own Claude Desktop connection. ChatGPT and hosted Grok appear as
unsupported local configurations. Configuration changes leave other server
entries in place and keep a private `.beankeeper-backup` beside the original.
`bk agents disconnect <client>` removes the managed entries and skill for that
client. Remove the Beankeeper extension from Claude Desktop Settings to
remove an extension-managed connection.

To install without changing agent settings, use `BK_NO_AGENTS=1` or the shell
installer's `--no-agents` option. PowerShell accepts `-NoAgents`.
The Windows setup executable accepts `/NOAGENTS=1`. The background service can
still run and you can connect a client later.

For manual foreground hosting, the owner can still run
`bk --db /path/to/books.db setup` once in a terminal. It creates encrypted books,
stores the passphrase in the OS keychain, prepares a private recovery kit and
merges the `beankeeper` MCP server into Claude Desktop, Claude Code and Codex
configs. Restart the agent client afterward.

The default preset is read-only. `--preset bookkeeper` adds posting and proposals
with initial per-company USD spend caps, without approval or administrative
grants. Use proposal tools for work requiring human review. Never ask for, read,
print, copy or attach the recovery kit, passphrase store or owner-token file.
The human owns backup/recovery and approval credentials.

Manual setup configs invoke `bk mcp bridge` with an explicit endpoint and a
private agent-token file path. This bridge connects to the running `bk serve`.
For manual hosting, the owner can start `bk --db /path/to/books.db serve` or
rerun `setup --force`. Stored passphrases are resolved automatically
for enrolled ledgers; explicit upstream secret sources take precedence.

`setup --force` resumes without deleting data, rotating the root or changing an
existing preset. Capability changes and token renewal use upstream commands
under the human owner's control. A token is secret and expires after one year.
New companies need owner-configured spend limits before agent use.

## Guide the owner through book setup

Start with `setup_status` over MCP or `bk setup status --json`. Use `next_step`
and the recorded facts to skip questions the owner has already answered.

1. Ask whether the owner keeps personal books, business books, or both. Confirm
   the reporting currency and the start of the financial year for each book.
2. Ask for the names of their real accounts and the dates and amounts of any
   opening balances. Confirm which balances are already recorded.
3. Ask which statements they want to import and which have already been
   entered. Transaction currencies and existing postings alone leave those
   settings and import history unconfirmed.

Summarize what is already recorded, what the owner has confirmed, and what
still needs an answer or a statement. Only begin a setup session when the
installed ledger exposes a scoped write tool for the next confirmed step.

The owner can authorize a session through `ledger_setup_begin` or `bk setup
begin`, using their own `ledger/admin` access on the selected encrypted ledger.
For the CLI, set `--db` to the `books` path reported by `bk service status
--json`. Give the owner the setup agent's Ed25519 public key. Have the
owner deliver the short-lived `ledger/setup` credential through their local
secret store. Keep the agent's private key and bearer token out of chat and logs.
Use a separate MCP connection for the setup credential. For a local stdio
client, read `mcp_endpoint` from `bk service status --json` and run
`bk mcp bridge --url ENDPOINT --token-file PRIVATE_FILE`. Point `PRIVATE_FILE`
at the private setup-token file and keep the regular ledger connection.

Keep the returned setup generation for the session's writes. Use the same
request reference when retrying a step, and stop if the tool reports a conflict.

After summarizing the outcome, have the owner finish the session or revoke it
if they want to cancel access. Both actions deactivate the setup grant. Posted
entries are corrected through normal ledger reversals. See the
[ledger setup session commands](https://github.com/Govcraft/beankeeper/blob/2399d1566ce3730368ca53e044e47ba3cf72610f/docs/beankeeper/references/commands.md#setup-sessions)
for the CLI flags and MCP arguments.

Setup opens the UI origin. Ask the human to open the one-time pairing URL from
the private `.desktop.log`, or run `bk serve` in the foreground to see it in the
terminal, then select **Pair this browser** on that page within five minutes.
Opening or refreshing the page leaves the link usable; only the confirmation
POST consumes it. A paired browser can browse the financial snapshot, accounts, transactions,
financial reports, tax summaries and read-only reconciliation. Use the stable
view IDs and query filters in [docs/reports.md](docs/reports.md) for saved report
links. Tax CSV/JSON downloads require the human browser session; agent bearer
tokens cannot authorize them. These summaries are not filed tax forms.

Answer the financial question in plain words before offering a view. Use the
structured numbers and chartable series from ledger tools to draw charts in the
conversation when useful. The owner never needs Desktop to understand the answer.
Offer a deep link only for optional visuals, source detail or human review.

Call `ui_get_link` on your normal ledger connection (`ledger/read`) with the
same `view_id` and `state` used by `ui_show_view`. This tool succeeds without a
paired browser, UI credential or live tab, works under `--read-only`, and never
navigates. Put its `url` in your answer as an optional detail link. Do not call
`ui_show_view` merely to obtain a URL. The CLI equivalent is
`bk --json ui link --view-id VIEW --company SLUG ...`; it does not need a running
server or ledger. Success returns the same `url`, `path`, `view_id` and `state`.
Invalid selectors return a named `VALIDATION` error.

Always carry the answer's exact company slug, currency, account code and date
bounds into the link. Spending categories identified by expense accounts map to
`state.account`. `state.category` means an exact entry tax-category tag;
`state.payee` means the exact `metadata.payee` string, preserving case. Both are
register filters. Use inclusive `from`/`to` and optional inclusive
`compare_from`/`compare_to` bounds for two periods, or `compare_to` alone for a
balance-sheet snapshot comparison; see supported views and all
identifiers in the [deep-link contract](docs/deep-links.md). Do not substitute a
category's display name for its identifier or a fuzzy text search for a payee.

For a financial summary, pass `desktop_view.link` to `ui_get_link` when it is
present. Preserve the date bounds and complete account selections in summary and
cash-flow chart links. See [summary and chart link fields](docs/deep-links.md#shared-upstream-identifiers).

Example after answering an October spending question:

```json
{"name":"ui_get_link","arguments":{"view_id":"transactions","state":{"company":"acme","account":"5000","currency":"USD","from":"2026-10-01","to":"2026-10-31","compare_from":"2026-09-01","compare_to":"2026-09-30"}}}
```

If the browser is unpaired, the human opens the terminal's one-time pairing URL
in that same browser and confirms pairing. Desktop then restores the requested
view. Keep pairing and bearer credentials out of shared detail URLs. A saved
view URL conveys no authority. Use `ui_show_view` only when the owner wants you
to change their connected tab, with the separate navigation connection described
below.

Browser monetary values are formatted in major units with currency symbols and
grouping. Reports use the currency carried by ledger amounts. Register
`min_amount`/`max_amount` filters still use integer minor units; JSON/CSV downloads
and raw audit/proposal payloads retain their original units.

Income statements default to January 1 of the end year through today when dates
are omitted. The form shows the effective dates, and empty ranges include a
note and a link to company transactions across all dates. For historical books,
pass explicit `from` and `to` dates, such as `2025-01-01` and `2025-12-31` for
the 2025 demo. Zero totals alone do not mean the company has no activity.

The review queue at `/review?company=SLUG` lists pending agent proposals. Link
the human to `/review/PROPOSAL_ID?company=SLUG` to inspect entries, metadata and
current spend caps, then approve or reject using their paired browser session.
The queue also shows the last full ledger verification and a recheck button.
For navigation, configure a second MCP connection with a separate `ui/navigate`
token issued by the human owner. Ledger and UI grants cannot share a token.
Current setup presets provide ledger tokens only. Call `ui_status`, give its
association link to the human to open in an already paired browser, then call
`ui_show_view` with a supported `view_id` and URL-addressable `state`. If several
tabs are connected, supply a `tab_id` from status. `ui_highlight` accepts exact
DOM IDs listed by status and never clicks or submits a control. Missing rows are
a no-op. Reports and reconciliation accept the stable view IDs and filters in
[docs/reports.md](docs/reports.md), either directly or via `view_id: "reports"`
and `state.report_kind`. Use `report-filters` or `report-results` from status to
highlight those regions. When no matching live tab is available,
`ui_show_view` and `ui_highlight` return `isError: true`, code `NO_CONNECTED_TAB`,
`queued: false`, a message and a `pairing_url`. Tell the human nothing was displayed,
provide the connection link, and retry after association and tab connection.
`ui_status` succeeds even without tabs, so use it to check readiness. Browser
pairing alone does not associate your UI credential. A queued result is not
browser confirmation; use `ui_status` to inspect the loaded view.
`--read-only` removes navigation/highlight mutations. See the
[navigation catalog](docs/navigation.md) for arguments and examples.
Desktop's supported workflow is agent proposes, human approves in the browser.
Do not ask for approval tokens or call MCP approval tools on the human's behalf.
The server supplies its own ephemeral human authority behind cookie/CSRF checks;
no agent token is an approver on browser routes. Do not grant the agent owner
authority to work around these boundaries.

See [docs/setup.md](docs/setup.md) for the full command reference, all config and
credential paths, headless test options, lifecycle and recovery instructions.
Ledger, library and schema work belongs in Govcraft/beankeeper.

Desktop license and update state is available to the human at `/settings` in a
paired browser. Cached update results show readable UTC attempt times and
explain failed or incomplete checks; reading Settings does not contact the feed.
Results are tied to the running Desktop version. A version change, or a legacy
cache without a version, hides stale results until the next check refreshes them.
The daily retry limit, including failures, applies within each version.
The human can activate a downloaded file with `bk license activate FILE`,
inspect it with `bk license`, check the signed release feed with
`bk update-check`, and explicitly install an eligible release with
`bk self-update`. Verified licenses explicitly show `active and valid`; the
separate open/ended update window is informational and never expires perpetual
use. Update eligibility uses the signed release date. Missing activations and
verification failures show their specific reason.
Stable builds check stable releases; RC builds check stable
and `-rc.N` releases among the 100 most recent GitHub release records. All
updates require the signed manifest and bundles from the selected exact tag.
License files, claim codes and recovery tokens are human-owned;
never ask for them or perform activation on the human's behalf. Normal ledger
and agent work does not require online license verification. Automatic update
checks can be disabled with `BEANKEEPER_NO_UPDATE_CHECK=1`. See
[license and update commands](docs/license-updates.md).
