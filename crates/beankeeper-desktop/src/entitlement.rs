//! Offline, per-ledger licensing at the Desktop write boundary.

use std::{
    fmt, fs, io,
    path::{Path, PathBuf},
};

use beankeeper_cli::{
    db::{Db, LedgerWriteAuthorizer, LedgerWriteKind, WriteDenied},
    error::CliError,
    mcp::LedgerTarget,
};
use secrecy::ExposeSecret;
use semver::Version;

use crate::{
    instance::BooksPath,
    license::{self, LicenseError, LicenseId, MajorVersion, VerifiedLicense},
    private_file::PrivateFile,
    product::{self, ProductError},
};

#[derive(Clone, Copy)]
pub(crate) enum EnforcementPolicy {
    Free,
    Enforced(MajorVersion),
}

impl EnforcementPolicy {
    pub(crate) fn is_official() -> bool {
        env!("BEANKEEPER_OFFICIAL_BUILD") == "1"
    }

    pub(crate) fn current() -> Self {
        Version::parse(env!("CARGO_PKG_VERSION")).map_or(Self::Free, |version| {
            Self::from_release(&version, Self::is_official())
        })
    }

    fn from_release(version: &Version, official: bool) -> Self {
        if official
            && version.pre.is_empty()
            && let Ok(major) = MajorVersion::try_from(version.major)
        {
            return Self::Enforced(major);
        }
        Self::Free
    }
}

enum Entitlement {
    Active(VerifiedLicense),
    Missing,
    Revoked(LicenseId),
    Invalid(ProductError),
    WrongMajor(VerifiedLicense),
}

impl fmt::Display for Entitlement {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Active(license) => {
                return write!(
                    f,
                    "These books are writable. License {} covers this release.",
                    license.id()
                );
            }
            Self::Missing => f.write_str(
                "These books are read-only because they have no active Desktop license.",
            )?,
            Self::Revoked(id) => write!(
                f,
                "These books are read-only because Desktop license {id} was revoked."
            )?,
            Self::Invalid(error) => write!(
                f,
                "These books are read-only because Desktop could not verify their license ({error})."
            )?,
            Self::WrongMajor(license) => write!(
                f,
                "These books are read-only because their Desktop license covers {}.x releases.",
                license.eligible_major()
            )?,
        }
        f.write_str(" You can still read, export, back up, restore, and recover your books. Buy a license at https://beankeeper.app or use `bk --db BOOKS license activate FILE` to activate one for these books.")
    }
}

impl Entitlement {
    fn authorize(self) -> Result<(), WriteDenied> {
        match self {
            Self::Active(_) => Ok(()),
            denied => Err(denied.to_string().into()),
        }
    }
}

/// The path is resolved before the authorizer is attached to its ledger connection.
#[derive(Clone)]
pub(crate) struct LedgerAccess {
    books: BooksPath,
    policy: EnforcementPolicy,
    #[cfg(test)]
    test_files: Option<TestFiles>,
}

impl From<BooksPath> for LedgerAccess {
    fn from(books: BooksPath) -> Self {
        Self {
            books,
            policy: EnforcementPolicy::current(),
            #[cfg(test)]
            test_files: None,
        }
    }
}

impl LedgerAccess {
    pub(crate) fn protect(self, db: Db, target: LedgerTarget) -> Db {
        db.with_write_authorizer(target, self)
    }

    pub(crate) fn ordinary_write(&self, ledger: LedgerTarget) -> Result<(), CliError> {
        self.authorize_write(&ledger, LedgerWriteKind::Ordinary)
            .map_err(|reason| CliError::WriteDenied { ledger, reason })
    }

    pub(crate) fn status(&self) -> String {
        match self.policy {
            EnforcementPolicy::Free => "This Desktop build allows writes without a license.".into(),
            EnforcementPolicy::Enforced(major) => self.current_entitlement(major).to_string(),
        }
    }

    fn current_entitlement(&self, major: MajorVersion) -> Entitlement {
        #[cfg(test)]
        if let Some(files) = &self.test_files {
            return self
                .evaluate(&files.state, major, || {
                    let keys = license::TrustedKeys::from_json(files.keys)?;
                    let revocations =
                        license::VerifiedRevocations::verify(files.revocations, &keys)?;
                    Ok((keys, revocations))
                })
                .unwrap_or_else(Entitlement::Invalid);
        }

        product::state_dir()
            .and_then(|state| {
                self.evaluate(&state, major, || {
                    let keys = product::trusted_keys()?;
                    let revocations = license::release_revocations(&keys)?;
                    Ok((keys, revocations))
                })
            })
            .unwrap_or_else(Entitlement::Invalid)
    }

    fn evaluate(
        &self,
        state: &Path,
        major: MajorVersion,
        trust: impl FnOnce()
            -> Result<(license::TrustedKeys, license::VerifiedRevocations), ProductError>,
    ) -> Result<Entitlement, ProductError> {
        let matching = Bindings(state).for_books(&self.books)?;
        if matching.is_empty() {
            return Ok(Entitlement::Missing);
        }
        let (keys, revocations) = trust()?;
        Ok(resolve(state, matching, &keys, &revocations, major))
    }
}

impl LedgerWriteAuthorizer for LedgerAccess {
    fn authorize_write(&self, _: &LedgerTarget, kind: LedgerWriteKind) -> Result<(), WriteDenied> {
        match (self.policy, kind) {
            (EnforcementPolicy::Free, _) | (_, LedgerWriteKind::Restore) => Ok(()),
            (EnforcementPolicy::Enforced(major), _) => self.current_entitlement(major).authorize(),
        }
    }
}

fn resolve(
    state: &Path,
    ids: Vec<LicenseId>,
    keys: &license::TrustedKeys,
    revocations: &license::VerifiedRevocations,
    major: MajorVersion,
) -> Entitlement {
    let mut refusal = Entitlement::Missing;
    for id in ids {
        match license::load(&license::activation_path(state, &id), keys) {
            Ok(Some(license)) if license.id() != &id => {
                refusal = Entitlement::Invalid(LicenseError::InvalidClaims.into());
            }
            Ok(Some(_)) if revocations.contains(&id) => refusal = Entitlement::Revoked(id),
            Ok(Some(license)) if license::entitlement(&license, major) => {
                return Entitlement::Active(license);
            }
            Ok(Some(license)) => refusal = Entitlement::WrongMajor(license),
            Ok(None) => {}
            Err(error) => refusal = Entitlement::Invalid(error.into()),
        }
    }
    refusal
}

#[derive(PartialEq, Eq)]
struct LedgerBinding([u8; 32]);

impl From<&BooksPath> for LedgerBinding {
    fn from(books: &BooksPath) -> Self {
        Self(books.digest())
    }
}

impl TryFrom<&[u8]> for LedgerBinding {
    type Error = io::Error;

    fn try_from(bytes: &[u8]) -> Result<Self, Self::Error> {
        bytes.try_into().map(Self).map_err(io::Error::other)
    }
}

/// One private binding file per signed license ID. Its name is the uniqueness
/// key, and atomic create makes concurrent first activations agree on one ledger.
pub(crate) struct Bindings<'a>(pub(crate) &'a Path);

impl Bindings<'_> {
    fn dir(&self) -> PathBuf {
        self.0.join("license-bindings")
    }

    fn path(&self, id: &LicenseId) -> PathBuf {
        self.dir().join(id.to_string())
    }

    pub(crate) fn bind(&self, id: &LicenseId, books: &BooksPath) -> Result<(), ProductError> {
        let path = self.path(id);
        let binding = LedgerBinding::from(books);
        match license::write_private(&binding.0, &path, license::SaveMode::Create) {
            Ok(()) => Ok(()),
            Err(LicenseError::Io(error)) if error.kind() == io::ErrorKind::AlreadyExists => {
                if Self::read(&path)? == Some(binding) {
                    Ok(())
                } else {
                    Err(ProductError::Configuration(
                        "This license is already bound to another set of books.",
                    ))
                }
            }
            Err(error) => Err(error.into()),
        }
    }

    fn read(path: &Path) -> Result<Option<LedgerBinding>, ProductError> {
        let file = match PrivateFile::open(path) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let bytes = file.read_bytes(32)?;
        Ok(Some(LedgerBinding::try_from(bytes.expose_secret())?))
    }

    fn for_books(&self, books: &BooksPath) -> Result<Vec<LicenseId>, ProductError> {
        let entries = match fs::read_dir(self.dir()) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error.into()),
        };
        let mut ids = Vec::new();
        let binding = LedgerBinding::from(books);
        for entry in entries {
            let entry = entry?;
            let name = entry.file_name();
            if let Some(id) = name
                .to_str()
                .and_then(|name| name.parse::<LicenseId>().ok())
                && Self::read(&entry.path())?.as_ref() == Some(&binding)
            {
                ids.push(id);
            }
        }
        ids.sort_unstable_by_key(ToString::to_string);
        Ok(ids)
    }
}

#[cfg(test)]
#[derive(Clone)]
struct TestFiles {
    state: PathBuf,
    keys: &'static [u8],
    revocations: &'static [u8],
}

#[cfg(test)]
mod tests;
