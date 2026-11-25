use std::path::PathBuf;

use beankeeper_cli::{
    db::{Db, SchemaVersion, current_schema_version},
    error::CliError,
};

use super::{
    LocalBackups,
    catalog::{BackupId, Catalog, Reason},
};

/// Classification of the live books while Desktop holds the catalog lock.
pub(super) enum BooksState<'a> {
    Current(CurrentBooks<'a>),
    NeedsMigration(NeedsMigration<'a>),
}

impl<'a> TryFrom<(&'a LocalBackups, &'a Catalog<'a>)> for BooksState<'a> {
    type Error = CliError;

    fn try_from((books, catalog): (&'a LocalBackups, &'a Catalog<'a>)) -> Result<Self, CliError> {
        match Db::open_snapshot(books.path(), books.key()) {
            Ok(snapshot) => Ok(Self::Current(CurrentBooks {
                books,
                snapshot,
                catalog,
            })),
            Err(error) if is_older_schema(&error) => Ok(Self::NeedsMigration(NeedsMigration {
                books,
                catalog,
                error,
            })),
            Err(error) => Err(error),
        }
    }
}

impl<'a> BooksState<'a> {
    pub(super) fn current(self) -> Result<CurrentBooks<'a>, CliError> {
        match self {
            Self::Current(books) => Ok(books),
            Self::NeedsMigration(books) => Err(books.error),
        }
    }
}

pub(super) struct CurrentBooks<'a> {
    books: &'a LocalBackups,
    snapshot: Db,
    catalog: &'a Catalog<'a>,
}

impl CurrentBooks<'_> {
    pub(super) fn snapshot(&self) -> &Db {
        &self.snapshot
    }

    pub(super) fn restore_source(&self, id: &BackupId) -> Result<PathBuf, CliError> {
        self.catalog.restore_source(id, self.books)
    }

    pub(super) fn open_existing(self) -> Result<Db, CliError> {
        Db::open_existing(self.books.path(), self.books.key())
    }
}

pub(super) struct NeedsMigration<'a> {
    books: &'a LocalBackups,
    catalog: &'a Catalog<'a>,
    error: CliError,
}

impl NeedsMigration<'_> {
    pub(super) fn open(self) -> Result<Db, CliError> {
        let (_, proof) = self.catalog.capture(self.books, Reason::BeforeMigration)?;
        Db::open_after_snapshot(proof, self.books.key())
    }
}

// The linked core exposes this mismatch as a Database diagnostic. Match its
// exact shape and require a supported, older schema. Other errors pass through.
fn is_older_schema(error: &CliError) -> bool {
    let CliError::Database(message) = error else {
        return false;
    };
    let Some((stored, expected)) = message
        .strip_prefix("not a supported beankeeper snapshot: schema version ")
        .and_then(|detail| detail.split_once("; expected "))
    else {
        return false;
    };
    let current = i64::from(current_schema_version());
    expected.parse() == Ok(current)
        && stored
            .parse::<i64>()
            .ok()
            .and_then(|version| SchemaVersion::try_from(version).ok())
            .is_some_and(|version| i64::from(version) < current)
}
