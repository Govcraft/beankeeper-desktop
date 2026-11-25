//! Local backup lifecycle. Ledger verification and audited writes belong to core.

mod archive;
mod books;
mod catalog;
mod release;
mod tools;

pub(crate) use tools::BackupTool;

pub use tools::{BackupOptions, run};

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
        let location = self.location()?;
        let catalog = location.lock()?;
        match BooksState::try_from((self, &catalog))? {
            BooksState::Current(books) => {
                Self::report_automatic(self.capture_if_due(&catalog));
                books.open_existing()
            }
            BooksState::NeedsMigration(books) => books.open(),
        }
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
        let backups = catalog.rotate(&retention)?;
        if let Some(reason) = retention.due(backups.first()) {
            catalog.capture(self, reason)?;
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

fn sync_directory(path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    fs::File::open(path)?.sync_all()?;
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}
