# Beankeeper Desktop

Beankeeper Desktop adds a local browser workspace to `bk`. Your own agent talks
to the authenticated MCP endpoint; the browser displays the workspace. This
preview shows your dashboard, accounts and transactions, with read-only ledger
views and a browser review queue for agent proposals. Live agent navigation
comes later.

Source-available. See [LICENSE.md](LICENSE.md); this repository is not published
as a crate. Ledger, schema and accounting changes belong in
[Govcraft/beankeeper](https://github.com/Govcraft/beankeeper).

## Build and run

Requires Rust 1.91 or newer and the C build tools needed by upstream's bundled
SQLCipher/OpenSSL. See [the frontend/toolchain decision](docs/frontend.md).

```sh
cargo build --release --locked
./target/release/bk --db /path/to/books.db setup
# Open the one-time pairing URL in /path/to/books.desktop.log.
# Later, reopen the paired workspace from another terminal:
./target/release/bk open
```

One process serves the embedded UI at `http://127.0.0.1:8750/` and Streamable HTTP
MCP at `http://127.0.0.1:8750/mcp`. No external asset directory or Node.js is
needed at runtime or build time. Direct links to `/transactions`, `/reports`,
`/accounts`, `/transactions/TRANSACTION_ID?company=SLUG`, `/review`, and
`/review/PROPOSAL_ID?company=SLUG` work after pairing.
Unknown URLs return 404.

`bk setup` creates an encrypted ledger, stores its passphrase in the native OS
keychain, writes a private recovery kit and agent token, merges MCP configs for
Claude Desktop, Claude Code and Codex, and starts the local UI. The default agent
is read-only; `--preset bookkeeper` adds posting and proposals with spend caps,
without approval rights. Keep the recovery kit and separate owner credential
offline. See [the setup reference](docs/setup.md) for flags, exact files touched,
headless CI usage, recovery and current pairing integration limits.

`bk serve` requires an existing ledger and resolves the passphrase once without
prompting, using upstream's `--passphrase-file`, `--passphrase-fd` or
`BEANKEEPER_PASSPHRASE_CMD`, then the stored credential for setup-enrolled ledgers.
All upstream CLI commands remain available, including
`bk mcp serve` for stdio and `bk mcp serve --http` for MCP-only HTTP. Desktop
extends upstream's command parser and delegates those commands to its dispatcher.
Use `bk --help` or `bk serve --help` for options.
The MCP router receives the resolved passphrase through an in-memory pipe.
Encrypted Desktop startup currently requires Unix;
[beankeeper#109](https://github.com/Govcraft/beankeeper/issues/109) tracks a
resolved-secret router factory for other platforms.

```sh
bk --db /path/to/books.db serve --bind 127.0.0.1:8751 --read-only
bk open --bind 127.0.0.1:8751
```

`--read-only` removes mutation tools regardless of the agent's capabilities.
Port 0 selects an available port; use the URL printed at startup with `bk open
--bind 127.0.0.1:PORT`. `bk open` checks `/health` with a two-second timeout,
without proxies or redirects, before launching the default browser. It fails
clearly if Desktop is unavailable and does not start a server. Ctrl-C stops the
listener and cancels upstream MCP sessions before HTTP shutdown completes.

## MCP credentials and the local HTTP boundary

Configure your agent's Streamable HTTP client with the `/mcp` URL and an
`Authorization: Bearer <agent-token>` header. Issue an agent token with upstream's
`bk key init` and `bk agent issue` commands for an encrypted ledger. Request
credentials, revocation checks and capability rules all come from upstream's
public `mcp::http_router`; Desktop copies no authentication or ledger handlers.
Every MCP request needs a valid token, even if policy enforcement is off.
Upstream's `BK_AGENT_TOKEN` / `--agent-token` fallback remains supported when a
request has no Authorization header. Keep this fallback private to your process;
it is never embedded in HTML or returned to the browser.

Every request, including UI assets, health, unknown URLs and MCP, must carry
exactly one allowed Host with the actual bound port: `127.0.0.1:PORT`,
`localhost:PORT`, or `[::1]:PORT`. When Origin is present it must exactly match
`http://HOST` for that request, including the port. Missing Origin is allowed for
native clients. Nonlocal, malformed, duplicate or null origins are rejected,
as are rebinding hostnames and wrong ports. There is no permissive CORS layer.
UI responses also forbid framing and inline/external scripts through CSP.
These checks complement MCP credentials and the separate browser session.

## Pair the browser and connect your agent

Open the one-time URL printed by `bk serve` within five minutes. It exchanges a
256-bit launch secret for an `HttpOnly`, `SameSite=Strict` cookie, then redirects
to the workspace. The URL cannot be reused. Opening the raw origin or `bk open`
without a valid cookie shows a pairing screen. Sessions expire after 12 hours
and all links and sessions are invalidated when the server restarts. The paired
workspace can create another browser link or unpair the current browser.

MCP works before any browser is paired. Successful authenticated responses
include a `Beankeeper-Pairing-Url` header until the agent has a browser target.
Open that association link in a paired browser and confirm the connection.
The association link alone cannot create a human session. Browser writes require
both the exact Origin and a per-session CSRF token; MCP bearer tokens cannot
act as human credentials. Plain HTTP loopback cookies omit `Secure` for browser
compatibility and are not suitable for a remote deployment.

See [browser pairing and session design](docs/browser-pairing.md) for expiry,
credential separation, localhost cookie policy and the D4 integration API.

Non-loopback binds fail unless `--allow-remote` is explicit. That flag permits a
plaintext listener, **without TLS**, and keeps the same strict local Host/Origin
rules. It does not enable arbitrary remote browser origins. Remote binding is an
advanced development option, not a supported public deployment mode. Prefer the
default `127.0.0.1` bind (or `[::1]` for IPv6). Other loopback aliases are
rejected because they are outside the UI Host allowlist. A wildcard listener accepts local
connections; a specific remote-interface listener needs a client supplying an
allowed Host and network access to that interface.

## Browse your books

To try the views with fictional data, initialize a separate demo ledger:

```sh
bk init --demo --path ./desktop-demo.db
bk --db ./desktop-demo.db serve
```

After starting `bk serve`, open its one-time pairing URL as described above.
Every ledger page, including reports and proposal review, requires D3's
paired human session. Agent credentials cannot unlock browser views, and browser
cookies cannot authorize MCP. Health and CSS remain public and data-free behind
Host/Origin checks. After pairing, reopen a saved view URL to restore its filters.

- **Dashboard** (`/`): company balances and recent activity, with links to
  accounts and the register. Multi-currency books require a currency selection.
- **Accounts** (`/accounts?company=acme`): all accounts for the selected company,
  grouped by account type, with links to each account's register.
- **Register** (`/transactions?company=acme`): filter by account, inclusive date
  range, amount bounds in integer minor units, payee/reference text and clearance
  status. Filters stay in the URL and restore on reload.
- **Detail** (`/transactions/ID?company=acme`): entries, metadata and audit history,
  including agent identity and token ID when the ledger records them.

Choose a company in each view. Without a URL company filter, `--company` or
`BEANKEEPER_COMPANY` supplies the default; otherwise the first listed company is
selected visibly. No company data is combined silently. These pages never edit
the ledger. Ask your agent to prepare changes and review its proposals in the
browser. Reports remain a placeholder for a later issue.

Register pages show at most 50 matches. Search and clearance use bounded scans
of existing upstream transaction queries, so a page can have fewer matches while
more source transactions remain. Follow **Continue** until the register reports
completion. Pagination links preserve all filters; changing filters restarts
from the beginning. Page counts and scan/truncation notices describe the source
query rather than claiming an exact post-filter match count. Audit history also
signals truncation. An empty ledger, no matching transactions, invalid filters,
missing records and read failures have explicit states.

## Review agent proposals

Open `/review?company=acme` to see pending proposals created through `bk txn
propose` or MCP. Each item links to `/review/PROPOSAL_ID?company=acme`, showing
proposer identity, debit total in currency minor units, posting description,
current spend caps, every entry and the complete frozen payload. Upstream does
not store a separate proposal rationale or creation-time cap snapshot, so the
posting description supplies the purpose and caps are labeled as current.

Approve posts once through upstream's atomic approval API. Reject records an
optional reason without posting. A resolved detail keeps the decision and links
to the posted transaction. Repeated or competing decisions cannot post twice.
The local human rejects proposals rather than impersonating the proposer to
withdraw them.

Only a paired browser with the exact Origin and its CSRF token can decide.
Bearer credentials fail these routes, even alongside a browser cookie or with
`ledger/approve`. For enforced ledgers, each server launch uses the decrypted
local trust root to issue a unique, 12-hour `ledger/read` + `ledger/approve`
identity solely for the local human. Its token stays in server memory and is
reverified for expiry and revocation on every review request. The audit names
`desktop-local-human` and records this identity separately from the proposer.
No owner-token file is read. Restart and pair again after this authority expires.
Issuance adds an audited token record per launch; no schema changes are made.
With policy enforcement off, upstream records the same actor name without a
token identity. `--read-only` restricts agent MCP tools; human browser decisions
remain available. Upstream CLI/MCP approval behavior is unchanged, but Desktop's
supported workflow is **agent proposes, human approves in the browser**.

The queue checks the ledger on first load and displays the last result and UTC
check time. **Re-run verification** performs the complete `bk verify`, including
the hash chain, accounting, proposals and policy integrity. Results are cached
for this server launch and may become stale after external changes. Browser
decisions invalidate the cache and always run a fresh full verification within
the resolution savepoint. Failure rolls back the decision. A failed check is
shown explicitly, without claiming the chain is intact. Tests demonstrate this
by changing an audit row in a temporary encrypted ledger after removing its
append-only update trigger; the next explicit check reports failure.

## Development checks

```sh
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo nextest run --workspace
cargo test --workspace
cargo +1.91.0 check --workspace --all-targets --locked
```

CI runs fmt, clippy with warnings denied, tests (including the embedded UI build)
on stable, and a separate Rust 1.91 check. `Cargo.lock` pins the development git
dependency on `Govcraft/beankeeper`'s `main` branch. Update it deliberately with
`cargo update -p beankeeper-cli` when adopting upstream changes.
