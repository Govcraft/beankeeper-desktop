//! Ledger-specific credentials in native OS storage, with an explicit headless seam.

use std::{
    error::Error,
    fmt, fs,
    io::{self, Write},
    path::{Path, PathBuf},
};

use beankeeper_cli::cli::PassphraseOptions;
use secrecy::{ExposeSecret, SecretString};
use tempfile::NamedTempFile;

use crate::private_file::PrivateFile;

#[cfg(any(unix, windows))]
const PASSPHRASE_FILE_LIMIT: usize = 64 * 1024;

/// Native credential service name, shared across supported operating systems.
pub const SERVICE: &str = "beankeeper-desktop";

/// Restrict a regular credential file before writing any secret bytes.
///
/// Unix files use mode 0600. Windows replaces the entire DACL with a protected
/// ACL granting access only to the current process user and Local System.
///
/// # Errors
/// Rejects symlinks, nonregular files, and failures to enforce permissions.
pub fn restrict_file(path: &Path) -> io::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "credential path must be a regular file, not a symbolic link",
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    }
    #[cfg(windows)]
    restrict_windows_acl(path)?;
    #[cfg(not(any(unix, windows)))]
    return Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "private credential files are unsupported on this platform",
    ));
    #[cfg(any(unix, windows))]
    PrivateFile::open(path).map(drop)
}

/// Protect a staging directory before the ledger is initialized inside it.
///
/// # Errors
/// Rejects symlinks and failures to make the directory private.
pub fn restrict_directory(path: &Path) -> io::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "staging path must be a directory",
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    #[cfg(windows)]
    restrict_windows_acl(path)?;
    Ok(())
}

#[cfg(windows)]
fn restrict_windows_acl(path: &Path) -> io::Result<()> {
    use windows_permissions::{
        LocalBox, SecurityDescriptor,
        constants::{SeObjectType, SecurityInformation},
        utilities, wrappers,
    };
    let user = utilities::current_process_sid()?;
    let descriptor: LocalBox<SecurityDescriptor> =
        format!("D:P(A;;FA;;;{user})(A;;FA;;;SY)").parse()?;
    let dacl = descriptor
        .dacl()
        .ok_or_else(|| io::Error::other("private credential DACL is missing"))?;
    wrappers::SetNamedSecurityInfo(
        path,
        SeObjectType::SE_FILE_OBJECT,
        SecurityInformation::Owner | SecurityInformation::Dacl | SecurityInformation::ProtectedDacl,
        Some(user.as_ref()),
        None,
        Some(dacl),
        None,
    )?;
    Ok(())
}

/// Credential failures with no secret-bearing platform diagnostics.
#[derive(Debug)]
pub enum KeychainError {
    /// The operating system refused a filesystem operation.
    Io(io::Error),
    /// Native secure storage is unavailable or locked.
    Unavailable,
    /// Native storage refused access, usually because it is locked.
    Locked,
    /// No native credential provider could be reached.
    NoProvider,
    /// A ledger path cannot be represented by the native credential API.
    InvalidLedgerPath,
    /// Empty credentials are never accepted.
    EmptyPassphrase,
    /// A file store or its parent has unsafe access or is a symbolic link.
    InsecureFile,
    /// Private file storage requires Unix permissions or Windows protected DACLs.
    UnsupportedFileStore,
}

impl fmt::Display for KeychainError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "credential file operation failed: {error}"),
            Self::Locked => formatter.write_str("Beankeeper needs your computer's keyring to be unlocked. Sign out of your computer and back in, then say 'Set up my books' again."),
            Self::NoProvider => formatter.write_str("Beankeeper couldn’t reach your computer’s password safe. On Ubuntu or Debian, I can help install the gnome-keyring package or enable an existing Secret Service provider, then retry setting up your books."),
            Self::Unavailable => formatter.write_str(
                "OS keychain is unavailable or locked; unlock your login keychain or Secret Service, or recover the ledger with your recorded passphrase",
            ),
            Self::InvalidLedgerPath => formatter.write_str("ledger path is not valid for OS credential storage"),
            Self::EmptyPassphrase => formatter.write_str("stored passphrase cannot be empty"),
            Self::InsecureFile => formatter.write_str("credential file must be a regular, private file in a private directory (Unix modes 0600 and 0700)"),
            Self::UnsupportedFileStore => formatter.write_str("file credential storage requires Unix permissions or Windows protected DACLs"),
        }
    }
}

impl Error for KeychainError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            _ => None,
        }
    }
}

impl From<io::Error> for KeychainError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<keyring::Error> for KeychainError {
    fn from(error: keyring::Error) -> Self {
        // Never retain backend text: malformed credential errors can contain secrets.
        match error {
            keyring::Error::NoStorageAccess(_) => Self::Locked,
            keyring::Error::NoDefaultStore | keyring::Error::PlatformFailure(_) => Self::NoProvider,
            _ => Self::Unavailable,
        }
    }
}

/// Boundary for storing and retrieving the secret associated with one ledger.
pub trait SecretStore {
    /// Save a nonempty passphrase, replacing an existing credential if authorized.
    ///
    /// # Errors
    /// Returns an error when storage is unavailable or the secret is empty.
    fn store(&self, ledger: &Path, passphrase: &SecretString) -> Result<(), KeychainError>;

    /// Return the passphrase, or `None` when this ledger has no credential.
    ///
    /// # Errors
    /// Returns an error when storage is unavailable or an entry is invalid.
    fn load(&self, ledger: &Path) -> Result<Option<SecretString>, KeychainError>;
}

/// Native Keychain on macOS, Credential Manager on Windows, Secret Service on Linux.
#[derive(Debug, Clone, Copy, Default)]
pub struct OsKeychain;

/// Resolve aliases so `./books.db` and its absolute path use the same credential.
///
/// The parent must exist when the ledger has not been created yet.
///
/// # Errors
/// Returns an I/O error for an inaccessible ledger or parent directory.
pub fn canonical_ledger(ledger: &Path) -> Result<PathBuf, KeychainError> {
    match fs::canonicalize(ledger) {
        Ok(path) => Ok(path),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            let name = ledger.file_name().ok_or(KeychainError::InvalidLedgerPath)?;
            let parent = ledger
                .parent()
                .filter(|path| !path.as_os_str().is_empty())
                .unwrap_or(Path::new("."));
            Ok(fs::canonicalize(parent)?.join(name))
        }
        Err(error) => Err(error.into()),
    }
}

fn entry(ledger: &Path) -> Result<keyring::Entry, KeychainError> {
    let path = canonical_ledger(ledger)?;
    let account = path.to_str().ok_or(KeychainError::InvalidLedgerPath)?;
    keyring::Entry::new(SERVICE, account).map_err(KeychainError::from)
}

fn validate_passphrase(passphrase: &SecretString) -> Result<(), KeychainError> {
    if passphrase.expose_secret().is_empty() {
        Err(KeychainError::EmptyPassphrase)
    } else {
        Ok(())
    }
}

impl SecretStore for OsKeychain {
    fn store(&self, ledger: &Path, passphrase: &SecretString) -> Result<(), KeychainError> {
        validate_passphrase(passphrase)?;
        entry(ledger)?
            .set_password(passphrase.expose_secret())
            .map_err(KeychainError::from)
    }

    fn load(&self, ledger: &Path) -> Result<Option<SecretString>, KeychainError> {
        decode_native_password(entry(ledger)?.get_password())
    }
}

// All three native backends pass through this seam. Platform diagnostics may
// carry raw secret bytes, so intentionally discard them before constructing our error.
fn decode_native_password(
    result: Result<String, keyring::Error>,
) -> Result<Option<SecretString>, KeychainError> {
    match result {
        Ok(password) => {
            let password = SecretString::from(password);
            validate_passphrase(&password)?;
            Ok(Some(password))
        }
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(error) => Err(error.into()),
    }
}

/// Private passphrase file for headless setups and isolated CI environments.
///
/// The caller associates this file with a ledger in Desktop's setup metadata.
/// Setup uses this only when explicitly requested. Service installation may select
/// it for a new ledger if the native store is unavailable.
#[derive(Debug, Clone)]
pub struct FileSecretStore {
    path: PathBuf,
}

impl FileSecretStore {
    /// Select a credential file in a private directory.
    #[must_use]
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    /// Credential file recorded in Desktop's setup metadata.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

#[cfg(any(unix, windows))]
fn check_private_parent(path: &Path) -> Result<(), KeychainError> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(KeychainError::InsecureFile);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o077 != 0 {
            return Err(KeychainError::InsecureFile);
        }
    }
    Ok(())
}

#[cfg(any(unix, windows))]
fn file_store_error(error: io::Error) -> KeychainError {
    if error.kind() == io::ErrorKind::PermissionDenied {
        KeychainError::InsecureFile
    } else {
        KeychainError::Io(error)
    }
}

#[cfg(any(unix, windows))]
impl SecretStore for FileSecretStore {
    fn store(&self, _ledger: &Path, passphrase: &SecretString) -> Result<(), KeychainError> {
        validate_passphrase(passphrase)?;
        let parent = self
            .path
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        if !parent.exists() {
            fs::create_dir_all(parent)?;
            restrict_directory(parent)?;
        }
        check_private_parent(parent)?;
        match PrivateFile::open(&self.path) {
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(file_store_error(error)),
        }
        let mut pending = NamedTempFile::new_in(parent)?;
        restrict_file(pending.path())?;
        pending.write_all(passphrase.expose_secret().as_bytes())?;
        pending.as_file().sync_all()?;
        pending
            .persist(&self.path)
            .map_err(|error| KeychainError::Io(error.error))?;
        Ok(())
    }

    fn load(&self, _ledger: &Path) -> Result<Option<SecretString>, KeychainError> {
        let parent = self
            .path
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        let file = match PrivateFile::open(&self.path) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(file_store_error(error)),
        };
        check_private_parent(parent)?;
        let passphrase = file.read_secret(PASSPHRASE_FILE_LIMIT)?;
        validate_passphrase(&passphrase)?;
        Ok(Some(passphrase))
    }
}

#[cfg(not(any(unix, windows)))]
impl SecretStore for FileSecretStore {
    fn store(&self, _ledger: &Path, _passphrase: &SecretString) -> Result<(), KeychainError> {
        Err(KeychainError::UnsupportedFileStore)
    }

    fn load(&self, _ledger: &Path) -> Result<Option<SecretString>, KeychainError> {
        Err(KeychainError::UnsupportedFileStore)
    }
}

/// Keeps an ephemeral owner-only passphrase file alive while upstream reads it.
///
/// Dropping this guard removes the file. It never exposes its contents via Debug.
#[derive(Debug)]
pub struct StoredPassphrase {
    file: NamedTempFile,
}

impl StoredPassphrase {
    /// The file path supplied to upstream's existing passphrase resolver.
    #[must_use]
    pub fn path(&self) -> &Path {
        self.file.path()
    }
}

/// Supply a stored passphrase only when no explicit upstream source was chosen.
///
/// Keep the returned guard alive until the dispatched command has completed.
/// Call this only for ledgers with setup metadata so legacy CLI commands never
/// require an available OS keychain.
///
/// # Errors
/// Returns credential lookup or private temporary-file failures.
pub fn apply_stored_passphrase(
    options: &mut PassphraseOptions,
    ledger: &Path,
    store: &dyn SecretStore,
) -> Result<Option<StoredPassphrase>, KeychainError> {
    if options.passphrase_fd.is_some()
        || options.passphrase_file.is_some()
        || std::env::var("BEANKEEPER_PASSPHRASE_CMD").is_ok_and(|value| !value.is_empty())
    {
        return Ok(None);
    }
    let Some(passphrase) = store.load(ledger)? else {
        return Ok(None);
    };
    validate_passphrase(&passphrase)?;
    let file = super::temporary_passphrase(&passphrase)?;
    options.passphrase_file = Some(file.path().to_owned());
    Ok(Some(StoredPassphrase { file }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use beankeeper_cli::cli::Cli;
    use clap::Parser;

    struct FixedStore;
    impl SecretStore for FixedStore {
        fn store(&self, _ledger: &Path, _secret: &SecretString) -> Result<(), KeychainError> {
            Ok(())
        }
        fn load(&self, _ledger: &Path) -> Result<Option<SecretString>, KeychainError> {
            Ok(Some("not-for-logs".into()))
        }
    }

    #[test]
    fn canonical_identity_survives_ledger_creation() -> Result<(), Box<dyn Error>> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("books.db");
        let before = canonical_ledger(&path)?;
        fs::File::create(&path)?;
        assert_eq!(canonical_ledger(&path)?, before);
        Ok(())
    }

    #[test]
    fn upstream_bridge_is_private_redacted_and_removed() -> Result<(), Box<dyn Error>> {
        let mut cli = Cli::try_parse_from(["bk", "company", "list"])?;
        let guard = apply_stored_passphrase(&mut cli.passphrase, &cli.db, &FixedStore)?
            .ok_or("missing guard")?;
        let path = guard.path().to_owned();
        assert_eq!(fs::read_to_string(&path)?, "not-for-logs");
        assert!(!format!("{guard:?}").contains("not-for-logs"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(fs::metadata(&path)?.permissions().mode() & 0o777, 0o600);
        }
        drop(guard);
        assert!(!path.exists());
        Ok(())
    }

    #[test]
    fn explicit_passphrase_sources_win() -> Result<(), Box<dyn Error>> {
        for args in [
            vec!["bk", "--passphrase-file", "chosen", "company", "list"],
            vec!["bk", "--passphrase-fd", "3", "company", "list"],
        ] {
            let mut cli = Cli::try_parse_from(args)?;
            assert!(apply_stored_passphrase(&mut cli.passphrase, &cli.db, &FixedStore)?.is_none());
        }
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn file_store_roundtrip_rejects_public_or_symlink_credentials() -> Result<(), Box<dyn Error>> {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let directory = tempfile::Builder::new()
            .permissions(fs::Permissions::from_mode(0o700))
            .tempdir()?;
        let path = directory.path().join("secret");
        let ledger = directory.path().join("books.db");
        let store = FileSecretStore::new(path.clone());
        assert!(store.load(&ledger)?.is_none());
        store.store(&ledger, &"private-passphrase".into())?;
        assert_eq!(
            store
                .load(&ledger)?
                .ok_or("secret missing")?
                .expose_secret(),
            "private-passphrase"
        );
        assert_eq!(fs::metadata(&path)?.permissions().mode() & 0o777, 0o600);
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644))?;
        assert!(matches!(
            store.load(&ledger),
            Err(KeychainError::InsecureFile)
        ));
        assert!(matches!(
            store.store(&ledger, &"replacement".into()),
            Err(KeychainError::InsecureFile)
        ));
        fs::remove_file(&path)?;
        let target = directory.path().join("target");
        fs::write(&target, "must-survive")?;
        symlink(&target, &path)?;
        assert!(matches!(
            store.store(&ledger, &"replacement".into()),
            Err(KeychainError::InsecureFile)
        ));
        assert_eq!(fs::read_to_string(target)?, "must-survive");
        Ok(())
    }

    #[test]
    fn native_failures_are_typed_and_backend_secrets_are_discarded() {
        let locked = KeychainError::from(keyring::Error::NoStorageAccess(Box::new(
            io::Error::other("private-secret"),
        )));
        assert!(matches!(locked, KeychainError::Locked));
        let setup = crate::setup::SetupError::from(locked);
        assert!(matches!(
            setup,
            crate::setup::SetupError::KeyringLocked { .. }
        ));
        assert!(setup.to_string().contains("Sign out"));
        assert!(!format!("{setup:?}").contains("private-secret"));
        for error in [
            keyring::Error::NoDefaultStore,
            keyring::Error::PlatformFailure(Box::new(io::Error::other("private-secret"))),
        ] {
            let setup = crate::setup::SetupError::from(KeychainError::from(error));
            assert!(matches!(
                setup,
                crate::setup::SetupError::NoKeyringProvider { .. }
            ));
            assert!(setup.to_string().contains("gnome-keyring"));
            assert!(setup.technical_detail().is_some());
            assert!(!format!("{setup:?}").contains("private-secret"));
        }
    }

    #[test]
    fn rejects_empty_credentials_without_touching_native_store() {
        assert!(matches!(
            OsKeychain.store(Path::new("missing"), &"".into()),
            Err(KeychainError::EmptyPassphrase)
        ));
        assert!(!format!("{}", KeychainError::Unavailable).contains("not-for-logs"));
    }

    #[test]
    fn native_backend_seam_distinguishes_missing_entries_and_redacts_platform_errors()
    -> Result<(), Box<dyn Error>> {
        assert!(decode_native_password(Err(keyring::Error::NoEntry))?.is_none());
        let value = decode_native_password(Ok("native-secret".into()))?.ok_or("missing secret")?;
        assert_eq!(value.expose_secret(), "native-secret");
        assert!(!format!("{value:?}").contains("native-secret"));
        let error =
            decode_native_password(Err(keyring::Error::BadEncoding(b"native-secret".to_vec())))
                .err()
                .ok_or("expected platform error")?;
        assert!(!format!("{error:?}: {error}").contains("native-secret"));
        assert!(matches!(
            decode_native_password(Ok(String::new())),
            Err(KeychainError::EmptyPassphrase)
        ));
        Ok(())
    }

    #[test]
    fn restricts_regular_credentials_and_rejects_directories() -> Result<(), Box<dyn Error>> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("credential");
        fs::write(&path, b"")?;
        restrict_file(&path)?;
        PrivateFile::open(&path)?;
        assert!(restrict_file(directory.path()).is_err());
        assert!(PrivateFile::open(directory.path()).is_err());
        #[cfg(unix)]
        {
            use std::os::unix::fs::{PermissionsExt, symlink};
            fs::set_permissions(&path, fs::Permissions::from_mode(0o644))?;
            assert!(PrivateFile::open(&path).is_err());
            let link = directory.path().join("link");
            symlink(&path, &link)?;
            assert!(restrict_file(&link).is_err());
            assert_eq!(fs::metadata(&path)?.permissions().mode() & 0o777, 0o644);
        }
        Ok(())
    }
}
