//! Signed release discovery and fail-closed installation. No ledger access.

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
    time::Duration,
};
use tempfile::NamedTempFile;

const REPOSITORY: &str = "https://github.com/Govcraft/beankeeper-desktop";
const DAY: u64 = 86_400;
const MAX_METADATA: u64 = 2 * 1024 * 1024;
const MAX_ARCHIVE: u64 = 256 * 1024 * 1024;
const MAX_BINARY: u64 = 512 * 1024 * 1024;

/// Update failures never contain HTTP bodies, credentials or ledger content.
#[derive(Debug)]
pub enum UpdateError {
    Io(io::Error),
    Network,
    Invalid(&'static str),
    Signature,
    Json(serde_json::Error),
}
impl fmt::Display for UpdateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(e) => write!(f, "update file operation failed: {e}"),
            Self::Network => f.write_str(
                "update download failed; check connectivity and retry after the daily interval",
            ),
            Self::Invalid(reason) => write!(f, "update refused: {reason}"),
            Self::Signature => f.write_str(
                "Sigstore verification failed; install cosign and require a valid release bundle",
            ),
            Self::Json(_) => f.write_str("invalid update manifest or cache"),
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

/// Signed metadata. The detached bundle authenticates the exact JSON bytes.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub schema_version: u32,
    pub version: String,
    pub released_at: DateTime<Utc>,
    pub artifacts: BTreeMap<String, Artifact>,
}
/// SHA-256 of the archive named by the release layout contract.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Artifact {
    pub sha256: String,
}

fn release_version(value: &str) -> Result<semver::Version, UpdateError> {
    let version =
        semver::Version::parse(value).map_err(|_| UpdateError::Invalid("invalid version"))?;
    if !version.pre.is_empty() || !version.build.is_empty() {
        return Err(UpdateError::Invalid("only stable releases are supported"));
    }
    Ok(version)
}
impl Manifest {
    fn parse(bytes: &[u8]) -> Result<Self, UpdateError> {
        let manifest: Self = serde_json::from_slice(bytes)?;
        release_version(&manifest.version)?;
        if manifest.schema_version != 1
            || manifest.released_at.timestamp() <= 0
            || manifest.artifacts.is_empty()
            || manifest
                .artifacts
                .values()
                .any(|a| a.sha256.len() != 64 || !a.sha256.bytes().all(|b| b.is_ascii_hexdigit()))
        {
            return Err(UpdateError::Invalid("unsupported manifest or checksum"));
        }
        Ok(manifest)
    }
    /// Compare versions numerically, including multi-digit minor versions.
    pub fn newer_than(&self, current: &str) -> Result<bool, UpdateError> {
        Ok(release_version(&self.version)? > release_version(current)?)
    }
}

/// Verify release blobs against one exact workflow/tag and GitHub's OIDC issuer.
/// Tests inject an offline verifier at this boundary; CLI always uses `Cosign`.
pub trait BundleVerifier {
    fn verify(&self, artifact: &Path, bundle: &Path, version: &str) -> Result<(), UpdateError>;
}
/// Uses cosign from PATH, without shell interpolation or insecure fallbacks.
pub struct Cosign;
impl BundleVerifier for Cosign {
    fn verify(&self, artifact: &Path, bundle: &Path, version: &str) -> Result<(), UpdateError> {
        release_version(version)?;
        if !bundle.is_file() || fs::metadata(bundle)?.len() == 0 {
            return Err(UpdateError::Signature);
        }
        let status = cosign_command(artifact, bundle, version)?
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

fn cosign_command(artifact: &Path, bundle: &Path, version: &str) -> Result<Command, UpdateError> {
    release_version(version)?;
    let identity =
        format!("{REPOSITORY}/.github/workflows/release.yml@refs/tags/beankeeper-cli-v{version}");
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
    Ok(command)
}

fn verify_bytes(
    bytes: &[u8],
    bundle: &[u8],
    version: &str,
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
/// Parse constrained metadata to select a tag, then authenticate its exact bytes.
pub fn verify_manifest(
    bytes: &[u8],
    bundle: &[u8],
    verifier: &dyn BundleVerifier,
) -> Result<Manifest, UpdateError> {
    let manifest = Manifest::parse(bytes)?;
    verify_bytes(bytes, bundle, &manifest.version, verifier)?;
    Ok(manifest)
}

/// Explicit supported targets match the packaging matrix.
pub fn target() -> Result<&'static str, UpdateError> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") => Ok("x86_64-unknown-linux-gnu"),
        ("linux", "aarch64") => Ok("aarch64-unknown-linux-gnu"),
        ("macos", "x86_64") => Ok("x86_64-apple-darwin"),
        ("macos", "aarch64") => Ok("aarch64-apple-darwin"),
        ("windows", "x86_64") => Ok("x86_64-pc-windows-msvc"),
        _ => Err(UpdateError::Invalid("unsupported update platform")),
    }
}
fn archive_name(version: &str, target: &str) -> String {
    let extension = if target.contains("windows") {
        "zip"
    } else {
        "tar.gz"
    };
    format!("bk-{version}-{target}.{extension}")
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
        .map_err(|_| UpdateError::Network)?;
    let mut bytes = Vec::new();
    response.take(limit + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit {
        return Err(UpdateError::Invalid("download exceeds size limit"));
    }
    Ok(bytes)
}
fn asset_url(version: &str, name: &str) -> String {
    format!("{REPOSITORY}/releases/download/beankeeper-cli-v{version}/{name}")
}

#[derive(Default, Deserialize, Serialize)]
struct Cache {
    last_attempt: u64,
    manifest: Option<Manifest>,
    message: String,
}
fn read_cache(path: &Path) -> Result<Cache, UpdateError> {
    match fs::read(path) {
        Ok(bytes) => Ok(serde_json::from_slice(&bytes)?),
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
/// Last checked state, read without any network request. Opt-out hides stale results.
pub fn status(path: &Path, disabled: bool) -> Result<String, UpdateError> {
    if disabled {
        return Ok("Automatic update checks are disabled.".into());
    }
    let cache = read_cache(path)?;
    if cache.last_attempt == 0 {
        return Ok("Updates have not been checked. Run `bk update-check`.".into());
    }
    Ok(format!(
        "{} Last attempt: {} (Unix seconds).",
        cache.message, cache.last_attempt
    ))
}
/// At most one network attempt per day, including failures and process restarts.
/// Opt-out is evaluated before files or the network are accessed.
pub fn check(path: &Path, disabled: bool, now: u64) -> Result<String, UpdateError> {
    check_with(path, disabled, now, || {
        let url = format!(
            "{REPOSITORY}/releases/latest/download/update-manifest.json?version={}&os={}",
            env!("CARGO_PKG_VERSION"),
            std::env::consts::OS
        );
        let bytes = download(&url, MAX_METADATA)?;
        let bundle = download(
            &format!("{REPOSITORY}/releases/latest/download/update-manifest.json.sigstore"),
            MAX_METADATA,
        )?;
        verify_manifest(&bytes, &bundle, &Cosign)
    })
}
fn check_with(
    path: &Path,
    disabled: bool,
    now: u64,
    fetch: impl FnOnce() -> Result<Manifest, UpdateError>,
) -> Result<String, UpdateError> {
    if disabled {
        return status(path, true);
    }
    let mut cache = read_cache(path)?;
    if cache.last_attempt != 0 && now.saturating_sub(cache.last_attempt) < DAY {
        return status(path, false);
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
        cache = read_cache(path)?;
        if cache.last_attempt != 0 && now.saturating_sub(cache.last_attempt) < DAY {
            return status(path, false);
        }
        cache.last_attempt = now.max(1);
        cache.message = "Update check did not complete.".into();
        write_cache(path, &cache)?;
        match fetch() {
            Ok(manifest) => {
                cache.message = if manifest.newer_than(env!("CARGO_PKG_VERSION"))? {
                    format!(
                        "Desktop {} is available. Run `bk self-update`.",
                        manifest.version
                    )
                } else {
                    "Desktop is up to date.".into()
                };
                cache.manifest = Some(manifest);
            }
            Err(error) => {
                cache.message = error.to_string();
                cache.manifest = None;
            }
        }
        write_cache(path, &cache)?;
        status(path, false)
    })();
    let _ = fs::remove_file(lock);
    result
}

/// Download only the matching archive and mandatory D13 checksum/bundle files.
/// Re-verifies cached manifest bytes via their signed release asset before install.
pub fn self_update(cache: &Path, destination: &Path) -> Result<(), UpdateError> {
    let cached = read_cache(cache)?.manifest.ok_or(UpdateError::Invalid(
        "run bk update-check successfully first",
    ))?;
    let bytes = download(
        &asset_url(&cached.version, "update-manifest.json"),
        MAX_METADATA,
    )?;
    let bundle = download(
        &asset_url(&cached.version, "update-manifest.json.sigstore"),
        MAX_METADATA,
    )?;
    let manifest = verify_manifest(&bytes, &bundle, &Cosign)?;
    if manifest.version != cached.version || !manifest.newer_than(env!("CARGO_PKG_VERSION"))? {
        return Err(UpdateError::Invalid(
            "release is not newer than this Desktop binary",
        ));
    }
    crate::product::require_update_entitlement(manifest.released_at).map_err(|_| {
        UpdateError::Invalid("activate a license covering this release's update date")
    })?;
    let target = target()?;
    let name = archive_name(&manifest.version, target);
    let archive = download(&asset_url(&manifest.version, &name), MAX_ARCHIVE)?;
    let archive_bundle = download(
        &asset_url(&manifest.version, &format!("{name}.sigstore")),
        MAX_METADATA,
    )?;
    let checksums = download(
        &asset_url(&manifest.version, "checksums-sha256.txt"),
        MAX_METADATA,
    )?;
    let checksums_bundle = download(
        &asset_url(&manifest.version, "checksums-sha256.txt.sigstore"),
        MAX_METADATA,
    )?;
    install(
        &manifest,
        target,
        ReleaseFiles {
            archive: &archive,
            bundle: &archive_bundle,
            checksums: &checksums,
            checksums_bundle: &checksums_bundle,
        },
        destination,
        &Cosign,
    )
}

fn sha256(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// A release's downloaded evidence, verified before any executable replacement.
pub struct ReleaseFiles<'a> {
    pub archive: &'a [u8],
    pub bundle: &'a [u8],
    pub checksums: &'a [u8],
    pub checksums_bundle: &'a [u8],
}

/// Verify all evidence and atomically replace a destination with just its binary.
/// The caller supplies a manifest authenticated by `verify_manifest`.
pub fn install(
    manifest: &Manifest,
    target: &str,
    files: ReleaseFiles<'_>,
    destination: &Path,
    verifier: &dyn BundleVerifier,
) -> Result<(), UpdateError> {
    let ReleaseFiles {
        archive,
        bundle,
        checksums,
        checksums_bundle,
    } = files;
    release_version(&manifest.version)?;
    let name = archive_name(&manifest.version, target);
    let artifact = manifest
        .artifacts
        .get(target)
        .ok_or(UpdateError::Invalid("release does not support this target"))?;
    verify_bytes(checksums, checksums_bundle, &manifest.version, verifier)?;
    verify_bytes(archive, bundle, &manifest.version, verifier)?;
    let hash = sha256(archive);
    let lines = std::str::from_utf8(checksums)
        .map_err(|_| UpdateError::Invalid("invalid checksum manifest"))?;
    let matching: Vec<_> = lines
        .lines()
        .filter_map(|line| line.split_once("  "))
        .filter(|(_, filename)| *filename == name)
        .collect();
    if hash != artifact.sha256 || matching.len() != 1 || matching[0].0 != hash {
        return Err(UpdateError::Invalid(
            "archive checksum mismatch or missing/duplicate checksum",
        ));
    }
    let expected = format!(
        "bk-{}-{target}/{}",
        manifest.version,
        if target.contains("windows") {
            "bk.exe"
        } else {
            "bk"
        }
    );
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
    target: &str,
    expected: &str,
    output: &mut NamedTempFile,
) -> Result<(), UpdateError> {
    let mut found = false;
    if target.contains("windows") {
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
pub fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
/// Start a best-effort daily check without delaying ledger startup.
pub fn background(path: PathBuf, disabled: bool) {
    if disabled {
        return;
    }
    std::thread::spawn(move || {
        loop {
            let _ = check(&path, false, now());
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
            _version: &str,
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
    fn manifest(archive: &[u8], target: &str) -> Manifest {
        Manifest {
            schema_version: 1,
            version: "9.10.0".into(),
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
        header.set_size(content.len() as u64);
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
        let command = cosign_command(Path::new("archive"), Path::new("bundle"), "9.10.0")?;
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
                "https://github.com/Govcraft/beankeeper-desktop/.github/workflows/release.yml@refs/tags/beankeeper-cli-v9.10.0",
                "--certificate-oidc-issuer",
                "https://token.actions.githubusercontent.com",
                "--bundle",
                "bundle",
                "archive"
            ]
        );
        assert!(cosign_command(Path::new("archive"), Path::new("bundle"), "../../tag").is_err());
        Ok(())
    }
    #[test]
    fn signed_manifest_authenticates_exact_bytes_and_semantic_version() -> TestResult {
        let bytes = serde_json::to_vec(&manifest(b"fixture", "x86_64-unknown-linux-gnu"))?;
        let parsed = verify_manifest(&bytes, &sign(&bytes), &OfflineVerifier)?;
        assert!(parsed.newer_than("9.9.99")?);
        assert!(!parsed.newer_than("10.0.0")?);
        let mut tampered = bytes.clone();
        tampered.push(b' ');
        assert!(verify_manifest(&tampered, &sign(&bytes), &OfflineVerifier).is_err());
        assert!(verify_manifest(&bytes, &[], &OfflineVerifier).is_err());
        assert!(verify_manifest(&bytes, b"invalid bundle", &OfflineVerifier).is_err());
        let mut unsupported = serde_json::to_value(parsed)?;
        unsupported["schema_version"] = 2.into();
        let bytes = serde_json::to_vec(&unsupported)?;
        assert!(verify_manifest(&bytes, &sign(&bytes), &OfflineVerifier).is_err());
        assert!(release_version("1.0.0/../../payload").is_err());
        assert!(release_version("1.0.0-rc.1").is_err());
        Ok(())
    }
    #[test]
    fn opt_out_daily_cache_failure_and_clock_rollback_make_no_extra_requests() -> TestResult {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("state/updates.json");
        assert!(
            check_with(&path, true, DAY, || panic!("opt-out contacted network"))?
                .contains("disabled")
        );
        assert!(!path.exists());
        let result = check_with(&path, false, DAY, || {
            Ok(manifest(b"fixture", "x86_64-unknown-linux-gnu"))
        })?;
        assert!(result.contains("9.10.0 is available"));
        for now in [DAY, DAY + 1, 1, 2 * DAY - 1] {
            assert!(
                check_with(&path, false, now, || panic!("cache contacted network"))?
                    .contains("9.10.0 is available")
            );
        }
        assert!(
            check_with(&path, false, 2 * DAY, || Err(UpdateError::Network))?
                .contains("download failed")
        );
        assert!(read_cache(&path)?.manifest.is_none());
        check_with(&path, false, 2 * DAY + 1, || {
            panic!("failed check was retried early")
        })?;
        assert!(
            check_with(&path, true, 3 * DAY, || panic!("opt-out ignored"))?.contains("disabled")
        );
        Ok(())
    }
    #[test]
    fn concurrent_check_lock_avoids_network() -> TestResult {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("updates.json");
        fs::write(path.with_extension("lock"), b"")?;
        assert!(
            check_with(&path, false, DAY, || panic!(
                "concurrent check contacted network"
            ))?
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
        let target = "x86_64-unknown-linux-gnu";
        let archive = archive("9.10.0", target);
        let manifest = manifest(&archive, target);
        let checksums = format!("{}  {}\n", sha256(&archive), archive_name("9.10.0", target));
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
                    ReleaseFiles {
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
            format!("{}  {}\n", "0".repeat(64), archive_name("9.10.0", target)),
            checksums.repeat(2),
            String::new(),
        ] {
            assert!(
                install(
                    &manifest,
                    target,
                    ReleaseFiles {
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
        install(&manifest, target, files, &destination, &OfflineVerifier)?;
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
    fn corrupt_archive_and_wrong_target_leave_destination_intact() -> TestResult {
        let directory = tempfile::tempdir()?;
        let destination = directory.path().join("bk");
        fs::write(&destination, b"old executable")?;
        let target = "x86_64-unknown-linux-gnu";
        for bytes in [
            b"invalid tar".to_vec(),
            archive("9.10.0", "aarch64-apple-darwin"),
        ] {
            let manifest = manifest(&bytes, target);
            let list = format!("{}  {}\n", sha256(&bytes), archive_name("9.10.0", target));
            assert!(
                install(
                    &manifest,
                    target,
                    ReleaseFiles {
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
        archive.start_file("bk-9.10.0-x86_64-pc-windows-msvc/bk.exe", options)?;
        archive.write_all(b"Windows executable fixture")?;
        let bytes = archive.finish()?.into_inner();
        let mut output = NamedTempFile::new()?;
        extract_binary(
            &bytes,
            "x86_64-pc-windows-msvc",
            "bk-9.10.0-x86_64-pc-windows-msvc/bk.exe",
            &mut output,
        )?;
        assert_eq!(fs::read(output.path())?, b"Windows executable fixture");
        Ok(())
    }
}
