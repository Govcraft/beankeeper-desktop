---
name: beankeeper-desktop
description: Work with the user's encrypted local Beankeeper Desktop workspace over authenticated MCP.
---

# Beankeeper Desktop

Ask the human to run `bk --db /path/to/books.db setup` once in a terminal. The
command creates encrypted books, stores the passphrase in the OS keychain,
prepares a private recovery kit and merges the `beankeeper` MCP server into
Claude Desktop, Claude Code and Codex configs. Restart the agent client afterward.

The default preset is read-only. `--preset bookkeeper` adds posting and proposals
with initial per-company USD spend caps, without approval or administrative
grants. Use proposal tools for work requiring human review. Never ask for, read,
print, copy or attach the recovery kit, passphrase store or owner-token file.
The human owns backup/recovery and approval credentials.

Client configs invoke `bk mcp bridge` with a private agent-token file path. The
bridge connects to the running `bk serve`; it does not start its own ledger
server. When unavailable, tell the human to start `bk --db /path/to/books.db
serve` or rerun `setup --force`. Stored passphrases are resolved automatically
for enrolled ledgers; explicit upstream secret sources take precedence.

`setup --force` resumes without deleting data, rotating the root or changing an
existing preset. Capability changes and token renewal use upstream commands
under the human owner's control. A token is secret and expires after one year.
New companies need owner-configured spend limits before agent use.

Setup opens the UI origin. Ask the human to open the one-time pairing URL from
the private `.desktop.log`, or run `bk serve` in the foreground to see it in the
terminal. A paired browser can browse the dashboard, accounts, transactions,
financial reports, tax summaries and read-only reconciliation. Use the stable
view IDs and query filters in [docs/reports.md](docs/reports.md) for saved report
links. Tax CSV/JSON downloads require the human browser session; agent bearer
tokens cannot authorize them. These summaries are not filed tax forms.

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
highlight those regions. No tab is required:
read the textual fallback and say clearly when nothing was displayed. A queued
result is not browser confirmation; use `ui_status` to inspect the loaded view.
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
paired browser. The human can activate a downloaded file with `bk license
activate FILE`, inspect it with `bk license`, check the signed release feed with
`bk update-check`, and explicitly install an eligible release with
`bk self-update`. License files, claim codes and recovery tokens are human-owned;
never ask for them or perform activation on the human's behalf. Normal ledger
and agent work does not require online license verification. Automatic update
checks can be disabled with `BEANKEEPER_NO_UPDATE_CHECK=1`. See
[license and update commands](docs/license-updates.md).
