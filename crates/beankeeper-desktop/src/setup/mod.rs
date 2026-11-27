//! Encrypted Desktop onboarding using upstream ledger and identity APIs.

mod access;
pub mod config;
pub mod keychain;
pub(crate) mod managed;

pub(crate) use access::ManagedAccess;
pub use access::{AccessOptions, AgentAccess, Preset};

use std::{
    error::Error,
    fmt, fs,
    io::{self, IsTerminal, Write},
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::LazyLock,
    time::Duration,
};

use beankeeper_cli::{
    capability::Capability,
    cli::{Cli, PassphraseOptions},
    db::{
        self, Actor, Db, audit,
        audit::{ActorLabel, BlankActorLabel},
    },
    error::CliError,
    identity::{self, Admin, Granted, IssueRequest, IssuedCredential, VerifiedAgent, WriteMode},
    passphrase,
};
use clap::{Args, parser::ValueSource};
use secrecy::{ExposeSecret, SecretString};
use serde::de::IgnoredAny;

use crate::{
    private_file::{BEARER_FILE_LIMIT, PrivateFile},
    service::{self, ServiceError},
};
use keychain::{FileSecretStore, OsKeychain, SecretStore};

const AGENT_NAME: &str = "desktop_agent";
const SETUP_METADATA_LIMIT: usize = 64 * 1024;

const OWNER_CAPABILITIES: &[Capability] = &[
    Capability::Read,
    Capability::Post,
    Capability::Propose,
    Capability::Approve,
    Capability::Reverse,
    Capability::Reconcile,
    Capability::Budget,
    Capability::Admin,
];

/// One-command setup. All credential inputs are file/descriptor based.
#[derive(Args, Clone, Debug)]
pub struct SetupOptions {
    /// Resume an existing encrypted ledger; never delete it or rotate its root.
    #[arg(long, alias = "resume")]
    pub force: bool,
    /// Do not prompt or display secrets; recovery kit remains private on disk.
    #[arg(long)]
    pub non_interactive: bool,
    /// Seed upstream's sample companies and transactions.
    #[arg(long)]
    pub demo: bool,
    /// Agent capability preset for standalone setup (default: read-only).
    #[arg(long, value_enum)]
    pub preset: Option<Preset>,
    /// Private recovery kit path (default: ledger path with .recovery.txt extension).
    #[arg(long)]
    pub recovery_kit: Option<PathBuf>,
    /// Home directory containing client configs (defaults to the OS user's home).
    #[arg(long)]
    pub config_home: Option<PathBuf>,
    /// Explicit file-backed secret store for headless/test use; bypasses OS keychain.
    #[arg(long)]
    pub secret_store_file: Option<PathBuf>,
    /// Desktop launch options.
    #[command(flatten)]
    pub launch: SetupLaunchOptions,
    /// Local Desktop address; requires a nonzero port.
    #[arg(long, default_value = "127.0.0.1:8750")]
    pub bind: SocketAddr,
}

/// Books selected for setup after applying the managed resume default.
pub struct SetupBooks(PathBuf);

impl TryFrom<(&Path, Option<ValueSource>, &SetupOptions)> for SetupBooks {
    type Error = ServiceError;

    fn try_from(
        (path, source, options): (&Path, Option<ValueSource>, &SetupOptions),
    ) -> Result<Self, Self::Error> {
        if options.force && source == Some(ValueSource::DefaultValue) {
            Ok(Self(
                service::managed_books()?.unwrap_or_else(|| path.into()),
            ))
        } else {
            Ok(Self(path.into()))
        }
    }
}

impl From<SetupBooks> for PathBuf {
    fn from(books: SetupBooks) -> Self {
        books.0
    }
}

/// Options controlling how setup opens the local Desktop workspace.
#[derive(Args, Clone, Debug, Default)]
pub struct SetupLaunchOptions {
    /// Do not start a background Desktop server.
    #[arg(long)]
    pub no_start: bool,
    /// Do not launch the default browser.
    #[arg(long)]
    pub no_open: bool,
}

/// Setup failures; credential content is never included in diagnostics.
#[derive(Debug)]
pub enum SetupError {
    /// A file or terminal operation failed.
    Io(io::Error),
    /// A filesystem boundary denied a write to this directory.
    WriteDenied(crate::folders::WriteDenied),
    /// An upstream ledger operation failed.
    Ledger(CliError),
    /// An OS credential operation failed.
    Keychain(keychain::KeychainError),
    /// A classified native password safe failure.
    Keyring(keychain::KeyringFailure),
    /// Existing configuration cannot be safely merged.
    Config(config::ConfigError),
    /// Inputs or existing state need user attention.
    Invalid(&'static str),
    /// Server startup or browser launch failed.
    Desktop(crate::DesktopError),
}

impl SetupError {
    fn at(self, file: &Path) -> Self {
        match self {
            Self::Io(error) | Self::Keychain(keychain::KeychainError::Io(error)) => {
                crate::folders::WriteDenied::at(crate::folders::Directory::containing(file), &error)
                    .map_or_else(|| Self::Io(error), Self::WriteDenied)
            }
            Self::Config(config::ConfigError::Io(path, error)) => crate::folders::WriteDenied::at(
                crate::folders::Directory::containing(&path),
                &error,
            )
            .map_or_else(
                || Self::Config(config::ConfigError::Io(path, error)),
                Self::WriteDenied,
            ),
            error => error,
        }
    }
    pub(crate) fn service_file(error: crate::service::ServiceError) -> Self {
        match error {
            crate::service::ServiceError::WriteDenied(error) => Self::WriteDenied(error),
            crate::service::ServiceError::Io(error) => Self::Io(error),
            _ => Self::Invalid("cannot update private setup state"),
        }
    }
}
impl fmt::Display for SetupError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(f, "setup file operation failed: {error}"),
            Self::WriteDenied(error) => error.fmt(f),
            Self::Ledger(error) => write!(f, "{error}"),
            Self::Keychain(error) => write!(f, "{error}"),
            Self::Keyring(failure) => failure.fmt(f),
            Self::Config(error) => write!(f, "{error}"),
            Self::Invalid(message) => f.write_str(message),
            Self::Desktop(error) => write!(f, "{error}"),
        }
    }
}
impl Error for SetupError {}
impl From<io::Error> for SetupError {
    fn from(value: io::Error) -> Self {
        Self::Io(value)
    }
}
impl From<CliError> for SetupError {
    fn from(value: CliError) -> Self {
        Self::Ledger(value)
    }
}
impl From<keychain::KeychainError> for SetupError {
    fn from(value: keychain::KeychainError) -> Self {
        match value {
            keychain::KeychainError::Keyring(failure) => Self::Keyring(failure),
            other => Self::Keychain(other),
        }
    }
}
impl From<config::ConfigError> for SetupError {
    fn from(value: config::ConfigError) -> Self {
        Self::Config(value)
    }
}
impl From<crate::DesktopError> for SetupError {
    fn from(value: crate::DesktopError) -> Self {
        Self::Desktop(value)
    }
}

/// Run onboarding with the platform keychain or an explicitly selected file store.
///
/// # Errors
/// Fails closed on existing ledgers, conflicting client configs and secret storage errors.
pub fn run(cli: &mut Cli, options: &SetupOptions) -> Result<(), SetupError> {
    let mut options = options.clone();
    let registration = if options.force
        && crate::service::managed_books_match(&cli.db)
            .map_err(|_| SetupError::Invalid("cannot read managed service registration"))?
    {
        options.launch = SetupLaunchOptions {
            no_start: true,
            no_open: true,
        };
        ClientRegistration::Skip
    } else {
        ClientRegistration::Standalone
    };
    let store = configured_store(&cli.db, &options)?;
    run_with_store(
        cli,
        &options,
        store.as_deref().unwrap_or(&OsKeychain),
        registration,
        None,
    )?;
    eprintln!("{}", crate::product::setup_trial(&cli.db));
    Ok(())
}

#[derive(Clone, Copy)]
pub(crate) enum ClientRegistration<'a> {
    Standalone,
    Managed { previous_executable: &'a Path },
    Skip,
}

/// Passphrase storage for fresh service ledgers. Existing ledgers keep their store.
#[derive(Clone, Copy, Default)]
pub(crate) enum ServiceSecretStorage {
    PrivateFile,
    #[default]
    NativeWithFileFallback,
    NativeRequired,
}

/// Provision service books without prompting, printing secrets or opening a browser.
/// Existing ledgers retain their recorded store, identity and preset.
///
/// # Errors
/// Returns the same fail-closed errors as interactive setup.
pub(crate) fn run_service(
    cli: &mut Cli,
    home: &Path,
    bind: SocketAddr,
    registration: ClientRegistration<'_>,
    storage: ServiceSecretStorage,
    state: &Path,
) -> Result<(), SetupError> {
    run_service_with_native_store(cli, home, bind, registration, storage, &OsKeychain)
        .map_err(|error| error.at(&cli.db))?;
    eprintln!("{}", crate::product::setup_trial_at(&cli.db, state));
    Ok(())
}

fn run_service_with_native_store(
    cli: &mut Cli,
    home: &Path,
    bind: SocketAddr,
    registration: ClientRegistration<'_>,
    storage: ServiceSecretStorage,
    native_store: &dyn SecretStore,
) -> Result<(), SetupError> {
    let mut options = SetupOptions::from_service(home, bind);
    if matches!(storage, ServiceSecretStorage::PrivateFile) && !cli.db.try_exists()? {
        options.secret_store_file = Some(cli.db.with_extension("passphrase"));
    }
    let pending = PendingBootstrap::load(&cli.db)?;
    if let Some(file) = pending.and_then(|pending| pending.secret_store_file) {
        options.secret_store_file = Some(file);
    }
    let store = configured_store(&cli.db, &options)?;
    run_with_store(
        cli,
        &options,
        store.as_deref().unwrap_or(native_store),
        registration,
        Some(storage),
    )
}

fn configured_store(
    ledger: &Path,
    options: &SetupOptions,
) -> Result<Option<Box<dyn SecretStore>>, SetupError> {
    match &options.secret_store_file {
        Some(path) => Ok(Some(Box::new(FileSecretStore::new(path.clone())))),
        None => registered_store(ledger),
    }
}

impl SetupOptions {
    fn managed_access_change(&self) -> managed::AccessChange {
        self.preset
            .map_or(managed::AccessChange::Migrate, |preset| {
                managed::AccessChange::Set(preset.into())
            })
    }

    fn from_service(home: &Path, bind: SocketAddr) -> Self {
        Self {
            force: true,
            non_interactive: true,
            demo: false,
            preset: None,
            recovery_kit: None,
            config_home: Some(home.into()),
            secret_store_file: None,
            launch: SetupLaunchOptions {
                no_start: true,
                no_open: true,
            },
            bind,
        }
    }
}

#[derive(Clone, Copy)]
enum SetupMode {
    Fresh,
    Resume,
}

impl SetupMode {
    fn for_ledger(ledger: &Path, force: bool) -> Result<Self, SetupError> {
        match (ledger.try_exists()?, force) {
            (false, _) => Ok(Self::Fresh),
            (true, true) => Ok(Self::Resume),
            (true, false) => Err(SetupError::Invalid(
                "ledger already exists; use --force to resume setup without deleting data or rotating keys",
            )),
        }
    }
}

#[derive(Clone, Copy)]
enum Provisioning {
    Standalone(Preset),
    Managed(SetupMode),
}

impl From<(Option<Preset>, ClientRegistration<'_>, SetupMode)> for Provisioning {
    fn from(
        (preset, registration, mode): (Option<Preset>, ClientRegistration<'_>, SetupMode),
    ) -> Self {
        match registration {
            ClientRegistration::Standalone => Self::Standalone(preset.unwrap_or_default()),
            ClientRegistration::Managed { .. } | ClientRegistration::Skip => Self::Managed(mode),
        }
    }
}

impl Provisioning {
    fn access(self) -> ManagedAccess {
        match self {
            Self::Standalone(preset) => preset.into(),
            Self::Managed(_) => ManagedAccess::default(),
        }
    }
}

fn prepare_setup(
    cli: &mut Cli,
    options: &SetupOptions,
    storage: Option<ServiceSecretStorage>,
) -> Result<(SetupMode, PathBuf), SetupError> {
    crate::server::validate_bind(options.bind, false)?;
    if options.bind.port() == 0 {
        return Err(SetupError::Invalid("setup requires a nonzero --bind port"));
    }
    if !options.non_interactive && !io::stdin().is_terminal() {
        return Err(SetupError::Invalid(
            "setup requires a terminal; use --non-interactive for automation",
        ));
    }
    let mode = SetupMode::for_ledger(&cli.db, options.force)?;
    let home = options
        .config_home
        .clone()
        .or_else(user_home)
        .ok_or(SetupError::Invalid(
            "cannot find user home; supply --config-home",
        ))?;
    fs::create_dir_all(
        cli.db
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
            .unwrap_or(Path::new(".")),
    )?;
    cli.db = keychain::canonical_ledger(&cli.db)?;
    if matches!(
        (storage, mode),
        (Some(ServiceSecretStorage::NativeRequired), SetupMode::Fresh)
    ) {
        require_unused_store_marker(&cli.db)?;
    }
    Ok((mode, home))
}

fn run_with_store(
    cli: &mut Cli,
    options: &SetupOptions,
    store: &dyn SecretStore,
    registration: ClientRegistration<'_>,
    storage: Option<ServiceSecretStorage>,
) -> Result<(), SetupError> {
    let (mode, home) = prepare_setup(cli, options, storage)?;
    let provisioning = Provisioning::from((options.preset, registration, mode));
    let token_file = cli.db.with_extension("agent-token");
    let owner_file = cli.db.with_extension("owner-token");
    let kit = options
        .recovery_kit
        .clone()
        .unwrap_or_else(|| cli.db.with_extension("recovery.txt"));
    let url = format!("{}mcp", crate::server::local_url(options.bind));
    let executable = std::env::current_exe()?;
    let plan =
        prepare_client_configs(registration, &home, &executable, &cli.db, &token_file, &url)?;
    // Validate output collisions before creating a ledger or storing credentials.
    if matches!(mode, SetupMode::Fresh)
        && !(storage.is_some() && PendingBootstrap::load(&cli.db)?.is_some())
        && [token_file.as_path(), owner_file.as_path(), kit.as_path()]
            .iter()
            .any(|path| path.exists())
    {
        return Err(SetupError::Invalid(
            "setup output already exists; choose a fresh ledger/recovery path",
        ));
    }
    let paths = SetupPaths {
        token_file: &token_file,
        owner_file: &owner_file,
        kit: &kit,
    };
    validate_distinct_outputs(
        &cli.db,
        &paths,
        plan.iter().flat_map(config::ConfigPlan::paths),
        options.secret_store_file.as_deref(),
        cli.passphrase.passphrase_file.as_deref(),
    )?;
    if kit.exists() {
        PrivateFile::open(&kit)?;
    }
    let (secret, automatic_file) = bootstrap_secret(
        cli,
        store,
        storage,
        mode,
        options.secret_store_file.as_deref(),
    )?;
    if !kit.exists() {
        private_create(
            &kit,
            recovery_kit(&cli.db, &owner_file, secret.expose_secret()).as_bytes(),
        )?;
    }
    let temporary = temporary_passphrase(&secret).map_err(|error| {
        let directory = temporary_secret_directory();
        crate::folders::WriteDenied::at(directory, &error)
            .map_or_else(|| SetupError::Io(error), SetupError::WriteDenied)
    })?;
    cli.passphrase.passphrase_fd = None;
    cli.passphrase.passphrase_file = Some(temporary.path().into());
    if matches!(mode, SetupMode::Fresh) {
        initialize_ledger(cli, &secret, options.demo)?;
    }
    let db = crate::backups::open_for_provisioning(&cli.db, &secret)?;
    provision(&db, provisioning, &token_file, &owner_file)?;
    if storage.is_none() && matches!(registration, ClientRegistration::Skip) {
        managed::reconcile(&db, &cli.db, options.managed_access_change())?;
    }
    write_store_marker(
        &cli.db,
        options.secret_store_file.as_deref(),
        automatic_file.as_deref(),
    )?;
    let configs = match plan {
        Some(plan) => plan.write()?,
        None => Vec::new(),
    };
    // The service CLI renders its result separately. Managed provisioning
    // also runs on stdio MCP, where stdout belongs to the transport.
    if storage.is_none() {
        println!(
            "Encrypted ledger: {}\nRecovery kit: {}\nKeep a separate offline copy of the recovery kit. Losing both it and the keychain means losing access.",
            cli.db.display(),
            paths.kit.display(),
        );
    }
    finish_setup(cli, options, &paths, configs, &secret, &temporary, mode)?;
    if storage.is_some() {
        PendingBootstrap::finish(&cli.db)?;
    }
    Ok(())
}

fn require_unused_store_marker(ledger: &Path) -> Result<(), SetupError> {
    match ledger.with_extension("desktop.json").symlink_metadata() {
        Ok(_) => Err(SetupError::Invalid(
            "Desktop setup metadata exists without its ledger; restore the original ledger before retrying extension setup",
        )),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn prepare_client_configs(
    registration: ClientRegistration<'_>,
    home: &Path,
    executable: &Path,
    ledger: &Path,
    token_file: &Path,
    url: &str,
) -> Result<Option<config::ConfigPlan>, SetupError> {
    let plan = match registration {
        ClientRegistration::Standalone => Some(config::prepare_configs(
            home, executable, ledger, token_file, url,
        )?),
        ClientRegistration::Managed {
            previous_executable,
        } => Some(config::prepare_service_configs(
            home,
            executable,
            previous_executable,
        )?),
        ClientRegistration::Skip => None,
    };
    Ok(plan)
}

/// A private durable record proves that a managed fresh setup reached secret
/// storage. It contains only the store location, never the secret itself.
#[derive(serde::Serialize, serde::Deserialize)]
#[serde(try_from = "PendingBootstrapData")]
struct PendingBootstrap {
    secret_store_file: Option<PathBuf>,
}
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct PendingBootstrapData {
    secret_store_file: Option<PathBuf>,
}
impl TryFrom<PendingBootstrapData> for PendingBootstrap {
    type Error = SetupError;
    fn try_from(data: PendingBootstrapData) -> Result<Self, Self::Error> {
        if data
            .secret_store_file
            .as_ref()
            .is_some_and(|path| !path.is_absolute())
        {
            return Err(SetupError::Invalid(
                "unfinished setup requires an absolute secret-store path",
            ));
        }
        Ok(Self {
            secret_store_file: data.secret_store_file,
        })
    }
}
impl PendingBootstrap {
    fn path(books: &Path) -> PathBuf {
        books.with_extension("setup-pending.json")
    }
    fn load(books: &Path) -> Result<Option<Self>, SetupError> {
        match crate::service::read_private(&Self::path(books)) {
            Ok(pending) => Ok(Some(pending)),
            Err(crate::service::ServiceError::Io(error))
                if error.kind() == io::ErrorKind::NotFound =>
            {
                Ok(None)
            }
            Err(error) => Err(SetupError::service_file(error)),
        }
    }
    fn save(&self, books: &Path) -> Result<(), SetupError> {
        crate::service::private_json(&Self::path(books), self).map_err(SetupError::service_file)
    }
    fn finish(books: &Path) -> io::Result<()> {
        match fs::remove_file(Self::path(books)) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            result => result,
        }
    }
}

fn bootstrap_secret(
    cli: &Cli,
    store: &dyn SecretStore,
    storage: Option<ServiceSecretStorage>,
    mode: SetupMode,
    explicit_file: Option<&Path>,
) -> Result<(SecretString, Option<PathBuf>), SetupError> {
    if storage.is_some()
        && matches!(mode, SetupMode::Fresh)
        && let Some(pending) = PendingBootstrap::load(&cli.db)?
    {
        let secret = store.load(&cli.db)?.ok_or(SetupError::Invalid(
            "unfinished setup's stored passphrase is unavailable; restore it before retrying",
        ))?;
        return Ok((secret, pending.secret_store_file));
    }
    let (secret, automatic) = store_ledger_secret(cli, store, storage, mode)?;
    if storage.is_some() && matches!(mode, SetupMode::Fresh) {
        PendingBootstrap {
            secret_store_file: explicit_file
                .map(Path::to_path_buf)
                .or_else(|| automatic.clone()),
        }
        .save(&cli.db)?;
    }
    Ok((secret, automatic))
}

fn store_ledger_secret(
    cli: &Cli,
    store: &dyn SecretStore,
    storage: Option<ServiceSecretStorage>,
    mode: SetupMode,
) -> Result<(SecretString, Option<PathBuf>), SetupError> {
    let supplied = passphrase::resolve_passphrase(
        cli.passphrase.passphrase_fd,
        cli.passphrase.passphrase_file.as_deref(),
        false,
    )?;
    let secret = match (supplied, mode) {
        (Some(secret), _) => secret,
        (None, SetupMode::Resume) => store.load(&cli.db)?.ok_or(SetupError::Invalid(
            "keychain entry missing; restore the passphrase from your recovery kit using --passphrase-file",
        ))?,
        (None, SetupMode::Fresh) => generate_passphrase()?,
    };
    // Verify supplied recovery credentials before replacing a stored credential.
    if matches!(mode, SetupMode::Resume) {
        crate::backups::open(&cli.db, Some(&secret))?;
    }
    // Establish a recoverable credential before initialization.
    let automatic_file = match (store.store(&cli.db, &secret), storage, mode) {
        (Ok(()), _, _) => None,
        (
            Err(keychain::KeychainError::Unavailable | keychain::KeychainError::Keyring(_)),
            Some(ServiceSecretStorage::NativeWithFileFallback),
            SetupMode::Fresh,
        ) => {
            let fallback = cli.db.with_extension("passphrase");
            FileSecretStore::new(fallback.clone()).store(&cli.db, &secret)?;
            eprintln!("OS secret store unavailable; using a private credential file.");
            Some(fallback)
        }
        (Err(error), _, _) => return Err(error.into()),
    };
    Ok((secret, automatic_file))
}

fn finish_setup(
    cli: &Cli,
    options: &SetupOptions,
    paths: &SetupPaths<'_>,
    configs: Vec<PathBuf>,
    secret: &SecretString,
    temporary: &tempfile::NamedTempFile,
    mode: SetupMode,
) -> Result<(), SetupError> {
    if matches!(mode, SetupMode::Fresh) {
        let backups =
            crate::backups::LocalBackups::new(cli.db.as_path().try_into()?, Some(secret.clone()));
        backups.automatic();
    }
    eprintln!(
        "Agent token is a secret: {}. Owner credential stays separate: {}.",
        paths.token_file.display(),
        paths.owner_file.display()
    );
    for path in configs {
        eprintln!("MCP config: {}", path.display());
    }
    if !options.non_interactive && matches!(mode, SetupMode::Fresh) {
        show_once(secret)?;
    }
    if !options.launch.no_start {
        super_start(cli, options, temporary)?;
    }
    if !options.launch.no_open {
        if options.launch.no_start {
            let token = PrivateFile::open(paths.token_file)?.read_secret(BEARER_FILE_LIMIT)?;
            if !running_for_ledger(options.bind, &token)? {
                return Err(crate::DesktopError::NotRunning(crate::server::local_url(
                    options.bind,
                ))
                .into());
            }
        }
        crate::browser::open(crate::cli::OpenOptions {
            bind: Some(options.bind.try_into().map_err(SetupError::Invalid)?),
        })?;
    }
    Ok(())
}

struct SetupPaths<'a> {
    token_file: &'a Path,
    owner_file: &'a Path,
    kit: &'a Path,
}

fn validate_distinct_outputs<'a>(
    ledger: &Path,
    paths: &SetupPaths<'_>,
    config_paths: impl Iterator<Item = &'a Path>,
    store_file: Option<&Path>,
    passphrase_file: Option<&Path>,
) -> Result<(), SetupError> {
    let mut outputs = vec![
        ledger.to_owned(),
        paths.token_file.to_owned(),
        paths.owner_file.to_owned(),
        paths.kit.to_owned(),
        ledger.with_extension("desktop.json"),
        ledger.with_extension("agent-access.json"),
        ledger.with_extension("agent-access.lock"),
        ledger.with_extension("desktop.log"),
    ];
    outputs.extend(config_paths.map(Path::to_path_buf));
    outputs.extend(store_file.map(Path::to_path_buf));
    outputs.extend(passphrase_file.map(Path::to_path_buf));
    let mut seen = std::collections::HashSet::new();
    for path in outputs {
        if !seen.insert(future_path(&path)?) {
            return Err(SetupError::Invalid(
                "ledger, credential, recovery, source and client config paths must be distinct",
            ));
        }
    }
    Ok(())
}

fn initialize_ledger(cli: &Cli, secret: &SecretString, demo: bool) -> Result<(), SetupError> {
    // SQLCipher and demo writes stay inside an owner-only staging directory.
    let parent = cli
        .db
        .parent()
        .ok_or(SetupError::Invalid("ledger needs a parent directory"))?;
    let mut staging_builder = tempfile::Builder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        staging_builder.permissions(fs::Permissions::from_mode(0o700));
    }
    let staging = staging_builder.tempdir_in(parent)?;
    keychain::restrict_directory(staging.path())?;
    let staged = staging.path().join("books.db");
    if demo {
        beankeeper_cli::commands::init::create_demo_ledger(
            &staged,
            Some(secret),
            &cli.principal(),
        )?;
    } else {
        Db::open(&staged, Some(secret))?;
    }
    set_private(&staged)?;
    // An atomic no-clobber link also protects against a concurrent setup.
    fs::hard_link(&staged, &cli.db)?;
    set_private(&cli.db)?;
    Ok(())
}

fn write_store_marker(
    ledger: &Path,
    explicit_file: Option<&Path>,
    automatic_file: Option<&Path>,
) -> Result<(), SetupError> {
    let marker = ledger.with_extension("desktop.json");
    if !marker.exists() || explicit_file.is_some() {
        let file_store = explicit_file
            .or(automatic_file)
            .map(std::path::absolute)
            .transpose()?;
        let metadata = serde_json::json!({"secret_store_file": file_store});
        if marker.exists() {
            PrivateFile::open(&marker)?;
            let mut pending = tempfile::NamedTempFile::new_in(
                marker
                    .parent()
                    .ok_or(SetupError::Invalid("metadata needs parent"))?,
            )?;
            keychain::restrict_file(pending.path())?;
            pending.write_all(metadata.to_string().as_bytes())?;
            pending.as_file().sync_all()?;
            pending.persist(&marker).map_err(|error| error.error)?;
        } else {
            private_create(&marker, metadata.to_string().as_bytes())?;
        }
    }
    Ok(())
}

pub(crate) struct OwnerCredential(VerifiedAgent);

impl TryFrom<VerifiedAgent> for OwnerCredential {
    type Error = CliError;

    fn try_from(identity: VerifiedAgent) -> Result<Self, Self::Error> {
        if !identity.allows(Capability::Admin)? {
            return Err(CliError::Forbidden(
                "owner credential lacks ledger/admin".into(),
            ));
        }
        Ok(Self(identity))
    }
}

impl TryFrom<OwnerCredential> for Actor {
    type Error = BlankActorLabel;

    fn try_from(owner: OwnerCredential) -> Result<Self, Self::Error> {
        static OWNER_LABEL: LazyLock<Result<ActorLabel, BlankActorLabel>> =
            LazyLock::new(|| "desktop-owner".parse());

        let label = LazyLock::force(&OWNER_LABEL).clone()?;
        Ok(Self::new(label).with_identity(owner.0))
    }
}

enum SetupAuthority {
    Owner(Box<OwnerCredential>),
    FirstRun,
}

impl SetupAuthority {
    fn resolve(db: &Db, owner_file: &Path) -> Result<Self, SetupError> {
        let conn = db.read();
        let policy = db::policy::get(conn)?;
        let registered = has_registered_tokens(identity::list(conn)?)?;
        if !(policy.enforce || registered || owner_file.try_exists()?) {
            return Ok(Self::FirstRun);
        }
        let raw = PrivateFile::open(owner_file)
            .and_then(|file| file.read_secret(BEARER_FILE_LIMIT))
            .map_err(|_| {
                SetupError::Invalid(
                    "existing ledger requires its private owner credential; recover it before resuming setup",
                )
            })?;
        let owner = OwnerCredential::try_from(identity::verify(conn, raw.expose_secret().trim())?)?;
        Ok(Self::Owner(Box::new(owner)))
    }

    fn into_actor(self) -> Result<Actor, CliError> {
        Ok(match self {
            Self::Owner(owner) => Actor::try_from(*owner)?,
            Self::FirstRun => Actor::new("desktop-setup".parse()?),
        })
    }
}

fn has_registered_tokens(wire: serde_json::Value) -> Result<bool, SetupError> {
    // Upstream exposes JSON rather than a token count. Any registry row
    // requires the owner regardless of its contents, including revoked tokens.
    let rows: Vec<IgnoredAny> = serde_json::from_value(wire)
        .map_err(|_| SetupError::Invalid("upstream returned an invalid agent registry"))?;
    Ok(!rows.is_empty())
}

fn provision(
    db: &Db,
    provisioning: Provisioning,
    token_file: &Path,
    owner_file: &Path,
) -> Result<(), SetupError> {
    let conn = db.read();
    if token_file.exists() {
        let raw = PrivateFile::open(token_file)?.read_secret(BEARER_FILE_LIMIT)?;
        let agent = identity::verify(conn, raw.expose_secret().trim())?;
        if !matches!(provisioning, Provisioning::Managed(SetupMode::Resume))
            && Preset::try_from(&agent) != Ok(provisioning.access().selection().0)
        {
            return Err(SetupError::Invalid(
                "existing token has a different preset; use upstream agent issuance for explicit credential changes",
            ));
        }
        if !db::policy::get(conn)?.enforce {
            return Err(SetupError::Invalid(
                "existing setup policy is not enforced; review policy before resuming",
            ));
        }
        return Ok(());
    }
    let actor = SetupAuthority::resolve(db, owner_file)?.into_actor()?;
    if !beankeeper_cli::commands::setup::status(db)?.owner_key_initialized {
        Granted::<Admin>::with_db(db, &actor, identity::init_root)?;
    }
    let bootstrap_owner = if actor.identity().is_none() {
        let issued = issue(
            db,
            &actor,
            "desktop_owner",
            OWNER_CAPABILITIES,
            WriteMode::default(),
        )?;
        let bearer = issued.token().expose_secret();
        let owner = identity::verify(conn, bearer)?;
        private_create(owner_file, bearer.as_bytes())?;
        Some(actor.clone().with_identity(owner))
    } else {
        None
    };
    let actor = bootstrap_owner.as_ref().unwrap_or(&actor);
    if db::list_companies(conn)?.is_empty() {
        Granted::<Admin>::with_db(db, actor, |grant| {
            db::create_company(grant, &"personal".parse()?, &"Personal".parse()?, None)
        })?;
    }
    let (capabilities, mode) = provisioning.access().grants();
    let issued = issue(db, actor, AGENT_NAME, capabilities, mode)?;
    Granted::<Admin>::with_db(db, actor, |grant| audit::set_policy(grant, true))?;
    let origin = match provisioning {
        Provisioning::Standalone(_) => managed::Origin::OwnerChoice,
        Provisioning::Managed(_) => managed::Origin::Default,
    };
    managed::record(token_file, issued.token_jti.clone(), origin)?;
    private_create(token_file, issued.token().expose_secret().as_bytes())?;
    Ok(())
}

fn issue(
    db: &Db,
    actor: &Actor,
    name: &str,
    capabilities: &[Capability],
    write_mode: WriteMode,
) -> Result<IssuedCredential, SetupError> {
    let capabilities = capabilities
        .iter()
        .map(|capability| capability.path().to_owned())
        .collect::<Vec<_>>();
    Ok(Granted::<Admin>::with_db(db, actor, |grant| {
        identity::issue_credential_with_mode(
            grant,
            &IssueRequest {
                name,
                capabilities: &capabilities,
                ttl: Duration::from_hours(8760),
                public_key: None,
                generate_key: true,
            },
            write_mode,
        )
    })?)
}

pub(crate) fn generate_passphrase() -> Result<SecretString, SetupError> {
    use fmt::Write;
    let mut bytes = [0; 32];
    getrandom::fill(&mut bytes)
        .map_err(|_| SetupError::Invalid("OS random generator unavailable"))?;
    let mut value = String::with_capacity(64);
    for byte in bytes {
        write!(value, "{byte:02x}")
            .map_err(|_| SetupError::Invalid("cannot format recovery secret"))?;
    }
    Ok(value.into())
}

fn recovery_kit(ledger: &Path, owner: &Path, passphrase: &str) -> String {
    format!(
        "Beankeeper Desktop recovery kit\nKEEP PRIVATE. Store an offline copy separately from the ledger.\nLedger: {}\nPassphrase: {passphrase}\nOwner token file: {} (never share with agents; back this file up separately too)\n\nDesktop saves automatic local backups. Ask your agent to list or restore a backup while your books are available.\nIf the ledger file is missing, find its snapshot at .beankeeper-backups/<ledger-filename>/<bkbackup_…>/books.db beside the original ledger. From the ledger's directory, run bk --db <new-ledger> --passphrase-file <private-passphrase-file> recover --input .beankeeper-backups/<ledger-filename>/<bkbackup_…>/books.db. With policy enforcement, supply the matching private owner credential through BK_AGENT_TOKEN. Keep the sibling attachments directory with the recovered ledger.\nIf keychain access is lost, copy ONLY the Passphrase value to a private file and use --passphrase-file, then rerun setup --force.\nA backup is encrypted and still needs this passphrase. There is no backdoor.\nTokens expire after one year; use upstream agent issue/revoke to renew them.\n",
        ledger.display(),
        owner.display()
    )
}

pub(crate) fn private_create(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    keychain::restrict_file(path).map_err(io::Error::other)?;
    file.write_all(bytes)?;
    file.sync_all()
}

fn set_private(path: &Path) -> io::Result<()> {
    keychain::restrict_file(path)
}

pub(crate) fn temporary_secret_directory() -> crate::folders::Directory {
    crate::folders::Directory::new(&std::env::temp_dir())
}
pub(crate) fn temporary_passphrase(secret: &SecretString) -> io::Result<tempfile::NamedTempFile> {
    let mut file = tempfile::NamedTempFile::new_in(temporary_secret_directory().as_ref())?;
    keychain::restrict_file(file.path())?;
    file.write_all(secret.expose_secret().as_bytes())?;
    file.flush()?;
    Ok(file)
}

fn show_once(secret: &SecretString) -> io::Result<()> {
    #[cfg(unix)]
    let terminal = "/dev/tty";
    #[cfg(windows)]
    let terminal = "CONOUT$";
    #[cfg(not(any(unix, windows)))]
    return Ok(());
    #[cfg(any(unix, windows))]
    {
        let mut output = fs::OpenOptions::new().write(true).open(terminal)?;
        writeln!(
            output,
            "Record your recovery passphrase now (shown once): {}",
            secret.expose_secret()
        )?;
        output.flush()
    }
}

pub(crate) fn user_home() -> Option<PathBuf> {
    std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" }).map(PathBuf::from)
}

fn super_start(
    cli: &Cli,
    options: &SetupOptions,
    temporary: &tempfile::NamedTempFile,
) -> Result<(), SetupError> {
    use std::{
        process::{Command, Stdio},
        time::Instant,
    };
    // Accept an existing server only if it authenticates this ledger's token.
    let token_path = cli.db.with_extension("agent-token");
    let token = PrivateFile::open(&token_path)?.read_secret(BEARER_FILE_LIMIT)?;
    if running_for_ledger(options.bind, &token)? {
        return Ok(());
    }
    let log = cli.db.with_extension("desktop.log");
    if !log.exists() {
        private_create(&log, b"")?;
    }
    set_private(&log)?;
    let output = fs::OpenOptions::new().append(true).open(log)?;
    let child = Command::new(std::env::current_exe()?)
        .arg("--db")
        .arg(&cli.db)
        .arg("--passphrase-file")
        .arg(temporary.path())
        .args(["serve", "--bind", &options.bind.to_string()])
        .env_remove("BK_AGENT_TOKEN")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(output)
        .spawn()?;
    let mut pending = PendingServer {
        child,
        ready: false,
    };
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if pending.child.try_wait()?.is_some() {
            return Err(SetupError::Invalid(
                "Desktop startup failed; inspect the private .desktop.log file",
            ));
        }
        if running_for_ledger(options.bind, &token)? {
            eprintln!("Desktop started (PID {}).", pending.child.id());
            pending.ready = true;
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(SetupError::Invalid(
                "Desktop startup timed out; inspect the private .desktop.log file",
            ));
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Resolve stored credentials only for ledgers enrolled by Desktop setup.
/// Explicit upstream sources always take precedence.
///
/// # Errors
/// Returns invalid metadata, keychain or private temporary-file errors.
pub fn stored_passphrase(cli: &mut Cli) -> Result<Option<keychain::StoredPassphrase>, SetupError> {
    stored_passphrase_for(&mut cli.passphrase, &cli.db)
}

pub(crate) fn stored_passphrase_for(
    options: &mut PassphraseOptions,
    path: &Path,
) -> Result<Option<keychain::StoredPassphrase>, SetupError> {
    if options.passphrase_fd.is_some()
        || options.passphrase_file.is_some()
        || std::env::var_os("BEANKEEPER_PASSPHRASE_CMD").is_some_and(|value| !value.is_empty())
    {
        return Ok(None);
    }
    if !path.exists() {
        return Ok(None);
    }
    let ledger = keychain::canonical_ledger(path)?;
    let marker = ledger.with_extension("desktop.json");
    if !marker.exists() {
        return Ok(None);
    }
    let store = registered_store(&ledger)?
        .ok_or(SetupError::Invalid("Desktop setup metadata is missing"))?;
    let guard = keychain::apply_stored_passphrase(options, &ledger, store.as_ref())?;
    if guard.is_none() {
        return Err(SetupError::Invalid(
            "keychain entry missing; recover the passphrase from your recovery kit with --passphrase-file",
        ));
    }
    Ok(guard)
}

fn registered_store(ledger: &Path) -> Result<Option<Box<dyn SecretStore>>, SetupError> {
    if !ledger.exists() {
        return Ok(None);
    }
    let marker = keychain::canonical_ledger(ledger)?.with_extension("desktop.json");
    let file = match PrivateFile::open(&marker) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let bytes = file.read_bytes(SETUP_METADATA_LIMIT)?;
    let metadata: serde_json::Value = serde_json::from_slice(bytes.expose_secret())
        .map_err(|_| SetupError::Invalid("invalid Desktop setup metadata"))?;
    let store: Box<dyn SecretStore> = match metadata.get("secret_store_file") {
        Some(serde_json::Value::String(path)) => Box::new(FileSecretStore::new(path.into())),
        Some(serde_json::Value::Null) => Box::new(OsKeychain),
        _ => return Err(SetupError::Invalid("invalid Desktop secret store metadata")),
    };
    Ok(Some(store))
}

pub(crate) fn running_for_ledger(
    bind: SocketAddr,
    token: &SecretString,
) -> Result<bool, SetupError> {
    if crate::instance::ListenerAddress::try_from(bind)
        .ok()
        .and_then(|address| crate::instance::Health::probe(address).ok())
        .is_none()
    {
        return Ok(false);
    }
    let client = reqwest::blocking::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(2))
        .build()
        .map_err(crate::DesktopError::from)?;
    let url = format!("{}mcp", crate::server::local_url(bind));
    let response = client.post(&url).bearer_auth(token.expose_secret().trim())
        .header("Accept", "application/json, text/event-stream")
        .json(&serde_json::json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{
            "protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"desktop-setup","version":env!("CARGO_PKG_VERSION")}
        }})).send().map_err(crate::DesktopError::from)?;
    if !response.status().is_success() {
        return Err(SetupError::Invalid(
            "the running Desktop server did not accept this ledger's token; stop it or choose a different --bind port",
        ));
    }
    let session = response
        .headers()
        .get("Mcp-Session-Id")
        .cloned()
        .ok_or(SetupError::Invalid(
            "running server did not establish an MCP session",
        ))?;
    let _ = client
        .delete(&url)
        .bearer_auth(token.expose_secret().trim())
        .header("Mcp-Session-Id", session)
        .header("MCP-Protocol-Version", "2025-11-25")
        .send();
    Ok(true)
}

// Resolve symlinked existing ancestors as well as paths not created yet.
fn future_path(path: &Path) -> io::Result<PathBuf> {
    let mut normalized = PathBuf::new();
    for component in std::path::absolute(path)?.components() {
        match component {
            std::path::Component::ParentDir => {
                normalized.pop();
            }
            std::path::Component::CurDir => {}
            other => normalized.push(other),
        }
    }
    let mut suffix = Vec::new();
    loop {
        match fs::canonicalize(&normalized) {
            Ok(mut base) => {
                for part in suffix.into_iter().rev() {
                    base.push(part);
                }
                return Ok(base);
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                suffix.push(
                    normalized
                        .file_name()
                        .ok_or_else(|| io::Error::other("path has no existing ancestor"))?
                        .to_os_string(),
                );
                normalized.pop();
            }
            Err(error) => return Err(error),
        }
    }
}

// Until readiness succeeds, every error path must terminate and reap the child.
struct PendingServer {
    child: std::process::Child,
    ready: bool,
}
impl Drop for PendingServer {
    fn drop(&mut self) {
        if !self.ready {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct UnavailableStore(keychain::KeyringFailure);
    impl SecretStore for UnavailableStore {
        fn store(
            &self,
            _ledger: &Path,
            _secret: &SecretString,
        ) -> Result<(), keychain::KeychainError> {
            Err(keychain::KeychainError::Keyring(self.0))
        }
        fn load(&self, _ledger: &Path) -> Result<Option<SecretString>, keychain::KeychainError> {
            Err(keychain::KeychainError::Keyring(self.0))
        }
    }

    #[test]
    fn extension_setup_with_unavailable_keychain_creates_no_ledger_or_credential()
    -> Result<(), Box<dyn Error>> {
        use clap::Parser;
        for failure in [
            keychain::KeyringFailure::Locked,
            keychain::KeyringFailure::NoProvider,
        ] {
            let root = tempfile::tempdir()?;
            keychain::restrict_directory(root.path())?;
            let books = root.path().join("books.db");
            let mut cli =
                Cli::try_parse_from(["bk", "--db", books.to_str().ok_or("path")?, "mcp", "serve"])?;
            let result = run_service_with_native_store(
                &mut cli,
                root.path(),
                "127.0.0.1:8750".parse()?,
                ClientRegistration::Skip,
                ServiceSecretStorage::NativeRequired,
                &UnavailableStore(failure),
            );
            assert!(matches!(result, Err(SetupError::Keyring(actual)) if actual == failure));
            assert!(fs::read_dir(root.path())?.next().is_none());
        }
        Ok(())
    }

    #[test]
    fn extension_setup_preserves_orphaned_store_metadata() -> Result<(), Box<dyn Error>> {
        use clap::Parser;
        let root = tempfile::tempdir()?;
        keychain::restrict_directory(root.path())?;
        let books = root.path().join("books.db");
        let marker = books.with_extension("desktop.json");
        let credential = books.with_extension("passphrase");
        let metadata = serde_json::json!({"secret_store_file": credential});
        private_create(&marker, metadata.to_string().as_bytes())?;
        let mut cli =
            Cli::try_parse_from(["bk", "--db", books.to_str().ok_or("path")?, "mcp", "serve"])?;
        let result = run_service_with_native_store(
            &mut cli,
            root.path(),
            "127.0.0.1:8750".parse()?,
            ClientRegistration::Skip,
            ServiceSecretStorage::NativeRequired,
            &UnavailableStore(keychain::KeyringFailure::NoProvider),
        );
        assert!(matches!(
            result,
            Err(SetupError::Invalid(message)) if message.contains("restore the original ledger")
        ));
        assert_eq!(fs::read_to_string(marker)?, metadata.to_string());
        assert!(!books.try_exists()?);
        assert!(!credential.try_exists()?);
        Ok(())
    }

    #[test]
    fn service_fallback_provisions_encrypted_books_once_and_keeps_recovery_private()
    -> Result<(), Box<dyn Error>> {
        use clap::Parser;
        let root = tempfile::tempdir()?;
        keychain::restrict_directory(root.path())?;
        let home = root.path().join("clients");
        let books = root.path().join("books.db");
        let mut cli =
            Cli::try_parse_from(["bk", "--db", books.to_str().ok_or("path")?, "mcp", "serve"])?;
        run_service_with_native_store(
            &mut cli,
            &home,
            "127.0.0.1:8750".parse()?,
            ClientRegistration::Managed {
                previous_executable: &std::env::current_exe()?,
            },
            ServiceSecretStorage::NativeWithFileFallback,
            &UnavailableStore(keychain::KeyringFailure::NoProvider),
        )?;
        let marker: serde_json::Value =
            serde_json::from_slice(&fs::read(books.with_extension("desktop.json"))?)?;
        assert_eq!(
            marker["secret_store_file"],
            books
                .with_extension("passphrase")
                .to_string_lossy()
                .as_ref()
        );
        for extension in ["passphrase", "recovery.txt", "agent-token", "owner-token"] {
            PrivateFile::open(&books.with_extension(extension))?;
        }
        assert!(!fs::read(&books)?.starts_with(b"SQLite format 3"));
        let recovery = fs::read(books.with_extension("recovery.txt"))?;
        let token = fs::read(books.with_extension("agent-token"))?;
        let mut cli =
            Cli::try_parse_from(["bk", "--db", books.to_str().ok_or("path")?, "mcp", "serve"])?;
        run_service(
            &mut cli,
            &home,
            "127.0.0.1:8750".parse()?,
            ClientRegistration::Managed {
                previous_executable: &std::env::current_exe()?,
            },
            ServiceSecretStorage::NativeWithFileFallback,
            &home.join("state"),
        )?;
        let mut cli =
            Cli::try_parse_from(["bk", "--db", books.to_str().ok_or("path")?, "mcp", "serve"])?;
        run_service_with_native_store(
            &mut cli,
            &home,
            "127.0.0.1:8750".parse()?,
            ClientRegistration::Skip,
            ServiceSecretStorage::NativeRequired,
            &UnavailableStore(keychain::KeyringFailure::NoProvider),
        )?;
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&fs::read(
                books.with_extension("desktop.json")
            )?)?,
            marker
        );
        assert_eq!(fs::read(books.with_extension("recovery.txt"))?, recovery);
        assert_eq!(fs::read(books.with_extension("agent-token"))?, token);
        Ok(())
    }

    #[test]
    fn provision_attributes_policy_to_the_owner_and_recovers_the_agent_file()
    -> Result<(), Box<dyn Error>> {
        let root = tempfile::tempdir()?;
        keychain::restrict_directory(root.path())?;
        let books = root.path().join("books.db");
        let db = Db::open(&books, Some(&SecretString::from("setup-policy-fixture")))?;
        let provisioning = Provisioning::Managed(SetupMode::Fresh);
        let owner_file = books.with_extension("owner-token");
        let agent_file = books.with_extension("agent-token");
        let read_identity = |path: &Path| -> Result<VerifiedAgent, Box<dyn Error>> {
            let token = PrivateFile::open(path)?.read_secret(BEARER_FILE_LIMIT)?;
            Ok(identity::verify(db.read(), token.expose_secret())?)
        };

        provision(&db, provisioning, &agent_file, &owner_file)?;
        let owner = read_identity(&owner_file)?;
        assert!(owner.allows(Capability::Admin)?);
        assert!(db::policy::get(db.read())?.enforce);
        let policy_events = || {
            let rows = audit::list_audit(
                db.read(),
                &audit::ListAuditParams {
                    actor: None,
                    agent_uri: None,
                    company_slug: None,
                    entity: Some(audit::AuditEntity::Policy),
                    action: Some(audit::AuditAction::PolicyChange),
                    limit: audit::AuditPageLimit::default(),
                    offset: audit::AuditPageOffset::default(),
                },
            )?;
            Ok::<_, CliError>(
                rows.into_iter()
                    .map(|row| row.token_jti)
                    .collect::<Vec<_>>(),
            )
        };
        assert_eq!(policy_events()?, [Some(owner.jti().to_owned())]);
        let agent = read_identity(&agent_file)?;
        assert_eq!(Preset::try_from(&agent), Ok(Preset::Bookkeeper));
        assert_eq!(agent.write_mode(), WriteMode::Direct);

        fs::remove_file(&agent_file)?;
        provision(&db, provisioning, &agent_file, &owner_file)?;
        assert_eq!(read_identity(&owner_file)?.jti(), owner.jti());
        assert_eq!(
            Preset::try_from(&read_identity(&agent_file)?),
            Ok(Preset::Bookkeeper)
        );
        assert_eq!(policy_events()?, [Some(owner.jti().to_owned())]);
        Ok(())
    }

    #[test]
    fn pending_managed_bootstrap_reuses_secret_and_kit_after_books_failure()
    -> Result<(), Box<dyn Error>> {
        let root = tempfile::tempdir()?;
        keychain::restrict_directory(root.path())?;
        let books = root.path().join("books.db");
        let passphrase = books.with_extension("passphrase");
        let store = FileSecretStore::new(passphrase.clone());
        let cli = <Cli as clap::Parser>::try_parse_from([
            "bk",
            "--db",
            books.to_str().ok_or("books path")?,
            "mcp",
            "serve",
        ])?;
        let storage = Some(ServiceSecretStorage::PrivateFile);
        let (first, _) =
            bootstrap_secret(&cli, &store, storage, SetupMode::Fresh, Some(&passphrase))?;
        let kit = books.with_extension("recovery.txt");
        private_create(
            &kit,
            recovery_kit(
                &books,
                &books.with_extension("owner-token"),
                first.expose_secret(),
            )
            .as_bytes(),
        )?;
        let before = fs::read(&kit)?;
        #[cfg(unix)]
        if rustix::process::geteuid().as_raw() != 0 {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(root.path(), fs::Permissions::from_mode(0o500))?;
            let failed = initialize_ledger(&cli, &first, false);
            fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700))?;
            assert!(failed.is_err());
        }
        let (retry, _) =
            bootstrap_secret(&cli, &store, storage, SetupMode::Fresh, Some(&passphrase))?;
        assert_eq!(first.expose_secret(), retry.expose_secret());
        let mut cli = cli;
        run_service_with_native_store(
            &mut cli,
            root.path(),
            (std::net::Ipv4Addr::LOCALHOST, 8750).into(),
            ClientRegistration::Skip,
            ServiceSecretStorage::PrivateFile,
            &store,
        )?;
        assert!(books.is_file());
        assert_eq!(before, fs::read(&kit)?);
        assert!(!PendingBootstrap::path(&books).exists());
        assert_eq!(
            store.load(&books)?.ok_or("stored secret")?.expose_secret(),
            first.expose_secret()
        );
        Ok(())
    }

    #[test]
    fn pending_native_fallback_recovers_its_file_store_before_books_exist()
    -> Result<(), Box<dyn Error>> {
        use clap::Parser;
        let root = tempfile::tempdir()?;
        keychain::restrict_directory(root.path())?;
        let books = root.path().join("books.db");
        let mut cli = Cli::try_parse_from([
            "bk",
            "--db",
            books.to_str().ok_or("books path")?,
            "mcp",
            "serve",
        ])?;
        let unavailable = UnavailableStore(keychain::KeyringFailure::NoProvider);
        let storage = ServiceSecretStorage::NativeWithFileFallback;
        let (secret, _) =
            bootstrap_secret(&cli, &unavailable, Some(storage), SetupMode::Fresh, None)?;
        run_service_with_native_store(
            &mut cli,
            root.path(),
            (std::net::Ipv4Addr::LOCALHOST, 8750).into(),
            ClientRegistration::Skip,
            storage,
            &unavailable,
        )?;
        let stored = FileSecretStore::new(books.with_extension("passphrase"))
            .load(&books)?
            .ok_or("missing fallback")?;
        assert_eq!(secret.expose_secret(), stored.expose_secret());
        assert!(books.is_file());
        Ok(())
    }

    #[test]
    fn malformed_registry_is_rejected() -> Result<(), SetupError> {
        assert!(!has_registered_tokens(serde_json::json!([]))?);
        assert!(has_registered_tokens(serde_json::json!([{}]))?);
        assert!(has_registered_tokens(serde_json::json!({"tokens": []})).is_err());
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn failed_startup_reaps_its_background_process() -> Result<(), Box<dyn Error>> {
        use std::process::{Command, Stdio};
        let child = Command::new("sleep").arg("30").spawn()?;
        let id = child.id();
        drop(PendingServer {
            child,
            ready: false,
        });
        assert!(
            !Command::new("kill")
                .args(["-0", &id.to_string()])
                .stderr(Stdio::null())
                .status()?
                .success()
        );
        Ok(())
    }
}
