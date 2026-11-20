# License and update commands

These Desktop commands do not open a ledger or migrate its schema. The
`license` module owns signature verification, atomic activation and entitlement;
the `updates` module owns release verification and installation. `bk serve`
continues to work without an activation while final licensing terms are pending.
An expired update window does not disable perpetual use.

## Configure issuer trust

Production signing keys are not available in this repository. Provision issuer
**public** keys independently before activation, using one of these options:

- At build time, set `BEANKEEPER_LICENSE_PUBLIC_KEYS_JSON` to a JSON object mapping
  key identifiers to unpadded base64url public keys. The compiler embeds this
  public trust map. No signing key belongs in a build.
- For a custom installation, set `BEANKEEPER_LICENSE_KEYS_FILE` to a JSON file with
  the same map. This explicit local trust override replaces the embedded map.

An empty map or unknown issuer fails closed. Never trust a public key copied
from an unverified license. Production release administrators must provision the
real public map before publishing a paid build. The D10 fixture key and fixture
license exist under `crates/beankeeper-desktop/tests/fixtures/` for offline tests.

## Activate and inspect

```sh
bk license activate /path/to/downloaded-license.json
bk license
```

File activation verifies offline before saving and preserves an existing
activation if validation fails. Inspecting the entitlement re-verifies it
offline. The result reports the license ID, perpetual use, and inclusive update
deadline, without printing purchaser information.

The private activated file is `license.json` inside the per-user Desktop state
directory:

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
bk license activate --code-file /private/storefront-key.txt --storefront polar
```

The default storefront is `polar`. `gumroad` and `etsy` use the same D10 adapter
contract and depend on service configuration. Etsy additionally requires
`BEANKEEPER_LICENSE_RECOVERY_TOKEN_FILE`, a private file containing a retained
32 to 512 byte recovery token different from the claim code. Generate the token
from at least 32 random bytes and keep it before redemption. If the connection
fails after a single-use code is consumed, recover the original file via D10's
`POST /api/v1/licenses/recover` using that retained token. Desktop never prints
or stores this recovery token. Public keys are checked for configuration before
redemption; the returned file must verify before activation.

## Check and install updates

```sh
bk update-check
bk self-update
```

The feed is the `update-manifest.json` asset on the latest stable release of
`Govcraft/beankeeper-desktop`, authenticated by `update-manifest.json.sigstore`.
`cosign` must be on PATH for both checks and installation. Missing cosign, missing
bundles, invalid checksums, wrong signing identity and invalid metadata all
prevent installation. This differs deliberately from the legacy command-line
installers' checksum-only fallback. No self-update runs automatically.

Checks send only `version` and `os` query parameters, with a fixed product User
Agent. They send no company, account, transaction, passphrase, license ID or
purchaser data. Downloads use bounded HTTPS requests. Cosign can retrieve its
public trust roots from Sigstore, independently of the GitHub metadata request.

`bk serve` checks after successful startup without delaying ledger access, then
once per day. `bk update-check` shares the persisted attempt time and process
lock, so failures, restarts, concurrent CLI/UI use and backward clock changes do
not cause repeated checks. A crash lock expires after one day. The cache is
`updates.json` in the same state directory. Errors are visible alongside the
last attempt time; there is no force-check bypass of the daily interval.

Set `BEANKEEPER_NO_UPDATE_CHECK=1` or `true` to opt out. This prevents checks
before accessing cache files or the network and disables `bk self-update` until
unset. Activation and all normal ledger commands never trigger update checks.
The paired browser's `/settings` page reads cached status without making an
HTTP request to the release feed. It requires the same human session and local
Host/Origin checks as other workspace pages.

Self-update re-authenticates the selected manifest, compares versions
numerically, and verifies that its signed release timestamp falls within the
activated update window. It downloads the matching target archive, checksum
list and both mandatory Sigstore bundles. Only the exact regular `bk` member
is copied into a temporary file beside the destination. All verification and
extraction finish before replacement. On Unix the rename is atomic; on Windows
`self-replace` handles the running executable and schedules old-file cleanup.
Permission or verification failures preserve the installed executable. No sudo
is launched. For a system-owned installation, use the OS installer with the
required privileges. Restart `bk serve` to run the new executable; no ledger or
configuration is replaced.
