# Frontend build decision

D2 uses semantic HTML and plain CSS in `ui/`, compiled into `bk` with Rust's
`include_str!`. There is no JavaScript runtime, framework, generated asset tree,
or separate frontend package to install. This is sufficient for a status page
and URL-addressable placeholders, and keeps the initial Rust workspace small.
D3 adds server-rendered pairing and session forms without introducing scripts.
Push events, navigation tools and ledger views belong to later Desktop issues.

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
working directory, and verify direct navigation to each placeholder URL.
The released binary needs no adjacent `ui/` or `dist/` directory.

The server owns explicit routes for `/`, `/transactions`, `/reports`, and
`/review`, with real 404s for unknown paths. New Rust handlers or a future SPA
can replace individual routes without changing the listener or MCP router.
Each page uses a shared shell, accessible navigation, a skip link and a visible
current-page indicator. The public pairing screen contains no credentials or ledger data. Authenticated
pages include session-bound CSRF form fields. Neither page exposes the session
cookie or agent credentials. There are no proposal controls, external fonts,
analytics or third-party assets. See [browser pairing](browser-pairing.md).

If future views need an asset bundler, adopt a small TypeScript/Vite build with
pnpm and document that decision in its own issue. D2 deliberately requires only
the Rust toolchain. Its restrictive CSP will need an explicit update when
browser scripts are introduced.
