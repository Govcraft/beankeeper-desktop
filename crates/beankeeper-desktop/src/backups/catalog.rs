use std::{
    fmt,
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    path::{Path, PathBuf},
    str::FromStr,
    time::Duration,
};

use beankeeper::types::Sha256Hex;
use beankeeper_cli::{
    db::{SchemaVersion, SnapshotOverwrite, VerifiedSnapshot},
    error::CliError,
};
use chrono::{DateTime, TimeDelta, Utc};
use mti::prelude::{MagicTypeId, MagicTypeIdExt, V7};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{
    instance::BooksPath,
    setup::keychain::{restrict_directory, restrict_file},
};

use super::{LocalBackups, archive, sync_directory};

const MANIFEST: &str = "backup.json";
const DATABASE: &str = "books.db";
const ATTACHMENTS: &str = "attachments.tar";
const ENCRYPTED_ATTACHMENTS: &str = "attachments.tar.age";
const RELEASE: &str = concat!(env!("CARGO_PKG_VERSION"), "/", env!("BK_CORE_REVISION"));

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub(super) struct BackupId(MagicTypeId);

impl BackupId {
    const PREFIX: &str = "bkbackup";
}

impl Default for BackupId {
    fn default() -> Self {
        Self(Self::PREFIX.create_type_id::<V7>())
    }
}

impl FromStr for BackupId {
    type Err = CliError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let id = value.parse::<MagicTypeId>().map_err(|_| invalid_id())?;
        if id.prefix().as_str() != Self::PREFIX {
            return Err(invalid_id());
        }
        Ok(Self(id))
    }
}

fn invalid_id() -> CliError {
    CliError::Validation("expected a local backup ID beginning with bkbackup_".into())
}

impl fmt::Display for BackupId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl TryFrom<String> for BackupId {
    type Error = CliError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        value.parse()
    }
}

impl From<BackupId> for String {
    fn from(id: BackupId) -> Self {
        id.to_string()
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum Reason {
    FirstBackup,
    BeforeMigration,
    Upgrade,
    Daily,
    Requested,
}

#[derive(Debug, Serialize, Deserialize)]
pub(super) struct Backup {
    id: BackupId,
    created_at: DateTime<Utc>,
    reason: Reason,
    release: String,
    #[serde(deserialize_with = "schema_version")]
    schema_version: SchemaVersion,
    encrypted: bool,
    bytes: u64,
    #[serde(with = "sha256_hex")]
    database_sha256: Sha256Hex,
    attachments: Option<Archive>,
}

#[derive(Debug, Serialize, Deserialize)]
struct Archive {
    files: usize,
    #[serde(with = "sha256_hex")]
    sha256: Sha256Hex,
}

mod sha256_hex {
    use super::{Deserialize, Sha256Hex};

    pub(super) fn serialize<S: serde::Serializer>(
        value: &Sha256Hex,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(value.as_str())
    }

    pub(super) fn deserialize<'de, D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Sha256Hex, D::Error> {
        String::deserialize(deserializer)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}

fn schema_version<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<SchemaVersion, D::Error> {
    SchemaVersion::try_from(i64::deserialize(deserializer)?).map_err(serde::de::Error::custom)
}

impl Backup {
    pub(super) fn plain_text(&self) -> String {
        format!(
            "{}  {}  {} bytes",
            self.id,
            self.created_at.to_rfc3339(),
            self.bytes
        )
    }

    pub(super) fn id(&self) -> &BackupId {
        &self.id
    }
}

#[derive(Serialize)]
pub(super) struct Retention {
    max_backups: usize,
    max_bytes: u64,
    interval_hours: u16,
}

impl Default for Retention {
    fn default() -> Self {
        Self {
            max_backups: 7,
            max_bytes: 2 * 1024 * 1024 * 1024,
            interval_hours: 24,
        }
    }
}

impl Retention {
    pub(super) const CHECK_INTERVAL: Duration = Duration::from_hours(1);

    pub(super) fn due(&self, latest: Option<&Backup>) -> Option<Reason> {
        let Some(latest) = latest else {
            return Some(Reason::FirstBackup);
        };
        if latest.release != RELEASE {
            return Some(Reason::Upgrade);
        }
        let age = Utc::now() - latest.created_at;
        (age >= TimeDelta::hours(i64::from(self.interval_hours))).then_some(Reason::Daily)
    }

    fn check_size(&self, bytes: u64) -> Result<(), CliError> {
        if bytes > self.max_bytes {
            return Err(CliError::Validation(
                "one backup exceeds the local retention size limit".into(),
            ));
        }
        Ok(())
    }
}

pub(super) struct BackupLocation(PathBuf);

impl TryFrom<&BooksPath> for BackupLocation {
    type Error = CliError;

    fn try_from(books: &BooksPath) -> Result<Self, Self::Error> {
        let parent = books
            .as_ref()
            .parent()
            .ok_or_else(|| CliError::Validation("ledger needs a directory for backups".into()))?;
        let root = parent.join(".beankeeper-backups");
        create_private_directory(&root)?;
        let name = books
            .as_ref()
            .file_name()
            .ok_or_else(|| CliError::Validation("ledger needs a filename for backups".into()))?;
        let path = root.join(name);
        create_private_directory(&path)?;
        Ok(Self(path))
    }
}

fn create_private_directory(path: &Path) -> io::Result<()> {
    match fs::create_dir(path) {
        Ok(()) => (),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => (),
        Err(error) => return Err(error),
    }
    restrict_directory(path)
}

impl BackupLocation {
    pub(super) fn lock(&self) -> Result<Catalog<'_>, CliError> {
        let path = self.0.join("catalog.lock");
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)?;
        restrict_file(&path)?;
        file.lock()?;
        let catalog = Catalog {
            location: self,
            _lock: file,
        };
        catalog.clean_incomplete()?;
        Ok(catalog)
    }
}

pub(super) struct Catalog<'a> {
    location: &'a BackupLocation,
    _lock: File,
}

impl Catalog<'_> {
    fn entry(&self, id: &BackupId) -> PathBuf {
        self.location.0.join(id.to_string())
    }

    fn entries(&self) -> Result<Vec<(BackupId, PathBuf)>, CliError> {
        let mut entries = Vec::new();
        for entry in fs::read_dir(&self.location.0)? {
            let entry = entry?;
            if entry.file_type()?.is_dir()
                && let Some(name) = entry.file_name().to_str()
                && let Ok(id) = name.parse()
            {
                entries.push((id, entry.path()));
            }
        }
        Ok(entries)
    }

    fn clean_incomplete(&self) -> Result<(), CliError> {
        for (_, path) in self.entries()? {
            if !path.join(MANIFEST).try_exists()? {
                fs::remove_dir_all(path)?;
            }
        }
        Ok(())
    }

    pub(super) fn list(&self) -> Result<Vec<Backup>, CliError> {
        let mut backups = self
            .entries()?
            .into_iter()
            .map(|(id, path)| read_manifest(&id, &path))
            .collect::<Result<Vec<_>, _>>()?;
        backups.sort_by_key(|backup| std::cmp::Reverse(backup.created_at));
        Ok(backups)
    }

    pub(super) fn rotate(&self, retention: &Retention) -> Result<Vec<Backup>, CliError> {
        let mut remaining = retention.max_bytes;
        let mut kept = Vec::new();
        for backup in self.list()? {
            if kept.len() == retention.max_backups || backup.bytes > remaining {
                fs::remove_dir_all(self.entry(backup.id()))?;
            } else {
                remaining -= backup.bytes;
                kept.push(backup);
            }
        }
        Ok(kept)
    }

    pub(super) fn capture(
        &self,
        books: &LocalBackups,
        reason: Reason,
    ) -> Result<(Backup, VerifiedSnapshot), CliError> {
        let id = BackupId::default();
        let directory = self.entry(&id);
        fs::create_dir(&directory)?;
        let pending = Pending(directory);
        restrict_directory(&pending.0)?;
        let database = pending.0.join(DATABASE);
        let proof = VerifiedSnapshot::create(
            books.path(),
            books.key(),
            &database,
            SnapshotOverwrite::default(),
        )?;
        let retention = Retention::default();
        retention.check_size(proof.bytes())?;
        let attachments = archive::capture(
            books.path(),
            &archive_path(&pending.0, proof.encrypted()),
            books.key(),
            retention.max_bytes - proof.bytes(),
        )?;
        let database_sha256 = digest(&database)?;
        let (attachments, bytes) = match attachments {
            Some((files, bytes, sha256)) => (
                Some(Archive { files, sha256 }),
                proof.bytes().saturating_add(bytes),
            ),
            None => (None, proof.bytes()),
        };
        let backup = Backup {
            id,
            created_at: Utc::now(),
            reason,
            release: RELEASE.into(),
            schema_version: proof.schema_version(),
            encrypted: proof.encrypted(),
            bytes,
            database_sha256,
            attachments,
        };
        retention.check_size(bytes)?;
        pending.commit(&backup)?;
        sync_directory(&self.location.0)?;
        self.rotate(&retention)?;
        Ok((backup, proof))
    }

    pub(super) fn restore_source(
        &self,
        id: &BackupId,
        books: &LocalBackups,
    ) -> Result<PathBuf, CliError> {
        let path = self.entry(id);
        let backup = read_manifest(id, &path)?;
        let database = path.join(DATABASE);
        check_digest(&database, &backup.database_sha256)?;
        if let Some(attachments) = &backup.attachments {
            let path = archive_path(&path, backup.encrypted);
            check_digest(&path, &attachments.sha256)?;
            archive::restore(books.path(), &path, books.key(), attachments.files)?;
        }
        Ok(database)
    }
}

fn archive_path(directory: &Path, encrypted: bool) -> PathBuf {
    directory.join(if encrypted {
        ENCRYPTED_ATTACHMENTS
    } else {
        ATTACHMENTS
    })
}

fn read_manifest(id: &BackupId, path: &Path) -> Result<Backup, CliError> {
    if !fs::symlink_metadata(path).is_ok_and(|meta| meta.is_dir()) {
        return Err(CliError::NotFound(format!("local backup {id} not found")));
    }
    let file = regular(&path.join(MANIFEST))?;
    let backup = serde_json::from_reader::<_, Backup>(file.take(16 * 1024))
        .map_err(|_| CliError::Validation(format!("local backup {id} has invalid metadata")))?;
    if &backup.id != id {
        return Err(CliError::Validation(format!(
            "local backup {id} has mismatched metadata"
        )));
    }
    Ok(backup)
}

struct Pending(PathBuf);

impl Pending {
    fn commit(&self, backup: &Backup) -> Result<(), CliError> {
        let mut manifest = tempfile::NamedTempFile::new_in(&self.0)?;
        serde_json::to_writer(manifest.as_file_mut(), backup).map_err(io::Error::other)?;
        manifest.as_file_mut().flush()?;
        manifest.as_file().sync_all()?;
        manifest
            .persist_noclobber(self.0.join(MANIFEST))
            .map_err(|error| error.error)?;
        sync_directory(&self.0)?;
        Ok(())
    }
}

impl Drop for Pending {
    fn drop(&mut self) {
        if !self.0.join(MANIFEST).exists() {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
}

pub(super) fn regular(path: &Path) -> io::Result<File> {
    if !fs::symlink_metadata(path)?.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "backup and attachment files must be regular files",
        ));
    }
    let file = File::open(path)?;
    if !file.metadata()?.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "backup and attachment files must be regular files",
        ));
    }
    Ok(file)
}

pub(super) fn digest(path: &Path) -> io::Result<Sha256Hex> {
    let mut sha = Sha256::new();
    let mut file = regular(path)?;
    let mut buffer = [0; 8192];
    loop {
        let bytes = file.read(&mut buffer)?;
        if bytes == 0 {
            return Ok(Sha256Hex::from_digest(sha.finalize().into()));
        }
        sha.update(&buffer[..bytes]);
    }
}

fn check_digest(path: &Path, expected: &Sha256Hex) -> Result<(), CliError> {
    if digest(path)? != *expected {
        return Err(CliError::Validation(
            "local backup file does not match its recorded SHA-256".into(),
        ));
    }
    Ok(())
}
