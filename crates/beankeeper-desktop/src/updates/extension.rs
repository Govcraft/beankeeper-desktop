//! Prepare the native Claude bundle using the Desktop release trust policy.

use super::{
    BundleVerifier, GithubRelease, MAX_ARCHIVE, MAX_METADATA, RELEASE_API, ReleaseVersion,
    Sha256Digest, Sigstore, Target, UpdateError, asset_url, download, listed_checksum,
    verify_bytes,
};
use crate::setup::keychain::{restrict_directory, restrict_file};
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
};
use tempfile::NamedTempFile;

// The listing includes all assets and their GitHub metadata for each release.
const MAX_RELEASE_LIST: u64 = 8 * 1024 * 1024;

struct ExtensionAsset {
    version: ReleaseVersion,
    target: Target,
}

impl ExtensionAsset {
    fn latest(metadata: &[u8], target: Target) -> Result<Self, UpdateError> {
        let releases: Vec<GithubRelease> = serde_json::from_slice(metadata)?;
        let version = releases
            .iter()
            .filter_map(GithubRelease::published_version)
            .max()
            .ok_or(UpdateError::Invalid("no supported published release found"))?;
        Ok(Self { version, target })
    }

    fn filename(&self) -> String {
        format!("bk-{}-{}.mcpb", self.version, self.target.triple())
    }

    fn verify(
        self,
        mut fetch: impl FnMut(&str, u64) -> Result<Vec<u8>, UpdateError>,
        verifier: &dyn BundleVerifier,
    ) -> Result<VerifiedBundle, UpdateError> {
        let name = self.filename();
        let url = |name| asset_url(&self.version, name);
        let checksums = fetch(&url("checksums-sha256.txt"), MAX_METADATA)?;
        let signature = fetch(&url("checksums-sha256.txt.sigstore"), MAX_METADATA)?;
        verify_bytes(&checksums, &signature, &self.version, verifier)?;
        let expected = listed_checksum(&checksums, &name)?.ok_or(UpdateError::Invalid(
            "missing SHA-256 checksum for the Claude extension",
        ))?;

        let bytes = fetch(&url(&name), MAX_ARCHIVE)?;
        if Sha256Digest(Sha256::digest(&bytes).into()) != expected {
            return Err(UpdateError::Invalid(
                "Claude extension SHA-256 checksum mismatch",
            ));
        }
        let signature = fetch(&url(&format!("{name}.sigstore")), MAX_METADATA)?;
        verify_bytes(&bytes, &signature, &self.version, verifier)?;
        Ok(VerifiedBundle { asset: self, bytes })
    }
}

/// Bytes authenticated against both signed release files before publishing.
struct VerifiedBundle {
    asset: ExtensionAsset,
    bytes: Vec<u8>,
}

/// Local handoff to the operating system. Created only from a verified bundle.
pub(crate) struct PreparedBundle {
    path: PathBuf,
    asset: ExtensionAsset,
}

impl PreparedBundle {
    #[cfg(not(target_os = "linux"))]
    pub(crate) fn open(&self) -> std::io::Result<()> {
        open::that_detached(&self.path)
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) fn version(&self) -> String {
        self.asset.version.to_string()
    }

    pub(crate) fn target(&self) -> &'static str {
        self.asset.target.triple()
    }
}

impl VerifiedBundle {
    fn publish(self, state: &Path) -> Result<PreparedBundle, UpdateError> {
        if !state.is_absolute() || state.to_str().is_none() {
            return Err(UpdateError::Invalid(
                "the Desktop state directory must have an absolute Unicode path",
            ));
        }
        fs::create_dir_all(state)?;
        restrict_directory(state)?;
        let directory = state.join("extension-downloads");
        fs::create_dir_all(&directory)?;
        restrict_directory(&directory)?;
        let path = directory.join(self.asset.filename());
        let mut file = NamedTempFile::new_in(directory)?;
        restrict_file(file.path())?;
        file.write_all(&self.bytes)?;
        file.as_file().sync_all()?;
        file.persist(&path).map_err(|error| error.error)?;
        Ok(PreparedBundle {
            path,
            asset: self.asset,
        })
    }
}

pub(crate) fn prepare(state: &Path) -> Result<PreparedBundle, UpdateError> {
    prepare_with(state, Target::current()?, download, &Sigstore)
}

fn prepare_with(
    state: &Path,
    target: Target,
    mut fetch: impl FnMut(&str, u64) -> Result<Vec<u8>, UpdateError>,
    verifier: &dyn BundleVerifier,
) -> Result<PreparedBundle, UpdateError> {
    let metadata = fetch(&format!("{RELEASE_API}?per_page=100"), MAX_RELEASE_LIST)?;
    ExtensionAsset::latest(&metadata, target)?
        .verify(fetch, verifier)?
        .publish(state)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::BTreeMap;

    const VERSION: &str = "1.2.0-rc.4";
    const ASSET: &str = "bk-1.2.0-rc.4-x86_64-unknown-linux-gnu.mcpb";
    const BUNDLE: &[u8] = b"test MCP bundle";
    const CHECKSUM: &str = "524803e0329936810ff3b92de3ca9052e3b4b0004d0e6540df9a0c8108c7163a";

    fn metadata() -> Vec<u8> {
        json!([
            {"tag_name":"beankeeper-desktop-v8.0.0","draft":true,"prerelease":false},
            {"tag_name":"beankeeper-desktop-v1.1.0","draft":false,"prerelease":false},
            {"tag_name":"beankeeper-desktop-v1.2.0-rc.4","draft":false,"prerelease":true},
            {"tag_name":"beankeeper-desktop-v9.0.0/../file","draft":false,"prerelease":false},
            {"tag_name":"beankeeper-desktop-v9.0.0-rc.1","draft":false,"prerelease":false}
        ])
        .to_string()
        .into_bytes()
    }

    #[test]
    fn latest_release_resolves_only_the_native_asset_names() -> Result<(), UpdateError> {
        for (target, triple) in [
            (Target::LinuxX64, "x86_64-unknown-linux-gnu"),
            (Target::LinuxArm64, "aarch64-unknown-linux-gnu"),
            (Target::MacX64, "x86_64-apple-darwin"),
            (Target::MacArm64, "aarch64-apple-darwin"),
            (Target::WindowsX64, "x86_64-pc-windows-msvc"),
        ] {
            let asset = ExtensionAsset::latest(&metadata(), target)?;
            assert_eq!(asset.filename(), format!("bk-{VERSION}-{triple}.mcpb"));
        }
        assert!(ExtensionAsset::latest(b"[]", Target::LinuxX64).is_err());
        Ok(())
    }

    struct TestVerifier;

    impl BundleVerifier for TestVerifier {
        fn verify(
            &self,
            artifact: &Path,
            bundle: &Path,
            version: &ReleaseVersion,
        ) -> Result<(), UpdateError> {
            assert_eq!(version.to_string(), VERSION);
            if fs::read(artifact)? != fs::read(bundle)? {
                return Err(UpdateError::Signature);
            }
            Ok(())
        }
    }

    struct ReleaseFixture(BTreeMap<String, Vec<u8>>);

    impl ReleaseFixture {
        fn new() -> Result<Self, UpdateError> {
            let checksums = format!("{CHECKSUM}  {ASSET}\n").into_bytes();
            let version = VERSION.parse()?;
            Ok(Self(BTreeMap::from([
                (format!("{RELEASE_API}?per_page=100"), metadata()),
                (
                    asset_url(&version, "checksums-sha256.txt"),
                    checksums.clone(),
                ),
                (
                    asset_url(&version, "checksums-sha256.txt.sigstore"),
                    checksums,
                ),
                (asset_url(&version, ASSET), BUNDLE.to_vec()),
                (
                    asset_url(&version, &format!("{ASSET}.sigstore")),
                    BUNDLE.to_vec(),
                ),
            ])))
        }

        fn replace(&mut self, file: &str, bytes: &[u8]) -> Result<(), UpdateError> {
            self.0
                .insert(asset_url(&VERSION.parse()?, file), bytes.to_vec());
            Ok(())
        }

        fn prepare(mut self, state: &Path) -> Result<PreparedBundle, UpdateError> {
            let bundle = prepare_with(
                state,
                Target::LinuxX64,
                |url, _limit| {
                    self.0
                        .remove(url)
                        .ok_or(UpdateError::Http(reqwest::StatusCode::NOT_FOUND))
                },
                &TestVerifier,
            )?;
            assert!(self.0.is_empty());
            Ok(bundle)
        }
    }

    #[test]
    fn verified_bytes_are_published_at_the_reported_path() -> Result<(), UpdateError> {
        let home = tempfile::tempdir()?;
        let state = home.path().join("state");
        let bundle = ReleaseFixture::new()?.prepare(&state)?;
        assert_eq!(bundle.version(), VERSION);
        assert_eq!(bundle.target(), "x86_64-unknown-linux-gnu");
        assert_eq!(bundle.path(), state.join("extension-downloads").join(ASSET));
        assert_eq!(fs::read(bundle.path())?, BUNDLE);
        Ok(())
    }

    #[test]
    fn corrupt_or_missing_release_evidence_never_publishes_a_bundle() -> Result<(), UpdateError> {
        let home = tempfile::tempdir()?;
        let state = home.path().join("state");
        let version = VERSION.parse()?;
        let signature_name = format!("{ASSET}.sigstore");
        for (file, content, expected) in [
            (
                "checksums-sha256.txt",
                Some(b"tampered".as_slice()),
                "signature",
            ),
            ("checksums-sha256.txt.sigstore", None, "missing"),
            (ASSET, Some(b"tampered".as_slice()), "checksum"),
            (
                &signature_name,
                Some(b"wrong signature".as_slice()),
                "signature",
            ),
            (&signature_name, Some(b"".as_slice()), "signature"),
            (&signature_name, None, "missing"),
        ] {
            let mut release = ReleaseFixture::new()?;
            if let Some(bytes) = content {
                release.replace(file, bytes)?;
            } else {
                release.0.remove(&asset_url(&version, file));
            }
            let result = release.prepare(&state);
            match expected {
                "signature" => assert!(matches!(result, Err(UpdateError::Signature))),
                "missing" => {
                    assert!(matches!(result, Err(UpdateError::Http(status)) if status == 404));
                }
                _ => assert!(
                    matches!(result, Err(UpdateError::Invalid(reason)) if reason.contains("checksum mismatch"))
                ),
            }
            assert!(!state.exists());
        }
        Ok(())
    }

    #[test]
    fn authenticated_checksum_list_must_name_the_asset_with_one_valid_digest()
    -> Result<(), UpdateError> {
        let home = tempfile::tempdir()?;
        let state = home.path().join("state");
        let row = format!("{CHECKSUM}  {ASSET}\n");
        for invalid in [
            row.replace(".mcpb", ".zip"),
            row.repeat(2),
            format!("invalid  {ASSET}\n"),
        ] {
            let mut release = ReleaseFixture::new()?;
            for file in ["checksums-sha256.txt", "checksums-sha256.txt.sigstore"] {
                release.replace(file, invalid.as_bytes())?;
            }
            assert!(matches!(
                release.prepare(&state),
                Err(UpdateError::Invalid(_))
            ));
            assert!(!state.exists());
        }
        Ok(())
    }
}
