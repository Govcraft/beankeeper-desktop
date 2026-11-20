# Frontend build decision

D2 uses semantic HTML and plain CSS in `ui/`, compiled into `bk` with Rust's
`include_str!`. There is no build-time JavaScript runtime, framework, generated
asset tree, or separate frontend package to install. Ledger views use server-rendered HTML
and GET forms, so filters remain URL-addressable without browser scripts. D3's
pairing and session forms protect every ledger view. D4 adds a small embedded
same-origin script for SSE navigation and highlights. Ordinary navigation and all human forms still work without scripts.

Install stable Rust with rustup, plus a C compiler, make, Perl and pkg-config
for upstream's bundled SQLCipher/OpenSSL build. On Ubuntu:

```sh
sudo apt-get install build-essential perl pkg-config
rustup toolchain install stable --component rustfmt,clippy
cargo build --release --locked
```

Edit `ui/index.html`, `ui/style.css` or `ui/navigation.js`, then rebuild `bk`.
Cargo tracks these `include_str!` inputs, so an asset change recompiles the embedding module.
CI's clippy and test builds compile these exact sources on every PR. There are
no checked-in generated assets to drift from their inputs. Integration tests
request both the HTML and CSS from a server started with an empty current
working directory, and verify pairing followed by direct navigation to each
view URL.
The released binary needs no adjacent `ui/` or `dist/` directory.

The server owns explicit routes for `/`, `/accounts`, `/transactions`,
`/transactions/{id}`, `/reports`, `/reports/{id}`, `/reconciliation`, and `/review`,
with real 404s for unknown paths.
New Rust handlers or a future SPA can replace individual routes without changing the listener or MCP router.
Each page uses a shared shell, accessible navigation, a skip link and a visible
current-page indicator. Only authenticated view and download responses contain
ledger data.
All ledger values are escaped as text or encoded into local links. Authenticated
pages retain D3 session controls and session-bound CSRF fields. The public pairing
screen contains no ledger data, session cookie or agent credentials. There are
no external fonts, analytics, third-party assets. Proposal controls use protected HTML forms.
See [browser pairing](browser-pairing.md).

If future views need an asset bundler, adopt a small TypeScript/Vite build with
pnpm and document that decision in its own issue. D2 deliberately requires only
the Rust toolchain. CSP now permits same-origin scripts and connections for D4,
while forbidding inline and third-party scripts.

## Core views and read boundary

D5 calls the pinned upstream CLI library's company, account, report, transaction
and audit query functions from a read-only `Db::open_snapshot` connection. Each
page runs on a blocking worker under one database read transaction. There is no
browser ledger JSON API, parallel ledger or migration. MCP retains its upstream
connection and credential enforcement on the same listener.

At startup Desktop resolves the original passphrase source once and supplies the
same secret directly to upstream's `http_router_with_host_tools` factory. It also
opens read-only view and association snapshots plus a separate connection for
browser review. No Unix pipe bridge or plaintext temporary file is needed.
One-shot passphrase FDs and commands retain their single-use behavior. The
association snapshot calls upstream's public identity verifier after successful
MCP responses solely to derive D3 routing keys matching the host callback.
Upstream remains responsible for authentication, live grants and tool dispatch.

`/`, `/accounts`, `/transactions`, `/transactions/{id}`, `/reports`,
`/reports/{id}`, `/reconciliation` and `/review`
all use D3's paired human-session middleware. The harness redeems the terminal's
one-time pairing link and uses its host-bound cookie for saved view URLs.
Agent bearer tokens never authorize browser pages, even alongside a valid human
cookie; browser cookies never authorize MCP. Host/Origin checks wrap all routes.
`Referrer-Policy: same-origin` preserves Origin for same-origin form POSTs while
suppressing cross-origin referrers. Pairing redemptions and errors retain
`no-referrer`; authenticated invite confirmation forms use `same-origin` so their
POSTs retain Origin. Health and CSS remain public and contain no ledger data.

Integration tests launch a real server against a temporary encrypted ledger and
exercise D3 pairing, protected deep links, errors, security headers and MCP
isolation.
Renderer tests seed accounts and posted transactions using upstream APIs and
cover dashboard, complete account lists, filters, source-cursor pagination,
transaction metadata, audit and agent identity, and company isolation.

## Browser review

D7 adds `/review`, `/review/{id}`, POST `/review/{id}/approve`, POST
`/review/{id}/reject`, and POST `/review/verify`. All inherit D3's human guard;
reads reject bearer headers, and writes additionally require Origin and CSRF.
Review uses upstream proposal queries and atomic audited resolutions without
custom mutation SQL. Full verification and resolution run in the same savepoint.
The read-only core view connection stays separate. Review pages read one SQLite
snapshot on a blocking worker.

Enforced ledgers require a verified approval identity. Desktop issues one unique
12-hour, read/approve-only identity per server launch from the local trust root,
keeps its token in memory, and verifies it again for each review operation. The
actor name is `desktop-local-human`; request bearer tokens, MCP fallback tokens
and offline owner-token files never supply that authority. Identity issuance is
audited. The browser guard is the human authorization boundary. Policy-disabled
ledgers use the same actor name without a token. `--read-only` remains an MCP
tool ceiling, independent of human decisions. No MCP approval handlers change.

The queue labels spend caps as current and uses the posting description as its
purpose because upstream stores no separate proposal rationale. It offers reject
rather than withdraw, which belongs to the proposer. Resolved detail URLs persist
for receipt/reload. The verification panel caches its last complete `bk verify`
result in memory, provides a protected explicit recheck, and invalidates after a
browser decision. External writes require a recheck; decisions always verify
fresh regardless of this cache.

## Reports and reconciliation

D6 uses upstream `commands::report::execute_json` on the same read-only snapshot
and blocking-worker boundary. HTML renders every payload field as escaped
text, definition lists and tables, including nested reconciliation details. JSON
downloads return the CLI envelope's `data` value. Tax CSV projects that payload
into upstream's column order without repeating classification or arithmetic;
integration tests compare its output with CLI CSV, including quoted payee names.
No subprocess, temporary export file, browser JSON credentials or schema change
is involved. The public `reports::VIEW_IDS` list and [URL reference](reports.md)
provide the D4 integration contract. Reconciliation exposes no mutation path.
See [agent navigation](navigation.md) for the SSE protocol, tab lifecycle, grant
contract, embedded receiver and integration tests.
