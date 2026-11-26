# Offline license contract

Desktop consumes claims schema 2 in the issuer's `beankeeper-license-v1`
Ed25519 envelope. Agent authentication uses separate PASETO bearer tokens.
No license, activation or update operation changes the ledger schema.
[Issuer's license format](https://github.com/Govcraft/beankeeper-license/blob/main/docs/license-format.md)

## Envelope and exact signed bytes

The JSON envelope has exactly four string fields. Unknown and duplicate fields
are rejected:

```json
{
  "format": "beankeeper-license-v1",
  "key_id": "issuer-2026",
  "payload": "<base64url of exact claims JSON bytes, without padding>",
  "signature": "<base64url of 64 Ed25519 signature bytes, without padding>"
}
```

Plain Ed25519 signs the UTF-8 bytes of `format`, LF, `key_id`, LF, `payload`,
with no trailing newline. The exact signing message is:

```text
beankeeper-license-v1\n<key_id>\n<payload>
```

Here `\n` means a single LF byte. Outer whitespace and field order do not affect
verification. The payload string is limited to 16,384 bytes; Desktop also bounds
the complete envelope at 32,768 bytes. Both encodings require the RFC 4648
URL-safe alphabet, no padding, and canonical trailing bits. The verifier checks
the signature before decoding claims; reserializing claims before verification
would change the signed contract.

Key identifiers have 1 to 64 ASCII letters, digits, underscores or hyphens.
Desktop selects the identifier from a separately provisioned public-key trust
store, never from a public key or URL supplied in the license. A trust-store JSON
object maps each exact identifier to a canonical unpadded base64url encoding of
its 32-byte Ed25519 public key. An empty trust store fails verification.

## Authenticated claims

```json
{
  "schema_version": 2,
  "license_id": "lic_01h455vb4pex5vsknk084sn02q",
  "product": "beankeeper-desktop",
  "purchaser_ref": "customer-fixture-1",
  "storefront": "polar",
  "issued_at": "2026-11-20T12:00:00Z",
  "eligible_major": 1,
  "seats": 1
}
```

The verifier rejects unsupported schema versions, duplicate and unknown fields,
a noncanonical `lic_` TypeID, and products other than `beankeeper-desktop`.
`purchaser_ref` is an opaque nonempty reference with at most 256 UTF-8 bytes,
without control characters or `@`; it is never an email address. `storefront` has
1 to 32 lowercase ASCII letters, digits, underscores or hyphens. Timestamps are
RFC 3339, with UTC year 1970 through 9999. The service normalizes timestamps to
UTC. `eligible_major` is a JSON integer from 1 through 4,294,967,295 inclusive.
`seats` is the integer 1, the contractual limit of one computer per license.
The signed file carries no device or ledger identifier.

Use is perpetual. A release is covered when the major version in its signed
manifest equals `eligible_major`. A major-1 license covers 1.0.0, 1.4.2 and
1.99.0-rc.1 for perpetual use. Verifying coverage requires local license bytes,
the issuer's public key and signed release metadata. Desktop rejects schema 1
files. Ask for a schema 2 file if you still have a schema 1 test license.

The service's public fixture key, ID `test-2026`, is:

```text
6kpsY-KcUgq-9VB7Ey7F-ZVHdq6-vnuSQh7qaRRG0iw
```

It comes from a public seed of 32 bytes equal to `0x07`. Desktop's tests verify
D10's independently generated [license-v2.json fixture](https://github.com/Govcraft/beankeeper-license/blob/main/tests/fixtures/license-v2.json).
This fixture key is **not trusted by default**. Production builds must provision
production public keys separately; signing secrets never ship in Desktop.
Rotation introduces a new identifier and public key before the service uses it.
Retain previous public keys to keep old perpetual licenses verifiable. Removing
a key deliberately rejects its previously issued files. Offline files cannot be
recalled by a server-side revocation; revocation blocks future downloads only.

## Isolation and storage

`crates/beankeeper-desktop/src/license.rs` exposes `verify_offline`, `activate`, `load`, and `entitlement`.
The server and ledger code do not implement license signatures or policy.
Activation verifies before atomically saving a private local file keyed by the
signed license ID. Failed verification preserves existing activations. Desktop
refuses symbolic links and publicly accessible license files. It re-verifies
the local files without an HTTP request. [License and update commands](license-updates.md)

## Optional redemption

D10 exposes `POST /api/v1/licenses/redeem` with JSON fields `storefront`, `key`,
and `recovery_token`. An Etsy claim code is single-use. Retain the recovery token
before redeeming; a retry after a lost response may already have spent the code.
`POST /api/v1/licenses/recover` returns the original file using that private
recovery token. Activation always verifies the returned license independently
before storing it. Network redemption is an explicit activation operation;
verification and normal Desktop use remain offline.
