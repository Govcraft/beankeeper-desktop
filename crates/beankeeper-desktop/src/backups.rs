//! Local backup lifecycle. Ledger verification and audited writes belong to core.

mod archive;
mod books;
mod catalog;
mod git_oid;
mod release;
mod tools;
mod upstream;

pub(crate) use tools::BackupTool;

pub use tools::{BackupOptions, run};
pub use upstream::run_upstream;

use std::{fs, path::Path, sync::Arc};

use beankeeper_cli::{db::Db, error::CliError};
use secrecy::SecretString;
use tokio_util::sync::CancellationToken;

use crate::instance::BooksPath;

use books::BooksState;
use catalog::{BackupLocation, Catalog, Retention};

#[derive(Clone)]
pub(crate) struct LocalBackups {
    books: BooksPath,
    passphrase: Option<Arc<SecretString>>,
}

#[derive(Clone, Copy)]
enum OpenMode {
    Normal,
    Provisioning,
    Replacement,
}

impl LocalBackups {
    pub(crate) fn new(books: BooksPath, passphrase: Option<SecretString>) -> Self {
        Self {
            books,
            passphrase: passphrase.map(Arc::new),
        }
    }

    fn path(&self) -> &Path {
        self.books.as_ref()
    }

    pub(crate) fn key(&self) -> Option<&SecretString> {
        self.passphrase.as_deref()
    }

    fn location(&self) -> Result<BackupLocation, CliError> {
        BackupLocation::try_from(&self.books)
    }

    pub(crate) fn open(&self) -> Result<Db, CliError> {
        self.open_with(OpenMode::Normal)
    }

    fn open_for_provisioning(&self) -> Result<Db, CliError> {
        self.open_with(OpenMode::Provisioning)
    }

    fn before_replacement(&self) -> Result<(), CliError> {
        self.open_with(OpenMode::Replacement).map(drop)
    }

    fn open_with(&self, mode: OpenMode) -> Result<Db, CliError> {
        let location = self.location()?;
        let catalog = location.lock()?;
        let plaintext = Self::new(self.books.clone(), None);
        let state = BooksState::try_from((self, &catalog)).or_else(|error| {
            if matches!(mode, OpenMode::Replacement) && self.key().is_some() {
                BooksState::try_from((&plaintext, &catalog)).map_err(|_| error)
            } else {
                Err(error)
            }
        })?;
        state.open(mode)
    }

    pub(crate) fn automatic(&self) {
        Self::report_automatic(self.location().and_then(|location| {
            let catalog = location.lock()?;
            self.capture_if_due(&catalog)
        }));
    }

    fn report_automatic(result: Result<(), CliError>) {
        if let Err(error) = result {
            eprintln!(
                "Automatic Beankeeper backup failed ({}): {error}",
                error.error_code()
            );
        }
    }

    fn capture_if_due(&self, catalog: &Catalog<'_>) -> Result<(), CliError> {
        let retention = Retention::default();
        let backups = catalog.list()?;
        if let Some(reason) = retention.due(&backups) {
            catalog.capture(self, reason)?;
        } else {
            catalog.rotate(&retention, &backups, None);
        }
        Ok(())
    }

    pub(crate) async fn schedule(self, cancellation: CancellationToken) {
        let mut interval = tokio::time::interval_at(
            tokio::time::Instant::now() + Retention::CHECK_INTERVAL,
            Retention::CHECK_INTERVAL,
        );
        loop {
            tokio::select! {
                () = cancellation.cancelled() => return,
                _ = interval.tick() => {
                    let backups = self.clone();
                    if let Err(error) = tokio::task::spawn_blocking(move || backups.automatic()).await {
                        eprintln!("Automatic Beankeeper backup worker failed: {error}");
                    }
                }
            }
        }
    }
}

pub(crate) fn open(books: &Path, passphrase: Option<&SecretString>) -> Result<Db, CliError> {
    LocalBackups::new(books.try_into()?, passphrase.cloned()).open()
}

pub(crate) fn open_for_provisioning(
    books: &Path,
    passphrase: &SecretString,
) -> Result<Db, CliError> {
    LocalBackups::new(books.try_into()?, Some(passphrase.clone())).open_for_provisioning()
}

fn sync_directory(path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    fs::File::open(path)?.sync_all()?;
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}
