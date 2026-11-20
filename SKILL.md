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
Navigation grants (D4) arrive in a separate issue.
Desktop's supported workflow is agent proposes, human approves in the browser.
Do not ask for approval tokens or call MCP approval tools on the human's behalf.
The server supplies its own ephemeral human authority behind cookie/CSRF checks;
no agent token is an approver on browser routes. Do not grant the agent owner
authority to work around these boundaries.

See [docs/setup.md](docs/setup.md) for the full command reference, all config and
credential paths, headless test options, lifecycle and recovery instructions.
Ledger, library and schema work belongs in Govcraft/beankeeper.
