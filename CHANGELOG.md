# Changelog

## [Unreleased]

- Build Desktop against current Beankeeper main, preserving named setup and audit errors and exposing structured statement discrepancy details through the upstream CLI and MCP tools.
- The transaction register hides reversed and cancelled pairs by default, with a checkbox and shareable links to include both entries.
- Tailor encrypted fictional demo books to a short chat interview with `demo_start` or `bk demo start --profile`.
- Open and pair the default browser on demand with `bk open` or the local `service_open` MCP tool. Desktop restores the requested view after pairing.
- Agents can correct posted categories in chat with `txn_recategorize` or `bk txn recategorize`, then undo the audited move by receipt.

## [0.1.0-rc.29] - 2026-11-24

- Desktop now uses Beankeeper where the bookkeeper agent can undo its own changes by receipt with `audit_undo` and reverse postings with `txn_reverse` in chat (Govcraft/beankeeper#287); propose-only agents get pending corrections for sign-off.
- Guide agents to correct their own work in chat with audited undo or reversal, report pending sign-off, and offer Desktop links for viewing.

## [0.1.0-rc.28] - 2026-11-24

- Teach agents to rename accounts with the rename tool, never create-and-transfer, and to settle typed card holds with `txn_settle_hold`.
- Desktop now uses Beankeeper with audited account renames: `account_rename` changes only the display name and returns a one-step undo receipt. Imports can settle a matching pending hold, and the Changes page names a restored account label.

## [0.1.0-rc.27] - 2026-11-24

- Teach agents to tag entries, record and settle pending card holds, and recover from tool errors in chat.
- Desktop now uses Beankeeper with typed transaction annotations: `txn_meta_set` accepts `{"status":"pending"}`, errors explain how to retry, and annotation edits can be undone.
- Desktop now uses Beankeeper with typed pending card holds: record a hold with `status: pending` on `txn_post`, then settle it atomically when the charge posts, so it isn't counted twice. The Changes page reports any undo kind.

## [0.1.0-rc.26] - 2026-11-23

- If a setup or posting tool fails, the agent reports the error and retries; it never asks the owner to add accounts or enter data in a Beankeeper app screen. Desktop now uses Beankeeper with inlined MCP schemas and propose-only setup.

## [0.1.0-rc.25] - 2026-11-23

- Set up books and accounts, post opening balances, and import statements in the existing agent chat, with an audited Undo link after each change.

## [0.1.0-rc.24] - 2026-11-23

- Collect changes under Unreleased and validate release versions, changelog sections, and tags when cutting a release.
- Try encrypted fictional books in the existing managed MCP session and discard them in chat or with `bk demo remove`. The CLI also supports `bk demo start`.
- The Desktop skill interviews owners in chat, saves categorization rules, tax and fee policies, and update preferences through ledger tools, and flags low-confidence categorizations in the next update.
- Explain which Beankeeper connection to use when Claude Desktop also shows Claude Code's managed connections.

## [0.1.0-rc.23] - 2026-11-23

### Added

- Report grouped audit receipts through `bk audit since` and `changes_since`.
  Agents can mark each delivered page seen for the local owner.
- Open a receipt in a paired browser and undo supported changes in one step.
  The ledger records a reversal or account archive and preserves the history.

## [0.1.0-rc.22] - 2026-11-23

### Fixed

- Claude Desktop and local MCP clients explain verified posting access. Owners
  can request an access change in chat or keep their read-only choice.

## [0.1.0-rc.21] - 2026-11-22

### Fixed

- Upgrade the original managed read-only default to direct bookkeeper access
  and preserve recorded owner choices.
- Change managed access from chat or the CLI and refresh the MCP tool list
  after a credential rotation.
- Read financial summaries on enforced books from a paired browser session,
  with owner access recorded in the Desktop service log.

## [0.1.0-rc.20] - 2026-11-22

### Changed

- Desktop setup status uses upstream ledger counts and keeps the suggested next
  step and verified agent access. Book profiles and account details are
  available through the per-book setup command and tool.
- Service lifecycle tools and commands use the same typed operations.
  `service_install` accepts `file_store` and `no_agents`, and lifecycle failures
  report named errors.

## [0.1.0-rc.19] - 2026-11-22

### Changed

- Fresh managed installs and standalone setup with `--preset bookkeeper` issue
  direct-posting credentials without spend caps. Standalone setup removes
  `--max-post` and `--daily-limit`.
- Setup and service status derive their plain-language guidance from the
  verified preset and signed write mode.
- Document optional sign-off and owner undo through audited reversal.

## [0.1.0-rc.18] - 2026-11-22

### Fixed

- Recognize Claude Code registrations that explicitly declare stdio or an empty
  environment. Install the managed skill and name custom fields in registration
  conflicts.

## [0.1.0-rc.17] - 2026-11-22

### Fixed

- Resolve CLI detail links against the live Desktop instance serving the
  requested books, including managed services on fallback ports.
- Remember the fallback port for future service starts and report a competing
  Desktop listener in service status and agent doctor.
- Serve the original bound socket so Windows can verify the managed listener
  during installation and startup.

## [0.1.0-rc.16] - 2026-11-22

### Changed

- Track Beankeeper main and use its typed company scopes and audited grants.
- Issue browser approval credentials using the private owner credential on
  enforced ledgers.

## [0.1.0-rc.15] - 2026-11-22

### Fixed

- Use the same validated session environment for every Linux systemd user
  command, preserving the launcher's explicit settings. Cover service status
  from Claude Desktop's minimal environment in native Linux CI.

## [0.1.0-rc.14] - 2026-11-22

### Fixed

- Advertise `ui_get_link` as a read-only tool on the managed ledger MCP
  connection and build its URL from the same typed view as `bk ui link`.

## [0.1.0-rc.13] - 2026-11-22

### Fixed

- Discover the Linux owner's session bus for lifecycle commands. Let managed
  bridges use a verified running endpoint without the login environment and
  report distinct session, endpoint and authentication failures.

## [0.1.0-rc.11] - 2026-11-21

### Added

- Per-user background hosting through `bk service install`, `uninstall`, `start`,
  `stop` and `status --json`, using systemd user units, macOS LaunchAgents and
  Windows logon tasks, with crash recovery and persistent stop controls.
- Local stdio lifecycle MCP tools, authenticated runtime endpoint discovery for
  the ledger bridge, IPv4 loopback port fallback and OS-standard data/log paths.
- Installer integration with opt-out, private file fallback when a new service
  ledger cannot use the native secret store, and private owner recovery material.
- Native lifecycle CI for Linux, macOS and Windows, including crash restart,
  binary replacement, book preservation and uninstall.

## [0.1.0-rc.10] - 2026-11-21

### Added

- Optional credential-free view links through the read-only `ui_get_link` MCP
  tool and `bk ui link`, using the same validation as agent navigation.
- Settings in the stable view catalog, exact register tax-category/payee filters,
  and explicit comparison periods in register and period-report links.
- Restore an unpaired browser's requested view after human pairing confirmation,
  with bounded destination storage and a same-origin continuation for paired
  browsers arriving from external links.

## [0.1.0-rc.9] - 2026-11-21

### Fixed

- Keep browser pairing links usable across duplicate or speculative GET/HEAD
  requests. Pairing now requires selecting **Pair this browser** to submit a
  same-origin POST. Only that POST consumes the five-minute, single-use token
  and issues a browser session cookie.

## [0.1.0-rc.8] - 2026-11-21

### Fixed

- Refresh update results after the running Desktop version changes, including
  caches from older releases without a recorded version. Settings ignores stale
  results without contacting the feed. Daily retry limits still apply within
  each version, including failed checks.

## [0.1.0-rc.7] - 2026-11-21

### Fixed

- Distinguish browser channel readiness from agent association in the live
  navigation notice, including association changes in already-open tabs.
- Return explicit `NO_CONNECTED_TAB` errors with recovery links when navigation
  or highlights have no matching live associated tab. Status queries and missing
  highlight targets in connected tabs retain their existing behavior.

## [0.1.0-rc.6] - 2026-11-20

### Fixed

- Show resolved income statement dates in the form and reporting-period summary.
  Empty ranges explain that no revenue or expense activity matches the selected
  dates and currency, with a link to company transactions across all dates.
  Offsetting postings are treated as activity, and JSON downloads retain the
  displayed period.

## [0.1.0-rc.5] - 2026-11-20

### Fixed

- Show exact, grouped major-unit currency amounts on the dashboard, register,
  transaction details, reports and proposal review, including spend caps and
  approval buttons. Report HTML prefers upstream decimal display fields and
  hides duplicate raw amount columns. JSON/CSV downloads and raw audit/payload
  details retain their original units.

## [0.1.0-rc.4] - 2026-11-20

### Fixed

- Discover published stable and RC releases for RC update checks without relying
  on GitHub's stable-only latest release. Select by semantic version and verify
  the manifest and bundle from the same exact tag. Stable clients stay on stable
  releases; signed RC manifests and self-update archives are now supported.

## [0.1.0-rc.3] - 2026-11-20

### Fixed

- Explicit active/valid, missing and unavailable license status in the CLI and
  paired browser settings. Display the update window separately from perpetual
  use, retaining signed release dates for update eligibility.

## [0.1.0-rc.2] - 2026-11-20

### Fixed

- Show readable UTC update-check times in the CLI and Settings, distinguish
  incomplete checks from successful results, and explain HTTP failures without
  assuming a connectivity problem. Existing update caches remain readable.

## [0.1.0-rc.1] - 2026-11-20

Prerelease of 0.1.0. The git tag `beankeeper-desktop-v0.1.0-rc.1` is published
as a GitHub prerelease and is not marked latest. Release artifacts embed the
production license public key `prod-2026` and
`https://license.beankeeper.govcraft.ai` from `packaging/license-embed.json`.

Update checks accept a prerelease *local* version (for example `0.1.0-rc.1`)
so an rc binary can compare itself to a stable manifest. Published update
manifests remain stable-only.

## [0.1.0] - 2026-11-20

Release tags use `beankeeper-desktop-vX.Y.Z`. There is no paid Apple or Windows
signing yet; installers may show Gatekeeper or SmartScreen warnings. Verify
artifacts with cosign and GitHub attestations as described in the README.

### Added

- Build-time `BEANKEEPER_LICENSE_PUBLIC_KEYS_JSON` and
  `BEANKEEPER_LICENSE_SERVICE_URL` (via `build.rs` / `option_env!`), with
  runtime overrides. A loopback HTTP license service is accepted so a locally
  built `bk` can `license activate` against a self-run issuer.
- A README "Unsigned builds" section covering Gatekeeper, SmartScreen, and
  Sigstore / provenance verification.
- Pin `beankeeper-cli` to git tag `beankeeper-cli-v0.27.0`.

- Isolated offline Ed25519 Desktop license activation compatible with D10,
  perpetual entitlements and inclusive update windows, plus optional HTTPS
  storefront-key or claim-code redemption.
- Daily opt-out signed release checks in CLI and paired browser settings, and
  explicit self-update requiring matching SHA-256 checksums and mandatory
  Sigstore bundles before binary replacement. Release packaging emits and signs
  the update manifest; no ledger or schema changes.
- Authenticated `ui_show_view`, `ui_highlight` and `ui_status` MCP host tools with
  separate upstream UI grants, D3-associated tab selection, bounded SSE events,
  confirmed browser state, readable no-tab pairing links and invalid-view errors.
- Embedded same-origin browser receiver, stable row highlight IDs and navigation
  catalog; no frontend build dependency, ledger handlers or schema changes.

- Company-scoped browser review queue and proposal deep links with complete
  posting details, proposer identity and current spend caps. Paired, CSRF/Origin
  protected human approval and rejection use upstream atomic audited operations.
- Full `bk verify` status with check time and explicit recheck, plus fresh
  verification before decisions. Enforced ledgers use an ephemeral server-only
  local-human identity; incoming agent credentials never authorize browser review.

- URL-addressable income statement, balance sheet, cash flow, spending/net-worth
  trends and budget variance using upstream's CLI report contract.
- Schedule C and 1099 summary tables and paired-browser CSV/JSON downloads, with
  bookkeeping-only disclaimers and preserved report filters.
- Read-only bank and settlement reconciliation with account, statement, as-of
  date and currency filters. Stable report view IDs for agent navigation.
- Per-launch, expiring, single-use browser pairing links, host-bound human
  session cookies, CSRF/Origin checks, and browser unpairing and additional links.
- Agent association invites issued only after successful upstream MCP
  authentication, with human confirmation and routing APIs for D4 navigation.
- Read-only dashboard, company-scoped accounts, filterable transaction register
  and transaction details with entries, metadata, audit and agent identity.
- Stable view URLs, bounded register pagination, explicit empty/error states and
  D3 paired human sessions on every ledger page. No schema changes.
- `bk setup` for encrypted onboarding, native OS passphrase storage, private
  recovery kits, least-privilege agent presets with spend limits, merged MCP
  client configuration and local server/browser startup. Added a stdio-to-HTTP
  `bk mcp bridge` and automatic stored-passphrase lookup for enrolled ledgers.
- Cross-platform release packaging with Linux tarballs and Debian packages,
  per-architecture macOS packages, and Windows ZIP/EXE installers. Unsigned CI
  dry runs, optional Apple/Azure signing hooks, and shared SHA-256, Sigstore and
  provenance coverage for all final release assets.
- Checksum-verifying shell and PowerShell installers with optional Sigstore
  bundle verification and documented fallback for older unsigned releases.
- Cargo workspace for the source-available Desktop `bk` binary, with stable
  format/lint/test CI and a Rust 1.91 compatibility check.
- `bk serve` hosting embedded status and placeholder views beside upstream's
  authenticated `/mcp` router, with loopback defaults, explicit remote-bind
  consent, port-specific Host/Origin validation and graceful MCP cancellation.
- `bk open` probing the running Desktop server before launching the browser.
- Frontend build and local server documentation. No ledger or schema changes.

### Fixed

- Navigate to real report and reconciliation views with URL-restorable filters
  and highlight their filter/results regions after integrating D6.

- Pass the resolved ledger secret directly to upstream's public host-tool router,
  preserving one-shot sources and removing the Unix-only passphrase pipe bridge.
