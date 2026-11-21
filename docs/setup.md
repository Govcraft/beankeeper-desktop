# Desktop setup command reference

Run `bk setup` to create an encrypted ledger, save its passphrase in the OS
keychain, prepare a private recovery kit, configure three MCP clients, start the
local server and open the UI. No account or model API key is required.

```sh
bk --db ~/books.db setup
bk --db ~/demo.db setup --demo --preset bookkeeper
```

Interactive setup generates a random 256-bit passphrase and shows it once on the
controlling terminal, never on redirected stdout/stderr. Record it and copy the
private recovery kit to offline storage. Noninteractive setup generates the same
kind of passphrase but only saves it privately; it never displays credentials.
An explicit upstream `--passphrase-file`, `--passphrase-fd`, or
`BEANKEEPER_PASSPHRASE_CMD` takes precedence over generation.

## Options

| Option | Behavior |
| --- | --- |
| `--non-interactive` | Suppress prompting and terminal secret display; required without a terminal. |
| `--preset read-only` | Default. Issue only `ledger/read`. |
| `--preset bookkeeper` | Issue `ledger/read`, `ledger/post`, `ledger/propose`. No approval or admin grants. |
| `--max-post N` | Bookkeeper per-post cap, USD cents; default 10000 ($100). |
| `--daily-limit N` | Bookkeeper per-day cap, USD cents; default 50000 ($500). |
| `--demo` | Use upstream demo initialization with its three sample companies. |
| `--force` | Resume an existing encrypted ledger. Preserve its data, root and valid setup token. |
| `--recovery-kit PATH` | Override the default private recovery kit path. |
| `--config-home PATH` | Use this home directory when locating all three client configs. |
| `--secret-store-file PATH` | Explicit Unix headless alternative to native keychain storage. |
| `--bind ADDRESS` | Canonical loopback address with a nonzero port; default `127.0.0.1:8750`. |
| `--no-start` | Skip server startup. |
| `--no-open` | Skip browser launch. Use with `--no-start` in CI. |

Spend limits apply separately to every initial company, in USD. Other currencies
in those companies require an explicit owner-configured bucket. New companies
need owner-configured limits before giving the agent access: upstream interprets
an absent company bucket as unrestricted. Resuming setup preserves the existing
preset and limits; it does not apply new limit options or rotate credentials.
Use upstream agent issuance/revocation and spend-limit commands for changes.
Tokens expire after one year. Setup presets do not grant `ui/navigate`; issue a
separate UI token as described below.

The bookkeeper agent cannot approve proposals or change its own spend limits.
Setup enables upstream capability enforcement. A separate owner token includes
approval and administration grants for human CLI management. That token is never
referenced by any MCP client config. Human browser approval arrives with the
separate browser review work; do not give your agent the owner token.

## Native keychain and recovery

Credentials use service `beankeeper-desktop` and the canonical absolute ledger
path as the account name:

- macOS uses the login Keychain through keyring's native Apple backend.
- Windows uses Credential Manager through its native Windows backend.
- Linux uses the desktop session's D-Bus Secret Service through the zbus backend,
  for example GNOME Keyring or a Secret Service enabled KWallet. The login
  collection must be unlocked and the session bus available.

A locked or unavailable keychain is an error. Setup never silently falls back to
plaintext storage. Linux CI uses the explicit file-store seam in an isolated
private temp directory; native backend response/error mappings are unit-tested.
macOS Keychain and Windows Credential Manager require a real logged-in platform
session for end-to-end testing. Windows files use a protected native DACL granting
only the current process user and SYSTEM; the Linux CI run cannot exercise it.

For an explicit headless Unix setup:

```sh
install -d -m 700 /private/beankeeper-secrets
bk --db /private/books.db setup --non-interactive \
  --secret-store-file /private/beankeeper-secrets/passphrase \
  --config-home /private/client-home --no-start --no-open
```

The file-store parent must be private (0700) and the file must be private (0600).
The selected store is recorded in Desktop metadata, so later `bk serve` and
upstream commands load it automatically. Explicit passphrase sources still win.
Upstream accepts a file or descriptor, so Desktop holds a private temporary file
only for the duration of dispatch; the credential is not put into process args
or environment variables. The Desktop server passes the resolved secret to the
MCP router through upstream's resolved-passphrase factory, so one-shot sources
are read once without the former Unix pipe bridge. Unenrolled ledgers retain
upstream behavior.

If the keychain entry is lost, recover the passphrase from the recovery kit into
a private file and run:

```sh
bk --db /path/books.db --passphrase-file /private/recovered-passphrase setup \
  --force --no-start --no-open
```

Setup checks that this passphrase decrypts the ledger before updating the store.
Keep an offline backup of the separate owner-token file as well; the passphrase
alone does not replace a lost or expired bearer credential under enforcement.
If both the keychain and recorded passphrase are lost, the ledger cannot be
recovered. There is no bypass or backdoor.

Create an encrypted snapshot with upstream `bk backup --output`, and restore it
with `bk --db <explicit-new-destination> --passphrase-file <private-file> restore
--input <backup>`. Supply the private owner credential through `BK_AGENT_TOKEN`
when upstream requires administration rights. A ledger backup does not contain
the separate recovery kit or credential files; back those up separately.

## Files touched

For `/path/books.db`, setup creates these owner-only files:

| Path | Content |
| --- | --- |
| `/path/books.db` | Upstream SQLCipher ledger, initialized without any schema changes. |
| `/path/books.recovery.txt` | Passphrase, ledger path and recovery/backup instructions. |
| `/path/books.agent-token` | Agent bearer token; secret, never print or commit. |
| `/path/books.owner-token` | Separate human owner credential; never share with an agent. |
| `/path/books.desktop.json` | Store selection metadata, without credential values. |
| `/path/books.desktop.log` | Background server diagnostics, created only on startup. |
| Selected headless store path | Passphrase, only when `--secret-store-file` is explicit. |

Use distinct ledger basenames because sidecars replace the ledger extension.
The default recovery file and tokens are created without overwriting existing
files. Configuration is validated before ledger creation. If setup stops after
initialization, rerun with `--force` and the same options. If initialization did
not create a ledger but left a recovery file, preserve that file and choose a
fresh ledger basename, or remove the unused artifacts after verifying them.

Each client gets a `beankeeper` server entry running the absolute `bk` executable
with `--db <absolute-ledger> mcp bridge --url <local-mcp-url> --token-file
<private-agent-token>`. The JSON/TOML contains a token path, never its value.
The bridge shares the running Desktop server rather than starting another ledger
session via `bk mcp serve`. It requires a running server and never launches one.

| Client | Exact config location relative to `--config-home` / user home |
| --- | --- |
| Claude Desktop, Linux | `.config/Claude/claude_desktop_config.json` |
| Claude Desktop, macOS | `Library/Application Support/Claude/claude_desktop_config.json` |
| Claude Desktop, Windows | `AppData/Roaming/Claude/claude_desktop_config.json` |
| Claude Code | `.claude.json`, user-scope `mcpServers` |
| Codex | `.codex/config.toml`, `mcp_servers` |

Setup prints every config path it touches. It preserves unrelated servers and
settings, including Codex TOML comments, and refuses malformed files or a
conflicting `beankeeper` entry. Each config write is atomic. Writes across the
three clients are not one transaction; rerunning completes a partial write.
Restart the clients after setup. If you move the binary or ledger, explicitly
remove or rename the old `beankeeper` entries before configuring a fresh setup.

Config formats follow the official [Claude Desktop MCP guide](https://modelcontextprotocol.io/docs/2026-07-28/develop/connect-local-servers),
[Claude Code MCP reference](https://code.claude.com/docs/en/mcp), and
[Codex MCP reference](https://developers.openai.com/codex/mcp).

## Server and browser lifecycle

The background child serves the selected ledger and writes only to the private
log. Setup waits for readiness and prints its PID. The child continues after
setup exits; stop it using that PID, or run `bk serve` in the foreground instead.
Setup reuses a running Desktop server only when an authenticated MCP handshake
accepts the newly configured ledger token. A server for another ledger is
rejected. Startup failures leave the ledger and recovery artifacts intact.

Setup opens the UI origin, which shows a pairing screen until the browser is
paired. Open the one-time pairing URL from the private `.desktop.log` within
five minutes, or run `bk serve` in the foreground to see the URL in the terminal.
Select **Pair this browser** on that page to confirm before the link expires.
Opening or refreshing the page does not consume the link.
After pairing, the dashboard, accounts and transactions are available. Agent
association uses a separate link; see [browser pairing](browser-pairing.md).
Live navigation remains follow-up work. The stdio bridge forwards current
request/response MCP tools, negotiated session/protocol headers, and JSON or SSE
POST replies. It deletes the session when stdin closes. Standalone server-push
GET subscriptions are not implemented in this bridge; add them when a Desktop
feature requires asynchronous MCP server notifications.

## Separate navigation credentials

The setup presets remain ledger-only. Upstream UI tokens use a separate `/ui/`
subject, so a preset cannot combine `ui/navigate` with ledger grants. The owner
can issue a second navigation token and configure a second MCP client entry at
the same Desktop endpoint. See [navigation setup](navigation.md) for the issue
command, supported tools, pairing and read-only behavior.
