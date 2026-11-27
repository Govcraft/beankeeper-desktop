# License and update commands

These Desktop commands do not open a ledger or migrate its schema. The
`license` module owns signature verification, atomic activation and entitlement;
the `updates` module owns release verification and installation. The approved
1.0 terms cover every 1.x release under a 1.x license. Release candidates run
without activation. [Beankeeper Desktop license agreement](../LICENSE.md)

Official stable releases from 1.0.0 apply read-only mode to each set of books
whose license is missing, invalid, revoked, expired or for another major version.
Reads, exports, backups, restores, restore undo and recovery are available.
Agents with existing authorization can undo an audited change with `audit_undo`
over MCP or `bk audit undo` in the CLI. Owners can also undo from Changes.

## Start a free trial

Setup automatically requests and activates a 30-day trial when these books have
no active license. The owner needs no email, account, terminal or token. An agent
can explicitly retry through `license_start_trial` on the local lifecycle MCP
connection, or `bk license trial`; both use the setup activation path.

Desktop creates an opaque random `MachineToken` once and keeps it privately in
`machine-token` inside its state directory. It sends only that token in a JSON
body to `POST /api/v1/licenses/trial`, never hardware identifiers. Concurrent
starts reuse the same saved token. The server returns the same trial on active
retries, so retrying never restarts its clock. Removing local activations keeps
the machine token. Wiping state can create a new identity; this anonymous limit
is not hardware attestation.

A network failure leaves setup and books available; ordinary writes wait for a
successful trial or another license. Retry later through the same tool. A spent
trial returns `TRIAL_ALREADY_USED` with a purchase link; rate limiting reports
`TRIAL_RATE_LIMITED` and the server's retry delay when available.

`bk license status`, `bk --json license status`, `license_status` and the license
portion of `service_status` report trial expiry and days remaining alongside
plain words and a Desktop settings link when available. Expired trials have
zero days remaining. At expiry, ordinary writes pause using the existing
license write boundary. Reads, export, backup, restore, recovery and audited
undo remain available; the books are unchanged.

Activating a paid license saves it before removing local trial activations and
bindings. The machine token stays private for idempotent retries. Trials have
no refund or in-app return. Ask the agent to activate a paid license after
purchase at https://beankeeper.app/#pricing.

## Configure issuer trust

Production signing keys are not available in this repository. Provision issuer
**public** keys independently before activation, using one of these options:

- At build time, set `BEANKEEPER_LICENSE_PUBLIC_KEYS_JSON` to a JSON object mapping
  key identifiers to unpadded base64url public keys. The compiler embeds this
  public trust map. No signing key belongs in a build.
- For online redemption, set `BEANKEEPER_LICENSE_SERVICE_URL` at build time or
  at runtime. The runtime value takes precedence. HTTP is allowed only on loopback.

For a custom installation, set `BEANKEEPER_LICENSE_KEYS_FILE` to a JSON file with
  the same map. This explicit local trust override replaces the embedded map.

An empty map or unknown issuer fails closed. Never trust a public key copied
from an unverified license. Production release administrators must provision the
real public map before publishing a paid build. The D10 fixture key and fixture
license exist under `crates/beankeeper-desktop/tests/fixtures/` for offline tests.

## Activate and inspect

Ask your agent to activate the license you received. On the local MCP connection,
`license_activate` accepts exactly one of `file_path` (a saved copy), `claim_code`
(online redemption), or `license` (signed text). The Claude Desktop extension
and local lifecycle connection provide both tools, including when the managed
service is stopped. On a direct ledger MCP connection, `license_status` reports
write access for the connected books.

```sh
bk license activate /path/to/downloaded.license
bk license activate --token-stdin < /path/to/downloaded.license
bk license
bk --json license
```

The local tools and CLI use the managed books when configured, otherwise the
default books. Use the CLI's `--db BOOKS` option to select another set. Use
`--json` on status or activation for the same structured result as local MCP.

Desktop verifies each PASETO v4.public token offline before saving it by license
ID. Activation binds the license to the canonical path of the selected books.
You can reactivate for the same path. Each additional set of books needs its
own license, including a copied ledger. If you activated a preview license
before creating books, activate it again after setup to bind it. Each perpetual license
covers one major version for perpetual use.

When writes are denied, Desktop returns `LEDGER_WRITE_DENIED` to the CLI and
MCP. The running Desktop exposes `license_status` to explain the current books'
status and return a settings link. Activation and local removal take effect
on the next write without restarting Desktop.

The `bk license` command and the paired browser's `/settings` page show
the selected books' write access and list verified installed licenses with their
major versions.
They also explain when there are no activations or when a file fails verification.

Desktop stores each private activation at `licenses/<license-id>.license`
inside the per-user state directory. If status finds an older JSON envelope, it
asks you to get a current license from the issuer.

| Platform | Default directory |
| --- | --- |
| Linux | `$XDG_STATE_HOME/beankeeper-desktop`, otherwise `~/.local/state/beankeeper-desktop` |
| macOS | `~/Library/Application Support/beankeeper-desktop` |
| Windows | `%LOCALAPPDATA%\beankeeper-desktop` |

`BEANKEEPER_DESKTOP_STATE_DIR` overrides the directory for isolated installations
and tests. New license directories/files use Unix modes 0700/0600 or protected
Windows ACLs. License status never reads the passphrase store or an agent token.

## Redeem a key or claim code

A downloaded license file is sufficient. To redeem online instead, configure
`BEANKEEPER_LICENSE_SERVICE_URL` with the D10 HTTPS origin and put the key or
claim code in a private text file. Redirects are refused. The key does not enter
process arguments or diagnostics.

```sh
bk --db /path/to/books.db license activate --code-file /private/storefront-key.txt --storefront polar
```

In the CLI and MCP, checkout codes beginning with `cs_live_` or `cs_test_`
select `stripe`. Other claim codes default to `etsy`. Use `--storefront` in
the CLI or `storefront` in MCP when redeeming a Polar or Gumroad key. Desktop
keeps the Etsy recovery credential in a private file before redemption. If the
service used the claim code but its first reply went missing, repeat activation
with the same claim code. Desktop uses its saved credential to recover the signed
license.
An existing CLI installation can still supply its own private credential file
with `BEANKEEPER_LICENSE_RECOVERY_TOKEN_FILE`.

Desktop verifies the redeemed license with the configured issuer keys before
saving it. Status and offline activation work without contacting the license
service.

## Remove a license from this computer

Local license removal: ask your agent to remove a specific installed license.
The `license_remove` MCP tool takes `license_id`; the CLI equivalent is
`bk license remove <license-id>`. Removal acts directly, without confirmation,
refund, or a server-side return. It removes only that local activation and binding,
leaving books and recovery information intact. The result reports whether the
selected books remain writable. In official stable builds, books without another
active license become read-only. Undo by asking the agent to activate the same
license again, provided it has not been revoked. License status includes each
license's typed `standing` (`valid` or `revoked`) and signed `storefront`; revoked licenses never authorize writes.
Revocation status reflects the signed list bundled with the installed release.

## Return a license and request a refund

Ask your agent to return your license, or run `bk license return`. Before the
single confirmation, Desktop explains:

> Returns within 60 days of purchase are refunded automatically. If the 60-day
> window has closed, the return is not accepted, your license stays active and
> nothing changes.

Confirming requests the return and refund. Paid features stop on this computer
on success. Your books stay yours and readable. The return never touches your
books, ledgers or recovery kit. Desktop cannot know the purchase date in advance;
the license service decides eligibility.

```sh
bk license return
bk --json license return --yes
bk license return --license-id lic_YOUR_LICENSE_ID --yes
```

If several licenses are installed, select one with `--license-id`. Agents can
use `--yes` after conveying the policy and obtaining the owner's confirmation.
The local MCP tool is `license_return`, beside `license_status` and
`license_activate` on `bk service mcp`. Supported clients show one confirmation
through MCP elicitation. Other clients receive the policy and must call again
with `confirm: true` after the owner's confirmation. Neither path requires a
Desktop screen.

On success, Desktop removes only the returned local license and records the
receipt ID in its local license event log. Keep the receipt for support. A
`pending` refund means the return was recorded and the refund is being submitted
automatically; `submitted` means the seller accepted it. A retried committed
challenge retrieves the same receipt and local removal is idempotent.

A closed window leaves your license unchanged and active. If a return reports that this license has no refund or in-app return, contact support@beankeeper.app. An unauthenticated
file or challenge cannot be returned. A revoked or replaced license is reported
as such. If the original seller cannot process the refund, the license stays
active: try again later or contact support@beankeeper.app. Every failed request
preserves the local license file.

## Check and install updates

```sh
bk update-check
bk self-update
```

Stable builds discover the latest stable release of
`Govcraft/beankeeper-desktop` through GitHub's releases API. RC builds query the
100 most recent release records and select the highest semantic version among
published stable and `-rc.N` releases, regardless of list order. Drafts, foreign
tags, other prerelease formats, build metadata and inconsistent prerelease flags
are ignored. An empty or unsupported release set is reported as a failed check.
An RC build can discover its own release before any stable release exists,
advance from `rc.2` to `rc.10`, or upgrade to the final stable release. A stable
build never upgrades to an RC release, even of a higher version.

The selected tag supplies both `update-manifest.json` and
`update-manifest.json.sigstore` through tag-specific download URLs. The API
only selects a tag; its asset URLs are never trusted. The manifest must verify
against that exact tag and name the same version. Discovery failures, missing
assets or invalid signatures are cached as failures without an unsigned fallback.
Checks and installation verify Sigstore evidence in-process, with no `cosign` prerequisite. Missing
bundles, invalid checksums, wrong signing identity and invalid metadata all
prevent installation. Self-update runs only when requested.

Discovery requests send the Desktop `version` and `os`, plus the RC list's
`per_page=100` pagination parameter, with a fixed product User Agent. They send
no company, account, transaction, passphrase, license ID or purchaser data. Downloads use bounded HTTPS requests. Verification uses the Sigstore crate's
embedded production trust roots without a separate network request. If those
roots do not cover future signing authorities or log keys, Desktop can recover
through `cosign verify-blob` when cosign is on PATH, with the same exact release
identity and issuer. It first checks the artifact digest, signature, and identity;
tampering and identity mismatches never select recovery. Cosign must then verify
the full bundle against its trust roots. Missing cosign or failed recovery leaves
verification closed with the same error.

`bk serve` checks after successful startup without delaying ledger access, then
once per day. `bk update-check` shares the persisted attempt time and process
lock, so failures, restarts, concurrent CLI/UI use and backward clock changes do
not cause repeated checks within the same Desktop version. Each attempt records
the running version in `updates.json` as `desktop_version`. A different running
version, including a downgrade or channel change, ignores the old result and
allows a fresh check immediately. Legacy caches without a version are handled
the same way. Settings hides these stale results and prompts for a check without
rewriting the cache or contacting the feed. The next `bk update-check` or
background server check replaces them. The process lock still prevents concurrent
checks across versions; a crash lock expires after one day. The cache is
`updates.json` in the same state directory. Both the CLI and Settings show the
outcome separately from a readable UTC last-attempt time, for example
`Last attempt: 2026-11-21 02:06:44 UTC.` An incomplete check is identified as
still running or interrupted. Failed checks explain the reason, including the
HTTP status for an unsuccessful release response, such as a missing release
file (HTTP 404). Failed and interrupted checks retain the daily retry interval
within their recorded version. There is no force-check bypass of that interval.

Set `BEANKEEPER_NO_UPDATE_CHECK=1` or `true` to opt out. This prevents checks
before accessing cache files or the network and disables `bk self-update` until
unset. Activation and all normal ledger commands never trigger update checks.
The paired browser's `/settings` page reads cached status without making an
HTTP request to the release feed. It requires the same human session and local
Host/Origin checks as other workspace pages.

Self-update re-authenticates the selected manifest and requires a newer version
whose major matches any active license on the installation. A 1.x license
covers every 1.x release regardless of dates, and 2.x requires a 2.x license.
For 0.x preview updates, use the signed installer. Self-update downloads the
matching target archive, checksum list and both mandatory Sigstore bundles.
Only the exact regular `bk` member
is copied into a temporary file beside the destination. All verification and
extraction finish before replacement. On Unix the rename is atomic; on Windows
`self-replace` handles the running executable and schedules old-file cleanup.
Permission or verification failures preserve the installed executable. No sudo
is launched. For a system-owned installation, use the OS installer with the
required privileges. Restart `bk serve` to run the new executable; no ledger or
configuration is replaced.

### Read cached Desktop updates

Use `update_status` on the local Beankeeper connection, or `bk update-status
--json`, to read the installed version and cached `desktop_updates` without a
network request. The typed result includes `state`; checked results carry
`last_attempt` (Unix seconds) and an `outcome` with `state`, the latest known
`version` and `release_notes_url` when known. Missing or version-mismatched
caches are `unchecked`; disabled checks are `disabled`. Failed and incomplete
checks never claim that Desktop is up to date.

Read this status when starting work if the tool is listed. Mention an available
release once per conversation in plain words, for example, “Beankeeper 1.10.0
is available. I can help update it.” Continue the owner's task. Never block
bookkeeping, repeat the notice on each call, or ask the owner to use a terminal
or handle tokens. Offer the returned release notes link only as optional detail.
When the owner asks for a fresh check, use the separate `update_check` tool
(`bk update-check` on the CLI); its existing daily retry limit still applies.
