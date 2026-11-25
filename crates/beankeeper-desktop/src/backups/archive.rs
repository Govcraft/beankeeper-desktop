//! The ledger is a core `SQLCipher` snapshot. Attachments are a separate tar
//! archive, encrypted with age and the same passphrase when the books are keyed.

use std::{
    fs::{self, File},
    io::{self, BufReader, Read, Write},
    path::{Path, PathBuf},
};

use beankeeper::types::Sha256Hex;
use beankeeper_cli::error::CliError;
use secrecy::SecretString;

use crate::setup::keychain::{restrict_directory, restrict_file};

use super::{
    catalog::{digest, regular},
    sync_directory,
};

fn store(books: &Path) -> PathBuf {
    books.with_file_name("attachments")
}

fn invalid(message: &str) -> CliError {
    CliError::Validation(message.into())
}

fn crypto(error: impl std::error::Error) -> CliError {
    CliError::Validation(format!("attachment archive encryption failed: {error}"))
}

fn is_directory(path: &Path) -> Result<bool, CliError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() => Ok(true),
        Ok(_) => Err(invalid("the attachment store must be a real directory")),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
}

pub(super) fn capture(
    books: &Path,
    destination: &Path,
    key: Option<&SecretString>,
    byte_limit: u64,
) -> Result<Option<(usize, u64, Sha256Hex)>, CliError> {
    let store = store(books);
    if !is_directory(&store)? {
        return Ok(None);
    }
    let mut attachments = Vec::new();
    let mut payload_bytes = 0_u64;
    for entry in fs::read_dir(&store)? {
        let entry = entry?;
        // Core publishes immutable SHA-256 names. Its staging files can appear
        // during concurrent imports and are outside the content store.
        if let Some(id) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse().ok())
        {
            let size = regular(&entry.path())?.metadata()?.len();
            payload_bytes = payload_bytes.saturating_add(size).saturating_add(1024);
            if payload_bytes > byte_limit {
                return Err(invalid(
                    "attachment files exceed the local backup size limit",
                ));
            }
            attachments.push((id, entry.path()));
        }
    }
    if attachments.is_empty() {
        return Ok(None);
    }

    let mut file = tempfile::NamedTempFile::new_in(
        destination
            .parent()
            .ok_or_else(|| invalid("attachment archive needs a parent directory"))?,
    )?;
    if let Some(key) = key {
        let mut writer = age::Encryptor::with_user_passphrase(key.clone())
            .wrap_output(file.as_file_mut())
            .map_err(crypto)?;
        write_tar(&mut writer, &attachments)?;
        writer.finish()?;
    } else {
        write_tar(file.as_file_mut(), &attachments)?;
    }
    file.as_file().sync_all()?;
    let bytes = file.as_file().metadata()?.len();
    if bytes > byte_limit {
        return Err(invalid(
            "attachment archive exceeds the local backup size limit",
        ));
    }
    file.persist_noclobber(destination)
        .map_err(|error| error.error)?;
    Ok(Some((attachments.len(), bytes, digest(destination)?)))
}

fn write_tar(output: impl Write, attachments: &[(Sha256Hex, PathBuf)]) -> Result<(), CliError> {
    let mut archive = tar::Builder::new(output);
    for (id, path) in attachments {
        if digest(path)? != *id {
            return Err(invalid("attachment does not match its content address"));
        }
        let mut file = regular(path)?;
        let mut header = tar::Header::new_gnu();
        header.set_size(file.metadata()?.len());
        header.set_mode(0o600);
        header.set_entry_type(tar::EntryType::Regular);
        archive.append_data(&mut header, id.as_str(), &mut file)?;
    }
    archive.finish()?;
    Ok(())
}

pub(super) fn restore(
    books: &Path,
    input: &Path,
    key: Option<&SecretString>,
    expected_files: usize,
) -> Result<(), CliError> {
    let input_file = BufReader::new(regular(input)?);
    let stage = tempfile::tempdir_in(
        books
            .parent()
            .ok_or_else(|| invalid("ledger needs a directory for restored attachments"))?,
    )?;
    restrict_directory(stage.path())?;
    let files = if input.extension().is_some_and(|ext| ext == "age") {
        let key =
            key.ok_or_else(|| invalid("encrypted attachments require the books' passphrase"))?;
        let decryptor = age::Decryptor::new_buffered(input_file).map_err(crypto)?;
        if !decryptor.is_scrypt() {
            return Err(invalid(
                "expected a passphrase-encrypted attachment archive",
            ));
        }
        let identity = age::scrypt::Identity::new(key.clone());
        let reader = decryptor
            .decrypt(std::iter::once(&identity as &dyn age::Identity))
            .map_err(crypto)?;
        unpack(reader, stage.path(), expected_files)?
    } else {
        unpack(input_file, stage.path(), expected_files)?
    };
    install(books, files)
}

fn unpack(
    input: impl Read,
    stage: &Path,
    expected: usize,
) -> Result<Vec<(Sha256Hex, PathBuf)>, CliError> {
    let mut archive = tar::Archive::new(input);
    let mut files = Vec::new();
    for entry in archive.entries()? {
        let mut entry = entry?;
        let name = entry.path()?;
        let id = name
            .to_str()
            .and_then(|name| name.parse::<Sha256Hex>().ok())
            .ok_or_else(|| invalid("attachment archive has an invalid content address"))?;
        if !entry.header().entry_type().is_file() || files.len() >= expected {
            return Err(invalid("attachment archive has unexpected entries"));
        }
        let path = stage.join(id.as_str());
        let mut file = File::create_new(&path)?;
        io::copy(&mut entry, &mut file)?;
        file.sync_all()?;
        if digest(&path)? != id {
            return Err(invalid("attachment does not match its content address"));
        }
        files.push((id, path));
    }
    // Force the age reader to authenticate the end of the stream before copy-in.
    io::copy(&mut archive.into_inner(), &mut io::sink())?;
    if files.len() != expected {
        return Err(invalid("attachment archive has missing entries"));
    }
    Ok(files)
}

fn install(books: &Path, files: Vec<(Sha256Hex, PathBuf)>) -> Result<(), CliError> {
    let store = store(books);
    if !is_directory(&store)? {
        match fs::create_dir(&store) {
            Ok(()) => restrict_directory(&store)?,
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                is_directory(&store)?;
            }
            Err(error) => return Err(error.into()),
        }
    }

    // Install by content address before the SQLite mutation. Additional blobs
    // are inert if the restore fails. Leaving live blobs in place also keeps
    // concurrent readers and the audited undo able to read their attachments.
    for (id, path) in files {
        let mut file = regular(&path)?;
        let destination = store.join(id.as_str());
        let mut staged = tempfile::NamedTempFile::new_in(&store)?;
        restrict_file(staged.path())?;
        io::copy(&mut file, staged.as_file_mut())?;
        staged.as_file().sync_all()?;
        match staged.persist_noclobber(&destination) {
            Ok(_) => (),
            Err(error) if error.error.kind() == io::ErrorKind::AlreadyExists => {
                if digest(&destination)? != id {
                    return Err(invalid("the live attachment store has a conflicting file"));
                }
            }
            Err(error) => return Err(error.error.into()),
        }
    }
    sync_directory(&store)?;
    if let Some(parent) = store.parent() {
        sync_directory(parent)?;
    }
    Ok(())
}
