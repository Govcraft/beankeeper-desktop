# Background service

Beankeeper can run quietly in the owner's login session. The agent can work with
local books without opening a browser. Installers set this up by default, or run:

```sh
bk service install
bk service status --json
```

First install creates encrypted local books, a random passphrase in the native
OS secret store, a private recovery kit, and separate owner and read-only agent
credentials. It registers login startup and starts the service. It never opens
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

## Commands

| Command | Result |
| --- | --- |
| `bk service install` | Create books on first use; register and start hosting. Reinstall updates the executable registration and preserves books, credentials, recovery kit and agent grants. |
| `bk service start` | Enable login startup and start installed hosting. |
| `bk service stop` | Stop hosting and disable login startup and automatic restarts until `start`. |
| `bk service status --json` | Report installed/running state, running Desktop version, PID, MCP endpoint, optional Desktop URL, books and log paths, and a plain-language message. |
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

The lifecycle tools are `service_install`, `service_uninstall`, `service_start`,
`service_stop` and `service_status`. They accept no arguments and return a
plain-language answer plus structured status or a named error. This separate
local stdio connection can restart a stopped HTTP server. It grants service
management, without ledger approval or administration capabilities. Only
configure it in agents the owner trusts to manage local hosting. The HTTP ledger
endpoint retains the existing upstream capability checks and browser pairing.
Restart agent clients once after installation to load the new connections.

Hosting always binds IPv4 `127.0.0.1`, trying port 8750 first and selecting an
available loopback port if it is occupied. The bound socket stays reserved
through server startup. A private runtime descriptor publishes the actual port;
`bridge --service` discovers it on each connection. Restart or reconnect the
ledger MCP client after service replacement. Explicit bridge `--url` and
`--token-file` options still work for manual servers.

Status authenticates the expected ledger token, verifies Desktop's health
identity and live PID, and reads the running version from the responding server.
It does not decrypt books or fetch a passphrase. Stale runtime files, unrelated
listeners and stopped managers do not count as a running service. A healthy
endpoint that rejects an expired or revoked agent token reports not ready;
the owner must renew that credential using upstream agent commands.

## Platform behavior and files

| Platform | Registration | Books and private recovery/credential sidecars | State and logs |
| --- | --- | --- | --- |
| Linux | `~/.config/systemd/user/beankeeper-desktop.service`, linked to a private unit in the state directory | `${XDG_DATA_HOME:-~/.local/share}/beankeeper-desktop/books.db` | `${XDG_STATE_HOME:-~/.local/state}/beankeeper-desktop/`, log at `logs/service.log` |
| macOS | `~/Library/LaunchAgents/ai.govcraft.beankeeper-desktop.plist` | `~/Library/Application Support/beankeeper-desktop/books.db` | Same Application Support directory; log at `~/Library/Logs/beankeeper-desktop/service.log` |
| Windows | Task Scheduler `Beankeeper-Desktop-<current-user-SID>`, interactive logon, limited privileges | `%LOCALAPPDATA%\beankeeper-desktop\books.db` | Same local app-data directory; log at `logs\service.log` |

State includes `service.json`, `service-runtime.json`, lifecycle/host lock files,
and native task/unit artifacts. It contains no passphrase or token value.
The private server log contains expiring pairing URLs. Keep it private and use
normal OS log maintenance if a long-lived installation accumulates diagnostics.
`BEANKEEPER_SERVICE_DATA_DIR`, `BEANKEEPER_DESKTOP_STATE_DIR` and
`BEANKEEPER_SERVICE_LOG_DIR` override storage with absolute paths. When using
custom paths, configure the same overrides in the lifecycle/bridge MCP client
environment. A service definition records them for the hosted process.

Linux uses `Restart=on-failure` with a two-second delay. Stop inspects its restart
policy, disables startup and masks the unit. Start unmasks and relinks it.
Lingering is left unchanged and is **not enabled by Beankeeper**. Hosting normally
ends when the last login session closes. An owner who explicitly wants hosting
after logout can enable lingering with `loginctl enable-linger "$USER"` and
reverse it with `loginctl disable-linger "$USER"`. A systemd user session and bus
are required; minimal containers without them can use foreground `bk serve`.

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
replacement and runs service installation again. Books and credentials survive.

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
