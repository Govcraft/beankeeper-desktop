//! Local backup lifecycle. Ledger verification and audited writes belong to core.

mod archive;
mod catalog;
mod tools;

pub(crate) use tools::BackupTool;

pub use tools::{BackupOptions, run};

use std::{fs, path::Path, sync::Arc};

use beankeeper_cli::{db::Db, error::CliError};
use secrecy::SecretString;
use tokio_util::sync::CancellationToken;

use crate::instance::BooksPath;

use catalog::{BackupLocation, Reason, Retention};

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

    // A noncurrent schema can only reach the write connection through core's
    // proof-consuming migration API. A key or database failure also fails closed.
    pub(crate) fn open(&self) -> Result<Db, CliError> {
        if Db::open_snapshot(self.path(), self.key()).is_ok() {
            self.automatic();
            return Db::open_existing(self.path(), self.key());
        }

        let location = self.location()?;
        let catalog = location.lock()?;
        // Another Desktop operation could have migrated while we waited.
        if Db::open_snapshot(self.path(), self.key()).is_ok() {
            return Db::open_existing(self.path(), self.key());
        }
        let (_, proof) = catalog.capture(self, Reason::BeforeMigration)?;
        Db::open_after_snapshot(proof, self.key())
    }

    pub(crate) fn automatic(&self) {
        if let Err(error) = self.capture_if_due() {
            eprintln!(
                "Automatic Beankeeper backup failed ({}): {error}",
                error.error_code()
            );
        }
    }

    fn capture_if_due(&self) -> Result<(), CliError> {
        let location = self.location()?;
        let catalog = location.lock()?;
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
