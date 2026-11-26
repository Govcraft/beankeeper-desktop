//! Offline Desktop entitlements, isolated from ledger and HTTP behavior.
//!
//! Perpetual PASETO v4.public licenses cover one signed release major.
//! Verification uses a local token and an independently provisioned public key.

use std::{
    collections::BTreeMap,
    error::Error,
    fmt, fs,
    io::{self, Write},
    num::NonZeroU32,
    path::{Path, PathBuf},
    str::FromStr,
};

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use chrono::{DateTime, Datelike, TimeDelta, Utc};
use ed25519_dalek::VerifyingKey;
use mti::prelude::MagicTypeId;
use rusty_paseto::{
    Error as PasetoError,
    core::{Footer, ImplicitAssertion, Key, Paseto, PasetoAsymmetricPublicKey, Public, V4},
};
use secrecy::ExposeSecret;
use serde::{Deserialize, Serialize};

use crate::private_file::PrivateFile;

const TOKEN_PREFIX: &str = "v4.public.";
const MAX_TOKEN_LEN: usize = 16_384;
// Text files may end with LF or CRLF.
pub(crate) const MAX_LICENSE_INPUT: usize = MAX_TOKEN_LEN + 2;
const ACTIVATIONS_DIR: &str = "licenses";

/// A validated service-issued license `TypeID`.
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

/// The positive major version covered by a Desktop license.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(try_from = "u64")]
pub struct MajorVersion(NonZeroU32);

impl TryFrom<u64> for MajorVersion {
    type Error = LicenseError;

    fn try_from(value: u64) -> Result<Self, Self::Error> {
        u32::try_from(value)
            .ok()
            .and_then(NonZeroU32::new)
            .map(Self)
            .ok_or(LicenseError::InvalidClaims)
    }
}

impl fmt::Display for MajorVersion {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(try_from = "u8")]
enum SeatCount {
    #[default]
    OneComputer,
}

impl TryFrom<u8> for SeatCount {
    type Error = LicenseError;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            1 => Ok(Self::OneComputer),
            _ => Err(LicenseError::InvalidClaims),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
enum Product {
    #[serde(rename = "beankeeper-desktop")]
    Desktop,
}

impl AsRef<str> for Product {
    fn as_ref(&self) -> &str {
        match self {
            Self::Desktop => "beankeeper-desktop",
        }
    }
}

/// The signed provenance of a purchase or grant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Storefront {
    /// Polar purchase.
    Polar,
    /// Etsy purchase.
    Etsy,
    /// Operator-issued license.
    Direct,
    /// Stripe purchase.
    Stripe,
    /// Complimentary grant.
    Comp,
    /// Reserved by the issuer for Gumroad.
    Gumroad,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(try_from = "String")]
struct LicenseTime(DateTime<Utc>);

impl TryFrom<String> for LicenseTime {
    type Error = LicenseError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        let time = DateTime::parse_from_rfc3339(&value)
            .map_err(|_| LicenseError::InvalidClaims)?
            .to_utc();
        if !(1970..=9999).contains(&time.year()) {
            return Err(LicenseError::InvalidClaims);
        }
        Ok(Self(time))
    }
}

impl LicenseTime {
    const CLOCK_SKEW: TimeDelta = TimeDelta::minutes(5);

    fn check_clock(self, now: DateTime<Utc>) -> Result<(), LicenseError> {
        if self.0.signed_duration_since(now) > Self::CLOCK_SKEW {
            return Err(LicenseError::InvalidClaims);
        }
        Ok(())
    }
}

// Deserialize the signed bytes directly. Serde rejects duplicate or unknown claims.
#[derive(Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
struct LicenseClaims {
    #[serde(rename = "jti")]
    license_id: LicenseId,
    product: Product,
    purchaser_ref: PurchaserRef,
    storefront: Storefront,
    #[serde(rename = "iat")]
    issued_at: LicenseTime,
    #[serde(rename = "nbf")]
    not_before: Option<LicenseTime>,
    eligible_major: MajorVersion,
    #[serde(default)]
    seats: SeatCount,
}

impl LicenseClaims {
    fn check_clock(&self, now: DateTime<Utc>) -> Result<(), LicenseError> {
        self.issued_at.check_clock(now)?;
        if let Some(not_before) = self.not_before {
            not_before.check_clock(now)?;
        }
        Ok(())
    }
}

/// A perpetual Desktop license authenticated by a configured issuer.
#[derive(Clone, PartialEq, Eq)]
pub struct VerifiedLicense(LicenseClaims);

impl fmt::Debug for VerifiedLicense {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("VerifiedLicense")
            .field("id", self.id())
            .field("eligible_major", &self.eligible_major())
            .finish_non_exhaustive()
    }
}

impl VerifiedLicense {
    /// Canonical service-issued identifier.
    #[must_use]
    pub fn id(&self) -> &LicenseId {
        &self.0.license_id
    }

    /// The major version covered for perpetual use and updates.
    #[must_use]
    pub fn eligible_major(&self) -> MajorVersion {
        self.0.eligible_major
    }

    /// Signed source of the purchase or grant.
    #[must_use]
    pub fn storefront(&self) -> Storefront {
        self.0.storefront
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(try_from = "String")]
struct PurchaserRef(String);

impl TryFrom<String> for PurchaserRef {
    type Error = LicenseError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        if value.trim().is_empty()
            || value.len() > 256
            || value.chars().any(char::is_control)
            || value.contains('@')
        {
            return Err(LicenseError::InvalidClaims);
        }
        Ok(Self(value))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Deserialize)]
#[serde(try_from = "String")]
struct KeyId(String);

impl TryFrom<String> for KeyId {
    type Error = LicenseError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        if !(1..=64).contains(&value.len())
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
        {
            return Err(LicenseError::InvalidFooter);
        }
        Ok(Self(value))
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct KeyFooter {
    kid: KeyId,
}

struct TokenFooter {
    json: String,
    kid: KeyId,
}

impl TryFrom<&str> for TokenFooter {
    type Error = LicenseError;

    fn try_from(encoded: &str) -> Result<Self, Self::Error> {
        let bytes = URL_SAFE_NO_PAD
            .decode(encoded)
            .map_err(|_| LicenseError::InvalidFooter)?;
        let json = String::from_utf8(bytes).map_err(|_| LicenseError::InvalidFooter)?;
        let KeyFooter { kid } =
            serde_json::from_str(&json).map_err(|_| LicenseError::InvalidFooter)?;
        Ok(Self { json, kid })
    }
}

struct UnverifiedToken<'a> {
    text: &'a str,
    footer: TokenFooter,
}

impl<'a> TryFrom<&'a [u8]> for UnverifiedToken<'a> {
    type Error = LicenseError;

    fn try_from(value: &'a [u8]) -> Result<Self, Self::Error> {
        if value.len() > MAX_LICENSE_INPUT {
            return Err(LicenseError::TooLarge);
        }
        let text = str::from_utf8(value).map_err(|_| LicenseError::InvalidToken)?;
        let text = text
            .strip_suffix("\r\n")
            .or_else(|| text.strip_suffix('\n'))
            .unwrap_or(text);
        if text.len() > MAX_TOKEN_LEN {
            return Err(LicenseError::TooLarge);
        }
        let parts = text
            .strip_prefix(TOKEN_PREFIX)
            .ok_or(LicenseError::InvalidToken)?;
        let (_, footer) = parts.split_once('.').ok_or(LicenseError::InvalidFooter)?;
        Ok(Self {
            text,
            footer: footer.try_into()?,
        })
    }
}

impl UnverifiedToken<'_> {
    fn verify(
        self,
        keys: &TrustedKeys,
        now: DateTime<Utc>,
    ) -> Result<VerifiedLicense, LicenseError> {
        let key = keys
            .0
            .get(&self.footer.kid)
            .ok_or(LicenseError::UnknownKey)?;
        let key = Key::from(key.0.to_bytes());
        // The prelude parser goes through a JSON Value, which collapses duplicate
        // claims. Use core verification to deserialize the original JSON once.
        let payload = Paseto::<V4, Public>::try_verify(
            self.text,
            &PasetoAsymmetricPublicKey::from(&key),
            Footer::from(self.footer.json.as_str()),
            ImplicitAssertion::from(Product::Desktop.as_ref()),
        )
        .map_err(|error| match PasetoError::from(error) {
            PasetoError::InvalidTokenStructure | PasetoError::Base64Decode(_) => {
                LicenseError::InvalidToken
            }
            PasetoError::FooterTooLarge => LicenseError::InvalidFooter,
            PasetoError::FromUtf8(_) => LicenseError::InvalidClaims,
            _ => LicenseError::Signature,
        })?;
        let claims: LicenseClaims =
            serde_json::from_str(&payload).map_err(|_| LicenseError::InvalidClaims)?;
        claims.check_clock(now)?;
        Ok(VerifiedLicense(claims))
    }
}

/// Explicit issuer public-key trust store. License content cannot add keys.
#[derive(Debug, Clone)]
pub struct TrustedKeys(BTreeMap<KeyId, IssuerPublicKey>);

#[derive(Debug, Clone, Deserialize)]
#[serde(try_from = "String")]
struct IssuerPublicKey(VerifyingKey);

impl TryFrom<String> for IssuerPublicKey {
    type Error = LicenseError;

    fn try_from(encoded: String) -> Result<Self, Self::Error> {
        let bytes: [u8; 32] = URL_SAFE_NO_PAD
            .decode(encoded)
            .ok()
            .and_then(|bytes| bytes.try_into().ok())
            .ok_or(LicenseError::TrustConfiguration)?;
        let key = VerifyingKey::from_bytes(&bytes).map_err(|_| LicenseError::TrustConfiguration)?;
        if key.is_weak() {
            return Err(LicenseError::TrustConfiguration);
        }
        Ok(Self(key))
    }
}

impl TrustedKeys {
    /// Parse a JSON object mapping key identifiers to unpadded base64url keys.
    ///
    /// # Errors
    /// Rejects malformed identifiers, invalid keys, and an empty trust store.
    pub fn from_json(bytes: &[u8]) -> Result<Self, LicenseError> {
        let keys: BTreeMap<KeyId, IssuerPublicKey> =
            serde_json::from_slice(bytes).map_err(|_| LicenseError::TrustConfiguration)?;
        if keys.is_empty() {
            return Err(LicenseError::Unconfigured);
        }
        Ok(Self(keys))
    }
}

/// Whether a verified license covers the given release major.
#[must_use]
pub fn entitlement(license: &VerifiedLicense, release_major: MajorVersion) -> bool {
    license.eligible_major() == release_major
}

/// Verify a PASETO v4.public license offline and return its authenticated claims.
///
/// Accepts a pasted token or the bytes read from a saved license.
///
/// # Errors
/// Rejects unsupported tokens, unauthenticated content, and invalid claims.
pub fn verify_offline(
    token: impl AsRef<[u8]>,
    keys: &TrustedKeys,
) -> Result<VerifiedLicense, LicenseError> {
    UnverifiedToken::try_from(token.as_ref())?.verify(keys, Utc::now())
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
) -> Result<VerifiedLicense, LicenseError> {
    let claims = verify_offline(bytes, keys)?;
    store_activation(bytes, path)?;
    Ok(claims)
}

/// Verify and store this activation alongside any other activated licenses.
pub(crate) fn activate_in(
    bytes: &[u8],
    keys: &TrustedKeys,
    state: &Path,
) -> Result<VerifiedLicense, LicenseError> {
    let claims = verify_offline(bytes, keys)?;
    let path = state
        .join(ACTIVATIONS_DIR)
        .join(format!("{}.license", claims.id()));
    store_activation(bytes, &path)?;
    Ok(claims)
}

/// Discover activations, including the legacy single-file location.
pub(crate) fn activation_paths(state: &Path) -> Result<Vec<PathBuf>, LicenseError> {
    let mut paths = Vec::new();
    match fs::read_dir(state.join(ACTIVATIONS_DIR)) {
        Ok(entries) => {
            for entry in entries {
                let path = entry?.path();
                if path
                    .extension()
                    .is_some_and(|extension| extension == "license" || extension == "json")
                {
                    paths.push(path);
                }
            }
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    paths.sort_unstable();
    let legacy = state.join("license.json");
    match fs::symlink_metadata(&legacy) {
        Ok(_) => paths.push(legacy),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    Ok(paths)
}

fn store_activation(bytes: &[u8], path: &Path) -> Result<(), LicenseError> {
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
    Ok(())
}

/// Read and reverify the local activation; no network request is performed.
///
/// # Errors
/// Refuses insecure files, malformed content, and licenses no longer trusted.
pub fn load(path: &Path, keys: &TrustedKeys) -> Result<Option<VerifiedLicense>, LicenseError> {
    let file = match PrivateFile::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let bytes = file.read_bytes(MAX_LICENSE_INPUT).map_err(|error| {
        if error.kind() == io::ErrorKind::FileTooLarge {
            LicenseError::TooLarge
        } else {
            error.into()
        }
    })?;
    verify_offline(bytes.expose_secret(), keys).map(Some)
}

/// License failures with no secret-bearing platform or claim diagnostics.
#[derive(Debug)]
pub enum LicenseError {
    /// Production issuer trust has not been provisioned.
    Unconfigured,
    /// The trusted public-key file is malformed.
    TrustConfiguration,
    /// The token exceeds the allocation bound.
    TooLarge,
    /// Unsupported or malformed PASETO token.
    InvalidToken,
    /// The token has a missing or malformed key selector.
    InvalidFooter,
    /// The license selects an issuer the client does not trust.
    UnknownKey,
    /// Signature verification failed.
    Signature,
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
            Self::TooLarge => "license token exceeds 16384 bytes",
            Self::InvalidToken => "expected a Desktop license token starting with v4.public. Ask the issuer for a current license if you have an older file",
            Self::InvalidFooter => "invalid or missing license key identifier",
            Self::UnknownKey => "license issuer is not in the trusted public-key store",
            Self::Signature => "invalid license signature",
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
    use ed25519_dalek::SigningKey;
    use rusty_paseto::core::{PasetoAsymmetricPrivateKey, Payload};
    use serde_json::{Value, json};

    const FIXTURE: &str = include_str!("../tests/fixtures/license-v4.public.txt");
    const LEGACY_ENVELOPE: &str = r#"{"format":"beankeeper-license-v1","key_id":"test-2026","payload":"e30","signature":"old"}"#;
    const FOOTER: &str = r#"{"kid":"test-2026"}"#;

    type TestResult = Result<(), Box<dyn Error>>;

    fn keys() -> Result<TrustedKeys, LicenseError> {
        TrustedKeys::from_json(include_bytes!("../tests/fixtures/license-public-keys.json"))
    }

    struct TestIssuer(SigningKey);

    impl TestIssuer {
        fn new() -> Result<Self, Box<dyn Error>> {
            let mut seed = [0; 32];
            getrandom::fill(&mut seed)?;
            Ok(Self(SigningKey::from_bytes(&seed)))
        }

        fn keys(&self) -> Result<TrustedKeys, Box<dyn Error>> {
            let key = URL_SAFE_NO_PAD.encode(self.0.verifying_key().to_bytes());
            Ok(TrustedKeys::from_json(&serde_json::to_vec(&json!({
                "test-2026": key,
                "alias": key,
            }))?)?)
        }

        fn sign(
            &self,
            payload: &str,
            footer: &str,
            assertion: Option<&str>,
        ) -> Result<String, PasetoError> {
            let key = Key::from(self.0.to_keypair_bytes());
            let mut token = Paseto::<V4, Public>::default();
            token
                .set_payload(Payload::from(payload))
                .set_footer(Footer::from(footer));
            if let Some(assertion) = assertion {
                token.set_implicit_assertion(ImplicitAssertion::from(assertion));
            }
            Ok(token.try_sign(&PasetoAsymmetricPrivateKey::from(&key))?)
        }

        fn sign_claims(&self, payload: &str) -> Result<String, PasetoError> {
            self.sign(payload, FOOTER, Some(Product::Desktop.as_ref()))
        }
    }

    fn valid_claims() -> Value {
        json!({
            "jti": "lic_01h455vb4pex5vsknk084sn02q",
            "iat": "2026-11-20T12:00:00Z",
            "product": "beankeeper-desktop",
            "purchaser_ref": "customer-test",
            "storefront": "polar",
            "eligible_major": 1
        })
    }

    #[test]
    fn published_fixture_activates_offline_with_perpetual_major_coverage() -> TestResult {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("private/license.json");
        assert!(load(&path, &keys()?)?.is_none());
        let claims = activate(FIXTURE.as_bytes(), &keys()?, &path)?;
        assert_eq!(load(&path, &keys()?)?, Some(claims.clone()));
        assert_eq!(claims.id().to_string(), "lic_01h455vb4pex5vsknk084sn02q");
        let windows_file = format!("{}\r\n", FIXTURE.trim_end());
        assert_eq!(verify_offline(windows_file, &keys()?)?, claims);
        assert!(entitlement(&claims, MajorVersion::try_from(1)?));
        assert!(!entitlement(&claims, MajorVersion::try_from(2)?));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(fs::metadata(path)?.permissions().mode() & 0o777, 0o600);
        }
        Ok(())
    }

    #[test]
    fn invalid_tokens_do_not_replace_an_activation() -> TestResult {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("saved.license");
        let key = keys()?;
        activate(FIXTURE.as_bytes(), &key, &path)?;
        let wrong_keys = TestIssuer::new()?.keys()?;
        assert!(matches!(
            verify_offline(FIXTURE, &wrong_keys),
            Err(LicenseError::Signature)
        ));
        let tampered = FIXTURE.replacen("v4.public.eyJ", "v4.public.fyJ", 1);
        assert!(matches!(
            activate(tampered.as_bytes(), &key, &path),
            Err(LicenseError::Signature)
        ));
        assert!(matches!(
            activate(LEGACY_ENVELOPE.as_bytes(), &key, &path),
            Err(LicenseError::InvalidToken)
        ));
        assert_eq!(fs::read_to_string(path)?, FIXTURE);
        Ok(())
    }

    #[test]
    fn authenticated_claims_enforce_the_issuer_contract() -> TestResult {
        let issuer = TestIssuer::new()?;
        let keys = issuer.keys()?;
        let valid = valid_claims();
        for (claim, invalid) in [
            ("schema_version", json!(2)),
            ("product", json!("another-product")),
            ("jti", json!("user_01h455vb4pex5vsknk084sn02q")),
            ("purchaser_ref", json!("person@example.com")),
            ("purchaser_ref", json!("  ")),
            ("purchaser_ref", json!("a\nb")),
            ("purchaser_ref", json!("é".repeat(129))),
            ("storefront", json!("POLAR")),
            ("storefront", json!("unknown")),
            ("seats", json!(0)),
            ("seats", json!(2)),
            ("seats", Value::Null),
            ("eligible_major", json!(0)),
            ("eligible_major", json!(-1)),
            ("eligible_major", json!(1.0)),
            ("eligible_major", json!("1")),
            ("eligible_major", Value::Null),
            ("eligible_major", json!(4_294_967_296_u64)),
            ("iat", json!("1969-12-31T23:59:59Z")),
            ("nbf", json!("1969-12-31T23:59:59Z")),
            ("iat", json!("9999-12-31T23:59:59-01:00")),
            ("nbf", json!("not a timestamp")),
            ("exp", json!("2030-01-01T00:00:00Z")),
            ("updates_until", json!("2027-11-20T12:00:00Z")),
        ] {
            let mut claims = valid.clone();
            claims[claim] = invalid;
            let token = issuer.sign_claims(&claims.to_string())?;
            assert!(
                matches!(
                    verify_offline(token, &keys),
                    Err(LicenseError::InvalidClaims)
                ),
                "invalid {claim}"
            );
        }
        let duplicated = valid.to_string().replacen('{', "{\"eligible_major\":2,", 1);
        assert!(matches!(
            verify_offline(issuer.sign_claims(&duplicated)?, &keys),
            Err(LicenseError::InvalidClaims)
        ));

        for (slug, storefront) in [
            ("polar", Storefront::Polar),
            ("etsy", Storefront::Etsy),
            ("direct", Storefront::Direct),
            ("stripe", Storefront::Stripe),
            ("comp", Storefront::Comp),
            ("gumroad", Storefront::Gumroad),
        ] {
            let mut claims = valid.clone();
            claims["storefront"] = json!(slug);
            claims["eligible_major"] = json!(u32::MAX);
            let token = issuer.sign_claims(&claims.to_string())?;
            let verified = verify_offline(token, &keys)?;
            assert_eq!(verified.storefront(), storefront);
            assert_eq!(verified.0.seats, SeatCount::OneComputer);
            assert_eq!(
                verified.eligible_major(),
                MajorVersion::try_from(u64::from(u32::MAX))?
            );
        }
        let mut missing_major = valid;
        missing_major
            .as_object_mut()
            .ok_or("test claims object")?
            .remove("eligible_major");
        assert!(matches!(
            verify_offline(issuer.sign_claims(&missing_major.to_string())?, &keys),
            Err(LicenseError::InvalidClaims)
        ));
        Ok(())
    }

    #[test]
    fn issue_and_not_before_times_use_the_same_five_minute_clock_budget() -> TestResult {
        let issuer = TestIssuer::new()?;
        let keys = issuer.keys()?;
        let now = "2026-11-20T12:00:00Z".parse()?;
        for claim in ["iat", "nbf"] {
            let mut claims = valid_claims();
            claims[claim] = json!("2026-11-20T12:05:00Z");
            let near = issuer.sign_claims(&claims.to_string())?;
            UnverifiedToken::try_from(near.as_bytes())?.verify(&keys, now)?;
            claims[claim] = json!("2026-11-20T12:05:00.001Z");
            let far = issuer.sign_claims(&claims.to_string())?;
            assert!(matches!(
                UnverifiedToken::try_from(far.as_bytes())?.verify(&keys, now),
                Err(LicenseError::InvalidClaims)
            ));
        }

        Ok(())
    }

    #[test]
    fn signature_binds_exact_footer_and_desktop_assertion() -> TestResult {
        let issuer = TestIssuer::new()?;
        let keys = issuer.keys()?;
        let payload = valid_claims().to_string();
        for assertion in [None, Some("another-product")] {
            assert!(matches!(
                verify_offline(issuer.sign(&payload, FOOTER, assertion)?, &keys),
                Err(LicenseError::Signature)
            ));
        }
        let spaced = r#"{ "kid" : "test-2026" }"#;
        let token = issuer.sign(&payload, spaced, Some(Product::Desktop.as_ref()))?;
        verify_offline(&token, &keys)?;
        let (body, _) = token.rsplit_once('.').ok_or("test token footer")?;
        for footer in [FOOTER, r#"{"kid":"alias"}"#] {
            assert!(matches!(
                verify_offline(format!("{body}.{}", URL_SAFE_NO_PAD.encode(footer)), &keys),
                Err(LicenseError::Signature)
            ));
        }
        assert!(matches!(
            verify_offline(
                format!("{body}.{}", URL_SAFE_NO_PAD.encode(r#"{"kid":"other"}"#)),
                &keys
            ),
            Err(LicenseError::UnknownKey)
        ));
        Ok(())
    }

    #[test]
    fn malformed_tokens_and_trust_maps_fail_closed() -> TestResult {
        let keys = keys()?;
        let (body, footer) = FIXTURE
            .trim_end()
            .rsplit_once('.')
            .ok_or("test token footer")?;
        for json in [
            r#"{"kid":"test-2026","key":"untrusted"}"#,
            r#"{"kid":"test-2026","kid":"test-2026"}"#,
            r#"{"kid":"bad key"}"#,
            "{}",
        ] {
            let token = format!("{body}.{}", URL_SAFE_NO_PAD.encode(json));
            assert!(matches!(
                verify_offline(token, &keys),
                Err(LicenseError::InvalidFooter)
            ));
        }
        for token in [
            body.to_owned(),
            format!("{body}."),
            format!("{body}.{footer}="),
        ] {
            assert!(matches!(
                verify_offline(token, &keys),
                Err(LicenseError::InvalidFooter)
            ));
        }
        for token in [
            FIXTURE.replacen("v4.public.", "v4.local.", 1),
            format!("v4.public.not+base64url.{footer}"),
            format!("v4.public.YQ.{footer}"),
        ] {
            assert!(matches!(
                verify_offline(token, &keys),
                Err(LicenseError::InvalidToken)
            ));
        }
        assert!(matches!(
            verify_offline("a".repeat(MAX_TOKEN_LEN + 1), &keys),
            Err(LicenseError::TooLarge)
        ));
        assert!(matches!(
            TrustedKeys::from_json(b"{}"),
            Err(LicenseError::Unconfigured)
        ));
        let weak = URL_SAFE_NO_PAD.encode([0; 32]);
        for map in [
            json!({"test-2026": weak}),
            json!({"test-2026": "invalid"}),
            json!({"bad key": "6kpsY-KcUgq-9VB7Ey7F-ZVHdq6-vnuSQh7qaRRG0iw"}),
        ] {
            assert!(matches!(
                TrustedKeys::from_json(&serde_json::to_vec(&map)?),
                Err(LicenseError::TrustConfiguration)
            ));
        }
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn storage_refuses_public_and_symlink_files() -> TestResult {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let directory = tempfile::tempdir()?;
        let target = directory.path().join("target");
        fs::write(&target, "preserve")?;
        let path = directory.path().join("license.json");
        symlink(&target, &path)?;
        assert!(activate(FIXTURE.as_bytes(), &keys()?, &path).is_err());
        assert!(load(&path, &keys()?).is_err());
        assert_eq!(fs::read_to_string(target)?, "preserve");
        fs::remove_file(&path)?;
        fs::write(&path, FIXTURE)?;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644))?;
        assert!(load(&path, &keys()?).is_err());
        assert!(activate(FIXTURE.as_bytes(), &keys()?, &path).is_err());
        Ok(())
    }
}
