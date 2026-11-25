# Desktop setup

Ask your connected agent to **Set up my books**. A Direct bookkeeper can
create books and accounts, apply a chart, record opening balances and import
the statements you share, all in the same chat. After each change it tells
you what it recorded. Ask in the same chat to undo a supported change.
[Chat setup guide](../SKILL.md#guide-the-owner-through-book-setup)

If it reports read-only access and you want it to make changes, say **Let my agent post**.
For managed books, the agent uses `access_set` to change access. Say **Make me
read-only** to keep posting off. Restarting cannot grant posting access.
[Background hosting and managed access](service.md#choose-a-write-mode)

## Bookkeeping interview

Once a book exists, your agent asks which accounts you use,
how to categorize recurring payees, how to handle sales tax and payment
processor fees, and when to tell you about changes. Choose updates after each
change, daily or weekly. The agent explains which updates its chat client can
deliver.

The agent saves clear answers directly and tells you what it recorded. You
can skip a question, ask what is saved, or change an answer later in chat.
Ask the agent to undo a policy or preference change to restore the previous
value with a new audit entry. When the agent posts using a best guess for a
category, it flags the guess in the next update so you can correct it.
[Agent interview and tool reference](../SKILL.md#ask-how-to-keep-the-books)

## Advanced manual setup

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

## Check setup progress

Run `bk --db BOOKS setup status --json` or call `setup_status` over MCP. Both
report the upstream counts of books, accounts and posted transactions, whether
the owner key exists, and Desktop's suggested next step. When you use a
credential, the response includes its verified access. The CLI reports a
missing ledger without creating one.

The upstream `ledger_setup_status` MCP tool returns the same counts and owner
key state. For each book's profile and accounts, call `ledger_setup_book_show`
or run `bk --db BOOKS setup book-show COMPANY --json`. Use the statement receipt
operation to look up a known import batch.

## Options

| Option | Behavior |
| --- | --- |
| `--non-interactive` | Suppress prompting and terminal secret display; required without a terminal. |
| `--preset read-only` | Default. Issue only `ledger/read`. |
| `--preset bookkeeper` | Issue `ledger/read`, `ledger/post` and `ledger/propose` in Direct mode without spend caps. |
| `--demo` | Use upstream demo initialization with its three sample companies. |
| `--force` or `--resume` | Resume an existing encrypted ledger. With no ledger selection, prefer the installed managed service’s books. |
| `--recovery-kit PATH` | Override the default private recovery kit path. |
| `--config-home PATH` | Use this home directory when locating all three client configs. |
| `--secret-store-file PATH` | Explicit private-file headless alternative to native keychain storage. |
| `--bind ADDRESS` | Canonical loopback address with a nonzero port; default `127.0.0.1:8750`. |
| `--no-start` | Skip server startup. |
| `--no-open` | Skip browser launch. Use with `--no-start` in CI. |

The bookkeeper reports what it posted and gives the owner the company and
transaction ID for [audited undo](service.md#choose-a-write-mode). Resuming
standalone setup preserves the existing credential and ledger policy. For the
installed managed books, resume also checks for the one-time legacy access
migration. An explicit `--preset` on managed resume records the owner’s choice.
The owner can also change managed access [in chat](service.md#choose-a-write-mode).
Tokens expire after one year.

Setup enables upstream capability enforcement. Keep the separate owner
credential for approval and administration private. The MCP client uses its
bookkeeper credential for supported corrections to its own work. For
`ui/navigate`, issue a separate UI credential as described below.

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

For an explicit headless setup (Unix paths shown):

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
On enforced ledgers, Desktop uses a live `ledger/admin` credential in the
private owner-token file to enable browser proposal approval. After restoring
the owner credential beside its ledger, restart Desktop and pair again.
If both the keychain and recorded passphrase are lost, the ledger cannot be
recovered. There is no bypass or backdoor.

## Automatic local backups

Desktop saves a first backup during setup or startup, then checks hourly for a
backup due after 24 hours. It also backs up before migrating a ledger and on
the first ledger open after the Desktop version or core revision changes.
When a backup fails, the service records the error in its log. A failed
pre-migration backup leaves the ledger at its existing schema.

The backup directory is `.beankeeper-backups/<ledger-filename>/` beside the
ledger. Each backup has a core `books.db` snapshot, backup metadata, and, when
there are attachments, an archive of the content-addressed files in
`attachments/`. For encrypted books, the snapshot and the
`attachments.tar.age` archive both use the original passphrase.
Desktop retains up to seven readable backups totaling up to 2 GiB per ledger. It
keeps the newest pre-migration or upgrade backup. If a new daily or requested
backup cannot fit alongside that copy, Desktop reports a size error. Desktop
also reports unreadable entries and rotation errors in its log or CLI
diagnostics. Core manages the separate undo snapshot recorded in each restore
receipt.

Ask your agent to list or restore a backup. With a ledger administrator grant,
the agent can restore directly and give you the receipt. It can undo that
restore on request while the receipt remains current. The live audit history
and credential policy stay in effect, and attachment files needed for the
restore or undo are available locally.

### Recover a missing ledger file

The private recovery kit contains the original passphrase. If the live file
is missing, a local administrator can run this from the original ledger's
directory.

```text
bk --db <new-ledger> --passphrase-file <private-file> recover --input .beankeeper-backups/<ledger-filename>/<bkbackup_…>/books.db
```

Supply the appropriate credential through `BK_AGENT_TOKEN` if enforcement
requires it. Preserve any existing sibling `attachments/` directory. If it
was also lost, decrypt the age archive with the same passphrase and extract it
into the new ledger's sibling `attachments/` directory.

Keep separate private copies of the recovery kit and owner credential file.

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
After pairing, the financial snapshot, accounts and transactions are available. Agent
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
