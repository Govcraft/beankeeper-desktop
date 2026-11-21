---
name: beankeeper-desktop
description: Work with the user's encrypted local Beankeeper Desktop workspace over authenticated MCP.
---

# Beankeeper Desktop

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

Start a setup conversation with `setup_status` over MCP, or `bk setup status
--json`. Use its `next_step` and ask only for information the books still need.
Ask whether the owner keeps personal books, business books, or both, then
confirm their reporting currency and when their financial year begins.
Confirm the names of their real accounts, the dates and amounts of any starting
balances, and whether they want to import older bank statements. If the books
already contain transactions, confirm what has been entered before proposing
additional work. Status can show currencies in existing transactions. The
current ledger does not store a default currency or fiscal-year start, and
existing transactions may lack import provenance. Ask the owner when a setting
or a statement's status is unclear. Summarize what is already set up and which
decisions or statements are still needed.

Setup opens the UI origin. Ask the human to open the one-time pairing URL from
the private `.desktop.log`, or run `bk serve` in the foreground to see it in the
terminal, then select **Pair this browser** on that page within five minutes.
Opening or refreshing the page leaves the link usable; only the confirmation
POST consumes it. A paired browser can browse the dashboard, accounts, transactions,
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
