# Frontend build decision

D2 uses semantic HTML and plain CSS in `ui/`, compiled into `bk` with Rust's
`include_str!`. There is no JavaScript runtime, framework, generated asset tree,
or separate frontend package to install. Ledger views use server-rendered HTML
and GET forms, so filters remain URL-addressable without browser scripts. D3's
pairing and session forms protect every ledger view. Push events and navigation
tools belong to D4.

Install stable Rust with rustup, plus a C compiler, make, Perl and pkg-config
for upstream's bundled SQLCipher/OpenSSL build. On Ubuntu:

```sh
sudo apt-get install build-essential perl pkg-config
rustup toolchain install stable --component rustfmt,clippy
cargo build --release --locked
```

Edit `ui/index.html` or `ui/style.css`, then rebuild `bk`. Cargo tracks both
`include_str!` inputs, so an asset change recompiles the embedding module.
CI's clippy and test builds compile these exact sources on every PR. There are
no checked-in generated assets to drift from their inputs. Integration tests
request both the HTML and CSS from a server started with an empty current
working directory, and verify pairing followed by direct navigation to each
view URL.
The released binary needs no adjacent `ui/` or `dist/` directory.

The server owns explicit routes for `/`, `/accounts`, `/transactions`,
`/transactions/{id}`, `/reports`, and `/review`, with real 404s for unknown paths.
New Rust handlers or a future SPA can replace individual routes without changing the listener or MCP router.
Each page uses a shared shell, accessible navigation, a skip link and a visible
current-page indicator. Only authenticated HTML responses contain ledger data.
All ledger values are escaped as text or encoded into local links. Authenticated
pages retain D3 session controls and session-bound CSRF fields. The public pairing
screen contains no ledger data, session cookie or agent credentials. There are
no external fonts, analytics, third-party assets or proposal controls.
See [browser pairing](browser-pairing.md).

If future views need an asset bundler, adopt a small TypeScript/Vite build with
pnpm and document that decision in its own issue. D2 deliberately requires only
the Rust toolchain. Its restrictive CSP will need an explicit update when
browser scripts are introduced.

## Core views and read boundary

D5 calls the pinned upstream CLI library's company, account, report, transaction
and audit query functions from a read-only `Db::open_snapshot` connection. Each
page runs on a blocking worker under one database read transaction. There is no
browser JSON API, parallel ledger or migration. MCP retains its upstream
connection and credential enforcement on the same listener.

At startup Desktop resolves the original passphrase source once, supplies the
same secret to upstream's router factory through an anonymous in-memory pipe FD
(`passphrase_fd`), and opens the read-only snapshot. A scoped writer runs while
the factory reads the pipe, so secrets larger than the pipe buffer cannot block
startup. Both pipe ends close before serving; the resolved secret is never
written to a temporary file. This preserves one-shot passphrase FDs and commands
with the source-based upstream factory Desktop currently uses. Encrypted Desktop
startup currently requires Unix because this bridge uses Unix passphrase FDs.
Upstream added a resolved-passphrase router factory in
[beankeeper#111](https://github.com/Govcraft/beankeeper/pull/111), closing #109;
Desktop has not yet integrated it.

`/`, `/accounts`, `/transactions`, `/transactions/{id}`, `/reports` and `/review`
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
