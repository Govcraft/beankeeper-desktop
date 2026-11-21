# Changelog

## [Unreleased]

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
