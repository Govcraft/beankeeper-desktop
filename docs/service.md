# Background service

Beankeeper can run quietly in the owner's login session. The agent can work with
local books without opening a browser. Installers set this up by default.

First install creates encrypted local books, a random passphrase in the native
OS secret store, a private recovery kit, and separate owner and bookkeeper
credentials. The bookkeeper credential posts transactions directly without
Desktop-provisioned spend limits.
The installer registers login startup and starts the service. It never opens
a browser or prints a passphrase, recovery key or bearer token. Keep an offline
copy of the recovery kit and owner credential. Those files belong to the owner;
the agent must not read or show them. The kit is created once and preserved on
reinstall. Recovery uses the existing [setup recovery procedure](setup.md#native-keychain-and-recovery).

A new service ledger falls back to a private passphrase file if the native store
is unavailable or locked. This is mode 0600 in a mode 0700 directory on Unix, or
a protected DACL granting only the owner and SYSTEM on Windows. The selected
store is recorded in the private Desktop metadata. `install --file-store` selects
this storage explicitly on headless hosts. Existing books always reuse their
recorded credential store; failure to unlock them never generates a replacement
passphrase. Ordinary `bk setup` continues to require an explicit file-store opt-in.

## Advanced CLI reference

| Command | Result |
| --- | --- |
| `bk service install` | Create books on first use; register and start hosting. Reinstall updates the executable registration, keeps the books and recovery kit, and checks the managed grant for the one-time legacy migration. |
| `bk service start` | Enable login startup and start installed hosting. |
| `bk service stop` | Stop hosting and disable login startup and automatic restarts until `start`. |
| `bk service status --json` | Report installed/running state, running Desktop version, PID, MCP endpoint, optional Desktop URL, books and log paths, and the running agent's preset and write mode. |
| `bk service access-set --preset read-only --json` | Change the managed agent’s grant under the local owner’s authority. See the write modes below. |
| `bk service uninstall` | Stop and remove hosting. Preserve all books, secrets, recovery material and MCP configuration. |
| `bk service mcp` | Serve local lifecycle tools over stdio, including while the HTTP service is stopped. |

`bk --db /absolute/path/books.db service install` selects other books on the first
install. Existing books must have compatible Desktop enrollment and credentials.
Pass `--no-agents` to leave existing MCP client configuration alone.
A conflicting existing MCP entry is left untouched and reported. Remove or rename
that entry after inspecting it before choosing a new service setup. Reinstalls
use the registered books; selecting a different ledger requires uninstalling the
service first. Service commands never delete ledger data or change its schema.
An unrelated preexisting native service definition is never overwritten.

## Agent access and endpoint discovery

Installation merges two entries into Claude Desktop, Claude Code and Codex:

- `beankeeper` runs `bk mcp bridge --service` with the private agent credential.
- `beankeeper-service` runs `bk service mcp` with the local OS user's authority.

`bk agents connect claude-code` recognizes existing entries with the managed
command and arguments, including entries written by `claude mcp add` with
`"type": "stdio"` or an empty `env`. Connect writes the managed entries in their
standard form and installs or updates the skill. If an entry has a non-empty
`env`, `connect`, `disconnect`, `detect`, and `doctor` report that field as a
conflict and preserve the entry. Remove `env` if the overrides are no longer
needed, or rename the entry to keep them, then retry.

The lifecycle tools are `service_install`, `service_uninstall`, `service_start`,
`service_stop`, `service_status` and `access_set`. They return a plain-language
answer plus structured status or a named error. `service_install` accepts
optional boolean arguments `file_store` and `no_agents`, equivalent to `--file-store` and
`--no-agents`. Both default to false. MCP installs use the managed books path.
Use `bk --db BOOKS service install` to select a particular ledger on first
install. The internal `install-extension` command is reserved for the native
bundle's first launch, when the bundle owns client registration and uses the
OS keychain.

This separate local stdio connection can restart a stopped HTTP server and
change the managed agent’s access. Configure the local stdio connections in
agents the owner trusts with these operations. The HTTP ledger endpoint
uses the agent’s verified capabilities and browser pairing.
Restart agent clients once after installation to load the new connections.

Hosting always binds IPv4 `127.0.0.1`. A fresh installation prefers port 8750.
When the configured port is occupied, the service saves an available fallback
port in `service.json` for future starts. The bound socket stays reserved through
server startup. A private runtime descriptor publishes the actual port for
status checks, `bk ui link` and each `bridge --service` connection. The managed
bridge reloads rotated credentials in the existing client session and signals
that the tool list changed. A bridge process from a release that loaded its
credential only at startup may need one MCP reconnect after upgrading.
Use explicit bridge `--url` and `--token-file` for manual servers.

Status verifies Desktop's launch identity and PID before using the private agent
credential to open a short MCP session. It reads the version from the responding
server and retrieves the verified `agent_access` from `setup_status`. The
plain-language `note` describes the verified access and explains how to undo
the legacy migration when `agent_access.migrated` is true.
Status reports `agent_access: null` when it cannot verify access with the
running service. The bridge and status check work from the published endpoint
even when the calling agent has no connection to the OS service manager.
Both status interfaces report `SERVICE_NOT_RUNNING` when an installed service
is stopped. Starting without an installation returns `SERVICE_NOT_INSTALLED`.
Lifecycle calls that need a missing Linux session bus return
`SESSION_ENV_UNAVAILABLE`. Status also reports a named error if the published
endpoint cannot be reached, its process identity differs, or MCP authentication fails.
If authentication fails because a credential has expired or been revoked,
explain the error. Credential renewal for manually administered ledgers is
covered by the upstream agent commands.

`bk service status` and `bk agents doctor` warn when another Desktop instance
is listening on port 8750. The warning names its books path when a local record
identifies it. Older Linux instances with an explicit `--db` can also be
identified from their launch arguments. If the path is unavailable, the warning
still identifies the competing listener. These checks never open competing books.

## Demo books on the managed connection

`demo_start` creates or reuses an encrypted sample ledger in a private directory
beside the managed books. The managed stdio bridge selects the returned target
for discovery and ledger calls on its existing session. Other sessions keep
their own selections. Direct HTTP clients call the demo controls on the startup
target and use `Beankeeper-Ledger-Target` for selected discovery and ledger calls.
Named results carry `_meta["beankeeper/ledgerTarget"]`. The startup result omits
that marker.

The demo enforces ledger policy with the startup credential's live signed
grants, write mode, and audit identity. Demo posts use the ordinary ledger
operations, including audited reversal. The service holds a fresh encryption
key in memory for each demo. [Change receipts and owner undo](changes.md) and
browser views use the owner's books.

`demo_remove` drains calls to the demo, closes the connection, and removes the
demo file. The invoking bridge switches to the owner's books after success.
Other bridges still targeting the removed demo return `DEMO_UNAVAILABLE`
until they start or remove the demo explicitly. A fresh demo receives a new
target. The service discards the demo on graceful shutdown. An abrupt stop can
leave an encrypted file whose key was lost with the process.

Use `bk demo start --json` and `bk demo remove --json` to manage the same
demo from shell automation. Owners can do both in chat.

## Choose a write mode

If your agent says it can only read your books, say **Let my agent post** in
chat when you want it to record transactions. Say **Make me read-only** to
turn posting off. Restarting cannot grant posting access. Your agent uses
`access_set` on the managed ledger or local lifecycle MCP connection and tells
you what changed. Beankeeper respects your choice to stay read-only.

With direct posting, your agent records each transaction under its own
identity and tells you what it recorded, with the company and transaction ID.
To undo it, ask for an audited reversal. An owner-controlled connection with
`ledger/reverse` uses `txn_reverse` to add the linked reversal.
You can also ask for propose-only access if you want to review proposed entries.

On startup or managed setup resume, Desktop moves the original legacy
read-only default to bookkeeper with direct posting. It preserves recorded
owner choices and recognized replacement grants. The next access status explains
the change and tells the owner that “make me read-only” undoes it in chat.

### Advanced access commands

Both MCP and CLI use the owner's private local authority. Each change issues a
grant and revokes the previous credential under the verified owner's identity.
Access applies across companies in the selected ledger.

| Chat request | `access_set` arguments | CLI |
| --- | --- | --- |
| Let my agent post. | `{"preset":"bookkeeper","write_mode":"direct"}` | `bk service access-set --preset bookkeeper --write-mode direct` |
| Switch to propose-only. | `{"preset":"bookkeeper","write_mode":"propose-only"}` | `bk service access-set --preset bookkeeper --write-mode propose-only` |
| Make me read-only. | `{"preset":"read-only"}` | `bk service access-set --preset read-only` |

Add `--json` for structured CLI status. For an audited reversal from an
authorized owner session, use `bk --db BOOKS --company COMPANY txn reverse ID`.

## Platform behavior and files

| Platform | Registration | Books and private recovery/credential sidecars | State and logs |
| --- | --- | --- | --- |
| Linux | `~/.config/systemd/user/beankeeper-desktop.service`, linked to a private unit in the state directory | `${XDG_DATA_HOME:-~/.local/share}/beankeeper-desktop/books.db` | `${XDG_STATE_HOME:-~/.local/state}/beankeeper-desktop/`, log at `logs/service.log` |
| macOS | `~/Library/LaunchAgents/ai.govcraft.beankeeper-desktop.plist` | `~/Library/Application Support/beankeeper-desktop/books.db` | Same Application Support directory; log at `~/Library/Logs/beankeeper-desktop/service.log` |
| Windows | Task Scheduler `Beankeeper-Desktop-<current-user-SID>`, interactive logon, limited privileges | `%LOCALAPPDATA%\beankeeper-desktop\books.db` | Same local app-data directory; log at `logs\service.log` |

State includes `service.json`, `service-runtime.json`, records under `instances/`
keyed by canonical books path and listener, lifecycle and host lock files, and
native task or unit artifacts. It contains no passphrase or token value.
The private server log contains expiring pairing URLs. Keep it private and use
normal OS log maintenance if a long-lived installation accumulates diagnostics.
`BEANKEEPER_SERVICE_DATA_DIR`, `BEANKEEPER_DESKTOP_STATE_DIR` and
`BEANKEEPER_SERVICE_LOG_DIR` override storage with absolute paths. When using
custom paths, configure the same overrides in the lifecycle and bridge MCP
client environment. Use the same Desktop state directory when starting a
foreground listener and generating CLI links for it. A service definition
records its overrides for the hosted process.

Linux uses `Restart=on-failure` with a two-second delay. Stop inspects its restart
policy, disables startup and masks the unit. Start unmasks and relinks it.
Lingering is left unchanged and is **not enabled by Beankeeper**. Hosting normally
ends when the last login session closes. An owner who explicitly wants hosting
after logout can enable lingering with `loginctl enable-linger "$USER"` and
reverse it with `loginctl disable-linger "$USER"`. A systemd user session and bus
are required; minimal containers without them can use foreground `bk serve`.
When the login environment is missing, lifecycle commands find the bus in the
owner's runtime directory (normally `/run/user/<UID>/bus`) after checking that
the directory and Unix socket both belong to that user. The commands preserve
explicit session settings. `bk service status --json` verifies the live local
endpoint even when the launcher supplies only `HOME`, `LOGNAME`, `USER`,
`SHELL` and `PATH`. If lifecycle discovery fails,
`SESSION_ENV_UNAVAILABLE` asks the owner to retry from an active login session
or pass its `XDG_RUNTIME_DIR` and `DBUS_SESSION_BUS_ADDRESS` to the launcher.
Managed agent registrations need no per-client session variables.

macOS uses `RunAtLoad` and a LaunchAgent that restarts after unsuccessful exit.
Stop disables the label and boots it out of the user's GUI domain. Start enables
and bootstraps it again. A logged-in GUI session is required.

Windows uses a current-user interactive logon task with limited privileges,
no runtime limit, and battery operation enabled. Its hidden runner waits for the
host and restarts it after a two-second delay whenever it exits. Task Scheduler
can report an executed action as completed even when the action returns an error,
so child-host recovery stays inside this small runner. The task also retains
native startup retry settings (one minute, up to 999 attempts). Stop disables the
task, signals the runner to terminate its child, and waits for the stop
acknowledgement. Diagnostics go to the protected log, and lifecycle helpers hide
their console windows. No administrator rights are needed for service management.

These registrations follow the native [systemd service contract](https://www.freedesktop.org/software/systemd/man/latest/systemd.service.html),
[launchd property-list contract](https://github.com/apple-oss-distributions/launchd/blob/main/man/launchd.plist.5),
and [Windows task restart settings](https://learn.microsoft.com/en-us/powershell/module/scheduledtasks/new-scheduledtasksettingsset).

## Installers and upgrades

The shell installer defaults to `~/.local/bin`; PowerShell defaults to
`%LOCALAPPDATA%\Programs\Beankeeper Desktop`. Both register hosting after verifying
and installing the binary. Reinstalling stops previous managed hosting before
replacement and runs service installation again. A self-update also restarts a
running service when it replaces that service’s executable. The managed access
migration runs when the updated service starts.

Skip background setup with `install.sh --no-service`, `install.ps1 -NoService`,
or `BK_NO_SERVICE=1`. The Windows EXE also accepts `/NOSERVICE=1` or an unchecked
**Run Beankeeper quietly at login** task. EXE uninstall removes hosting while
preserving all books. Package removal alone does not delete books or credentials;
run `bk service uninstall` before removing the binary.

The `.deb` post-install hook selects `SUDO_USER`, `PKEXEC_UID`, or the invoking
login user and runs `bk service install` through that user's available session
bus. The `.pkg` hook selects the console owner and their GUI launch domain.
Neither creates root-owned books. If no suitable login session can be identified,
the binary installation succeeds with instructions to run `bk service install`
once after login. A setup failure similarly leaves the installed binary and
private recovery artifacts available for diagnosis. Use `BK_NO_SERVICE=1` in the
package installer's environment to opt out, for example
`sudo env BK_NO_SERVICE=1 installer -pkg FILE.pkg -target /` on macOS or
`sudo env BK_NO_SERVICE=1 dpkg -i FILE.deb` on Linux. GUI package installation
uses the default behavior.

A failed or interrupted provisioning attempt never overwrites recovery material.
Preserve the private artifacts and follow the setup recovery procedure if a
credential or client configuration needs repair. Uninstall only removes hosting;
it is not an account reset or a book deletion command.
