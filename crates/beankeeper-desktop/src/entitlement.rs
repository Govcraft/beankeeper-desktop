//! Offline, per-ledger licensing at the Desktop write boundary.

use std::{
    fmt, fs, io,
    path::{Path, PathBuf},
    sync::Arc,
};

use beankeeper_cli::{
    db::{Db, LedgerWriteAuthorizer, LedgerWriteKind, WriteDenied},
    error::CliError,
    mcp::LedgerTarget,
};
use secrecy::ExposeSecret;

use crate::{
    instance::BooksPath,
    license::{self, ActiveLicense, LicenseError, LicenseId, MajorVersion, UnrevokedLicense},
    private_file::PrivateFile,
    product::{self, ProductError},
    release_policy::EnforcementPolicy,
};

include!(concat!(env!("OUT_DIR"), "/enforcement.rs"));

impl EnforcementPolicy {
    pub(crate) fn activation_major(self, license: &UnrevokedLicense) -> MajorVersion {
        match self {
            Self::Free => license.eligible_major(),
            Self::Enforced(major) => major,
        }
    }
}

pub(crate) enum Entitlement {
    Active(ActiveLicense),
    Missing,
    Revoked(LicenseId),
    Invalid(ProductError),
    WrongMajor(MajorVersion),
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
            Self::WrongMajor(major) => write!(
                f,
                "These books are read-only because their Desktop license covers {major}.x releases."
            )?,
        }
        f.write_str(" You can still read, export, back up, restore, recover your books, and undo eligible changes. Ask your agent to activate a license you already have, or visit https://beankeeper.app to buy one.")
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
            policy: ENFORCEMENT,
            #[cfg(test)]
            test_files: None,
        }
    }
}

pub(crate) enum LedgerLicenseStatus {
    Free,
    Enforced(Entitlement),
}

impl LedgerLicenseStatus {
    pub(crate) fn writable(&self) -> bool {
        matches!(self, Self::Free | Self::Enforced(Entitlement::Active(_)))
    }
}

impl fmt::Display for LedgerLicenseStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Free => f.write_str("This Desktop build allows writes without a license."),
            Self::Enforced(entitlement) => entitlement.fmt(f),
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

    pub(crate) fn status(&self) -> LedgerLicenseStatus {
        match self.policy {
            EnforcementPolicy::Free => LedgerLicenseStatus::Free,
            EnforcementPolicy::Enforced(major) => {
                LedgerLicenseStatus::Enforced(self.current_entitlement(major))
            }
        }
    }

    pub(crate) fn status_from(
        &self,
        state: &Path,
        load: impl FnMut(&LicenseId) -> CheckedActivation,
    ) -> LedgerLicenseStatus {
        match self.policy {
            EnforcementPolicy::Free => LedgerLicenseStatus::Free,
            EnforcementPolicy::Enforced(major) => {
                let entitlement = Bindings(state)
                    .for_books(&self.books)
                    .map_or_else(Entitlement::Invalid, |matching| {
                        resolve(matching, major, load)
                    });
                LedgerLicenseStatus::Enforced(entitlement)
            }
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
        Ok(resolve(matching, major, |id| {
            load_activation(&license::activation_path(state, id), &keys, &revocations)
        }))
    }
}

impl LedgerWriteAuthorizer for LedgerAccess {
    fn authorize_write(&self, _: &LedgerTarget, kind: LedgerWriteKind) -> Result<(), WriteDenied> {
        match (self.policy, kind) {
            (EnforcementPolicy::Free, _)
            | (_, LedgerWriteKind::Restore | LedgerWriteKind::Reversal) => Ok(()),
            (EnforcementPolicy::Enforced(major), _) => self.current_entitlement(major).authorize(),
        }
    }
}

pub(crate) type CheckedActivation = Result<Option<UnrevokedLicense>, Arc<LicenseError>>;

pub(crate) fn load_activation(
    path: &Path,
    keys: &license::TrustedKeys,
    revocations: &license::VerifiedRevocations,
) -> CheckedActivation {
    license::load(path, keys)
        .and_then(|license| {
            license
                .map(|license| revocations.admit(license))
                .transpose()
        })
        .map_err(Arc::new)
}

fn resolve(
    ids: Vec<LicenseId>,
    major: MajorVersion,
    mut load: impl FnMut(&LicenseId) -> CheckedActivation,
) -> Entitlement {
    let mut refusal = Entitlement::Missing;
    for id in ids {
        refusal = match load(&id) {
            Ok(Some(license)) if license.id() == &id => match (license, major).try_into() {
                Ok(active) => return Entitlement::Active(active),
                Err(LicenseError::WrongMajor(major)) => Entitlement::WrongMajor(major),
                Err(error) => Entitlement::Invalid(error.into()),
            },
            Ok(None) => continue,
            Err(error) => match error.as_ref() {
                LicenseError::Revoked(revoked) if revoked == &id => Entitlement::Revoked(id),
                LicenseError::Revoked(_) => {
                    Entitlement::Invalid(LicenseError::InvalidClaims.into())
                }
                _ => Entitlement::Invalid(ProductError::License(error)),
            },
            Ok(Some(_)) => Entitlement::Invalid(LicenseError::InvalidClaims.into()),
        };
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
    pub(crate) fn remove(&self, id: &LicenseId) -> io::Result<bool> {
        crate::private_file::remove_file(&self.path(id))
    }

    fn dir(&self) -> PathBuf {
        self.0.join("license-bindings")
    }

    fn path(&self, id: &LicenseId) -> PathBuf {
        self.dir().join(id.to_string())
    }

    pub(crate) fn bind(
        &self,
        license: &ActiveLicense,
        books: &BooksPath,
    ) -> Result<(), ProductError> {
        let path = self.path(license.id());
        let binding = LedgerBinding::from(books);
        match license::write_private(&binding.0, &path, license::SaveMode::Create) {
            Ok(()) => Ok(()),
            Err(LicenseError::Io(error)) if error.kind() == io::ErrorKind::AlreadyExists => {
                if Self::read(&path)? == Some(binding) {
                    Ok(())
                } else {
                    Err(ProductError::LicenseBoundElsewhere)
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
