# Desktop release packaging

The release workflow builds the Desktop `bk` binary, including its embedded UI,
from this repository and the upstream revision recorded in `Cargo.lock`. It does
not publish crates, modify ledgers or run schema migrations. Apple Developer
Program and Azure Artifact Signing accounts are **pending Roland's setup**.
Paid signing hooks are disabled by default. No production credentials are
required to build or merge this work.

Until Beankeeper ships a stable release, both upstream crates track the Git
`main` branch. `Cargo.lock` is the sole lock, and regular CI and releases build
with `--locked`. The `upstream-main` job refreshes both crates when CI selects
Rust checks and on a daily schedule. It reports the revisions before building,
linting and testing them. Once Beankeeper ships a stable release, switch both
crates to stable release tags or crates.io versions and retire or repurpose the
job.

## Artifacts and versions

Desktop uses its own workspace version. Keep the tag convention
`beankeeper-desktop-vVERSION` and
archive layout `bk-VERSION-TARGET/bk` (`bk.exe` on Windows), so release consumers
can use the same naming rules with **Govcraft/beankeeper-desktop** as the repo.
Prerelease tags use a Cargo version such as `0.1.0-rc.1` and the git tag
`beankeeper-desktop-v0.1.0-rc.1`. The release job marks `-rc.N` tags as
GitHub prereleases and does not set them as latest. Debian packages use the
upstream version `0.1.0~rc.1` so they sort before `0.1.0`.
Release builds embed `packaging/license-embed.json` (production license public
keys and `https://license.beankeeper.govcraft.ai`). Repository Actions variables
`BEANKEEPER_LICENSE_PUBLIC_KEYS_JSON` and `BEANKEEPER_LICENSE_SERVICE_URL` can
override the file; the public key is not a secret.
Inspect the compiled-in values with `bk license trust --json`; the release smoke test compares that output to the embed env, rather than grepping raw binary bytes. Windows installers also set a numeric `VersionInfoVersion` (`0.1.0-rc.1` becomes `0.1.0.1`) because Inno Setup cannot store a prerelease string in the PE version resource.

| Target | Assets |
| --- | --- |
| `x86_64-unknown-linux-gnu` | `bk-VERSION-x86_64-unknown-linux-gnu.tar.gz`, `bk-VERSION-x86_64-unknown-linux-gnu.deb` |
| `aarch64-unknown-linux-gnu` | `bk-VERSION-aarch64-unknown-linux-gnu.tar.gz`, `bk-VERSION-aarch64-unknown-linux-gnu.deb` |
| `x86_64-apple-darwin` | `bk-VERSION-x86_64-apple-darwin.tar.gz`, `bk-VERSION-x86_64-apple-darwin.pkg` |
| `aarch64-apple-darwin` | `bk-VERSION-aarch64-apple-darwin.tar.gz`, `bk-VERSION-aarch64-apple-darwin.pkg` |
| `x86_64-pc-windows-msvc` | `bk-VERSION-x86_64-pc-windows-msvc.zip`, `bk-VERSION-x86_64-pc-windows-msvc-setup.exe` |

Each target also produces `bk-VERSION-TARGET.mcpb` for Claude Desktop. These
bundles contain the final native binary, manifest, icon, license, and agent
skill. Full builds also produce `bk-VERSION-universal-apple-darwin.mcpb` with
both Mac architectures. All bundles use the same SHA-256, Sigstore bundle and
provenance as the other release assets.

### Deliver the Claude extension from beankeeper.app

The site’s **Get it for the Claude app** action can serve one release asset per
download. Use the universal Mac asset for Apple Silicon and Intel and
`bk-VERSION-x86_64-pc-windows-msvc.mcpb` for Windows x64. Linux has separate
x64 and arm64 bundles for compatible Claude Desktop builds with a systemd user
session, glibc 2.35 or newer, and a native Secret Service. Confirm Linux client
availability before offering the Linux download. Windows ARM64 has no native
bundle.

Resolve the newest signed release with the universal Mac bundle and the
requested platform asset at delivery time. The universal bundle identifies
releases with this first-run flow. Verify the download against the release
checksum and Sigstore identity before serving it from the site with its
`.mcpb` filename intact. Verify with the `.sigstore` file on the release side
so the owner receives one file. Link to Anthropic’s install instructions next
to the button. [Anthropic’s extension installation instructions](https://support.claude.com/en/articles/10949351-getting-started-with-local-mcp-servers-on-claude-desktop)

Sigstore verifies release origin. Claude’s own publisher display depends on
working MCPB signature verification and a trusted signing certificate
([#134](https://github.com/Govcraft/beankeeper-desktop/issues/134)). OS signing
uses the Apple and Azure signing paths below. Confirm the actual install
messages in the Claude app before enabling the site download.

### Verify the chat-only walkthrough

In a disposable, standard user profile with only Claude Desktop installed,
download the site’s single file, double-click it, and click **Install**.
In Claude, say **Set up my books** and check that Claude reaches the first
bookkeeping question in the same chat. Say **Create demo books** and check
that Claude can show fictional accounts. Record any OS or Claude warning and
whether Claude had to reopen before it listed the tools. Repeat on each
platform offered by the site. The packaging smoke test covers the bundled
executable and the same setup and practice tools over stdio.

After the path ships, record the YouTube walkthrough on a clean Mac using
the released download. Show the download, double-click, Claude's **Install**
prompt, and **Create demo books** in chat. Then ask for a summary of the
practice accounts to show the result. Link the video near the download on
beankeeper.app.

Linux binaries are built natively in Ubuntu 22.04 containers on both Linux
runner architectures. The release verifies a glibc 2.35 ceiling, then installs
the resulting package on Ubuntu 22.04 and Debian 12. The published
0.1.0-rc.11 Linux binaries require glibc 2.39. Debian packages use `amd64` /
`arm64`, install `/usr/bin/bk`, and conflict with upstream CLI packages that
own that path. macOS packages are architecture
specific, install `/usr/local/bin/bk`, and replace an existing binary at that
path. Windows uses Inno Setup 6 on the Windows runner to create an x64, per-user
installer in `%LOCALAPPDATA%\Programs\Beankeeper Desktop`, with an uninstaller.
The installer does not change PATH. Windows ARM64 is not yet supported.
Installers register per-user background hosting by default, creating encrypted
local books only for an identifiable logged-in owner. Use `BK_NO_SERVICE=1`, the
shell `--no-service`, PowerShell `-NoService`, or EXE `/NOSERVICE=1` to opt out.
Package hooks never create root-owned books. Without a suitable login session,
they leave setup for `bk service install` after login. See [service lifecycle](service.md).

Encrypted startup uses upstream's resolved-passphrase router factory on all
three platforms. The service retrieves its stored credential without prompting.
No Unix-only passphrase descriptor is required for Windows hosting.

`checksums-sha256.txt` contains every final archive, installer and MCP bundle. The tag-release
job uses the existing [D13 approach](https://github.com/Govcraft/beankeeper/pull/108):
`cosign sign-blob` creates `ARTIFACT.sigstore` for every asset and the checksum
manifest, and `actions/attest-build-provenance` attests all those same files.
OS signing and stapling finish **before** archives, hashes, bundles and provenance
are produced. Sigstore bundles do not go into the checksum manifest themselves.

## Dry runs without credentials

Pull requests package the targets their changes affect: CI's change planner
calls the shared `packaging.yml` workflow with, for example, only Windows for an
`install.ps1` change or every target for a `scripts/release.py` change (see
[CI.md](CI.md)). The release workflow also qualifies `main` every night by
packaging all targets. All paid signing and publication are disabled for PRs,
nightly and manual runs. To dry-run every target on demand:

```sh
gh workflow run release.yml --ref main
```

This builds all OS archives and installers, validates Linux `.deb` files using
`dpkg-deb`, inspects macOS package payloads, and silently installs the Windows
EXE into a runner temporary directory. It also runs the offline installer tests.
Download the `release-assets` workflow artifact for all packages and the checksum
manifest. A dry run creates no GitHub Release, Sigstore signatures, attestations
or release tags. It requires no signing secrets.

A local Linux packaging smoke test, from the repository root:

```sh
cargo build --release --locked
python3 scripts/release.py stage --target x86_64-unknown-linux-gnu --binary target/release/bk
python3 scripts/release.py installer --target x86_64-unknown-linux-gnu
python3 scripts/release.py archive --target x86_64-unknown-linux-gnu
python3 scripts/release.py mcpb --target x86_64-unknown-linux-gnu
python3 scripts/release.py checksums
dpkg-deb --info dist/bk-0.1.0-x86_64-unknown-linux-gnu.deb
(cd dist && sha256sum --check checksums-sha256.txt)
python3 -B -m unittest discover -s scripts/tests -v
```

Use `aarch64-unknown-linux-gnu` on native ARM Linux. If `CARGO_TARGET_DIR` is set,
pass its binary path to `--binary`. Python 3.11+ is required; packaging uses only
the standard library. Native `.pkg` creation additionally needs Xcode command
line tools; Windows EXE creation needs Inno Setup 6. Windows builds select the
runner's native Strawberry Perl through `OPENSSL_SRC_PERL` to avoid Git Bash's
incompatible Perl when compiling vendored OpenSSL. See
[OpenSSL's Perl requirements](https://github.com/openssl/openssl/blob/master/NOTES-PERL.md).
No Node toolchain is used.

## Preparing a release PR

1. In an ordinary PR, summarize the user-facing change in one line under
   `## [Unreleased]` in `CHANGELOG.md`. Leave the workspace and package versions
   as they are.
2. In the PM's release PR, bump `[workspace.package].version` in `Cargo.toml`
   and update the workspace package entries in `Cargo.lock`. Move the accumulated
   notes into a dated `## [VERSION] - YYYY-MM-DD` section directly below a fresh
   `## [Unreleased]` section. PR CI checks that the first release section matches
   the workspace version.
3. After the release PR merges, publish from its merge commit by pushing
   `beankeeper-desktop-vVERSION`. Tag preflight checks that the tag, workspace
   version, and first release section agree before any package builds or
   publication. The release job uses GitHub OIDC for Sigstore and provenance.

## Optional Apple hook, disabled by default

Set repository variable `ENABLE_APPLE_SIGNING=true` **only after** account setup
and review of a real signing run. The hook runs only for tag pushes and requires
all the following repository secrets:

| Secret | Value |
| --- | --- |
| `APPLE_CERTIFICATES_P12_BASE64` | Base64 PKCS#12 export containing both Developer ID Application and Developer ID Installer certificates and their private keys |
| `APPLE_CERTIFICATES_PASSWORD` | Password protecting that PKCS#12 export |
| `APPLE_APPLICATION_IDENTITY` | Full Developer ID Application signing identity, including team ID |
| `APPLE_INSTALLER_IDENTITY` | Full Developer ID Installer signing identity, including team ID |
| `APPLE_NOTARY_KEY_BASE64` | Base64 App Store Connect notarization API `.p8` key |
| `APPLE_NOTARY_KEY_ID` | Notarization API key ID |
| `APPLE_NOTARY_ISSUER` | App Store Connect issuer ID for that key |

`scripts/sign-macos.sh` imports the identities into a temporary keychain, signs
`bk` with hardened runtime and a timestamp, rebuilds and signs the `.pkg`, submits
it with `notarytool --wait`, then staples and validates the ticket. It removes
the temporary keychain and key material on exit. See Apple's
[Developer ID distribution guidance](https://developer.apple.com/developer-id/).

If the opt-in variable is absent/false **or any secret is missing**, the script
prints `Apple signing/notarization skipped` with the reason and keeps the
unsigned package. No Apple tools or network calls are needed for the skip path.
When explicitly enabled with complete credentials, signing/notarization errors
fail the job, so a broken signing configuration cannot silently publish an
apparently signed package.

## Optional Azure hook, disabled by default

Set repository variable `ENABLE_AZURE_SIGNING=true` only after the Artifact
Signing account and certificate profile exist. The Windows job requires:

| Secret | Value |
| --- | --- |
| `AZURE_TENANT_ID` | Microsoft Entra tenant ID |
| `AZURE_CLIENT_ID` | Signing app registration client ID |
| `AZURE_CLIENT_SECRET` | App registration secret |
| `AZURE_SIGNING_ENDPOINT` | Regional Artifact Signing endpoint URL |
| `AZURE_SIGNING_ACCOUNT` | Artifact Signing account name |
| `AZURE_CERTIFICATE_PROFILE` | Public-trust certificate profile name |

The app registration needs the Artifact Signing Certificate Profile Signer role.
See the [official action](https://github.com/Azure/artifact-signing-action) and
[account setup guide](https://learn.microsoft.com/en-us/azure/artifact-signing/quickstart).
The hook signs `bk.exe` before ZIP/installer creation, then signs the finished
setup EXE. With the opt-in disabled or incomplete credentials, logs say
`Authenticode signing skipped` and both the ZIP and installer remain unsigned.
An explicitly enabled signing attempt with complete credentials fails on errors.

Sigstore proves release origin; it does not replace Apple notarization or
Authenticode. Until paid signing is enabled, browser-downloaded packages may
show Gatekeeper or SmartScreen warnings. Even signed Windows downloads can need
time to establish reputation. Do not disable OS security checks automatically.

## Checksum-verifying command-line installers

The shell installer supports Linux and macOS. The PowerShell installer
supports x64 Windows with PowerShell 5.1 or later. Install `cosign` first.

```sh
curl -fsSLO https://beankeeper.app/download/latest/install.sh
BK_REQUIRE_SIGNATURES=1 BK_REQUIRE_AGENT_SETUP=1 BK_INSTALL="$HOME/.local/bin" sh install.sh
```

The shell installer creates its destination directory, defaulting to
`~/.local/bin`. A system directory can require sudo for binary placement.
Service setup still runs as the invoking desktop user. On Windows, use:

```powershell
Invoke-WebRequest -UseBasicParsing -Uri 'https://beankeeper.app/download/latest/install.ps1' -OutFile install.ps1
./install.ps1 -RequireSignatures -RequireAgentSetup -InstallDir "$env:LOCALAPPDATA\Programs\Beankeeper Desktop"
```

Both scripts default to `https://beankeeper.app/download/`. They read
`latest/version.txt` and download the archive, checksum list and both bundles
from that version's directory. Set `BK_VERSION` without the tag prefix to
select a specific signed release and skip discovery.

Set `BK_DOWNLOAD_BASE` to use another HTTPS mirror with the same layout.
The override `https://github.com/Govcraft/beankeeper-desktop/releases/download`
uses GitHub tag directories. Without `BK_VERSION`, that override lists
published GitHub releases with bundles for the platform and checksum list.
Regular installs there prefer stable releases when available. Guided setup
selects the highest signed version. For GitHub discovery only, the shell can
read metadata with `jq`, macOS's built-in `osascript`, or `python3`. Both
installers can use `GH_TOKEN` or `GITHUB_TOKEN` for the GitHub release listing.

Before extraction, the installers verify the archive and
`checksums-sha256.txt` with their Sigstore bundles and check the archive's
SHA-256 against the signed list. The certificate identity is:

```text
https://github.com/Govcraft/beankeeper-desktop/.github/workflows/release.yml@refs/tags/beankeeper-desktop-vVERSION
```

The OIDC issuer must be `https://token.actions.githubusercontent.com`. A
missing verifier or bundle, a failed signature check, or a checksum mismatch
stops installation on every download base. The
[agent-led guide](../SKILL.md) also checks for managed service and agent
registration commands before replacing an existing binary.

## Signed update feed and self-update

After the final archives are hashed, `scripts/release.py update-manifest`
generates `update-manifest.json`. The tag job signs it with a detached
`update-manifest.json.sigstore` bundle and includes it in build provenance and
release assets. Dry runs produce unsigned metadata only. Schema version 1 is:

```json
{
  "schema_version": 1,
  "version": "0.1.0",
  "released_at": "2026-11-20T12:00:00Z",
  "artifacts": {
    "x86_64-unknown-linux-gnu": {"sha256": "<64 lowercase hex digits>"}
  }
}
```

`released_at` is the UTC timestamp of the release tag's source commit, emitted
from Git metadata by the release workflow. The current v1 license format uses
this signed timestamp for the inclusive update-window comparison, independently
of the client clock. The approved 1.0 license terms use major-version eligibility.
[Beankeeper Desktop license agreement](../LICENSE.md) · [Major-version eligibility issue](https://github.com/Govcraft/beankeeper-desktop/issues/169)

The artifacts map covers the available archive targets from the matrix. Archive
names remain `bk-VERSION-TARGET.tar.gz` or `.zip`. Self-update uses these archives.

Stable clients discover GitHub's latest stable release. RC clients select the
highest stable or `-rc.N` semantic version among the 100 most recent release
records. Both fetch the manifest and bundle from the selected exact tag, and
require the signed manifest version to match. RC manifests use the same schema,
archive layout and signature requirements as stable manifests. Stable clients
never install prereleases.

The client parses bounded metadata only to select the expected tag identity,
then runs `cosign verify-blob --bundle ... --certificate-identity ...
--certificate-oidc-issuer https://token.actions.githubusercontent.com`. The exact
identity is this repository's `release.yml@refs/tags/beankeeper-desktop-vVERSION`,
as shown above. It uses the [official cosign verification contract](https://docs.sigstore.dev/cosign/verifying/verify/).

`bk self-update` verifies the signed JSON, the archive bundle, the checksum-list
bundle, and matching SHA-256 values in both signed metadata and the checksum
list. A missing `.sigstore` file always fails closed. The public download URLs,
filenames, signer identity and issuer are fixed by the client; metadata cannot
redirect installation to another repository, workflow, executable path or OS.
Only after verification does the client extract the exact regular executable
member and replace its binary. See the [command reference](license-updates.md)
for opt-out, caching, trust provisioning and private license storage.
