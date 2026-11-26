# License and update commands

These Desktop commands do not open a ledger or migrate its schema. The
`license` module owns signature verification, atomic activation and entitlement;
the `updates` module owns release verification and installation. The approved
1.0 terms cover every 1.x release under a 1.x license. Release candidates run
without activation. [Beankeeper Desktop license agreement](../LICENSE.md)

Official stable releases from 1.0.0 apply read-only mode to each set of books
whose license is missing, invalid, revoked or for another major version.
Reads, exports, backups, restores, restore undo and recovery are available.
Agents with existing authorization can undo an audited change with `audit_undo`
over MCP or `bk audit undo` in the CLI. Owners can also undo from Changes.

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
before creating books, activate it again after setup to bind it. Each license
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

The CLI defaults to `polar`. In MCP, checkout codes beginning with `cs_live_`
or `cs_test_` select `stripe`. Other claim codes default to `etsy`. Set
`storefront` when redeeming a Polar or Gumroad key. Desktop keeps the Etsy recovery
credential in a private file before redemption. If the service used the claim
code but its first reply went missing, repeat the activation with the same
claim code. Desktop uses its saved credential to recover the signed license.
An existing CLI installation can still supply its own private credential file
with `BEANKEEPER_LICENSE_RECOVERY_TOKEN_FILE`.

Desktop verifies the redeemed license with the configured issuer keys before
saving it. Status and offline activation work without contacting the license
service.

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
`cosign` must be on PATH for both checks and installation. Missing cosign, missing
bundles, invalid checksums, wrong signing identity and invalid metadata all
prevent installation. Self-update runs only when requested.

Discovery requests send the Desktop `version` and `os`, plus the RC list's
`per_page=100` pagination parameter, with a fixed product User Agent. They send
no company, account, transaction, passphrase, license ID or purchaser data. Downloads use bounded HTTPS requests. Cosign can retrieve its
public trust roots from Sigstore, independently of the GitHub metadata request.

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
