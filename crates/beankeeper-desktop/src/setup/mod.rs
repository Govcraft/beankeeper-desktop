//! Encrypted Desktop onboarding using upstream ledger and identity APIs.

pub mod config;
pub mod keychain;

use std::{
    error::Error,
    fmt, fs,
    io::{self, IsTerminal, Write},
    net::SocketAddr,
    path::{Path, PathBuf},
    time::Duration,
};

use beankeeper_cli::{
    cli::Cli,
    db::{self, Actor, Db, audit, spend_limits::SpendLimit},
    error::CliError,
    identity::{self, IssueRequest},
    passphrase,
};
use clap::{Args, ValueEnum};
use secrecy::{ExposeSecret, SecretString};

use keychain::{FileSecretStore, OsKeychain, SecretStore};

/// Least-privilege grants for the user's MCP client.
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum Preset {
    /// Read reports and ledger data.
    ReadOnly,
    /// Read, post within spend limits, and propose, without approval rights.
    Bookkeeper,
}

impl Preset {
    fn capabilities(self) -> Vec<String> {
        match self {
            Self::ReadOnly => vec!["ledger/read".into()],
            Self::Bookkeeper => vec![
                "ledger/read".into(),
                "ledger/post".into(),
                "ledger/propose".into(),
            ],
        }
    }
}

/// One-command setup. All credential inputs are file/descriptor based.
#[derive(Args, Debug)]
pub struct SetupOptions {
    /// Resume an existing encrypted ledger; never delete it or rotate its root.
    #[arg(long)]
    pub force: bool,
    /// Do not prompt or display secrets; recovery kit remains private on disk.
    #[arg(long)]
    pub non_interactive: bool,
    /// Seed upstream's sample companies and transactions.
    #[arg(long)]
    pub demo: bool,
    /// Agent capability preset.
    #[arg(long, value_enum, default_value = "read-only")]
    pub preset: Preset,
    /// Private recovery kit path (default: ledger path with .recovery.txt extension).
    #[arg(long)]
    pub recovery_kit: Option<PathBuf>,
    /// Home directory containing client configs (defaults to the OS user's home).
    #[arg(long)]
    pub config_home: Option<PathBuf>,
    /// Explicit file-backed secret store for headless/test use; bypasses OS keychain.
    #[arg(long)]
    pub secret_store_file: Option<PathBuf>,
    /// Do not start a background Desktop server.
    #[arg(long)]
    pub no_start: bool,
    /// Do not launch the default browser.
    #[arg(long)]
    pub no_open: bool,
    /// Local Desktop address; requires a nonzero port.
    #[arg(long, default_value = "127.0.0.1:8750")]
    pub bind: SocketAddr,
    /// Bookkeeper's maximum post in USD cents, per initial company.
    #[arg(long, default_value = "10000", value_parser = clap::value_parser!(i64).range(0..))]
    pub max_post: i64,
    /// Bookkeeper's daily spend in USD cents, per initial company.
    #[arg(long, default_value = "50000", value_parser = clap::value_parser!(i64).range(0..))]
    pub daily_limit: i64,
}

/// Setup failures; credential content is never included in diagnostics.
#[derive(Debug)]
pub enum SetupError {
    /// A file or terminal operation failed.
    Io(io::Error),
    /// An upstream ledger operation failed.
    Ledger(CliError),
    /// An OS credential operation failed.
    Keychain(keychain::KeychainError),
    /// Existing configuration cannot be safely merged.
    Config(config::ConfigError),
    /// Inputs or existing state need user attention.
    Invalid(&'static str),
    /// Server startup or browser launch failed.
    Desktop(crate::DesktopError),
}

impl fmt::Display for SetupError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(f, "setup file operation failed: {error}"),
            Self::Ledger(error) => write!(f, "{error}"),
            Self::Keychain(error) => write!(f, "{error}"),
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
        Self::Keychain(value)
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
    let store: Box<dyn SecretStore> = match &options.secret_store_file {
        Some(path) => Box::new(FileSecretStore::new(path.clone())),
        None => registered_store(&cli.db)?.unwrap_or_else(|| Box::new(OsKeychain)),
    };
    run_with_store(cli, options, store.as_ref(), None)
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
    previous_executable: &Path,
    file_store: bool,
) -> Result<(), SetupError> {
    let mut options = SetupOptions::from_service(home, bind);
    if file_store && !cli.db.exists() {
        options.secret_store_file = Some(cli.db.with_extension("passphrase"));
    }
    let store: Box<dyn SecretStore> = match &options.secret_store_file {
        Some(path) => Box::new(FileSecretStore::new(path.clone())),
        None => registered_store(&cli.db)?.unwrap_or_else(|| Box::new(OsKeychain)),
    };
    run_with_store(cli, &options, store.as_ref(), Some(previous_executable))
}

impl SetupOptions {
    fn from_service(home: &Path, bind: SocketAddr) -> Self {
        Self {
            force: true,
            non_interactive: true,
            demo: false,
            preset: Preset::ReadOnly,
            recovery_kit: None,
            config_home: Some(home.into()),
            secret_store_file: None,
            no_start: true,
            no_open: true,
            bind,
            max_post: 10_000,
            daily_limit: 50_000,
        }
    }
}

fn run_with_store(
    cli: &mut Cli,
    options: &SetupOptions,
    store: &dyn SecretStore,
    service_executable: Option<&Path>,
) -> Result<(), SetupError> {
    crate::server::validate_bind(options.bind, false)?;
    if options.bind.port() == 0 {
        return Err(SetupError::Invalid("setup requires a nonzero --bind port"));
    }
    if !options.non_interactive && !io::stdin().is_terminal() {
        return Err(SetupError::Invalid(
            "setup requires a terminal; use --non-interactive for automation",
        ));
    }
    let existed = cli.db.try_exists()?;
    if existed && !options.force {
        return Err(SetupError::Invalid(
            "ledger already exists; use --force to resume setup without deleting data or rotating keys",
        ));
    }
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
    let token_file = cli.db.with_extension("agent-token");
    let owner_file = cli.db.with_extension("owner-token");
    let kit = options
        .recovery_kit
        .clone()
        .unwrap_or_else(|| cli.db.with_extension("recovery.txt"));
    let url = format!("{}mcp", crate::server::local_url(options.bind));
    let executable = std::env::current_exe()?;
    let plan = match service_executable {
        Some(previous) => config::prepare_service_configs(&home, &executable, previous)?,
        None => config::prepare_configs(&home, &executable, &cli.db, &token_file, &url)?,
    };
    // Validate output collisions before creating a ledger or storing credentials.
    if !existed
        && [token_file.as_path(), owner_file.as_path(), kit.as_path()]
            .iter()
            .any(|path| path.exists())
    {
        return Err(SetupError::Invalid(
            "setup output already exists; choose a fresh ledger/recovery path",
        ));
    }
    let mut outputs = vec![
        cli.db.clone(),
        token_file.clone(),
        owner_file.clone(),
        kit.clone(),
        cli.db.with_extension("desktop.json"),
        cli.db.with_extension("desktop.log"),
    ];
    outputs.extend(plan.paths().map(Path::to_path_buf));
    if let Some(path) = &options.secret_store_file {
        outputs.push(path.clone());
    }
    if let Some(path) = &cli.passphrase.passphrase_file {
        outputs.push(path.clone());
    }
    let mut seen = std::collections::HashSet::new();
    for path in outputs {
        if !seen.insert(future_path(&path)?) {
            return Err(SetupError::Invalid(
                "ledger, credential, recovery, source and client config paths must be distinct",
            ));
        }
    }
    if kit.exists() {
        keychain::validate_private_file(&kit)?;
    }
    let supplied = passphrase::resolve_passphrase(
        cli.passphrase.passphrase_fd,
        cli.passphrase.passphrase_file.as_deref(),
        false,
    )?;
    let secret = match supplied {
        Some(secret) => secret,
        None if existed => store.load(&cli.db)?.ok_or(SetupError::Invalid("keychain entry missing; restore the passphrase from your recovery kit using --passphrase-file"))?,
        None => generate_passphrase()?,
    };
    // Verify supplied recovery credentials before replacing a stored credential.
    if existed {
        Db::open_existing(&cli.db, Some(&secret))?;
    }
    // Establish a recoverable credential before initialization.
    let fallback = cli.db.with_extension("passphrase");
    let automatic_file = match store.store(&cli.db, &secret) {
        Ok(()) => None,
        Err(keychain::KeychainError::Unavailable) if service_executable.is_some() && !existed => {
            FileSecretStore::new(fallback.clone()).store(&cli.db, &secret)?;
            eprintln!("OS secret store unavailable; using a private credential file.");
            Some(fallback)
        }
        Err(error) => return Err(error.into()),
    };
    if !kit.exists() {
        private_create(
            &kit,
            recovery_kit(&cli.db, &owner_file, secret.expose_secret()).as_bytes(),
        )?;
    }
    let temporary = temporary_passphrase(&secret)?;
    cli.passphrase.passphrase_fd = None;
    cli.passphrase.passphrase_file = Some(temporary.path().into());
    if !existed {
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
        beankeeper_cli::commands::init::run(cli, true, Some(&staged), false, options.demo)?;
        set_private(&staged)?;
        // An atomic no-clobber link also protects against a concurrent setup.
        fs::hard_link(&staged, &cli.db)?;
        set_private(&cli.db)?;
    }
    let db = Db::open_existing(&cli.db, Some(&secret))?;
    provision(
        &db,
        options,
        &token_file,
        &owner_file,
        service_executable.is_some() && existed,
    )?;
    let marker = cli.db.with_extension("desktop.json");
    if !marker.exists() || options.secret_store_file.is_some() {
        let file_store = options
            .secret_store_file
            .as_ref()
            .or(automatic_file.as_ref())
            .map(std::path::absolute)
            .transpose()?;
        let metadata = serde_json::json!({"secret_store_file": file_store});
        if marker.exists() {
            keychain::validate_private_file(&marker)?;
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
    let configs = plan.write()?;
    println!(
        "Encrypted ledger: {}\nRecovery kit: {}\nKeep a separate offline copy of the recovery kit. Losing both it and the keychain means losing access.",
        cli.db.display(),
        kit.display()
    );
    eprintln!(
        "Agent token is a secret: {}. Owner credential stays separate: {}.",
        token_file.display(),
        owner_file.display()
    );
    for path in configs {
        eprintln!("MCP config: {}", path.display());
    }
    if !options.non_interactive && !existed {
        show_once(&secret)?;
    }
    if !options.no_start {
        super_start(cli, options, &temporary)?;
    }
    if !options.no_open {
        if options.no_start {
            keychain::validate_private_file(&token_file)?;
            let token = fs::read_to_string(&token_file)?;
            if !running_for_ledger(options.bind, &token)? {
                return Err(crate::DesktopError::NotRunning(crate::server::local_url(
                    options.bind,
                ))
                .into());
            }
        }
        crate::browser::open(options.bind)?;
    }
    Ok(())
}

fn provision(
    db: &Db,
    options: &SetupOptions,
    token_file: &Path,
    owner_file: &Path,
    preserve_preset: bool,
) -> Result<(), SetupError> {
    let conn = db.conn();
    if token_file.exists() {
        keychain::validate_private_file(token_file)?;
        let raw = fs::read_to_string(token_file)?;
        let agent = identity::verify(conn, raw.trim())?;
        if !preserve_preset && agent.capabilities() != options.preset.capabilities() {
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
    let roots = identity::list(conn)?;
    let actor = if db::policy::get(conn)?.enforce
        || roots.as_array().is_some_and(|rows| !rows.is_empty())
    {
        keychain::validate_private_file(owner_file)?;
        let raw = fs::read_to_string(owner_file).map_err(|_| {
            SetupError::Invalid(
                "existing ledger requires its owner credential; recover it before resuming setup",
            )
        })?;
        let owner = identity::verify(conn, raw.trim())?;
        if !owner.allows(beankeeper_cli::capability::Capability::Admin)? {
            return Err(SetupError::Invalid("owner credential lacks ledger/admin"));
        }
        Actor::new("desktop-owner").with_identity(owner)
    } else {
        Actor::new("desktop-setup")
    };
    // init_root itself refuses replacement; only initialize a ledger with no root.
    let has_root: bool = conn
        .query_row("SELECT EXISTS(SELECT 1 FROM identity_root)", [], |row| {
            row.get(0)
        })
        .map_err(CliError::from)?;
    if !has_root {
        identity::init_root(conn, &actor)?;
    }
    if !owner_file.exists() {
        let owner_caps = [
            "ledger/read",
            "ledger/post",
            "ledger/propose",
            "ledger/approve",
            "ledger/reverse",
            "ledger/reconcile",
            "ledger/budget",
            "ledger/admin",
        ]
        .map(String::from);
        let issued = issue(db, &actor, "desktop_owner", &owner_caps)?;
        private_create(
            owner_file,
            issued["token"]
                .as_str()
                .ok_or(SetupError::Invalid("upstream returned no owner token"))?
                .as_bytes(),
        )?;
    }
    let mut companies = db::list_companies(conn)?;
    if companies.is_empty() {
        db::create_company(conn, "personal", "Personal", None)?;
        companies = db::list_companies(conn)?;
    }
    let issued = issue(db, &actor, "desktop_agent", &options.preset.capabilities())?;
    if options.preset == Preset::Bookkeeper {
        for company in companies {
            audit::set_spend_limit(
                conn,
                &actor,
                &SpendLimit {
                    agent_uri: issued["agent_uri"]
                        .as_str()
                        .ok_or(SetupError::Invalid("upstream returned no agent URI"))?
                        .into(),
                    company_slug: company.slug,
                    currency: "USD".into(),
                    max_post: Some(options.max_post),
                    daily_limit: Some(options.daily_limit),
                },
            )?;
        }
    }
    audit::set_policy(conn, &actor, true)?;
    private_create(
        token_file,
        issued["token"]
            .as_str()
            .ok_or(SetupError::Invalid("upstream returned no agent token"))?
            .as_bytes(),
    )?;
    Ok(())
}

fn issue(
    db: &Db,
    actor: &Actor,
    name: &str,
    capabilities: &[String],
) -> Result<serde_json::Value, CliError> {
    identity::issue(
        db.conn(),
        actor,
        &IssueRequest {
            name,
            capabilities,
            ttl: Duration::from_secs(365 * 24 * 3600),
            public_key: None,
            generate_key: true,
        },
    )
}

fn generate_passphrase() -> Result<SecretString, SetupError> {
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
        "Beankeeper Desktop recovery kit\nKEEP PRIVATE. Store an offline copy separately from the ledger.\nLedger: {}\nPassphrase: {passphrase}\nOwner token file: {} (never share with agents; back this file up separately too)\n\nBack up with bk --db <ledger> backup --output <backup>. With policy enforcement, supply the owner token through BK_AGENT_TOKEN.\nRestore with bk --db <new-ledger> --passphrase-file <private-passphrase-file> restore --input <backup>.\nIf keychain access is lost, copy ONLY the Passphrase value to a private file and use --passphrase-file, then rerun setup --force.\nA backup is encrypted and still needs this passphrase. There is no backdoor.\nTokens expire after one year; use upstream agent issue/revoke to renew them.\n",
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

fn temporary_passphrase(secret: &SecretString) -> io::Result<tempfile::NamedTempFile> {
    let mut file = tempfile::NamedTempFile::new()?;
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
    keychain::validate_private_file(&token_path)?;
    let token = fs::read_to_string(&token_path)?;
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
    if cli.passphrase.passphrase_fd.is_some()
        || cli.passphrase.passphrase_file.is_some()
        || std::env::var_os("BEANKEEPER_PASSPHRASE_CMD").is_some_and(|value| !value.is_empty())
    {
        return Ok(None);
    }
    if !cli.db.exists() {
        return Ok(None);
    }
    let ledger = keychain::canonical_ledger(&cli.db)?;
    let marker = ledger.with_extension("desktop.json");
    if !marker.exists() {
        return Ok(None);
    }
    let store = registered_store(&ledger)?
        .ok_or(SetupError::Invalid("Desktop setup metadata is missing"))?;
    let guard = keychain::apply_stored_passphrase(cli, store.as_ref())?;
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
    if !marker.exists() {
        return Ok(None);
    }
    keychain::validate_private_file(&marker)?;
    let bytes = fs::read(marker)?;
    let metadata: serde_json::Value = serde_json::from_slice(&bytes)
        .map_err(|_| SetupError::Invalid("invalid Desktop setup metadata"))?;
    let store: Box<dyn SecretStore> = match metadata.get("secret_store_file") {
        Some(serde_json::Value::String(path)) => Box::new(FileSecretStore::new(path.into())),
        Some(serde_json::Value::Null) => Box::new(OsKeychain),
        _ => return Err(SetupError::Invalid("invalid Desktop secret store metadata")),
    };
    Ok(Some(store))
}

pub(crate) fn running_for_ledger(bind: SocketAddr, token: &str) -> Result<bool, SetupError> {
    if crate::browser::open_with(bind, |_| Ok(())).is_err() {
        return Ok(false);
    }
    let client = reqwest::blocking::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(2))
        .build()
        .map_err(crate::DesktopError::from)?;
    let url = format!("{}mcp", crate::server::local_url(bind));
    let response = client.post(&url).bearer_auth(token)
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
        .bearer_auth(token)
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

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    struct UnavailableStore;
    impl SecretStore for UnavailableStore {
        fn store(
            &self,
            _ledger: &Path,
            _secret: &SecretString,
        ) -> Result<(), keychain::KeychainError> {
            Err(keychain::KeychainError::Unavailable)
        }
        fn load(&self, _ledger: &Path) -> Result<Option<SecretString>, keychain::KeychainError> {
            Err(keychain::KeychainError::Unavailable)
        }
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
        let options = SetupOptions::from_service(&home, "127.0.0.1:8750".parse()?);
        run_with_store(
            &mut cli,
            &options,
            &UnavailableStore,
            Some(&std::env::current_exe()?),
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
            keychain::validate_private_file(&books.with_extension(extension))?;
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
            &std::env::current_exe()?,
            false,
        )?;
        assert_eq!(fs::read(books.with_extension("recovery.txt"))?, recovery);
        assert_eq!(fs::read(books.with_extension("agent-token"))?, token);
        Ok(())
    }

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
