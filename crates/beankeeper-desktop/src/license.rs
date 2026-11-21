//! Offline Desktop entitlements, isolated from ledger and HTTP behavior.
//!
//! This boundary can be replaced without rewriting `bk serve`. Final Desktop
//! licensing terms remain Roland's decision. No signing secret or public test
//! verification key is trusted by default, and verification never phones home.

use std::{
    collections::BTreeMap,
    error::Error,
    fmt, fs,
    io::{self, Write},
    num::NonZeroU32,
    path::Path,
    str::FromStr,
};

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use chrono::{DateTime, Datelike, Utc};
use ed25519_dalek::{Signature, VerifyingKey};
use mti::prelude::MagicTypeId;
use secrecy::ExposeSecret;
use serde::{Deserialize, Serialize};

use crate::private_file::PrivateFile;

const FORMAT: &str = "beankeeper-license-v1";
const MAX_PAYLOAD: usize = 16_384;
const MAX_FILE: usize = 32_768;

/// A validated service-issued license TypeID.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LicenseId(MagicTypeId);

impl fmt::Display for LicenseId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, formatter)
    }
}

impl FromStr for LicenseId {
    type Err = LicenseError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let id = MagicTypeId::from_str(value).map_err(|_| LicenseError::InvalidClaims)?;
        if id.prefix().as_str() != "lic" || id != value {
            return Err(LicenseError::InvalidClaims);
        }
        Ok(Self(id))
    }
}

impl Serialize for LicenseId {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for LicenseId {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        String::deserialize(deserializer)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}

/// Authenticated entitlement fields from D10's license-file v1 contract.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LicenseClaims {
    /// Supported claims schema, exactly one.
    pub schema_version: u8,
    /// Canonical service-issued `lic_` TypeID.
    pub license_id: LicenseId,
    /// Consuming product, always `beankeeper-desktop` after verification.
    pub product: String,
    /// Opaque customer reference, never an email address.
    pub purchaser_ref: String,
    /// Storefront adapter slug.
    pub storefront: String,
    /// Issuance time, which may follow the original update deadline on reissue.
    pub issued_at: DateTime<Utc>,
    /// Last eligible release timestamp, inclusive. This does not expire use.
    pub updates_until: DateTime<Utc>,
    /// Contractual seats; no machine binding or activation counter is implied.
    pub seats: Option<NonZeroU32>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Envelope {
    format: String,
    key_id: String,
    payload: String,
    signature: String,
}

/// Explicit issuer public-key trust store. License content cannot add keys.
#[derive(Debug, Clone, Default)]
pub struct TrustedKeys(BTreeMap<String, VerifyingKey>);

impl TrustedKeys {
    /// Parse a JSON object mapping key identifiers to unpadded base64url keys.
    ///
    /// Production releases should provision this trust store independently of
    /// licenses. The public D10 fixture key is deliberately not embedded.
    ///
    /// # Errors
    /// Rejects malformed identifiers, invalid keys, and an empty trust store.
    pub fn from_json(bytes: &[u8]) -> Result<Self, LicenseError> {
        let encoded: BTreeMap<String, String> =
            serde_json::from_slice(bytes).map_err(|_| LicenseError::TrustConfiguration)?;
        if encoded.is_empty() {
            return Err(LicenseError::Unconfigured);
        }
        let keys = encoded
            .into_iter()
            .map(|(id, encoded)| {
                if !valid_key_id(&id) {
                    return Err(LicenseError::TrustConfiguration);
                }
                let bytes: [u8; 32] = decode(&encoded)?
                    .try_into()
                    .map_err(|_| LicenseError::TrustConfiguration)?;
                let key = VerifyingKey::from_bytes(&bytes)
                    .map_err(|_| LicenseError::TrustConfiguration)?;
                if key.is_weak() {
                    return Err(LicenseError::TrustConfiguration);
                }
                Ok((id, key))
            })
            .collect::<Result<_, _>>()?;
        Ok(Self(keys))
    }
}

/// A perpetual license's eligibility for one authenticated release timestamp.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Entitlement {
    /// An update deadline never disables an installed eligible release.
    pub perpetual_use: bool,
    /// Whether this release falls within the original update window.
    pub update_eligible: bool,
}

/// Evaluate authenticated claims against a trusted release date, never a clock.
#[must_use]
pub fn entitlement(claims: &LicenseClaims, released_at: DateTime<Utc>) -> Entitlement {
    Entitlement {
        perpetual_use: true,
        update_eligible: released_at <= claims.updates_until,
    }
}

/// Verify a license using only caller-provisioned public keys and local bytes.
///
/// # Errors
/// Rejects unsupported contracts, unauthenticated content, and invalid claims.
pub fn verify_offline(bytes: &[u8], keys: &TrustedKeys) -> Result<LicenseClaims, LicenseError> {
    if keys.0.is_empty() {
        return Err(LicenseError::Unconfigured);
    }
    if bytes.len() > MAX_FILE {
        return Err(LicenseError::TooLarge);
    }
    let envelope: Envelope =
        serde_json::from_slice(bytes).map_err(|_| LicenseError::InvalidEnvelope)?;
    if envelope.format != FORMAT || !valid_key_id(&envelope.key_id) {
        return Err(LicenseError::InvalidEnvelope);
    }
    let key = keys
        .0
        .get(&envelope.key_id)
        .ok_or(LicenseError::UnknownKey)?;
    if envelope.payload.len() > MAX_PAYLOAD || envelope.signature.len() != 86 {
        return Err(LicenseError::InvalidEnvelope);
    }
    let signature = Signature::from_slice(&decode(&envelope.signature)?)
        .map_err(|_| LicenseError::Signature)?;
    let message = format!("{FORMAT}\n{}\n{}", envelope.key_id, envelope.payload);
    key.verify_strict(message.as_bytes(), &signature)
        .map_err(|_| LicenseError::Signature)?;
    // Never decode or reserialize claims until the exact payload is authenticated.
    let claims: LicenseClaims = serde_json::from_slice(&decode(&envelope.payload)?)
        .map_err(|_| LicenseError::InvalidClaims)?;
    validate_claims(&claims)?;
    Ok(claims)
}

/// Verify before atomically replacing a private activated license file.
///
/// # Errors
/// Returns verification or filesystem errors. Failed verification preserves the
/// previous license. Existing symbolic links and public files are refused.
pub fn activate(
    bytes: &[u8],
    keys: &TrustedKeys,
    path: &Path,
) -> Result<LicenseClaims, LicenseError> {
    let claims = verify_offline(bytes, keys)?;
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    if !parent.exists() {
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(parent)?;
        }
        #[cfg(not(unix))]
        fs::create_dir_all(parent)?;
        crate::setup::keychain::restrict_directory(parent)?;
    }
    match PrivateFile::open(path) {
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    let mut pending = tempfile::NamedTempFile::new_in(parent)?;
    crate::setup::keychain::restrict_file(pending.path())?;
    pending.write_all(bytes)?;
    pending.as_file().sync_all()?;
    pending.persist(path).map_err(|error| error.error)?;
    Ok(claims)
}

/// Read and reverify the local activation; no network request is performed.
///
/// # Errors
/// Refuses insecure files, malformed content, and licenses no longer trusted.
pub fn load(path: &Path, keys: &TrustedKeys) -> Result<Option<LicenseClaims>, LicenseError> {
    let file = match PrivateFile::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let bytes = file.read_bytes(MAX_FILE).map_err(|error| {
        if error.kind() == io::ErrorKind::FileTooLarge {
            LicenseError::TooLarge
        } else {
            error.into()
        }
    })?;
    verify_offline(bytes.expose_secret(), keys).map(Some)
}

fn valid_key_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
}

fn valid_slug(value: &str, limit: usize) -> bool {
    !value.is_empty()
        && value.len() <= limit
        && value.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-' || byte == b'_'
        })
}

fn validate_claims(claims: &LicenseClaims) -> Result<(), LicenseError> {
    if claims.schema_version != 1
        || claims.product != "beankeeper-desktop"
        || !valid_slug(&claims.storefront, 32)
        || claims.purchaser_ref.is_empty()
        || claims.purchaser_ref.len() > 256
        || claims.purchaser_ref.chars().any(char::is_control)
        || claims.purchaser_ref.contains('@')
        || !(1970..=9999).contains(&claims.issued_at.year())
        || !(1970..=9999).contains(&claims.updates_until.year())
    {
        return Err(LicenseError::InvalidClaims);
    }
    Ok(())
}

fn decode(value: &str) -> Result<Vec<u8>, LicenseError> {
    let bytes = URL_SAFE_NO_PAD
        .decode(value)
        .map_err(|_| LicenseError::Encoding)?;
    if URL_SAFE_NO_PAD.encode(&bytes) != value {
        return Err(LicenseError::Encoding);
    }
    Ok(bytes)
}

/// License failures with no secret-bearing platform or claim diagnostics.
#[derive(Debug)]
pub enum LicenseError {
    /// Production issuer trust has not been provisioned.
    Unconfigured,
    /// The trusted public-key file is malformed.
    TrustConfiguration,
    /// The file exceeds the allocation bound.
    TooLarge,
    /// Unsupported or malformed envelope.
    InvalidEnvelope,
    /// The license selects an issuer the client does not trust.
    UnknownKey,
    /// Signature verification failed.
    Signature,
    /// Noncanonical base64url encoding.
    Encoding,
    /// Authenticated claims violate the D10 contract or target another product.
    InvalidClaims,
    /// Local activation storage failed.
    Io(io::Error),
}

impl fmt::Display for LicenseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Unconfigured => "license issuer public keys are not configured",
            Self::TrustConfiguration => "invalid trusted license public-key configuration",
            Self::TooLarge => "license file exceeds 32768 bytes",
            Self::InvalidEnvelope => "invalid or unsupported license envelope",
            Self::UnknownKey => "license issuer is not in the trusted public-key store",
            Self::Signature => "invalid license signature",
            Self::Encoding => "invalid canonical base64url license encoding",
            Self::InvalidClaims => "invalid license claims or wrong product",
            Self::Io(_) => "cannot read or save the private activated license file",
        })
    }
}

impl Error for LicenseError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            _ => None,
        }
    }
}

impl From<io::Error> for LicenseError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};
    use serde_json::{Value, json};

    // Independently generated D10 published vector, not a production trust key.
    const FIXTURE: &str = include_str!("../tests/fixtures/license.json");

    fn keys() -> TrustedKeys {
        TrustedKeys::from_json(include_bytes!("../tests/fixtures/license-public-keys.json"))
            .expect("fixture public key")
    }

    fn sign_payload(payload: &[u8]) -> Vec<u8> {
        let payload = URL_SAFE_NO_PAD.encode(payload);
        let key = SigningKey::from_bytes(&[7; 32]);
        let message = format!("{FORMAT}\ntest-2026\n{payload}");
        let signature = URL_SAFE_NO_PAD.encode(key.sign(message.as_bytes()).to_bytes());
        serde_json::to_vec(&json!({
            "format": FORMAT, "key_id":"test-2026", "payload":payload,
            "signature":signature
        }))
        .expect("fixture envelope")
    }

    #[test]
    fn published_fixture_activates_offline_and_update_deadline_is_inclusive()
    -> Result<(), Box<dyn Error>> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("private/license.json");
        assert!(load(&path, &keys())?.is_none());
        let claims = activate(FIXTURE.as_bytes(), &keys(), &path)?;
        assert_eq!(load(&path, &keys())?, Some(claims.clone()));
        assert_eq!(
            claims.license_id.to_string(),
            "lic_01h455vb4pex5vsknk084sn02q"
        );
        assert!(entitlement(&claims, claims.updates_until).update_eligible);
        let expired = entitlement(&claims, "2030-01-01T00:00:00Z".parse()?);
        assert!(expired.perpetual_use);
        assert!(!expired.update_eligible);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(fs::metadata(path)?.permissions().mode() & 0o777, 0o600);
        }
        Ok(())
    }

    #[test]
    fn tampered_and_wrong_key_rejected_without_replacing_activation() -> Result<(), Box<dyn Error>>
    {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("license.json");
        activate(FIXTURE.as_bytes(), &keys(), &path)?;
        let wrong = SigningKey::from_bytes(&[8; 32]);
        let wrong_keys = TrustedKeys::from_json(&serde_json::to_vec(&json!({
            "test-2026":URL_SAFE_NO_PAD.encode(wrong.verifying_key().to_bytes())
        }))?)?;
        assert!(verify_offline(FIXTURE.as_bytes(), &wrong_keys).is_err());
        let tampered = FIXTURE.replace("AHHH0LU", "BHHH0LU");
        assert!(activate(tampered.as_bytes(), &keys(), &path).is_err());
        assert_eq!(fs::read_to_string(&path)?, FIXTURE);
        assert!(verify_offline(FIXTURE.as_bytes(), &TrustedKeys::default()).is_err());
        Ok(())
    }

    #[test]
    fn authenticated_invalid_claims_and_duplicate_fields_are_rejected() -> Result<(), Box<dyn Error>>
    {
        let fixture: Envelope = serde_json::from_str(FIXTURE)?;
        let valid: Value = serde_json::from_slice(&decode(&fixture.payload)?)?;
        for changes in [
            json!({"schema_version":2}),
            json!({"product":"another-product"}),
            json!({"license_id":"user_01h455vb4pex5vsknk084sn02q"}),
            json!({"purchaser_ref":"person@example.com"}),
            json!({"storefront":"POLAR"}),
            json!({"seats":0}),
            json!({"seats":4294967296_u64}),
            json!({"issued_at":"1969-12-31T23:59:59Z"}),
            json!({"unexpected":true}),
        ] {
            let mut claims = valid.clone();
            for (name, value) in changes.as_object().ok_or("fixture object")? {
                claims[name] = value.clone();
            }
            assert!(verify_offline(&sign_payload(&serde_json::to_vec(&claims)?), &keys()).is_err());
        }
        let duplicated = String::from_utf8(decode(&fixture.payload)?)?.replacen(
            '{',
            "{\"schema_version\":1,",
            1,
        );
        assert!(verify_offline(&sign_payload(duplicated.as_bytes()), &keys()).is_err());
        let mut reissued = valid;
        reissued["updates_until"] = json!("2020-01-01T00:00:00Z");
        assert!(verify_offline(&sign_payload(&serde_json::to_vec(&reissued)?), &keys()).is_ok());
        Ok(())
    }

    #[test]
    fn rejects_envelope_extensions_unknown_keys_encodings_and_size() -> Result<(), Box<dyn Error>> {
        let valid: Value = serde_json::from_str(FIXTURE)?;
        for change in [
            json!({"format":"v2"}),
            json!({"key_id":"unknown"}),
            json!({"public_key":"untrusted"}),
            json!({"signature":"+".repeat(86)}),
            json!({"payload":"a".repeat(MAX_PAYLOAD + 1)}),
        ] {
            let mut envelope = valid.clone();
            for (name, value) in change.as_object().ok_or("fixture object")? {
                envelope[name] = value.clone();
            }
            assert!(verify_offline(&serde_json::to_vec(&envelope)?, &keys()).is_err());
        }
        assert!(verify_offline(&vec![b' '; MAX_FILE + 1], &keys()).is_err());
        assert!(TrustedKeys::from_json(b"{}").is_err());
        assert!(decode("AA=").is_err());
        assert!(decode("AB").is_err());
        Ok(())
    }

    #[test]
    fn signature_authenticates_key_alias_and_precedes_payload_decode() -> Result<(), Box<dyn Error>>
    {
        let mut envelope: Value = serde_json::from_str(FIXTURE)?;
        envelope["payload"] = json!("not base64url");
        assert!(matches!(
            verify_offline(&serde_json::to_vec(&envelope)?, &keys()),
            Err(LicenseError::Signature)
        ));
        let aliases =
            TrustedKeys::from_json(br#"{"alias":"6kpsY-KcUgq-9VB7Ey7F-ZVHdq6-vnuSQh7qaRRG0iw"}"#)?;
        let renamed = FIXTURE.replace("test-2026", "alias");
        assert!(matches!(
            verify_offline(renamed.as_bytes(), &aliases),
            Err(LicenseError::Signature)
        ));
        let duplicated = FIXTURE.replacen('{', "{\"format\":\"beankeeper-license-v1\",", 1);
        assert!(matches!(
            verify_offline(duplicated.as_bytes(), &keys()),
            Err(LicenseError::InvalidEnvelope)
        ));
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn storage_refuses_public_and_symlink_files() -> Result<(), Box<dyn Error>> {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let directory = tempfile::tempdir()?;
        let target = directory.path().join("target");
        fs::write(&target, "preserve")?;
        let path = directory.path().join("license.json");
        symlink(&target, &path)?;
        assert!(activate(FIXTURE.as_bytes(), &keys(), &path).is_err());
        assert!(load(&path, &keys()).is_err());
        assert_eq!(fs::read_to_string(target)?, "preserve");
        fs::remove_file(&path)?;
        fs::write(&path, FIXTURE)?;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644))?;
        assert!(load(&path, &keys()).is_err());
        assert!(activate(FIXTURE.as_bytes(), &keys(), &path).is_err());
        Ok(())
    }
}
