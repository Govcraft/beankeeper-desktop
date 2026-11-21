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
terminal, then select **Pair this browser** on that page within five minutes.
Opening or refreshing the page leaves the link usable; only the confirmation
POST consumes it. A paired browser can browse the dashboard, accounts, transactions,
financial reports, tax summaries and read-only reconciliation. Use the stable
view IDs and query filters in [docs/reports.md](docs/reports.md) for saved report
links. Tax CSV/JSON downloads require the human browser session; agent bearer
tokens cannot authorize them. These summaries are not filed tax forms.

Browser monetary values are formatted in major units with currency symbols and
grouping. Report HTML prefers upstream `*_display` decimal fields. Register
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
