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

An agent with CLI access can activate a token the owner pastes in chat. If the
issuer sent a plain-text license file, the agent can pass its path instead.

```sh
bk --db /path/to/books.db license activate --token 'PASTE_YOUR_LICENSE_TOKEN_HERE'
bk --db /path/to/books.db license activate /path/to/downloaded.license
bk --db /path/to/books.db license
```

Desktop verifies each PASETO v4.public token offline before saving it by license
ID and preserves existing activations if verification fails. Desktop binds the
license ID to the canonical local path of the selected books. Activation is
repeatable for that path. A license already bound to another path cannot unlock
a second set of books, including a copied ledger. Activate again with `--db` to
bind a preview activation saved without a ledger. Each license covers one major
version for perpetual use.

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

The default storefront is `polar`. `gumroad` and `etsy` use the same D10 adapter
contract and depend on service configuration. Etsy additionally requires
`BEANKEEPER_LICENSE_RECOVERY_TOKEN_FILE`, a private file containing a retained
32 to 512 byte recovery token different from the claim code. Generate the token
from at least 32 random bytes and keep it before redemption. If the connection
fails after a single-use code is consumed, recover the original token via D10's
`POST /api/v1/licenses/recover` using that retained token. Desktop never prints
or stores this recovery token. Public keys are checked for configuration before
redemption. Desktop verifies the returned token before activation.

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
