//! Signed release discovery and fail-closed installation. No ledger access.

use crate::license::{ActiveLicense, MajorVersion, UnrevokedLicense};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    error::Error,
    fmt, fs,
    io::{self, Read, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    str::FromStr,
    time::Duration,
};
use tempfile::NamedTempFile;

pub(crate) mod extension;

const REPOSITORY: &str = "https://github.com/Govcraft/beankeeper-desktop";
const RELEASE_API: &str = "https://api.github.com/repos/Govcraft/beankeeper-desktop/releases";
const TAG_PREFIX: &str = "beankeeper-desktop-v";
const DAY: u64 = 86_400;
const MAX_METADATA: u64 = 2 * 1024 * 1024;
const MAX_ARCHIVE: u64 = 256 * 1024 * 1024;
const MAX_BINARY: u64 = 512 * 1024 * 1024;

/// Update failures never contain HTTP bodies, credentials or ledger content.
#[derive(Debug)]
pub enum UpdateError {
    Io(io::Error),
    Network,
    Http(reqwest::StatusCode),
    Invalid(&'static str),
    /// No verified local license covers the release major.
    LicenseRequired(MajorVersion),
    Signature,
    Json(serde_json::Error),
}
impl fmt::Display for UpdateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(e) => write!(f, "update file operation failed: {e}"),
            Self::Network => f.write_str("the release server could not be reached"),
            Self::Http(status) if *status == reqwest::StatusCode::NOT_FOUND => {
                f.write_str("the release file was not found (HTTP 404)")
            }
            Self::Http(status) => {
                write!(f, "the release server returned HTTP {}", status.as_u16())
            }
            Self::Invalid(reason) => write!(f, "update refused: {reason}"),
            Self::LicenseRequired(major) => write!(
                f,
                "This update requires a Desktop license that covers {major}.x releases. Ask your agent to activate a license you already have."
            ),
            Self::Signature => f.write_str(
                "Sigstore verification failed; a valid signed release bundle is required",
            ),
            Self::Json(_) => f.write_str("invalid release metadata or update cache"),
        }
    }
}
impl Error for UpdateError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io(e) => Some(e),
            Self::Json(e) => Some(e),
            _ => None,
        }
    }
}
impl From<io::Error> for UpdateError {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}
impl From<serde_json::Error> for UpdateError {
    fn from(e: serde_json::Error) -> Self {
        Self::Json(e)
    }
}

// Wire metadata used for both incoming JSON and backwards-compatible cache
// entries. Deserializing it confers no authority to install an update.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    schema_version: u32,
    version: String,
    released_at: DateTime<Utc>,
    artifacts: BTreeMap<String, Artifact>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Artifact {
    sha256: String,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct ReleaseVersion(semver::Version);

impl ReleaseVersion {
    fn license_major(&self) -> Option<MajorVersion> {
        self.0.major.try_into().ok()
    }
}

impl fmt::Display for ReleaseVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl FromStr for ReleaseVersion {
    type Err = UpdateError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let version =
            semver::Version::parse(value).map_err(|_| UpdateError::Invalid("invalid version"))?;
        let supported_prerelease = version
            .pre
            .as_str()
            .strip_prefix("rc.")
            .is_some_and(|rc| !rc.is_empty() && rc.bytes().all(|byte| byte.is_ascii_digit()));
        if (!version.pre.is_empty() && !supported_prerelease) || !version.build.is_empty() {
            return Err(UpdateError::Invalid(
                "only stable and rc.N releases are supported",
            ));
        }
        Ok(Self(version))
    }
}

/// Local prereleases may opt into RC updates. Build metadata is prohibited.
fn installed_version(value: &str) -> Result<semver::Version, UpdateError> {
    let version =
        semver::Version::parse(value).map_err(|_| UpdateError::Invalid("invalid version"))?;
    if !version.build.is_empty() {
        return Err(UpdateError::Invalid("invalid version"));
    }
    Ok(version)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Sha256Digest([u8; 32]);

impl FromStr for Sha256Digest {
    type Err = UpdateError;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        fn nibble(byte: u8) -> Option<u8> {
            match byte {
                b'0'..=b'9' => Some(byte - b'0'),
                b'a'..=b'f' => Some(byte - b'a' + 10),
                b'A'..=b'F' => Some(byte - b'A' + 10),
                _ => None,
            }
        }
        let source: &[u8; 64] = text
            .as_bytes()
            .try_into()
            .map_err(|_| UpdateError::Invalid("invalid SHA-256 checksum"))?;
        let mut digest = [0; 32];
        for (byte, &[high, low]) in digest.iter_mut().zip(source.as_chunks::<2>().0) {
            let high = nibble(high).ok_or(UpdateError::Invalid("invalid SHA-256 checksum"))?;
            let low = nibble(low).ok_or(UpdateError::Invalid("invalid SHA-256 checksum"))?;
            *byte = high << 4 | low;
        }
        Ok(Self(digest))
    }
}

struct VerifiedManifest {
    version: ReleaseVersion,
    artifacts: BTreeMap<Target, Sha256Digest>,
    cache_record: Manifest,
}

impl VerifiedManifest {
    fn is_newer_than(&self, current: &semver::Version) -> bool {
        (!current.pre.is_empty() || self.version.0.pre.is_empty()) && self.version.0 > *current
    }

    fn require_newer(self, current: &semver::Version) -> Result<NewerRelease, UpdateError> {
        if !self.is_newer_than(current) {
            return Err(UpdateError::Invalid(
                "release is not newer than this Desktop binary",
            ));
        }
        Ok(NewerRelease(self))
    }
}

struct NewerRelease(VerifiedManifest);

impl NewerRelease {
    fn authorize(self, licenses: &[UnrevokedLicense]) -> Result<AuthorizedRelease, UpdateError> {
        let major = self.0.version.license_major().ok_or(UpdateError::Invalid(
            "this version is outside Desktop license coverage. Use the signed installer",
        ))?;
        if !licenses
            .iter()
            .any(|license| ActiveLicense::try_from((license.clone(), major)).is_ok())
        {
            return Err(UpdateError::LicenseRequired(major));
        }
        Ok(AuthorizedRelease(self.0))
    }
}

struct AuthorizedRelease(VerifiedManifest);

/// Discovery is untrusted: only a constrained tag is used, never API asset URLs.
#[derive(Deserialize)]
struct GithubRelease {
    tag_name: String,
    draft: bool,
    prerelease: bool,
}

fn select_release(
    releases: &[GithubRelease],
    current: &semver::Version,
) -> Result<ReleaseVersion, UpdateError> {
    releases
        .iter()
        .filter_map(GithubRelease::published_version)
        .filter(|version| !current.pre.is_empty() || version.0.pre.is_empty())
        .max()
        .ok_or(UpdateError::Invalid("no supported published release found"))
}

impl GithubRelease {
    fn published_version(&self) -> Option<ReleaseVersion> {
        let version: ReleaseVersion = self.tag_name.strip_prefix(TAG_PREFIX)?.parse().ok()?;
        let prerelease = !version.0.pre.is_empty();
        (!self.draft && self.prerelease == prerelease).then_some(version)
    }
}

fn fetch_manifest(
    current: &semver::Version,
    mut fetch: impl FnMut(&str, u64) -> Result<Vec<u8>, UpdateError>,
    verifier: &dyn BundleVerifier,
) -> Result<VerifiedManifest, UpdateError> {
    let prerelease = !current.pre.is_empty();
    let endpoint = if prerelease {
        format!("{RELEASE_API}?per_page=100&")
    } else {
        format!("{RELEASE_API}/latest?")
    };
    let bytes = fetch(
        &format!("{endpoint}version={current}&os={}", std::env::consts::OS),
        MAX_METADATA,
    )?;
    let releases = if prerelease {
        serde_json::from_slice::<Vec<GithubRelease>>(&bytes)?
    } else {
        vec![serde_json::from_slice::<GithubRelease>(&bytes)?]
    };
    let version = select_release(&releases, current)?;
    let bytes = fetch(&asset_url(&version, "update-manifest.json"), MAX_METADATA)?;
    let bundle = fetch(
        &asset_url(&version, "update-manifest.json.sigstore"),
        MAX_METADATA,
    )?;
    verify_manifest(&bytes, &bundle, Some(version), verifier)
}

/// Authenticate bytes against a constrained release tag.
trait BundleVerifier {
    fn verify(
        &self,
        artifact: &Path,
        bundle: &Path,
        version: &ReleaseVersion,
    ) -> Result<(), UpdateError>;
}

/// Verifies the same release identity, certificate chain and transparency log in-process.
struct Sigstore;

fn release_policy(version: &ReleaseVersion) -> sigstore_verify::VerificationPolicy {
    sigstore_verify::VerificationPolicy::new(
        format!("{REPOSITORY}/.github/workflows/release.yml@refs/tags/{TAG_PREFIX}{version}"),
        "https://token.actions.githubusercontent.com",
    )
}

impl BundleVerifier for Sigstore {
    fn verify(
        &self,
        artifact: &Path,
        bundle: &Path,
        version: &ReleaseVersion,
    ) -> Result<(), UpdateError> {
        use sigstore_verify::{trust_root, types::Bundle};
        let evidence =
            Bundle::from_json(&fs::read_to_string(bundle).map_err(|_| UpdateError::Signature)?)
                .map_err(|_| UpdateError::Signature)?;
        let root = trust_root::TrustedRoot::from_json(trust_root::SIGSTORE_PRODUCTION_TRUSTED_ROOT)
            .map_err(|_| UpdateError::Signature)?;
        let verifier = sigstore_verify::Verifier::new(&root).map_err(|_| UpdateError::Signature)?;
        match verifier.verify_reader(
            fs::File::open(artifact)?,
            &evidence,
            &release_policy(version),
        ) {
            Ok(_) => Ok(()),
            Err(error) => fallback_for_rotation(error, artifact, &evidence, version, |candidate| {
                let status = candidate
                    .cosign_command(bundle)
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status()
                    .map_err(|_| UpdateError::Signature)?;
                if status.success() {
                    Ok(())
                } else {
                    Err(UpdateError::Signature)
                }
            }),
        }
    }
}

mod rotation {
    use super::{
        Command, Path, REPOSITORY, ReleaseVersion, TAG_PREFIX, UpdateError, fs, release_policy,
    };

    // The verifier reports trust coverage before checking artifact cryptography.
    // Only this private validating constructor can admit an artifact to fallback.
    // It confers no installation authority: cosign must still verify the complete
    // chain, transparency evidence, exact identity and issuer against its trust root.
    pub(super) struct RotationCandidate<'a> {
        artifact: &'a Path,
        version: &'a ReleaseVersion,
    }

    impl<'a>
        TryFrom<(
            &'a Path,
            &sigstore_verify::types::Bundle,
            &'a ReleaseVersion,
        )> for RotationCandidate<'a>
    {
        type Error = UpdateError;

        fn try_from(
            (artifact, bundle, version): (
                &'a Path,
                &sigstore_verify::types::Bundle,
                &'a ReleaseVersion,
            ),
        ) -> Result<Self, Self::Error> {
            use sigstore_verify::{
                crypto,
                types::{HashAlgorithm, SignatureContent},
            };
            let certificate = bundle.signing_certificate().ok_or(UpdateError::Signature)?;
            let certificate =
                crypto::parse_certificate_info(certificate).map_err(|_| UpdateError::Signature)?;
            let policy = release_policy(version);
            if !certificate.identity.as_ref().is_some_and(|identity| {
                policy
                    .identity()
                    .is_some_and(|expected| expected.matches(identity))
            }) || certificate.issuer.as_deref() != policy.issuer()
            {
                return Err(UpdateError::Signature);
            }
            // Release artifacts use detached SHA-256 message signatures. Other
            // formats fail closed here instead of expanding the recovery surface.
            let SignatureContent::MessageSignature(signature) = &bundle.content else {
                return Err(UpdateError::Signature);
            };
            let digest = signature
                .message_digest
                .as_ref()
                .ok_or(UpdateError::Signature)?;
            let actual = crypto::sha256_reader(fs::File::open(artifact)?)?;
            if digest.algorithm != HashAlgorithm::Sha2256
                || digest.digest.as_bytes() != actual.as_bytes()
            {
                return Err(UpdateError::Signature);
            }
            crypto::verify_signature_prehashed(
                &certificate.public_key,
                actual.as_bytes(),
                &signature.signature,
                certificate.key_algorithm.default_signing_scheme(),
            )
            .map_err(|_| UpdateError::Signature)?;
            Ok(Self { artifact, version })
        }
    }

    impl RotationCandidate<'_> {
        pub(super) fn cosign_command(self, bundle: &Path) -> Command {
            let Self { artifact, version } = self;
            let identity = format!(
                "{REPOSITORY}/.github/workflows/release.yml@refs/tags/{TAG_PREFIX}{version}"
            );
            let mut command = Command::new("cosign");
            command
                .args([
                    "verify-blob",
                    "--timeout",
                    "30s",
                    "--certificate-identity",
                    &identity,
                    "--certificate-oidc-issuer",
                    "https://token.actions.githubusercontent.com",
                    "--bundle",
                ])
                .arg(bundle)
                .arg(artifact);
            command
        }
    }
}

use rotation::RotationCandidate;

fn fallback_for_rotation(
    error: sigstore_verify::Error,
    artifact: &Path,
    bundle: &sigstore_verify::types::Bundle,
    version: &ReleaseVersion,
    fallback: impl FnOnce(RotationCandidate<'_>) -> Result<(), UpdateError>,
) -> Result<(), UpdateError> {
    use sigstore_verify::{CertificateError, Error, TransparencyLogError};
    let uncovered = match error {
        Error::TransparencyLog(TransparencyLogError::UnknownLog { .. })
        | Error::Certificate(CertificateError::NoValidAuthority { .. }) => true,
        // sigstore-verify currently erases webpki's structured UnknownIssuer
        // reason. Match only that exact reason, never arbitrary chain errors.
        Error::Certificate(CertificateError::ChainInvalid(reason)) => {
            reason == "certificate chain validation failed: UnknownIssuer"
        }
        _ => false,
    };
    if !uncovered {
        return Err(UpdateError::Signature);
    }
    let candidate = RotationCandidate::try_from((artifact, bundle, version))?;
    fallback(candidate)
}

fn verify_bytes(
    bytes: &[u8],
    bundle: &[u8],
    version: &ReleaseVersion,
    verifier: &dyn BundleVerifier,
) -> Result<(), UpdateError> {
    if bundle.is_empty() {
        return Err(UpdateError::Signature);
    }
    let mut data = NamedTempFile::new()?;
    data.write_all(bytes)?;
    let mut signature = NamedTempFile::new()?;
    signature.write_all(bundle)?;
    verifier.verify(data.path(), signature.path(), version)
}

fn verify_manifest(
    bytes: &[u8],
    bundle: &[u8],
    expected: Option<ReleaseVersion>,
    verifier: &dyn BundleVerifier,
) -> Result<VerifiedManifest, UpdateError> {
    let manifest: Manifest = serde_json::from_slice(bytes)?;
    let version = match expected {
        Some(version) if manifest.version == version.to_string() => version,
        Some(_) => {
            return Err(UpdateError::Invalid(
                "manifest version does not match release tag",
            ));
        }
        None => manifest.version.parse()?,
    };
    if manifest.schema_version != 1
        || manifest.released_at.timestamp() <= 0
        || manifest.artifacts.is_empty()
    {
        return Err(UpdateError::Invalid("unsupported manifest"));
    }
    verify_bytes(bytes, bundle, &version, verifier)?;
    let mut artifacts = BTreeMap::new();
    for (platform, artifact) in &manifest.artifacts {
        let digest = artifact.sha256.parse()?;
        // Older binaries can still update when the release adds a platform.
        if let Some(target) = Target::from_triple(platform) {
            artifacts.insert(target, digest);
        }
    }
    Ok(VerifiedManifest {
        version,
        artifacts,
        cache_record: manifest,
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Target {
    LinuxX64,
    LinuxArm64,
    MacX64,
    MacArm64,
    WindowsX64,
}

impl Target {
    const ALL: [Self; 5] = [
        Self::LinuxX64,
        Self::LinuxArm64,
        Self::MacX64,
        Self::MacArm64,
        Self::WindowsX64,
    ];

    fn from_triple(triple: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|target| target.triple() == triple)
    }

    fn triple(self) -> &'static str {
        match self {
            Self::LinuxX64 => "x86_64-unknown-linux-gnu",
            Self::LinuxArm64 => "aarch64-unknown-linux-gnu",
            Self::MacX64 => "x86_64-apple-darwin",
            Self::MacArm64 => "aarch64-apple-darwin",
            Self::WindowsX64 => "x86_64-pc-windows-msvc",
        }
    }

    fn current() -> Result<Self, UpdateError> {
        match (std::env::consts::OS, std::env::consts::ARCH) {
            ("linux", "x86_64") => Ok(Self::LinuxX64),
            ("linux", "aarch64") => Ok(Self::LinuxArm64),
            ("macos", "x86_64") => Ok(Self::MacX64),
            ("macos", "aarch64") => Ok(Self::MacArm64),
            ("windows", "x86_64") => Ok(Self::WindowsX64),
            _ => Err(UpdateError::Invalid("unsupported update platform")),
        }
    }

    fn archive_extension(self) -> &'static str {
        match self {
            Self::WindowsX64 => "zip",
            Self::LinuxX64 | Self::LinuxArm64 | Self::MacX64 | Self::MacArm64 => "tar.gz",
        }
    }

    fn binary_name(self) -> &'static str {
        match self {
            Self::WindowsX64 => "bk.exe",
            Self::LinuxX64 | Self::LinuxArm64 | Self::MacX64 | Self::MacArm64 => "bk",
        }
    }
}

fn archive_name(version: &ReleaseVersion, target: Target) -> String {
    format!(
        "bk-{version}-{}.{}",
        target.triple(),
        target.archive_extension()
    )
}
fn download(url: &str, limit: u64) -> Result<Vec<u8>, UpdateError> {
    let client = reqwest::blocking::Client::builder()
        .https_only(true)
        .timeout(Duration::from_secs(30))
        .user_agent("beankeeper-desktop")
        .build()
        .map_err(|_| UpdateError::Network)?;
    let response = client
        .get(url)
        .send()
        .and_then(reqwest::blocking::Response::error_for_status)
        .map_err(|error| {
            error
                .status()
                .map_or(UpdateError::Network, UpdateError::Http)
        })?;
    let mut bytes = Vec::new();
    response.take(limit + 1).read_to_end(&mut bytes)?;
    if u64::try_from(bytes.len()).map_or(true, |len| len > limit) {
        return Err(UpdateError::Invalid("download exceeds size limit"));
    }
    Ok(bytes)
}
fn asset_url(version: &ReleaseVersion, name: &str) -> String {
    format!("{REPOSITORY}/releases/download/beankeeper-desktop-v{version}/{name}")
}

#[derive(Default, Deserialize, Serialize)]
#[serde(from = "CacheRecord")]
struct Cache {
    desktop_version: String,
    last_attempt: u64,
    attempt: AttemptResult,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
enum AttemptResult {
    #[default]
    Incomplete,
    Complete {
        manifest: Manifest,
    },
    Failed {
        failure: CheckFailure,
    },
}

// Only this wire boundary recognizes old prose. New caches contain an enum,
// and completed attempts carry the manifest required to report success.
#[derive(Deserialize)]
#[serde(untagged)]
enum CacheRecord {
    Typed {
        #[serde(default)]
        desktop_version: String,
        last_attempt: u64,
        attempt: AttemptResult,
    },
    Legacy {
        #[serde(default)]
        desktop_version: String,
        last_attempt: u64,
        manifest: Option<Manifest>,
        message: String,
    },
}

impl From<CacheRecord> for Cache {
    fn from(record: CacheRecord) -> Self {
        match record {
            CacheRecord::Typed {
                desktop_version,
                last_attempt,
                attempt,
            } => Self {
                desktop_version,
                last_attempt,
                attempt,
            },
            CacheRecord::Legacy {
                desktop_version,
                last_attempt,
                manifest,
                message,
            } => {
                let attempt = if message.is_empty() || message == "Update check did not complete." {
                    AttemptResult::Incomplete
                } else if let Some(manifest) = manifest {
                    AttemptResult::Complete { manifest }
                } else {
                    AttemptResult::Failed {
                        failure: CheckFailure::from(message),
                    }
                };
                Self {
                    desktop_version,
                    last_attempt,
                    attempt,
                }
            }
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum CheckFailure {
    Io { message: String },
    Network,
    Http { status: HttpStatus },
    Invalid { reason: String },
    LicenseRequired { major: MajorVersion },
    Signature,
    Json,
    LegacyDownload,
    Legacy { message: String },
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(try_from = "u16", into = "u16")]
struct HttpStatus(reqwest::StatusCode);

impl TryFrom<u16> for HttpStatus {
    type Error = <reqwest::StatusCode as TryFrom<u16>>::Error;
    fn try_from(value: u16) -> Result<Self, Self::Error> {
        reqwest::StatusCode::from_u16(value).map(Self)
    }
}

impl From<HttpStatus> for u16 {
    fn from(value: HttpStatus) -> Self {
        value.0.as_u16()
    }
}

impl From<UpdateError> for CheckFailure {
    fn from(error: UpdateError) -> Self {
        match error {
            UpdateError::Io(error) => Self::Io {
                message: error.to_string(),
            },
            UpdateError::Network => Self::Network,
            UpdateError::Http(status) => Self::Http {
                status: HttpStatus(status),
            },
            UpdateError::Invalid(reason) => Self::Invalid {
                reason: reason.into(),
            },
            UpdateError::LicenseRequired(major) => Self::LicenseRequired { major },
            UpdateError::Signature => Self::Signature,
            UpdateError::Json(_) => Self::Json,
        }
    }
}

impl From<String> for CheckFailure {
    fn from(message: String) -> Self {
        if message
            == "update download failed; check connectivity and retry after the daily interval"
        {
            Self::LegacyDownload
        } else {
            Self::Legacy { message }
        }
    }
}

impl fmt::Display for CheckFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io { message } => write!(f, "update file operation failed: {message}"),
            Self::Network => UpdateError::Network.fmt(f),
            Self::Http { status } => UpdateError::Http(status.0).fmt(f),
            Self::Invalid { reason } => write!(f, "update refused: {reason}"),
            Self::LicenseRequired { major } => UpdateError::LicenseRequired(*major).fmt(f),
            Self::Signature => UpdateError::Signature.fmt(f),
            Self::Json => f.write_str("invalid release metadata or update cache"),
            Self::LegacyDownload => f.write_str(
                "the release server could not be reached or returned an unsuccessful response",
            ),
            Self::Legacy { message } => f.write_str(message),
        }
    }
}

fn read_cache(path: &Path, current: &str) -> Result<Cache, UpdateError> {
    match fs::read(path) {
        Ok(bytes) => {
            let cache: Cache = serde_json::from_slice(&bytes)?;
            // Legacy caches have no version. Neither they nor results from a
            // different executable describe this version's update state.
            Ok(if cache.desktop_version == current {
                cache
            } else {
                Cache::default()
            })
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Cache::default()),
        Err(e) => Err(e.into()),
    }
}
fn write_cache(path: &Path, cache: &Cache) -> Result<(), UpdateError> {
    let parent = path
        .parent()
        .ok_or(UpdateError::Invalid("cache needs a parent directory"))?;
    fs::create_dir_all(parent)?;
    let mut pending = NamedTempFile::new_in(parent)?;
    serde_json::to_writer(&mut pending, cache)?;
    pending.as_file().sync_all()?;
    pending.persist(path).map_err(|e| e.error)?;
    Ok(())
}

/// Machine-readable update outcome, rendered as prose only for text callers.
#[derive(Debug, Serialize)]
#[serde(transparent)]
pub struct UpdateStatus(UpdateState);

#[derive(Debug, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
enum UpdateState {
    Disabled,
    Unchecked,
    InProgress,
    Checked {
        last_attempt: u64,
        outcome: CheckOutcome,
    },
}

#[derive(Debug, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
enum CheckOutcome {
    UpToDate { version: semver::Version },
    Available { version: semver::Version },
    Incomplete,
    Failed { failure: CheckFailure },
}

impl From<(ReleaseVersion, &semver::Version)> for CheckOutcome {
    fn from((release, installed): (ReleaseVersion, &semver::Version)) -> Self {
        if &release.0 > installed {
            Self::Available { version: release.0 }
        } else {
            Self::UpToDate { version: release.0 }
        }
    }
}

impl TryFrom<&Cache> for UpdateStatus {
    type Error = UpdateError;

    fn try_from(cache: &Cache) -> Result<Self, Self::Error> {
        if cache.last_attempt == 0 {
            return Ok(Self(UpdateState::Unchecked));
        }
        let outcome = match &cache.attempt {
            AttemptResult::Incomplete => CheckOutcome::Incomplete,
            AttemptResult::Complete { manifest } => CheckOutcome::from((
                manifest.version.parse::<ReleaseVersion>()?,
                &installed_version(&cache.desktop_version)?,
            )),
            AttemptResult::Failed { failure } => CheckOutcome::Failed {
                failure: failure.clone(),
            },
        };
        Ok(Self(UpdateState::Checked {
            last_attempt: cache.last_attempt,
            outcome,
        }))
    }
}

impl fmt::Display for UpdateStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (last_attempt, outcome) = match &self.0 {
            UpdateState::Disabled => return f.write_str("Automatic update checks are disabled."),
            UpdateState::Unchecked => {
                return f.write_str("Updates have not been checked. Run `bk update-check`.");
            }
            UpdateState::InProgress => {
                return f.write_str("An update check is already in progress; retry later.");
            }
            UpdateState::Checked {
                last_attempt,
                outcome,
            } => (last_attempt, outcome),
        };
        match outcome {
            CheckOutcome::UpToDate { .. } => f.write_str("Desktop is up to date.")?,
            CheckOutcome::Available { version } => {
                if let Ok(major) = MajorVersion::try_from(version.major) {
                    write!(f, "Desktop {version} is available. Run `bk self-update` with a license that covers {major}.x releases.")?;
                } else {
                    write!(f, "Desktop {version} is available. Use the signed installer.")?;
                }
            }
            CheckOutcome::Incomplete => f.write_str("The last update check has not completed. It may still be running or may have been interrupted.")?,
            CheckOutcome::Failed { failure } => write!(f, "Could not check for updates: {}. Checks run at most once per day.", failure.to_string().trim_end_matches('.'))?,
        }
        let attempted_at = i64::try_from(*last_attempt)
            .ok()
            .and_then(|seconds| DateTime::<Utc>::from_timestamp(seconds, 0))
            .map_or_else(
                || "unavailable (invalid cached time)".into(),
                |time| time.format("%Y-%m-%d %H:%M:%S UTC").to_string(),
            );
        write!(f, "\nLast attempt: {attempted_at}.")
    }
}

/// Whether background and manual update checks may use disk or network.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpdateChecks {
    Enabled,
    Disabled,
}

/// Last checked state, read without any network request. Opt-out hides stale results.
///
/// # Errors
/// Returns errors when the cached update status cannot be read.
pub fn status(path: &Path, checks: UpdateChecks) -> Result<String, UpdateError> {
    status_with(path, checks, env!("CARGO_PKG_VERSION"))
}
fn status_with(path: &Path, checks: UpdateChecks, current: &str) -> Result<String, UpdateError> {
    Ok(typed_status_with(path, checks, current)?.to_string())
}
fn typed_status_with(
    path: &Path,
    checks: UpdateChecks,
    current: &str,
) -> Result<UpdateStatus, UpdateError> {
    if checks == UpdateChecks::Disabled {
        return Ok(UpdateStatus(UpdateState::Disabled));
    }
    UpdateStatus::try_from(&read_cache(path, current)?)
}
/// At most one network attempt per day per Desktop version, including failures
/// and process restarts. Legacy caches are refreshed on the next check.
/// Opt-out is evaluated before files or the network are accessed.
///
/// # Errors
/// Returns errors when cached update state cannot be read or written.
pub fn check(path: &Path, checks: UpdateChecks, now: u64) -> Result<UpdateStatus, UpdateError> {
    check_status_with(path, checks, now, env!("CARGO_PKG_VERSION"), |current| {
        fetch_manifest(current, download, &Sigstore)
    })
}
fn check_status_with(
    path: &Path,
    checks: UpdateChecks,
    now: u64,
    current: &str,
    fetch: impl FnOnce(&semver::Version) -> Result<VerifiedManifest, UpdateError>,
) -> Result<UpdateStatus, UpdateError> {
    if checks == UpdateChecks::Disabled {
        return typed_status_with(path, UpdateChecks::Disabled, current);
    }
    let mut cache = read_cache(path, current)?;
    if cache.last_attempt != 0 && now.saturating_sub(cache.last_attempt) < DAY {
        return typed_status_with(path, UpdateChecks::Enabled, current);
    }
    let parent = path
        .parent()
        .ok_or(UpdateError::Invalid("cache needs a parent directory"))?;
    fs::create_dir_all(parent)?;
    // Advisory process lock prevents concurrent UI/CLI checks. A crash leaves a
    // harmless lock; recover only after one day, preserving the rate bound.
    let lock = path.with_extension("lock");
    match fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&lock)
    {
        Ok(_) => (),
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
            let age = fs::metadata(&lock)?
                .modified()?
                .elapsed()
                .unwrap_or_default();
            if age >= Duration::from_secs(DAY) {
                fs::remove_file(&lock)?;
            }
            return Ok(UpdateStatus(UpdateState::InProgress));
        }
        Err(e) => return Err(e.into()),
    }
    let result = (|| {
        // Re-read under lock to handle a concurrent completed checker.
        cache = read_cache(path, current)?;
        if cache.last_attempt != 0 && now.saturating_sub(cache.last_attempt) < DAY {
            return typed_status_with(path, UpdateChecks::Enabled, current);
        }
        current.clone_into(&mut cache.desktop_version);
        cache.last_attempt = now.max(1);
        cache.attempt = AttemptResult::Incomplete;
        write_cache(path, &cache)?;
        let installed = installed_version(current)?;
        let outcome = match fetch(&installed) {
            Ok(manifest) => {
                let outcome = CheckOutcome::from((manifest.version, &installed));
                cache.attempt = AttemptResult::Complete {
                    manifest: manifest.cache_record,
                };
                outcome
            }
            Err(error) => {
                let failure = CheckFailure::from(error);
                cache.attempt = AttemptResult::Failed {
                    failure: failure.clone(),
                };
                CheckOutcome::Failed { failure }
            }
        };
        write_cache(path, &cache)?;
        Ok(UpdateStatus(UpdateState::Checked {
            last_attempt: cache.last_attempt,
            outcome,
        }))
    })();
    let _ = fs::remove_file(lock);
    result
}

#[cfg(test)]
fn check_with(
    path: &Path,
    checks: UpdateChecks,
    now: u64,
    current: &str,
    fetch: impl FnOnce(&semver::Version) -> Result<VerifiedManifest, UpdateError>,
) -> Result<String, UpdateError> {
    Ok(check_status_with(path, checks, now, current, fetch)?.to_string())
}

/// Download only the matching archive and mandatory D13 checksum/bundle files.
/// Re-verifies cached manifest bytes via their signed release asset before install.
///
/// # Errors
/// Returns errors for unsupported targets, unavailable releases, ineligible licenses, or invalid artifacts.
pub fn self_update(cache: &Path, destination: &Path) -> Result<semver::Version, UpdateError> {
    let AttemptResult::Complete { manifest: cached } =
        read_cache(cache, env!("CARGO_PKG_VERSION"))?.attempt
    else {
        return Err(UpdateError::Invalid(
            "run bk update-check successfully first",
        ));
    };
    let version = cached.version.parse()?;
    let bytes = download(&asset_url(&version, "update-manifest.json"), MAX_METADATA)?;
    let bundle = download(
        &asset_url(&version, "update-manifest.json.sigstore"),
        MAX_METADATA,
    )?;
    let current = installed_version(env!("CARGO_PKG_VERSION"))?;
    let release =
        verify_manifest(&bytes, &bundle, Some(version), &Sigstore)?.require_newer(&current)?;
    let licenses = crate::product::load_update_licenses().map_err(|_| {
        UpdateError::Invalid("could not verify the Desktop license. Run `bk license` for details")
    })?;
    let authorized = release.authorize(&licenses)?;
    let version = &authorized.0.version;
    let target = Target::current()?;
    let name = archive_name(version, target);
    let archive = download(&asset_url(version, &name), MAX_ARCHIVE)?;
    let archive_bundle = download(
        &asset_url(version, &format!("{name}.sigstore")),
        MAX_METADATA,
    )?;
    let checksums = download(&asset_url(version, "checksums-sha256.txt"), MAX_METADATA)?;
    let checksums_bundle = download(
        &asset_url(version, "checksums-sha256.txt.sigstore"),
        MAX_METADATA,
    )?;
    install(
        &authorized,
        target,
        &ReleaseFiles {
            archive: &archive,
            bundle: &archive_bundle,
            checksums: &checksums,
            checksums_bundle: &checksums_bundle,
        },
        destination,
        &Sigstore,
    )?;
    Ok(version.0.clone())
}

struct ReleaseFiles<'a> {
    archive: &'a [u8],
    bundle: &'a [u8],
    checksums: &'a [u8],
    checksums_bundle: &'a [u8],
}

/// Verify artifact evidence and atomically replace the executable.
fn install(
    release: &AuthorizedRelease,
    target: Target,
    files: &ReleaseFiles<'_>,
    destination: &Path,
    verifier: &dyn BundleVerifier,
) -> Result<(), UpdateError> {
    let ReleaseFiles {
        archive,
        bundle,
        checksums,
        checksums_bundle,
    } = files;
    let manifest = &release.0;
    let version = &manifest.version;
    let name = archive_name(version, target);
    let artifact = manifest
        .artifacts
        .get(&target)
        .ok_or(UpdateError::Invalid("release does not support this target"))?;
    verify_bytes(checksums, checksums_bundle, version, verifier)?;
    verify_bytes(archive, bundle, version, verifier)?;
    let hash = Sha256Digest(Sha256::digest(archive).into());
    if hash != *artifact || listed_checksum(checksums, &name)? != Some(hash) {
        return Err(UpdateError::Invalid(
            "archive checksum mismatch or missing/duplicate checksum",
        ));
    }
    replace_binary(archive, version, target, destination)
}

fn listed_checksum(checksums: &[u8], name: &str) -> Result<Option<Sha256Digest>, UpdateError> {
    let lines = std::str::from_utf8(checksums)
        .map_err(|_| UpdateError::Invalid("invalid checksum manifest"))?;
    let mut matching = lines
        .lines()
        .filter_map(|line| line.split_once("  "))
        .filter(|(_, filename)| *filename == name);
    let listed = matching.next().map(|(digest, _)| digest.parse());
    if matching.next().is_some() {
        return Err(UpdateError::Invalid("duplicate SHA-256 checksum"));
    }
    listed.transpose()
}

fn replace_binary(
    archive: &[u8],
    version: &ReleaseVersion,
    target: Target,
    destination: &Path,
) -> Result<(), UpdateError> {
    let expected = format!("bk-{version}-{}/{}", target.triple(), target.binary_name());
    let parent = destination.parent().ok_or(UpdateError::Invalid(
        "installation needs a parent directory",
    ))?;
    let mut pending = NamedTempFile::new_in(parent)?;
    extract_binary(archive, target, &expected, &mut pending)?;
    if pending.as_file().metadata()?.len() == 0 {
        return Err(UpdateError::Invalid("empty executable"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        pending
            .as_file()
            .set_permissions(fs::Permissions::from_mode(0o755))?;
    }
    pending.as_file().sync_all()?;
    #[cfg(windows)]
    if std::env::current_exe()?.as_path() == destination {
        // Windows cannot unlink a running executable. The helper arranges its
        // replacement and cleanup without changing verification requirements.
        self_replace::self_replace(pending.path())?;
        return Ok(());
    }
    pending.persist(destination).map_err(|e| e.error)?;
    Ok(())
}
fn copy_binary(reader: impl Read, output: &mut NamedTempFile) -> Result<(), UpdateError> {
    if io::copy(&mut reader.take(MAX_BINARY + 1), output)? > MAX_BINARY {
        return Err(UpdateError::Invalid("executable exceeds size limit"));
    }
    Ok(())
}
fn extract_binary(
    bytes: &[u8],
    target: Target,
    expected: &str,
    output: &mut NamedTempFile,
) -> Result<(), UpdateError> {
    let mut found = false;
    if target == Target::WindowsX64 {
        let mut archive = zip::ZipArchive::new(io::Cursor::new(bytes))
            .map_err(|_| UpdateError::Invalid("invalid ZIP archive"))?;
        for i in 0..archive.len() {
            let entry = archive
                .by_index(i)
                .map_err(|_| UpdateError::Invalid("invalid ZIP entry"))?;
            if entry
                .name()
                .map_err(|_| UpdateError::Invalid("invalid ZIP filename"))?
                == expected
            {
                if found || !entry.is_file() || entry.is_symlink() {
                    return Err(UpdateError::Invalid("duplicate or nonregular executable"));
                }
                copy_binary(entry, output)?;
                found = true;
            }
        }
    } else {
        let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(bytes));
        for entry in archive.entries()? {
            let entry = entry?;
            if entry.path()?.as_ref() == Path::new(expected) {
                if found || !entry.header().entry_type().is_file() {
                    return Err(UpdateError::Invalid("duplicate or nonregular executable"));
                }
                copy_binary(entry, output)?;
                found = true;
            }
        }
    }
    if found {
        Ok(())
    } else {
        Err(UpdateError::Invalid("release binary missing from archive"))
    }
}

/// Current Unix time for throttling; never derived from ledger data.
#[must_use]
pub fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
/// Start a best-effort daily check without delaying ledger startup.
pub fn background(path: PathBuf, checks: UpdateChecks) {
    if checks == UpdateChecks::Disabled {
        return;
    }
    std::thread::spawn(move || {
        loop {
            let _ = check(&path, UpdateChecks::Enabled, now());
            std::thread::park_timeout(Duration::from_secs(DAY));
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signature, Signer, SigningKey};
    type TestResult = Result<(), Box<dyn Error>>;
    // Cryptographic offline substitute only at the verifier boundary. Production
    // uses in-process Sigstore verification to validate Fulcio identity and transparency evidence.
    struct OfflineVerifier;
    impl BundleVerifier for OfflineVerifier {
        fn verify(
            &self,
            artifact: &Path,
            bundle: &Path,
            _version: &ReleaseVersion,
        ) -> Result<(), UpdateError> {
            let signature =
                Signature::from_slice(&fs::read(bundle)?).map_err(|_| UpdateError::Signature)?;
            SigningKey::from_bytes(&[42; 32])
                .verifying_key()
                .verify_strict(&fs::read(artifact)?, &signature)
                .map_err(|_| UpdateError::Signature)
        }
    }
    fn sign(bytes: &[u8]) -> Vec<u8> {
        SigningKey::from_bytes(&[42; 32])
            .sign(bytes)
            .to_bytes()
            .to_vec()
    }
    fn sha256(bytes: &[u8]) -> String {
        use std::fmt::Write as _;
        let mut text = String::with_capacity(64);
        for byte in Sha256::digest(bytes) {
            let _ = write!(text, "{byte:02x}");
        }
        text
    }
    fn verified(manifest: &Manifest) -> Result<VerifiedManifest, UpdateError> {
        let bytes = serde_json::to_vec(manifest)?;
        verify_manifest(&bytes, &sign(&bytes), None, &OfflineVerifier)
    }
    fn fixture_license(bytes: &[u8]) -> Result<UnrevokedLicense, crate::license::LicenseError> {
        let keys = crate::license::TrustedKeys::from_json(include_bytes!(
            "../tests/fixtures/license-public-keys.json"
        ))?;
        let revocations = crate::license::VerifiedRevocations::verify(
            include_bytes!("../tests/fixtures/revocations-empty.json"),
            &keys,
        )?;
        revocations.admit(crate::license::verify_offline(bytes, &keys)?)
    }
    fn authorize(manifest: &Manifest, current: &str) -> Result<AuthorizedRelease, Box<dyn Error>> {
        let license = fixture_license(include_bytes!("../tests/fixtures/license-v4.public.txt"))?;
        Ok(verified(manifest)?
            .require_newer(&installed_version(current)?)?
            .authorize(&[license])?)
    }
    fn manifest(archive: &[u8], target: &str) -> Manifest {
        Manifest {
            schema_version: 1,
            version: "1.10.0".into(),
            released_at: "2026-11-20T00:00:00Z".parse().expect("fixture date"),
            artifacts: BTreeMap::from([(
                target.into(),
                Artifact {
                    sha256: sha256(archive),
                },
            )]),
        }
    }
    fn archive(version: &str, target: &str) -> Vec<u8> {
        let content = b"fixture executable";
        let mut archive = tar::Builder::new(flate2::write::GzEncoder::new(
            Vec::new(),
            flate2::Compression::default(),
        ));
        let mut header = tar::Header::new_gnu();
        header.set_size(u64::try_from(content.len()).expect("fixture payload fits u64"));
        header.set_mode(0o755);
        header.set_cksum();
        archive
            .append_data(
                &mut header,
                format!("bk-{version}-{target}/bk"),
                &content[..],
            )
            .expect("fixture tar");
        archive
            .into_inner()
            .expect("tar finished")
            .finish()
            .expect("gzip finished")
    }
    #[test]
    fn in_process_verifier_authenticates_a_real_release_and_rejects_tampering() -> TestResult {
        let bytes = include_bytes!("../tests/fixtures/sigstore/checksums-sha256.txt");
        let bundle = include_bytes!("../tests/fixtures/sigstore/checksums-sha256.txt.sigstore");
        let version = "0.1.0-rc.33".parse()?;
        verify_bytes(bytes, bundle, &version, &Sigstore)?;
        assert!(verify_bytes(b"changed", bundle, &version, &Sigstore).is_err());
        assert!(verify_bytes(bytes, bundle, &"0.1.0-rc.32".parse()?, &Sigstore).is_err());
        assert!(verify_bytes(bytes, b"{}", &version, &Sigstore).is_err());
        let mut evidence: serde_json::Value = serde_json::from_slice(bundle)?;
        evidence["verificationMaterial"]["tlogEntries"] = serde_json::json!([]);
        assert!(verify_bytes(bytes, &serde_json::to_vec(&evidence)?, &version, &Sigstore).is_err());
        Ok(())
    }

    #[test]
    fn trust_rotation_fallback_requires_an_intact_authorized_artifact() -> TestResult {
        use sigstore_verify::{CertificateError, Error, TransparencyLogError, types::Bundle};
        let bytes = include_bytes!("../tests/fixtures/sigstore/checksums-sha256.txt");
        let bundle = Bundle::from_json(include_str!(
            "../tests/fixtures/sigstore/checksums-sha256.txt.sigstore"
        ))?;
        let version = "0.1.0-rc.33".parse()?;
        let mut artifact = NamedTempFile::new()?;
        artifact.write_all(bytes)?;
        let calls = std::cell::Cell::new(0);
        let fallback = |candidate: RotationCandidate<'_>| {
            let command = candidate.cosign_command(Path::new("bundle.sigstore"));
            assert_eq!(command.get_program(), "cosign");
            assert_eq!(command.get_args().last(), Some(artifact.path().as_os_str()));
            calls.set(calls.get() + 1);
            Ok(())
        };
        let unknown_log = || {
            Error::TransparencyLog(TransparencyLogError::UnknownLog {
                log_id: "rotated-key".into(),
            })
        };
        fallback_for_rotation(unknown_log(), artifact.path(), &bundle, &version, fallback)?;
        assert_eq!(calls.get(), 1);
        assert!(
            fallback_for_rotation(
                Error::SignatureInvalid("invalid signature".into()),
                artifact.path(),
                &bundle,
                &version,
                fallback
            )
            .is_err()
        );
        assert!(
            fallback_for_rotation(
                Error::Certificate(CertificateError::ChainInvalid(
                    "invalid certificate signature".into()
                )),
                artifact.path(),
                &bundle,
                &version,
                fallback
            )
            .is_err()
        );
        assert!(
            fallback_for_rotation(
                unknown_log(),
                artifact.path(),
                &bundle,
                &"0.1.0-rc.32".parse()?,
                fallback
            )
            .is_err()
        );
        let mut invalid_signature = bundle.clone();
        if let sigstore_verify::types::SignatureContent::MessageSignature(signature) =
            &mut invalid_signature.content
        {
            signature.signature = sigstore_verify::types::SignatureBytes::from_bytes(b"invalid");
        }
        assert!(
            fallback_for_rotation(
                unknown_log(),
                artifact.path(),
                &invalid_signature,
                &version,
                fallback
            )
            .is_err()
        );
        // Even if root coverage fails first, a changed artifact must never
        // execute cosign. No PATH mutation or external executable is needed.
        fs::write(artifact.path(), b"tampered artifact")?;
        assert!(
            fallback_for_rotation(unknown_log(), artifact.path(), &bundle, &version, fallback)
                .is_err()
        );
        assert_eq!(calls.get(), 1);
        Ok(())
    }

    #[test]
    fn rotation_candidate_is_required_to_construct_the_authenticated_release_command() -> TestResult
    {
        let mut artifact = NamedTempFile::new()?;
        artifact.write_all(include_bytes!(
            "../tests/fixtures/sigstore/checksums-sha256.txt"
        ))?;
        let bundle = sigstore_verify::types::Bundle::from_json(include_str!(
            "../tests/fixtures/sigstore/checksums-sha256.txt.sigstore"
        ))?;
        let version = "0.1.0-rc.33".parse()?;
        let candidate = RotationCandidate::try_from((artifact.path(), &bundle, &version))?;
        let command = candidate.cosign_command(Path::new("release.sigstore"));
        assert_eq!(command.get_program(), "cosign");
        assert_eq!(command.get_args().collect::<Vec<_>>(), [
            "verify-blob".as_ref(), "--timeout".as_ref(), "30s".as_ref(),
            "--certificate-identity".as_ref(),
            "https://github.com/Govcraft/beankeeper-desktop/.github/workflows/release.yml@refs/tags/beankeeper-desktop-v0.1.0-rc.33".as_ref(),
            "--certificate-oidc-issuer".as_ref(), "https://token.actions.githubusercontent.com".as_ref(),
            "--bundle".as_ref(), "release.sigstore".as_ref(), artifact.path().as_os_str(),
        ]);
        Ok(())
    }

    #[test]
    fn official_verifier_pins_exact_identity_and_issuer_without_bypass_flags() -> TestResult {
        let policy = release_policy(&ReleaseVersion::from_str("1.10.0")?);
        assert_eq!(
            policy.issuer(),
            Some("https://token.actions.githubusercontent.com")
        );
        assert!(policy.verifies_tlog());
        assert!(policy.identity().is_some_and(|identity| identity.matches(
            &sigstore_verify::SubjectAltName::Uri("https://github.com/Govcraft/beankeeper-desktop/.github/workflows/release.yml@refs/tags/beankeeper-desktop-v1.10.0".into())
        )));
        assert!(ReleaseVersion::from_str("../../tag").is_err());
        Ok(())
    }
    #[test]
    fn signed_manifest_authenticates_exact_bytes_and_semantic_version() -> TestResult {
        let bytes = serde_json::to_vec(&manifest(b"fixture", "x86_64-unknown-linux-gnu"))?;
        let parsed = verify_manifest(&bytes, &sign(&bytes), None, &OfflineVerifier)?;
        assert!(parsed.is_newer_than(&installed_version("1.9.99")?));
        assert!(!parsed.is_newer_than(&installed_version("10.0.0")?));
        assert!(parsed.is_newer_than(&installed_version("1.10.0-rc.1")?));
        let mut tampered = bytes.clone();
        tampered.push(b' ');
        assert!(verify_manifest(&tampered, &sign(&bytes), None, &OfflineVerifier).is_err());
        assert!(verify_manifest(&bytes, &[], None, &OfflineVerifier).is_err());
        assert!(verify_manifest(&bytes, b"invalid bundle", None, &OfflineVerifier).is_err());
        let mut unsupported = serde_json::to_value(parsed.cache_record)?;
        unsupported["schema_version"] = 2.into();
        let bytes = serde_json::to_vec(&unsupported)?;
        assert!(verify_manifest(&bytes, &sign(&bytes), None, &OfflineVerifier).is_err());
        assert!(ReleaseVersion::from_str("1.0.0/../../payload").is_err());
        assert!(ReleaseVersion::from_str("1.0.0-rc.1").is_ok());
        assert!(installed_version("1.0.0-rc.1").is_ok());
        assert!(installed_version("1.0.0/../../payload").is_err());
        Ok(())
    }
    #[test]
    fn release_channels_use_semver_and_reject_unsupported_versions() -> TestResult {
        for version in [
            "1.0.0-beta.1",
            "1.0.0-rc",
            "1.0.0-rc.01",
            "1.0.0-rc.1.extra",
            "1.0.0-rc.1+build",
            "1.0.0+build",
            "v1.0.0",
            "../1.0.0",
        ] {
            assert!(
                ReleaseVersion::from_str(version).is_err(),
                "accepted {version}"
            );
        }
        let mut manifest = manifest(b"fixture", "x86_64-unknown-linux-gnu");
        for (candidate, current, newer) in [
            ("0.1.0-rc.10", "0.1.0-rc.2", true),
            ("0.1.0-rc.2", "0.1.0-rc.10", false),
            ("0.1.0-rc.2", "0.1.0-rc.2", false),
            ("0.1.0", "0.1.0-rc.10", true),
            ("0.1.0-rc.10", "0.1.0", false),
            ("0.2.0-rc.1", "0.1.0", false),
            ("0.2.0", "0.1.0", true),
        ] {
            manifest.version = candidate.into();
            assert_eq!(
                verified(&manifest)?.is_newer_than(&installed_version(current)?),
                newer,
                "{candidate} vs {current}"
            );
        }
        Ok(())
    }

    #[test]
    fn release_selection_ignores_order_drafts_foreign_tags_and_other_channels() -> TestResult {
        let releases: Vec<GithubRelease> = serde_json::from_value(serde_json::json!([
            {"tag_name":"beankeeper-desktop-v0.1.0-rc.2","draft":false,"prerelease":true},
            {"tag_name":"beankeeper-desktop-v0.1.0","draft":false,"prerelease":false},
            {"tag_name":"beankeeper-desktop-v9.0.0","draft":true,"prerelease":false},
            {"tag_name":"beankeeper-cli-v9.0.0","draft":false,"prerelease":false},
            {"tag_name":"beankeeper-desktop-v9.0.0-beta.1","draft":false,"prerelease":true},
            {"tag_name":"beankeeper-desktop-v9.0.0-rc.1","draft":false,"prerelease":false},
            {"tag_name":"beankeeper-desktop-v9.0.0","draft":false,"prerelease":true},
            {"tag_name":"beankeeper-desktop-v0.2.0-rc.10","draft":false,"prerelease":true},
            {"tag_name":"beankeeper-desktop-v0.2.0-rc.2","draft":false,"prerelease":true}
        ]))?;
        assert_eq!(
            select_release(&releases, &installed_version("0.1.0-rc.1")?)?.to_string(),
            "0.2.0-rc.10"
        );
        assert_eq!(
            select_release(&releases, &installed_version("0.1.0")?)?.to_string(),
            "0.1.0"
        );
        assert_eq!(
            select_release(&releases[7..], &installed_version("0.1.0-rc.1")?)?.to_string(),
            "0.2.0-rc.10"
        );
        assert!(select_release(&releases[7..], &installed_version("0.1.0")?).is_err());
        assert!(select_release(&[], &installed_version("0.1.0-rc.1")?).is_err());
        assert!(installed_version("invalid").is_err());
        Ok(())
    }

    #[test]
    fn discovery_pins_manifest_and_bundle_to_selected_tag_and_caches_rc_result() -> TestResult {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("updates.json");
        for (current, version) in [
            ("0.1.0-rc.1", "0.1.0-rc.1"),
            ("0.1.0-rc.2", "0.1.0-rc.10"),
            ("0.1.0-rc.10", "0.1.0"),
            ("0.1.0", "0.1.0"),
        ] {
            let mut manifest = manifest(b"fixture", "x86_64-unknown-linux-gnu");
            manifest.version = version.into();
            let bytes = serde_json::to_vec(&manifest)?;
            let release = serde_json::json!({
                "tag_name": format!("{TAG_PREFIX}{version}"),
                "draft": false, "prerelease": version.contains("-rc."),
                "assets": [{"browser_download_url": "https://untrusted.example/payload"}]
            });
            let metadata = if current.contains("-rc.") {
                serde_json::to_vec(&vec![release])?
            } else {
                serde_json::to_vec(&release)?
            };
            let mut urls = Vec::new();
            let fetched = fetch_manifest(
                &installed_version(current)?,
                |url, limit| {
                    assert_eq!(limit, MAX_METADATA);
                    urls.push(url.to_owned());
                    match urls.len() {
                        1 => Ok(metadata.clone()),
                        2 => Ok(bytes.clone()),
                        3 => Ok(sign(&bytes)),
                        _ => panic!("unexpected request"),
                    }
                },
                &OfflineVerifier,
            )?;
            assert_eq!(fetched.version.to_string(), version);
            let endpoint = if current.contains("-rc.") {
                format!("{RELEASE_API}?per_page=100&")
            } else {
                format!("{RELEASE_API}/latest?")
            };
            assert_eq!(
                urls,
                [
                    format!("{endpoint}version={current}&os={}", std::env::consts::OS),
                    asset_url(&ReleaseVersion::from_str(version)?, "update-manifest.json"),
                    asset_url(
                        &ReleaseVersion::from_str(version)?,
                        "update-manifest.json.sigstore"
                    ),
                ]
            );
            let checked_at = now();
            let result = check_with(
                &path,
                UpdateChecks::Enabled,
                checked_at,
                env!("CARGO_PKG_VERSION"),
                |_| Ok(fetched),
            )?;
            let AttemptResult::Complete { manifest } =
                read_cache(&path, env!("CARGO_PKG_VERSION"))?.attempt
            else {
                return Err("missing cached manifest".into());
            };
            assert_eq!(manifest.version, version);
            assert_eq!(
                check_with(
                    &path,
                    UpdateChecks::Enabled,
                    checked_at + 1,
                    env!("CARGO_PKG_VERSION"),
                    |_| panic!("daily cache ignored")
                )?,
                result
            );
            fs::remove_file(&path)?;
        }
        Ok(())
    }

    #[test]
    fn discovery_rejects_mismatched_signed_manifests_and_missing_bundles() -> TestResult {
        let mut manifest = manifest(b"fixture", "x86_64-unknown-linux-gnu");
        manifest.version = "0.1.0-rc.2".into();
        let bytes = serde_json::to_vec(&manifest)?;
        for (tag, bundle, expected) in [
            (
                "0.1.0-rc.1",
                sign(&bytes),
                "manifest version does not match release tag",
            ),
            ("0.1.0-rc.2", Vec::new(), "Sigstore verification failed"),
            (
                "0.1.0-rc.2",
                b"invalid bundle".to_vec(),
                "Sigstore verification failed",
            ),
        ] {
            let result = fetch_manifest(
                &installed_version("0.1.0-rc.1")?,
                |url, _| {
                    if url.starts_with(RELEASE_API) {
                        Ok(serde_json::to_vec(
                            &serde_json::json!([{ "tag_name": format!("{TAG_PREFIX}{tag}"), "draft": false, "prerelease": true }]),
                        )?)
                    } else if url.ends_with(".sigstore") {
                        Ok(bundle.clone())
                    } else {
                        Ok(bytes.clone())
                    }
                },
                &OfflineVerifier,
            );
            assert!(result.is_err_and(|error| error.to_string().contains(expected)));
        }
        assert!(release_policy(&ReleaseVersion::from_str("0.1.0-rc.2")?).verifies_tlog());
        Ok(())
    }
    #[test]
    fn legacy_cache_messages_map_once_into_typed_attempts() -> TestResult {
        let manifest = manifest(b"fixture", "x86_64-unknown-linux-gnu");
        for (message, previous_manifest, state, kind, expected) in [
            (
                "",
                None,
                "incomplete",
                None,
                "The last update check has not completed.",
            ),
            (
                "Update check did not complete.",
                Some(&manifest),
                "incomplete",
                None,
                "The last update check has not completed.",
            ),
            (
                "complete",
                Some(&manifest),
                "complete",
                None,
                "Desktop 1.10.0 is available.",
            ),
            (
                "Desktop 1.10.0 is available. Use the signed installer.",
                Some(&manifest),
                "complete",
                None,
                "Desktop 1.10.0 is available.",
            ),
            (
                "Desktop is up to date.",
                Some(&manifest),
                "complete",
                None,
                "Desktop 1.10.0 is available.",
            ),
            (
                "update download failed; check connectivity and retry after the daily interval",
                None,
                "failed",
                Some("legacy_download"),
                "Could not check for updates: the release server could not be reached or returned an unsuccessful response. Checks run at most once per day.",
            ),
            (
                "the release server could not be reached",
                None,
                "failed",
                Some("legacy"),
                "Could not check for updates: the release server could not be reached. Checks run at most once per day.",
            ),
            (
                "complete",
                None,
                "failed",
                Some("legacy"),
                "Could not check for updates: complete. Checks run at most once per day.",
            ),
        ] {
            let cache: Cache = serde_json::from_value(serde_json::json!({
                "desktop_version": "1.0.0",
                "last_attempt": DAY,
                "manifest": previous_manifest,
                "message": message,
            }))?;
            let status = UpdateStatus::try_from(&cache)?.to_string();
            assert!(status.starts_with(expected), "{status}");
            let typed = serde_json::to_value(&cache)?;
            assert_eq!(typed["attempt"]["state"], state);
            if let Some(kind) = kind {
                assert_eq!(typed["attempt"]["failure"]["kind"], kind);
            }
            assert!(typed.get("message").is_none());
            assert!(typed.get("manifest").is_none());
            let restored: Cache = serde_json::from_value(typed)?;
            assert_eq!(UpdateStatus::try_from(&restored)?.to_string(), status);
        }
        Ok(())
    }

    #[test]
    fn update_failures_keep_their_kind_and_wording_through_cache_serialization() -> TestResult {
        let errors = [
            UpdateError::Io(io::Error::new(io::ErrorKind::PermissionDenied, "fixture")),
            UpdateError::Network,
            UpdateError::Http(reqwest::StatusCode::NOT_FOUND),
            UpdateError::Invalid("fixture reason"),
            UpdateError::LicenseRequired(MajorVersion::try_from(1)?),
            UpdateError::Signature,
            UpdateError::Json(
                serde_json::from_str::<Manifest>("invalid")
                    .err()
                    .ok_or("fixture JSON error")?,
            ),
        ];
        for error in errors {
            let expected = error.to_string();
            let failure = CheckFailure::from(error);
            let cache = Cache {
                desktop_version: "1.0.0".into(),
                last_attempt: DAY,
                attempt: AttemptResult::Failed { failure },
            };
            let typed = serde_json::to_value(&cache)?;
            assert_ne!(typed["attempt"]["failure"]["kind"], "legacy");
            let restored: Cache = serde_json::from_value(typed)?;
            let AttemptResult::Failed { failure } = restored.attempt else {
                return Err("failure changed state".into());
            };
            assert_eq!(failure.to_string(), expected);
        }
        assert!(
            serde_json::from_value::<Cache>(serde_json::json!({
                "desktop_version": "1.0.0", "last_attempt": DAY,
                "attempt": { "state": "complete" }
            }))
            .is_err()
        );
        assert!(
            serde_json::from_value::<CheckFailure>(serde_json::json!({
                "kind": "http", "status": 0
            }))
            .is_err()
        );
        Ok(())
    }

    #[test]
    fn legacy_status_is_unchecked_without_changing_cache() -> TestResult {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("updates.json");
        // Exact cache shape and failure message written by Desktop 0.1.0-rc.1.
        let bytes = br#"{"last_attempt":1795226804,"manifest":null,"message":"update download failed; check connectivity and retry after the daily interval"}"#;
        fs::write(&path, bytes)?;
        assert_eq!(
            status(&path, UpdateChecks::Enabled)?,
            "Updates have not been checked. Run `bk update-check`."
        );
        assert_eq!(fs::read(&path)?, bytes);
        Ok(())
    }

    #[test]
    fn legacy_and_other_version_caches_refresh_once_without_waiting_a_day() -> TestResult {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("updates.json");
        let current = "0.1.0-rc.7";
        let attempted_at = 1_795_226_804;
        for previous in [None, Some("0.1.0-rc.1"), Some("0.1.0-rc.10"), Some("0.1.0")] {
            // Exercise stale failures and successful update recommendations,
            // including a downgrade and a switch between stable/RC channels.
            for message in [
                "the release server could not be reached",
                "Desktop 0.1.0-rc.7 is available. Use the signed installer.",
            ] {
                let mut cached_manifest = manifest(b"fixture", "x86_64-unknown-linux-gnu");
                cached_manifest.version = current.into();
                let mut cache = serde_json::json!({
                    "last_attempt": attempted_at,
                    "manifest": message.starts_with("Desktop").then_some(&cached_manifest),
                    "message": message,
                });
                if let Some(previous) = previous {
                    cache["desktop_version"] = previous.into();
                }
                let bytes = serde_json::to_vec(&cache)?;
                fs::write(&path, &bytes)?;
                assert_eq!(
                    status_with(&path, UpdateChecks::Enabled, current)?,
                    "Updates have not been checked. Run `bk update-check`."
                );
                assert_eq!(fs::read(&path)?, bytes);

                let result = check_with(
                    &path,
                    UpdateChecks::Enabled,
                    attempted_at + 1,
                    current,
                    |_| {
                        let pending = read_cache(&path, current)?;
                        assert_eq!(pending.desktop_version, current);
                        assert!(matches!(pending.attempt, AttemptResult::Incomplete));
                        verified(&cached_manifest)
                    },
                )?;
                assert!(result.starts_with("Desktop is up to date."));
                assert_eq!(status_with(&path, UpdateChecks::Enabled, current)?, result);
                let persisted: serde_json::Value = serde_json::from_slice(&fs::read(&path)?)?;
                assert_eq!(persisted["desktop_version"], current);
                assert_eq!(persisted["last_attempt"], attempted_at + 1);
                for time in [attempted_at, attempted_at + 2, attempted_at + DAY] {
                    assert_eq!(
                        check_with(&path, UpdateChecks::Enabled, time, current, |_| panic!(
                            "same version retried within a day"
                        ))?,
                        result
                    );
                }
            }
        }
        Ok(())
    }

    #[test]
    fn version_changes_refresh_failed_checks_but_same_version_remains_rate_limited() -> TestResult {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("updates.json");
        let attempted_at = 1_795_226_804;
        for current in ["0.1.0-rc.1", "0.1.0-rc.7", "0.1.0", "0.2.0-rc.1"] {
            let result = check_with(&path, UpdateChecks::Enabled, attempted_at, current, |_| {
                Err(UpdateError::Network)
            })?;
            assert!(result.starts_with("Could not check for updates:"));
            assert_eq!(read_cache(&path, current)?.desktop_version, current);
            assert!(matches!(
                read_cache(&path, current)?.attempt,
                AttemptResult::Failed { .. }
            ));
            assert_eq!(
                check_with(
                    &path,
                    UpdateChecks::Enabled,
                    attempted_at + 1,
                    current,
                    |_| panic!("failed check retried within a day")
                )?,
                result
            );
            assert!(
                check_with(
                    &path,
                    UpdateChecks::Enabled,
                    attempted_at + DAY,
                    current,
                    |_| { Err(UpdateError::Signature) }
                )?
                .contains("Sigstore verification failed")
            );
        }
        Ok(())
    }

    #[test]
    fn unchecked_and_disabled_status_need_no_valid_cache() -> TestResult {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("updates.json");
        assert_eq!(
            status(&path, UpdateChecks::Enabled)?,
            "Updates have not been checked. Run `bk update-check`."
        );
        assert!(!path.exists());
        fs::write(&path, b"invalid cache")?;
        assert_eq!(
            status(&path, UpdateChecks::Disabled)?,
            "Automatic update checks are disabled."
        );
        assert!(status(&path, UpdateChecks::Enabled).is_err());
        Ok(())
    }

    #[test]
    fn update_result_serializes_outcome_and_version_without_rendered_prose() -> TestResult {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("updates.json");
        let result = check_status_with(&path, UpdateChecks::Enabled, DAY, "0.1.0-rc.33", |_| {
            verified(&manifest(b"fixture", "x86_64-unknown-linux-gnu"))
        })?;
        let data = serde_json::to_value(&result)?;
        assert_eq!(data["state"], "checked");
        assert_eq!(data["last_attempt"], DAY);
        assert_eq!(data["outcome"]["state"], "available");
        assert_eq!(data["outcome"]["version"], "1.10.0");
        assert!(data["outcome"].get("message").is_none());
        assert!(
            result
                .to_string()
                .starts_with("Desktop 1.10.0 is available.")
        );
        Ok(())
    }

    #[test]
    fn incomplete_attempt_and_invalid_timestamp_do_not_claim_success() -> TestResult {
        let mut cache = Cache {
            desktop_version: env!("CARGO_PKG_VERSION").into(),
            last_attempt: 1_795_226_804,
            attempt: AttemptResult::Incomplete,
        };
        assert_eq!(
            UpdateStatus::try_from(&cache)?.to_string(),
            "The last update check has not completed. It may still be running or may have been interrupted.\nLast attempt: 2026-11-21 02:06:44 UTC."
        );
        for seconds in [i64::MAX.unsigned_abs(), u64::MAX] {
            cache.last_attempt = seconds;
            let status = UpdateStatus::try_from(&cache)?.to_string();
            assert!(status.ends_with("Last attempt: unavailable (invalid cached time)."));
            assert!(!status.contains(&seconds.to_string()));
        }
        Ok(())
    }

    #[test]
    fn completed_checks_distinguish_success_http_network_and_verification_failures() -> TestResult {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("updates.json");
        let available = manifest(b"fixture", "x86_64-unknown-linux-gnu");
        let mut older = available.clone();
        older.version = "0.0.0".into();
        let cases = [
            (
                verified(&available),
                "Desktop 1.10.0 is available. Run `bk self-update` with a license that covers 1.x releases.",
            ),
            (verified(&older), "Desktop is up to date."),
            (
                Err(UpdateError::Http(reqwest::StatusCode::NOT_FOUND)),
                "Could not check for updates: the release file was not found (HTTP 404). Checks run at most once per day.",
            ),
            (
                Err(UpdateError::Http(reqwest::StatusCode::SERVICE_UNAVAILABLE)),
                "Could not check for updates: the release server returned HTTP 503. Checks run at most once per day.",
            ),
            (
                Err(UpdateError::Network),
                "Could not check for updates: the release server could not be reached. Checks run at most once per day.",
            ),
            (
                Err(UpdateError::Signature),
                "Could not check for updates: Sigstore verification failed; a valid signed release bundle is required. Checks run at most once per day.",
            ),
        ];
        for (day, (result, expected)) in cases.into_iter().enumerate() {
            let now = 1_795_226_804 + u64::try_from(day)? * DAY;
            let expected = format!(
                "{expected}\nLast attempt: {} UTC.",
                DateTime::<Utc>::from_timestamp(i64::try_from(now)?, 0)
                    .ok_or("fixture timestamp")?
                    .format("%Y-%m-%d %H:%M:%S")
            );
            assert_eq!(
                check_with(
                    &path,
                    UpdateChecks::Enabled,
                    now,
                    env!("CARGO_PKG_VERSION"),
                    |_| result
                )?,
                expected
            );
            assert_eq!(status(&path, UpdateChecks::Enabled)?, expected);
            assert_eq!(
                check_with(
                    &path,
                    UpdateChecks::Enabled,
                    now + 1,
                    env!("CARGO_PKG_VERSION"),
                    |_| panic!("cached status fetched again")
                )?,
                expected
            );
        }
        Ok(())
    }

    #[test]
    fn opt_out_daily_cache_failure_and_clock_rollback_make_no_extra_requests() -> TestResult {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("state/updates.json");
        assert!(
            check_with(
                &path,
                UpdateChecks::Disabled,
                DAY,
                env!("CARGO_PKG_VERSION"),
                |_| panic!("opt-out contacted network")
            )?
            .contains("disabled")
        );
        assert!(!path.exists());
        let result = check_with(
            &path,
            UpdateChecks::Enabled,
            DAY,
            env!("CARGO_PKG_VERSION"),
            |_| verified(&manifest(b"fixture", "x86_64-unknown-linux-gnu")),
        )?;
        assert!(result.contains("1.10.0 is available"));
        for now in [DAY, DAY + 1, 1, 2 * DAY - 1] {
            assert!(
                check_with(
                    &path,
                    UpdateChecks::Enabled,
                    now,
                    env!("CARGO_PKG_VERSION"),
                    |_| panic!("cache contacted network")
                )?
                .contains("1.10.0 is available")
            );
        }
        assert!(
            check_with(
                &path,
                UpdateChecks::Enabled,
                2 * DAY,
                env!("CARGO_PKG_VERSION"),
                |_| Err(UpdateError::Network)
            )?
            .contains("Could not check for updates: the release server could not be reached")
        );
        assert!(matches!(
            read_cache(&path, env!("CARGO_PKG_VERSION"))?.attempt,
            AttemptResult::Failed { .. }
        ));
        check_with(
            &path,
            UpdateChecks::Enabled,
            2 * DAY + 1,
            env!("CARGO_PKG_VERSION"),
            |_| panic!("failed check was retried early"),
        )?;
        assert!(
            check_with(
                &path,
                UpdateChecks::Disabled,
                3 * DAY,
                env!("CARGO_PKG_VERSION"),
                |_| panic!("opt-out ignored")
            )?
            .contains("disabled")
        );
        Ok(())
    }
    #[test]
    fn concurrent_check_lock_avoids_network() -> TestResult {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("updates.json");
        fs::write(path.with_extension("lock"), b"")?;
        assert!(
            check_with(
                &path,
                UpdateChecks::Enabled,
                DAY,
                env!("CARGO_PKG_VERSION"),
                |_| panic!("concurrent check contacted network")
            )?
            .contains("in progress")
        );
        Ok(())
    }
    #[test]
    fn installation_requires_valid_bundles_and_both_checksums_and_preserves_old_binary()
    -> TestResult {
        let directory = tempfile::tempdir()?;
        let destination = directory.path().join("bk");
        fs::write(&destination, b"old executable")?;
        let target = Target::LinuxX64;
        let archive = archive("1.10.0", target.triple());
        let wire = manifest(&archive, target.triple());
        let manifest = authorize(&wire, "1.9.0")?;
        let checksums = format!(
            "{}  {}\n",
            sha256(&archive),
            archive_name(&ReleaseVersion::from_str("1.10.0")?, target)
        );
        let bundle = sign(&archive);
        let checksums_bundle = sign(checksums.as_bytes());
        for (artifact_signature, checksum_signature, list) in [
            (&[][..], &checksums_bundle[..], checksums.as_bytes()),
            (&bundle[..], &[][..], checksums.as_bytes()),
            (&b"invalid"[..], &checksums_bundle[..], checksums.as_bytes()),
            (&bundle[..], &b"invalid"[..], checksums.as_bytes()),
            (&bundle[..], &checksums_bundle[..], &b"modified list"[..]),
        ] {
            assert!(
                install(
                    &manifest,
                    target,
                    &ReleaseFiles {
                        archive: &archive,
                        bundle: artifact_signature,
                        checksums: list,
                        checksums_bundle: checksum_signature
                    },
                    &destination,
                    &OfflineVerifier
                )
                .is_err()
            );
            assert_eq!(fs::read(&destination)?, b"old executable");
        }
        // Correctly signed checksum lists can still disagree with the artifact.
        for list in [
            format!(
                "{}  {}\n",
                "0".repeat(64),
                archive_name(&ReleaseVersion::from_str("1.10.0")?, target)
            ),
            checksums.repeat(2),
            String::new(),
        ] {
            assert!(
                install(
                    &manifest,
                    target,
                    &ReleaseFiles {
                        archive: &archive,
                        bundle: &bundle,
                        checksums: list.as_bytes(),
                        checksums_bundle: &sign(list.as_bytes())
                    },
                    &destination,
                    &OfflineVerifier
                )
                .is_err()
            );
            assert_eq!(fs::read(&destination)?, b"old executable");
        }
        let files = ReleaseFiles {
            archive: &archive,
            bundle: &bundle,
            checksums: checksums.as_bytes(),
            checksums_bundle: &checksums_bundle,
        };
        install(&manifest, target, &files, &destination, &OfflineVerifier)?;
        assert_eq!(fs::read(&destination)?, b"fixture executable");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&destination)?.permissions().mode() & 0o777,
                0o755
            );
        }
        assert_eq!(directory.path().read_dir()?.count(), 1);
        Ok(())
    }
    #[test]
    fn install_accepts_either_case_for_signed_hexadecimal_checksums() -> TestResult {
        let directory = tempfile::tempdir()?;
        let destination = directory.path().join("bk");
        let target = Target::LinuxX64;
        let archive = archive("1.10.0", target.triple());
        let mut wire = manifest(&archive, target.triple());
        let artifact = wire
            .artifacts
            .get_mut(target.triple())
            .ok_or("fixture artifact")?;
        artifact.sha256.make_ascii_uppercase();
        let name = archive_name(&ReleaseVersion::from_str("1.10.0")?, target);
        let checksums = format!("{}  {name}\n", artifact.sha256);
        let release = authorize(&wire, "1.9.0")?;
        install(
            &release,
            target,
            &ReleaseFiles {
                archive: &archive,
                bundle: &sign(&archive),
                checksums: checksums.as_bytes(),
                checksums_bundle: &sign(checksums.as_bytes()),
            },
            &destination,
            &OfflineVerifier,
        )?;
        assert_eq!(fs::read(&destination)?, b"fixture executable");
        Ok(())
    }

    #[test]
    fn signed_release_requires_a_newer_version_and_exact_major_regardless_of_dates() -> TestResult {
        let mut wire = manifest(b"fixture", Target::LinuxX64.triple());
        let license = fixture_license(include_bytes!("../tests/fixtures/license-v4.public.txt"))?;
        let current = installed_version("0.1.0-rc.2")?;
        for date in ["1990-01-01T00:00:00Z", "2090-01-01T00:00:00Z"] {
            wire.released_at = date.parse()?;
            for version in ["1.0.0", "1.4.2", "1.99.0-rc.1"] {
                wire.version = version.into();
                verified(&wire)?
                    .require_newer(&current)?
                    .authorize(std::slice::from_ref(&license))?;
            }
            wire.version = "2.0.0".into();
            let error = verified(&wire)?
                .require_newer(&current)?
                .authorize(std::slice::from_ref(&license))
                .err()
                .ok_or("major 2 should be refused")?;
            assert!(matches!(error, UpdateError::LicenseRequired(_)));
            assert!(error.to_string().contains("covers 2.x releases"));
        }
        wire.version = "1.10.0".into();
        assert!(
            verified(&wire)?
                .require_newer(&installed_version("1.10.0")?)
                .is_err()
        );
        for version in ["0.1.0-rc.3", "4294967296.0.0"] {
            wire.version = version.into();
            assert!(
                verified(&wire)?
                    .require_newer(&current)?
                    .authorize(std::slice::from_ref(&license))
                    .is_err()
            );
        }
        Ok(())
    }

    #[test]
    fn any_active_license_can_authorize_an_installation_update() -> TestResult {
        let major_one = fixture_license(include_bytes!("../tests/fixtures/license-v4.public.txt"))?;
        let major_two = fixture_license(include_bytes!(
            "../tests/fixtures/license-major-2-v4.public.txt"
        ))?;
        let mut wire = manifest(b"fixture", Target::LinuxX64.triple());
        let current = installed_version("1.0.0")?;
        assert!(matches!(
            verified(&wire)?.require_newer(&current)?.authorize(&[]),
            Err(UpdateError::LicenseRequired(_))
        ));
        let licenses = [major_one, major_two];
        for version in ["1.10.0", "2.0.0"] {
            wire.version = version.into();
            verified(&wire)?
                .require_newer(&current)?
                .authorize(&licenses)?;
        }
        wire.version = "3.0.0".into();
        assert!(matches!(
            verified(&wire)?
                .require_newer(&current)?
                .authorize(&licenses),
            Err(UpdateError::LicenseRequired(_))
        ));
        Ok(())
    }

    #[test]
    fn rc_release_installs_only_after_manifest_archive_and_checksum_verification() -> TestResult {
        let directory = tempfile::tempdir()?;
        let destination = directory.path().join("bk");
        fs::write(&destination, b"old executable")?;
        let version = "1.0.0-rc.10";
        let target = Target::LinuxX64;
        let archive = archive(version, target.triple());
        let mut manifest = manifest(&archive, target.triple());
        manifest.version = version.into();
        let manifest = authorize(&manifest, "1.0.0-rc.2")?;
        let checksums = format!(
            "{}  {}\n",
            sha256(&archive),
            archive_name(&ReleaseVersion::from_str(version)?, target)
        );
        for bundle in [b"invalid".to_vec(), sign(&archive)] {
            let result = install(
                &manifest,
                target,
                &ReleaseFiles {
                    archive: &archive,
                    bundle: &bundle,
                    checksums: checksums.as_bytes(),
                    checksums_bundle: &sign(checksums.as_bytes()),
                },
                &destination,
                &OfflineVerifier,
            );
            if bundle == b"invalid" {
                assert!(result.is_err());
                assert_eq!(fs::read(&destination)?, b"old executable");
            } else {
                result?;
                assert_eq!(fs::read(&destination)?, b"fixture executable");
            }
        }
        Ok(())
    }
    #[test]
    fn corrupt_archive_and_wrong_target_leave_destination_intact() -> TestResult {
        let directory = tempfile::tempdir()?;
        let destination = directory.path().join("bk");
        fs::write(&destination, b"old executable")?;
        let target = Target::LinuxX64;
        for bytes in [
            b"invalid tar".to_vec(),
            archive("1.10.0", "aarch64-apple-darwin"),
        ] {
            let manifest = authorize(&manifest(&bytes, target.triple()), "1.9.0")?;
            let list = format!(
                "{}  {}\n",
                sha256(&bytes),
                archive_name(&ReleaseVersion::from_str("1.10.0")?, target)
            );
            assert!(
                install(
                    &manifest,
                    target,
                    &ReleaseFiles {
                        archive: &bytes,
                        bundle: &sign(&bytes),
                        checksums: list.as_bytes(),
                        checksums_bundle: &sign(list.as_bytes())
                    },
                    &destination,
                    &OfflineVerifier
                )
                .is_err()
            );
            assert_eq!(fs::read(&destination)?, b"old executable");
        }
        Ok(())
    }
    #[test]
    fn windows_zip_selects_only_expected_regular_binary() -> TestResult {
        let mut archive = zip::ZipWriter::new(io::Cursor::new(Vec::new()));
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        archive.start_file("ignored", options)?;
        archive.write_all(b"must not install")?;
        archive.start_file("bk-1.10.0-x86_64-pc-windows-msvc/bk.exe", options)?;
        archive.write_all(b"Windows executable fixture")?;
        let bytes = archive.finish()?.into_inner();
        let mut output = NamedTempFile::new()?;
        extract_binary(
            &bytes,
            Target::WindowsX64,
            "bk-1.10.0-x86_64-pc-windows-msvc/bk.exe",
            &mut output,
        )?;
        assert_eq!(fs::read(output.path())?, b"Windows executable fixture");
        Ok(())
    }
}
