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
                "This update requires a Desktop license that covers {major}.x releases. Paste your license with `bk license activate --token TOKEN` or supply a license file."
            ),
            Self::Signature => f.write_str(
                "Sigstore verification failed; install cosign and require a valid release bundle",
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

/// Uses cosign from PATH with fixed trust identity and no shell interpolation.
struct Cosign;
impl BundleVerifier for Cosign {
    fn verify(
        &self,
        artifact: &Path,
        bundle: &Path,
        version: &ReleaseVersion,
    ) -> Result<(), UpdateError> {
        if !bundle.is_file() || fs::metadata(bundle)?.len() == 0 {
            return Err(UpdateError::Signature);
        }
        let status = cosign_command(artifact, bundle, version)
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
    }
}

fn cosign_command(artifact: &Path, bundle: &Path, version: &ReleaseVersion) -> Command {
    let identity =
        format!("{REPOSITORY}/.github/workflows/release.yml@refs/tags/{TAG_PREFIX}{version}");
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
struct Cache {
    #[serde(default)]
    desktop_version: String,
    last_attempt: u64,
    manifest: Option<Manifest>,
    message: String,
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

fn cached_status(cache: &Cache) -> String {
    let message = match cache.message.as_str() {
        "" | "Update check did not complete." => {
            "The last update check has not completed. It may still be running or may have been interrupted."
                .into()
        }
        // Older caches lost the HTTP status. Do not imply a connectivity problem
        // when the response could have been a missing release feed instead.
        "update download failed; check connectivity and retry after the daily interval" => {
            "Could not check for updates: the release server could not be reached or returned an unsuccessful response. Checks run at most once per day."
                .into()
        }
        message if cache.manifest.is_some() => message.to_owned(),
        message => format!(
            "Could not check for updates: {}. Checks run at most once per day.",
            message.trim_end_matches('.')
        ),
    };
    let attempted_at = i64::try_from(cache.last_attempt)
        .ok()
        .and_then(|seconds| DateTime::<Utc>::from_timestamp(seconds, 0))
        .map_or_else(
            || "unavailable (invalid cached time)".into(),
            |time| time.format("%Y-%m-%d %H:%M:%S UTC").to_string(),
        );
    format!("{message}\nLast attempt: {attempted_at}.")
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
    if checks == UpdateChecks::Disabled {
        return Ok("Automatic update checks are disabled.".into());
    }
    let cache = read_cache(path, current)?;
    if cache.last_attempt == 0 {
        return Ok("Updates have not been checked. Run `bk update-check`.".into());
    }
    Ok(cached_status(&cache))
}
/// At most one network attempt per day per Desktop version, including failures
/// and process restarts. Legacy caches are refreshed on the next check.
/// Opt-out is evaluated before files or the network are accessed.
///
/// # Errors
/// Returns errors when cached update state cannot be read or written.
pub fn check(path: &Path, checks: UpdateChecks, now: u64) -> Result<String, UpdateError> {
    check_with(path, checks, now, env!("CARGO_PKG_VERSION"), |current| {
        fetch_manifest(current, download, &Cosign)
    })
}
fn check_with(
    path: &Path,
    checks: UpdateChecks,
    now: u64,
    current: &str,
    fetch: impl FnOnce(&semver::Version) -> Result<VerifiedManifest, UpdateError>,
) -> Result<String, UpdateError> {
    if checks == UpdateChecks::Disabled {
        return status_with(path, UpdateChecks::Disabled, current);
    }
    let mut cache = read_cache(path, current)?;
    if cache.last_attempt != 0 && now.saturating_sub(cache.last_attempt) < DAY {
        return status_with(path, UpdateChecks::Enabled, current);
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
            return Ok("An update check is already in progress; retry later.".into());
        }
        Err(e) => return Err(e.into()),
    }
    let result = (|| {
        // Re-read under lock to handle a concurrent completed checker.
        cache = read_cache(path, current)?;
        if cache.last_attempt != 0 && now.saturating_sub(cache.last_attempt) < DAY {
            return status_with(path, UpdateChecks::Enabled, current);
        }
        current.clone_into(&mut cache.desktop_version);
        cache.last_attempt = now.max(1);
        cache.message = "Update check did not complete.".into();
        write_cache(path, &cache)?;
        let installed = installed_version(current)?;
        match fetch(&installed) {
            Ok(manifest) => {
                cache.message = if !manifest.is_newer_than(&installed) {
                    "Desktop is up to date.".into()
                } else if let Some(major) = manifest.version.license_major() {
                    format!(
                        "Desktop {} is available. Run `bk self-update` with a license that covers {major}.x releases.",
                        manifest.version
                    )
                } else {
                    format!(
                        "Desktop {} is available. Use the signed installer.",
                        manifest.version
                    )
                };
                cache.manifest = Some(manifest.cache_record);
            }
            Err(error) => {
                cache.message = error.to_string();
                cache.manifest = None;
            }
        }
        write_cache(path, &cache)?;
        status_with(path, UpdateChecks::Enabled, current)
    })();
    let _ = fs::remove_file(lock);
    result
}

/// Download only the matching archive and mandatory D13 checksum/bundle files.
/// Re-verifies cached manifest bytes via their signed release asset before install.
///
/// # Errors
/// Returns errors for unsupported targets, unavailable releases, ineligible licenses, or invalid artifacts.
pub fn self_update(cache: &Path, destination: &Path) -> Result<(), UpdateError> {
    let cached = read_cache(cache, env!("CARGO_PKG_VERSION"))?
        .manifest
        .ok_or(UpdateError::Invalid(
            "run bk update-check successfully first",
        ))?;
    let version = cached.version.parse()?;
    let bytes = download(&asset_url(&version, "update-manifest.json"), MAX_METADATA)?;
    let bundle = download(
        &asset_url(&version, "update-manifest.json.sigstore"),
        MAX_METADATA,
    )?;
    let current = installed_version(env!("CARGO_PKG_VERSION"))?;
    let release =
        verify_manifest(&bytes, &bundle, Some(version), &Cosign)?.require_newer(&current)?;
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
        &Cosign,
    )
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
    // always invokes cosign to validate Fulcio identity and transparency evidence.
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
    fn official_verifier_pins_exact_identity_and_issuer_without_bypass_flags() -> TestResult {
        let command = cosign_command(
            Path::new("archive"),
            Path::new("bundle"),
            &ReleaseVersion::from_str("1.10.0")?,
        );
        let arguments: Vec<_> = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            arguments,
            [
                "verify-blob",
                "--timeout",
                "30s",
                "--certificate-identity",
                "https://github.com/Govcraft/beankeeper-desktop/.github/workflows/release.yml@refs/tags/beankeeper-desktop-v1.10.0",
                "--certificate-oidc-issuer",
                "https://token.actions.githubusercontent.com",
                "--bundle",
                "bundle",
                "archive"
            ]
        );
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
            assert_eq!(
                read_cache(&path, env!("CARGO_PKG_VERSION"))?
                    .manifest
                    .ok_or("missing cached manifest")?
                    .version,
                version
            );
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
        let command = cosign_command(
            Path::new("archive"),
            Path::new("bundle"),
            &ReleaseVersion::from_str("0.1.0-rc.2")?,
        );
        assert!(command.get_args().any(|arg| arg == format!("{REPOSITORY}/.github/workflows/release.yml@refs/tags/beankeeper-desktop-v0.1.0-rc.2").as_str()));
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
                        assert_eq!(pending.message, "Update check did not complete.");
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
            assert!(read_cache(&path, current)?.manifest.is_none());
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
    fn incomplete_attempt_and_invalid_timestamp_do_not_claim_success() {
        let mut cache = Cache {
            desktop_version: env!("CARGO_PKG_VERSION").into(),
            last_attempt: 1_795_226_804,
            // A previous successful manifest may remain during a new attempt.
            manifest: Some(manifest(b"fixture", "x86_64-unknown-linux-gnu")),
            message: "Update check did not complete.".into(),
        };
        assert_eq!(
            cached_status(&cache),
            "The last update check has not completed. It may still be running or may have been interrupted.\nLast attempt: 2026-11-21 02:06:44 UTC."
        );
        for seconds in [i64::MAX.unsigned_abs(), u64::MAX] {
            cache.last_attempt = seconds;
            let status = cached_status(&cache);
            assert!(status.ends_with("Last attempt: unavailable (invalid cached time)."));
            assert!(!status.contains(&seconds.to_string()));
        }
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
                "Could not check for updates: Sigstore verification failed; install cosign and require a valid release bundle. Checks run at most once per day.",
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
        assert!(
            read_cache(&path, env!("CARGO_PKG_VERSION"))?
                .manifest
                .is_none()
        );
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
