# Changelog

## [Unreleased]

### Added

- Cargo workspace for the source-available Desktop `bk` binary, with stable
  format/lint/test CI and a Rust 1.91 compatibility check.
- `bk serve` hosting embedded status and placeholder views beside upstream's
  authenticated `/mcp` router, with loopback defaults, explicit remote-bind
  consent, port-specific Host/Origin validation and graceful MCP cancellation.
- `bk open` probing the running Desktop server before launching the browser.
- Frontend build and local server documentation. No ledger or schema changes.
