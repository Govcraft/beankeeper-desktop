# Beankeeper Desktop

Beankeeper Desktop adds a local browser workspace to `bk`. Your own agent talks
to the authenticated MCP endpoint; the browser displays the workspace. This
initial preview includes a running-status page and placeholders for transactions,
reports and human review. Browser pairing and agent associations protect the
workspace; live views and browser approvals come later.

Source-available. See [LICENSE.md](LICENSE.md); this repository is not published
as a crate. Ledger, schema and accounting changes belong in
[Govcraft/beankeeper](https://github.com/Govcraft/beankeeper).

## Build and run

Requires Rust 1.91 or newer and the C build tools needed by upstream's bundled
SQLCipher/OpenSSL. See [the frontend/toolchain decision](docs/frontend.md).

```sh
cargo build --release --locked
# Initialize and configure a ledger with the upstream bk commands first.
./target/release/bk --db /path/to/books.db --passphrase-file /secure/passphrase serve
# Open the one-time pairing URL printed in this terminal.
# Later, reopen the paired workspace from another terminal:
./target/release/bk open
```

One process serves the embedded UI at `http://127.0.0.1:8750/` and Streamable HTTP
MCP at `http://127.0.0.1:8750/mcp`. No external asset directory or Node.js is
needed at runtime or build time. Direct links to `/transactions`, `/reports`,
and `/review` work after pairing. Unknown URLs return 404.

`bk serve` requires an existing ledger and resolves the passphrase once without
prompting, using upstream's `--passphrase-file`, `--passphrase-fd` or
`BEANKEEPER_PASSPHRASE_CMD`. All upstream CLI commands remain available, including
`bk mcp serve` for stdio and `bk mcp serve --http` for MCP-only HTTP. Desktop
extends upstream's command parser and delegates those commands to its dispatcher.
Use `bk --help` or `bk serve --help` for options.

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
