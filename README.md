# Beankeeper Desktop

Your AI agent keeps your books through MCP or the `bk` command line. It can
post, recategorize, and reverse transactions through either interface. When you
ask about your finances, it runs reports and answers in plain words with data
it can chart.

Beankeeper Desktop is the paid, self-hosted layer. It runs as a local service
with signed updates and offline licensing.

Ask your agent what changed since you last looked or ask it to undo its
work in chat. The ledger records the correction and keeps the original
history. Receipt links are available if you want to see the details.
[Changes and audited undo](docs/changes.md)

Source-available. See [LICENSE.md](LICENSE.md); this repository is not published
as a crate. Ledger, schema and accounting changes belong in
[Govcraft/beankeeper](https://github.com/Govcraft/beankeeper).
See the [contribution guide](https://github.com/Govcraft/beankeeper-desktop/blob/main/docs/CONTRIBUTING.md) for feedback and contribution policies.

## Release packages

The packaging workflow produces Linux tarballs and `.deb` files, macOS `.pkg`
files for both architectures, and an x64 Windows ZIP and EXE installer. Paid OS
signing is disabled while the Apple and Azure accounts are pending. Tag releases
include SHA-256 checksums, Sigstore bundles and build provenance. The shell and
PowerShell installers read the latest release version from beankeeper.app and
require cosign to verify both the platform archive and the checksum list.
See [release packaging and verification](docs/RELEASE.md) for artifact names,
installation commands, unsigned dry runs and signing configuration.

From an agent that can run commands on your computer, ask:

> Set up Beankeeper from https://beankeeper.app/install.md

The agent uses your existing installation when it supports setup. For a new
install, it downloads the latest release for your platform and verifies both signatures
and SHA-256 before running the downloaded binary.

Once your books exist, your agent asks how you want recurring
payees, sales tax and processor fees recorded, and how often you want updates.
It saves your answers as standing instructions and tells you what it saved.
Ask in chat any time to see or change those instructions.
[Bookkeeping interview](docs/setup.md#bookkeeping-interview)

## Claude Desktop extension

When **Get it for the Claude app** is available on beankeeper.app, download
the Beankeeper extension, double-click it, and click **Install** in Claude.
Then say **Set up my books** or **Create demo books**. Claude sets up encrypted
books on your computer and continues in the same chat.
[Skills over MCP](https://skills.extensions.modelcontextprotocol.io/) lets
compatible clients load Beankeeper's setup, correction, and reporting guide
directly from the extension or the `beankeeper` stdio connection. The guide
updates with Beankeeper and needs no separate installation.
[Claude Desktop setup and removal](docs/claude-desktop.md)

## Unsigned builds

There is no paid Apple or Windows signing until revenue is proven. Release
binaries are still checksummed, signed with Sigstore (keyless cosign), and
attested with GitHub build provenance. Browser-downloaded installers will show
OS warnings.

**macOS:** a browser-downloaded `.pkg` is blocked as an unidentified developer.
On macOS 15 or later, open **System Settings → Privacy & Security** and choose
**Open Anyway**. The older right-click → Open bypass no longer works. A
`curl | sh` tarball install is not quarantined and does not show that prompt.

**Windows:** the setup `.exe` shows SmartScreen **Windows protected your PC**.
Choose **More info → Run anyway**. A ZIP extracted in Explorer may also warn
because of the Mark of the Web.

Verify a downloaded asset before installing (replace `VERSION` and the
filename):

```sh
cosign verify-blob --bundle bk-desktop-VERSION-ASSET.sigstore \
  --certificate-identity https://github.com/Govcraft/beankeeper-desktop/.github/workflows/release.yml@refs/tags/beankeeper-desktop-vVERSION \
  --certificate-oidc-issuer https://token.actions.githubusercontent.com \
  bk-desktop-VERSION-ASSET
gh attestation verify bk-desktop-VERSION-ASSET --repo Govcraft/beankeeper-desktop \
  --cert-identity https://github.com/Govcraft/beankeeper-desktop/.github/workflows/release.yml@refs/tags/beankeeper-desktop-vVERSION \
  --source-ref refs/tags/beankeeper-desktop-vVERSION
```

Exact archive names and the `BK_DOWNLOAD_BASE` override are in the
[release guide](docs/RELEASE.md).

## Run quietly in the background

Installers now run `bk service install` for the logged-in owner. Beankeeper keeps
its encrypted books locally, starts at login and recovers after a crash. Your
agent can use it without opening a browser.

Installation configures ledger MCP plus local lifecycle tools, including while
the server is stopped. The bridge discovers the actual loopback endpoint, with
a fallback when port 8750 is occupied. On Linux, lifecycle commands can find the
owner's session bus when Claude Desktop, an IDE, tmux or SSH launches `bk`
without the login environment. Status verifies the live service directly.
Recovery material stays in a private file under the owner's control.
Managed installs default to a bookkeeper credential that posts directly and
attributes each write to the agent. Upgrades switch the original managed
read-only default to direct posting and preserve deliberate access choices.
Say **Make me read-only** to turn posting off, or ask for propose-only access
if you want to review proposed entries. Restarting cannot grant posting access.
[Background hosting and managed access](docs/service.md)

Your agent can report what it changed as soon as it finishes and offer to
undo supported changes in chat.

### Advanced service commands

```sh
bk service install
bk service status --json
bk service stop
bk service start
bk service uninstall  # preserves books and recovery material
```

Run `bk agents connect claude-code` to register or refresh the Claude Code
connection and skill. Existing entries with the managed command and arguments
can include `"type": "stdio"` and an empty `env`.

If Beankeeper is already available, ask your agent to install or upgrade the
Claude Desktop extension, or run `bk agents extension`. The command requires
`cosign`. [Install or upgrade the signed extension](docs/claude-desktop.md#install-or-upgrade-from-beankeeper)

## Build and run

Requires Rust 1.91 or newer and the C build tools needed by upstream's bundled
SQLCipher/OpenSSL. See [the frontend/toolchain decision](docs/frontend.md).

```sh
cargo build --release --locked
./target/release/bk --db /path/to/books.db setup
# Open Desktop and select Pair this browser if prompted:
./target/release/bk open
```

One process serves the embedded UI at `http://127.0.0.1:8750/` and Streamable HTTP
MCP at `http://127.0.0.1:8750/mcp`. No external asset directory or Node.js is
needed at runtime or build time. Direct links to `/transactions`, `/reports`,
`/accounts`, `/transactions/TRANSACTION_ID?company=SLUG`, `/reports/REPORT_ID`,
`/reconciliation`, `/review`, and `/review/PROPOSAL_ID?company=SLUG` work after
pairing.
Unknown URLs return 404.

`bk setup` creates an encrypted ledger, stores its passphrase in the native OS
keychain, writes a private recovery kit and agent token, merges MCP configs for
Claude Desktop, Claude Code and Codex, and starts the local UI. The default agent
is read-only. `--preset bookkeeper` enables direct posting and proposals without
spend caps or approval rights. Keep the recovery kit and separate owner
credential offline. See [the setup reference](docs/setup.md) for flags, exact
files touched, headless CI usage, recovery and current pairing integration limits.

Use `bk setup status --json` or the `setup_status` MCP tool to check setup
progress. Both report the ledger counts and suggest the next step. For details
on a particular book, use `bk setup book-show` or `ledger_setup_book_show`.

Complete a monthly statement with `bk reconcile statement` or the
`reconcile_statement` MCP tool. If the ending balances differ, the
`STATEMENT_DISCREPANCY` error includes both balances and their difference in
`details`. The statement stays open and the clearings are preserved.

`bk serve` requires an existing ledger and resolves the passphrase once without
prompting, using upstream's `--passphrase-file`, `--passphrase-fd` or
`BEANKEEPER_PASSPHRASE_CMD`, then the stored credential for setup-enrolled ledgers.
All upstream CLI commands remain available, including
`bk mcp serve` for stdio and `bk mcp serve --http` for MCP-only HTTP. Desktop
extends upstream's command parser and delegates those commands to its dispatcher.
Use `bk --help` or `bk serve --help` for options.
The MCP router receives the already-resolved passphrase through upstream's
public host-tool factory, preserving one-shot secret sources without a pipe or
temporary file.

```sh
bk --db /path/to/books.db serve --bind 127.0.0.1:8751 --read-only
bk open --bind 127.0.0.1:8751
```

`--read-only` removes mutation tools regardless of the agent's capabilities.
Port 0 selects an available port; use the URL printed at startup with `bk open
--bind 127.0.0.1:PORT`. `bk open` checks `/health` with a two-second timeout,
without proxies or redirects, before opening a fresh pairing link in the default
browser. With no `--bind`, it prefers the running managed service. Ctrl-C stops the
listener and cancels upstream MCP sessions before HTTP shutdown completes.

## MCP credentials and the local HTTP boundary

Configure your agent's Streamable HTTP client with the `/mcp` URL and an
`Authorization: Bearer <agent-token>` header. Issue an agent token with upstream's
`bk key init` and `bk agent issue` commands for an encrypted ledger. Request
credentials, revocation checks and capability rules all come from upstream's
public `mcp::http_router_with_host_tools`; Desktop copies no authentication or
ledger handlers.
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
UI responses also forbid framing, inline scripts and third-party scripts through CSP.
These checks complement MCP credentials and the separate browser session.

## Pair the browser and connect your agent

Ask your assistant to open Beankeeper Desktop or run `bk open`. If prompted,
select **Pair this browser** within five minutes. Opening or refreshing the
link leaves it usable. The button submits a same-origin POST that exchanges the
one-time secret for an `HttpOnly`, `SameSite=Strict` cookie, then opens the
requested view. The local `service_open` MCP tool is available through the
`beankeeper-service` connection and the Claude Desktop extension. Sessions
expire after 12 hours. Restarting the server clears all links and sessions.
The paired workspace can create another browser link or unpair the current browser.

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

## Optional detail links

Answer in words first. Agents can then offer a matching Desktop view with
`ui_get_link` on the managed ledger MCP connection with its ordinary
`ledger/read` grant, including under `--read-only`. It returns a
credential-free absolute URL and structured
view/filter identifiers without opening or changing a browser. No pairing,
association or live tab is needed to generate a link. Browser access still
requires human pairing.

The CLI uses the same contract and checks which running Desktop instance
serves the requested books. It can do this without opening the books:

```sh
bk ui link --view-id transactions --company acme --account 5000 \
  --from 2026-10-01 --to 2026-10-31
bk --json ui link --view-id income-statement --company acme \
  --from 2026-10-01 --to 2026-10-31 \
  --compare-from 2026-09-01 --compare-to 2026-09-30
```

Without `--db` or `BEANKEEPER_DB`, the CLI selects the configured managed books
when available. Use `bk --db /path/to/demo.db ui link ...` for other books. The
listener is discovered automatically, including when it uses a fallback port.
If several instances serve the same books, use `--bind 127.0.0.1:PORT` to select
one. `DESKTOP_BOOKS_NOT_SERVED` means the CLI found no matching live instance.
All current views, including
`settings` and record details, have stable IDs. `ui_get_link` and `ui_show_view`
share validation; unsupported filters return `VALIDATION` rather than being
silently omitted. Include the company, currency and explicit dates from the
financial answer to make its link reproducible.

An unpaired browser remembers the requested view for five minutes. Pairing in
that same browser opens the original destination with its filters. An already
paired browser arriving from an external site can use **continue to your
requested view** if its Strict cookie was withheld. Each listener serves one
books file and has its own browser session. A separate listener needs an initial
pairing and keeps it for the life of that session. See the
[deep-link contract](docs/deep-links.md) for filter mappings, comparisons,
pairing lifetime and examples.

## Agent navigation

The `ui_show_view`, `ui_highlight` and `ui_status` MCP tools require a separate
`ui/navigate` token. Current setup presets supply ledger credentials only;
upstream rejects mixed ledger/UI grants. Connect a second client entry to the
same `/mcp` endpoint using the UI token, call `ui_status`, and open its association
link in a paired browser and confirm the connection. Browser pairing alone
does not link an agent; the live notice says navigation is ready only after
association. When no matching tab can receive navigation or highlights, those
tools return `isError: true` with code `NO_CONNECTED_TAB`, `queued: false`,
a message and a `pairing_url`. `ui_status` still succeeds and reports the current
pairing state and tabs. With multiple connected tabs, choose the `tab_id` from status.
Navigation/highlight are disabled by `--read-only`; status remains available.

Financial snapshot, account, register, transaction, review, financial report, tax
summary and reconciliation URLs are supported with restorable filters. Tool
results distinguish queued requests from the browser's confirmed view. See the [tool catalog, grant
commands and tab design](docs/navigation.md) for arguments, examples and tests.

## Try it with demo books

Ask your connected agent, “Show me what Beankeeper can do with sample books.”
You can start with a quick demo or answer a few questions in chat to get
fictional books shaped around your work, account types, and currencies. Try
reports and practice bookkeeping in the encrypted demo. Ask your agent to
discard it when you’re done. [Agent demo guide](SKILL.md#try-it-with-demo-books)

## Browse your books

Open the optional Desktop link from your agent to explore your own books.
Every ledger page, report download and proposal review requires D3's
paired human session. Agent credentials cannot unlock browser views, and browser
cookies cannot authorize MCP. Health and CSS remain public and data-free behind
Host/Origin checks. After pairing, reopen a saved view URL to restore its filters.

- **Your books** (`/`) shows a short financial snapshot from the same summary
  used by the agent, with optional links to the detailed views.
- **Accounts** (`/accounts?company=acme`): all accounts for the selected company,
  grouped by account type, with links to each account's register.
- **Register** (`/transactions?company=acme`): filter by account, inclusive date
  range, amount bounds in integer minor units, payee/reference text and clearance
  status. Reversed and cancelled pairs are hidden by default. Choose **Show
  reversed/cancelled pairs** and select **Apply** to include both transactions.
  Filters stay in the URL and restore on reload.
- **Detail** (`/transactions/ID?company=acme`): entries, metadata and audit history,
  including agent identity and token ID when the ledger records them.
- **Reports** (`/reports`): income statement, balance sheet, cash flow, spending
  and net-worth trends, and budget variance. Date and currency filters use the
  CLI report rules; every report offers a JSON download. The income statement
  shows its effective dates and explains ranges with no revenue or expense
  activity, with a link to company transactions across all dates.
- **Tax summaries** (`/reports/schedule-c` and `/reports/1099`): tables and CSV/JSON
  downloads, including unmatched Schedule C categories and payee thresholds.
  These are bookkeeping summaries, not tax advice or filed forms.
- **Reconciliation** (`/reconciliation`): account and statement clearance balances,
  outstanding items and bank or settlement mode. This view is read-only.

Browser amounts use currency symbols, comma grouping and currency-specific
precision, such as `$55,620.00`, `MX$40,324.70`, `¥1,234` and `BHD 1.234`.
Report HTML uses upstream decimal display fields in place of raw amount columns.
Register amount filters still accept integer minor units. JSON/CSV downloads,
raw audit snapshots and the full proposed posting retain their original units.

Choose a company in each view. Without a URL company filter, `--company` or
`BEANKEEPER_COMPANY` supplies the default; otherwise the first listed company is
selected visibly. No company data is combined silently. These pages never edit
the ledger. Ask your agent to prepare changes and review its proposals in the
browser. See [report URLs and filters](docs/reports.md) for stable navigation
IDs, date defaults, budget month constraints and download formats.

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
proposer identity, formatted debit total, posting description,
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
`ledger/approve`. For enforced ledgers, Desktop reads the private
`<ledger-stem>.owner-token` file created by `bk setup`. A live `ledger/admin`
credential in that file authorizes issuance of a unique, 12-hour
`ledger/read` + `ledger/approve` credential for the local human. It stays in
server memory and Desktop checks it for expiry and revocation on each review
request. The audit names `desktop-local-human` and records this identity
separately from the proposer. If the private owner credential is missing or
rejected, Desktop still serves verification, ledger views and MCP. The review
page explains how to restore browser approval. Restart Desktop and pair again
after approval access expires.
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

CI runs fmt, clippy with warnings denied, tests including the embedded UI build,
and a separate Rust 1.91 check. `Cargo.lock` records the same upstream revision
for the library and CLI. The `upstream-main` job refreshes both crates and
tests the latest upstream commit on each pull request, push to `main`, and daily.

## License and updates

`bk license` shows the local entitlement. Activate a D10 license offline with
`bk license activate /path/to/license.json`. Updates through the signed deadline
are included; use of an installed eligible release is perpetual. Desktop does
not contact the license service after activation. Final Desktop licensing terms
remain Roland's decision; this preview adds no runtime lockout.

Both `bk license` and **License and updates** (`/settings`) explicitly show
`active and valid` after offline verification. The update window is shown
separately as open or ended according to the local clock. An ended window does
not expire perpetual use; update eligibility uses the signed release date.
Missing activations and verification failures show their specific reason.

`bk update-check` checks the signed GitHub release manifest at most once daily,
including failed attempts, for each running Desktop version. After a version
change, the next check refreshes the cache immediately; caches from older builds
without a recorded version are also refreshed. Settings ignores these stale
results without contacting the feed. Stable builds check the latest stable
release. RC builds select the highest stable or `-rc.N` version among the 100
most recent GitHub release records, so checks also work before the first stable release.
Both manifest and signature are fetched from the selected tag. `bk serve`
checks in the background, and paired browsers can read the cached result at **License and updates** (`/settings`).
Both show a readable UTC last-attempt time and explain incomplete checks,
including HTTP failures, separately from up-to-date or update-available results.
Set `BEANKEEPER_NO_UPDATE_CHECK=1` to disable checks before any network access.
Requests contain only the Desktop version and OS, with no ledger information.
`bk self-update` installs an eligible release only after mandatory checksum and
Sigstore verification; it requires `cosign` on PATH. It never falls back to an
unsigned installation. Restart the server after installing.


To activate a locally issued license (no storefront), build with the issuer
public key and then activate the license file. Runtime
`BEANKEEPER_LICENSE_KEYS_FILE` and `BEANKEEPER_LICENSE_SERVICE_URL` override the
built-in values. HTTP is allowed only for a loopback license service.

```sh
# KEY_ID and PUBLIC_KEY come from beankeeper-license keygen / public-key
export BEANKEEPER_LICENSE_PUBLIC_KEYS_JSON="{\"$KEY_ID\":\"$PUBLIC_KEY\"}"
# optional, if you will redeem a claim code against a local server:
# export BEANKEEPER_LICENSE_SERVICE_URL="http://127.0.0.1:8080"
cargo build -p beankeeper-desktop
./target/debug/bk license activate "$HOME/beankeeper-desktop-license.json"
./target/debug/bk license
```

The license service issues that file with `beankeeper-license issue --email --plan --expires`
or `POST /api/v1/admin/licenses`. See the license service README for the full
local test flow.

See the [license/update command reference](docs/license-updates.md),
[offline license contract](docs/license-format.md), and
[release verification contract](docs/RELEASE.md#signed-update-feed-and-self-update).
Production public keys must be provisioned separately; the published fixture key
is confined to tests and is never a default trust key.
