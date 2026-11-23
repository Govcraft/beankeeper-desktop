//! A per-user, per-ledger and per-book cursor outside the audited ledger.

use std::{
    fs::{self, OpenOptions},
    io::{self, Write},
    path::PathBuf,
};

use beankeeper_cli::{
    db::{
        Db,
        audit::receipts::{AuditCursor, ReceiptPageLimit, list_receipts},
        companies::CompanySlug,
    },
    error::CliError,
};
use secrecy::ExposeSecret;

use crate::{instance::BooksPath, private_file::PrivateFile, product, setup::keychain};

pub(super) struct Marker(PathBuf);

impl Marker {
    pub(super) fn new(books: &BooksPath, company: &CompanySlug) -> Result<Self, CliError> {
        let state = product::state_dir().map_err(|error| CliError::General(error.to_string()))?;
        Ok(Self(
            state
                .join("changes")
                .join(books.state_key())
                .join(company.as_str()),
        ))
    }

    pub(super) fn read(&self) -> Result<AuditCursor, CliError> {
        match PrivateFile::open(&self.0.join("seen")) {
            Ok(file) => file.read_secret(32)?.expose_secret().parse(),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(AuditCursor::default()),
            Err(error) => Err(error.into()),
        }
    }

    pub(super) fn advance(
        &self,
        db: &Db,
        company: &CompanySlug,
        through: AuditCursor,
    ) -> Result<AuditCursor, CliError> {
        fs::create_dir_all(&self.0)?;
        keychain::restrict_directory(&self.0)?;
        // Keep the lock file in place when replacing the cursor file. CLI and
        // MCP writers must lock the same inode across atomic replacements.
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(self.0.join("lock"))?;
        lock.lock()?;
        let current = self.read()?;
        let pending = PendingMarker::try_from((db, company, current, through))?;
        if pending.0 <= current {
            return Ok(current);
        }
        let mut file = tempfile::NamedTempFile::new_in(&self.0)?;
        keychain::restrict_file(file.path())?;
        write!(file, "{}", pending.0)?;
        file.as_file().sync_all()?;
        file.persist(self.0.join("seen"))
            .map_err(|error| error.error)?;
        Ok(pending.0)
    }
}

struct PendingMarker(AuditCursor);

impl TryFrom<(&Db, &CompanySlug, AuditCursor, AuditCursor)> for PendingMarker {
    type Error = CliError;

    fn try_from(
        (db, company, current, through): (&Db, &CompanySlug, AuditCursor, AuditCursor),
    ) -> Result<Self, Self::Error> {
        let next = list_receipts(db, company, current, ReceiptPageLimit::default())?;
        if through > super::page_through(&next)? {
            return Err(CliError::Validation(
                "last seen cannot advance beyond the next page of changes".into(),
            ));
        }
        Ok(Self(through))
    }
}
