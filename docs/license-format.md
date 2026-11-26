# Offline Desktop license contract

A Desktop license is a single PASETO v4.public token. Desktop verifies it
offline using an independently provisioned Ed25519 public key. Agent
authentication uses separate tokens. [Issuer contract](https://github.com/Govcraft/beankeeper-license/issues/12)

## Token and issuer keys

The token is `v4.public.<body>.<footer>`. Both segments use unpadded base64url.
The body holds the claims JSON and its Ed25519 signature. The clear footer
contains one JSON field, `kid`, for example `{"kid":"prod-2026"}`. The key ID
has 1 to 64 ASCII letters, digits, hyphens or underscores. A token may be at most
16,384 bytes. Text files may end in LF or CRLF.

Desktop uses `kid` to select a key from the local trust map. The map associates
key IDs with 32-byte Ed25519 public keys encoded as unpadded base64url. Desktop
authenticates the original payload and exact footer bytes with the selected key
and the implicit assertion `beankeeper-desktop`. An empty map or unknown key
fails verification. Rotation adds a key before the issuer uses it. Keep prior
keys available to verify previously issued licenses.

The issuer's public test token and key are under
`crates/beankeeper-desktop/tests/fixtures/`. Production trust is provisioned
separately. [Trust configuration and activation](license-updates.md)

## Authenticated claims

| Claim | Rule |
| --- | --- |
| `jti` | Canonical `lic_` TypeID. |
| `iat` | Required RFC 3339 issue time. |
| `nbf` | Optional RFC 3339 earliest use time. The issuer sets it to `iat` on new licenses. |
| `product` | `beankeeper-desktop`. |
| `storefront` | `polar`, `etsy`, `direct`, `stripe`, `comp`, or the reserved `gumroad`. |
| `purchaser_ref` | Opaque reference of 1 to 256 UTF-8 bytes with at least one non-whitespace character, no control characters and no `@`. |
| `eligible_major` | Positive unsigned 32-bit integer identifying the one covered major release line. |
| `seats` | The integer 1, the one-computer limit per license. Omission defaults to 1. |

Desktop rejects duplicate and unknown claims, including `exp`. Both timestamps
must fall in UTC years 1970 through 9999 and be at most five minutes ahead of the
local verifier clock. Each license covers all releases in its signed major
version for perpetual use. A 1.x license covers 1.0.0 and 1.4.2. A 2.x release
requires a 2.x license.

`license::verify_offline` authenticates a token supplied as text or bytes and
returns a `VerifiedLicense`. The original claims JSON is deserialized directly
into Rust types after signature verification. The verifier keeps the token
format, entitlement, and local activation separate from ledger operations.

## Activation and redemption

Paste the token or supply a plain-text `.license` file. Activation verifies the
token before atomically storing it by signed license ID. Status re-verifies
saved activations offline. If you have a file in the older JSON envelope format,
ask the issuer for a current license token.

Claim-code redemption is an explicit online action. The issuer returns the same
plain-text token used for offline activation. Desktop verifies it with the
provisioned keys before saving it. [Redemption and recovery](license-updates.md#redeem-a-key-or-claim-code)

## Signed revocations in releases

On the protected license host, run
`beankeeper-license --config /etc/beankeeper-license/config.toml export-revocations`
and save the signed output as `packaging/revocations.json` in the release PR.
Include a fresh signed snapshot for each official stable release, even if the
list is empty. Tagged builds embed that file into the binary. Packaging checks
the issuer signature against the embedded public keys. The private signing
keys stay on the license host.

Desktop verifies the exact `beankeeper-revocations-v1\n<key_id>\n<payload>`
message before reading the UTC generation time and the revoked license IDs.
Desktop checks the snapshot locally for ledger access and update eligibility.
