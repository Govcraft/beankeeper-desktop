//! Per-user background hosting. No ledger schema or accounting operations live here.

mod extension;
mod manager;
mod mcp;
mod session;

use std::{
    error::Error,
    fmt, fs,
    io::{self, Read, Write},
    net::{Ipv4Addr, SocketAddr, TcpListener},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use beankeeper_cli::cli::Cli;
use clap::{Args, Subcommand};
use reqwest::{Url, blocking::Client, redirect::Policy};
use secrecy::ExposeSecret;
use serde::{Deserialize, Serialize};

use crate::{
    bridge,
    private_file::PrivateFile,
    setup::{self, ClientRegistration, ServiceSecretStorage, keychain},
};

pub(crate) use extension::connection as extension_connection;
use session::UserSessionEnv;

const DEFAULT_PORT: u16 = 8750;
const SERVICE_STATE_LIMIT: usize = 64 * 1024;

/// Lifecycle controls run with the current OS user's authority.
#[derive(Args, Debug)]
pub struct ServiceOptions {
    /// Service operation.
    #[command(subcommand)]
    pub command: ServiceCommand,
}

/// Available per-user lifecycle operations.
#[derive(Subcommand, Debug)]
pub enum ServiceCommand {
    /// Create encrypted books if needed, register login startup and start the service.
    Install {
        /// Use a private passphrase file instead of the native secret store (headless hosts).
        #[arg(long)]
        file_store: bool,
        /// Leave all MCP client configurations unchanged.
        #[arg(long)]
        no_agents: bool,
    },
    /// Internal first-launch install for an MCP bundle that manages its own client registration.
    #[command(hide = true)]
    InstallExtension,
    /// Disable and remove the service, preserving books, recovery material and credentials.
    Uninstall,
    /// Enable login startup and start the installed service.
    Start,
    /// Persistently disable login startup and crash restarts until start is requested.
    Stop,
    /// Report authenticated health, running version, endpoint and paths.
    Status {
        /// Emit structured JSON, including when stopped or not installed.
        #[arg(long)]
        json: bool,
    },
    /// Serve lifecycle tools over local stdio MCP, even when the HTTP server is stopped.
    Mcp,
    /// Internal service entry point.
    #[command(hide = true)]
    Run,
}

/// Service lifecycle failures with stable machine-readable codes.
#[derive(Debug)]
pub enum ServiceError {
    /// Filesystem or process execution failure.
    Io(io::Error),
    /// Invalid or conflicting service configuration.
    Configuration(&'static str),
    /// An OS manager command failed. Secret-bearing output is never included.
    Manager(&'static str),
    /// This Linux process cannot locate the owner's session bus.
    SessionEnvUnavailable,
    /// No managed background service is installed.
    NotInstalled,
    /// The advertised loopback endpoint could not be reached.
    EndpointConnect,
    /// The responding endpoint did not match the service descriptor.
    EndpointIdentity,
    /// The endpoint was reached, but opening an authenticated MCP session failed.
    EndpointMcp(Box<crate::bridge::BridgeError>),
    /// Encrypted provisioning failed.
    Setup(Box<setup::SetupError>),
    /// Desktop hosting failed.
    Desktop(Box<crate::DesktopError>),
    /// A live authenticated server did not become ready in time.
    NotReady,
}

impl ServiceError {
    /// Stable error name returned by lifecycle MCP tools.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::Io(_) => "SERVICE_IO",
            Self::Configuration(_) => "SERVICE_CONFIGURATION",
            Self::Manager(_) => "SERVICE_MANAGER",
            Self::SessionEnvUnavailable => "SESSION_ENV_UNAVAILABLE",
            Self::NotInstalled => "SERVICE_NOT_INSTALLED",
            Self::EndpointConnect => "SERVICE_ENDPOINT_UNREACHABLE",
            Self::EndpointIdentity => "SERVICE_ENDPOINT_IDENTITY",
            Self::EndpointMcp(error) => error.diagnostic_code(),
            Self::Setup(_) => "SERVICE_SETUP",
            Self::Desktop(_) => "SERVICE_HOST",
            Self::NotReady => "SERVICE_NOT_READY",
        }
    }
}
impl fmt::Display for ServiceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(f, "service I/O failed: {error}"),
            Self::Configuration(message) | Self::Manager(message) => f.write_str(message),
            Self::SessionEnvUnavailable => f.write_str("Cannot find your Linux session bus. Run Beankeeper as the owner from an active login session, or pass that session's XDG_RUNTIME_DIR and DBUS_SESSION_BUS_ADDRESS to your terminal or agent."),
            Self::NotInstalled => f.write_str("Beankeeper is not installed as a background service. Run bk service install."),
            Self::EndpointConnect => f.write_str("Cannot connect to the published Desktop endpoint. Run bk service status --json and inspect the private service log."),
            Self::EndpointIdentity => f.write_str("The published endpoint did not identify itself as the expected Beankeeper process. Run bk service status --json and inspect the private service log."),
            Self::EndpointMcp(error) => error.fmt(f),
            Self::Setup(error) => write!(f, "{error}"),
            Self::Desktop(error) => write!(f, "{error}"),
            Self::NotReady => f.write_str("service did not become ready; inspect the private service log and run `bk service status --json`"),
        }
    }
}
impl Error for ServiceError {}
impl From<io::Error> for ServiceError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}
impl From<setup::SetupError> for ServiceError {
    fn from(error: setup::SetupError) -> Self {
        Self::Setup(Box::new(error))
    }
}
impl From<crate::DesktopError> for ServiceError {
    fn from(error: crate::DesktopError) -> Self {
        Self::Desktop(Box::new(error))
    }
}

/// OS-standard per-user storage. Overrides support isolated and headless installations.
#[derive(Debug, Clone)]
pub(crate) struct Paths {
    pub home: PathBuf,
    pub data: PathBuf,
    pub state: PathBuf,
    pub logs: PathBuf,
    pub definition: PathBuf,
}

impl Paths {
    pub fn discover() -> Result<Self, ServiceError> {
        let home = setup::user_home().ok_or(ServiceError::Configuration(
            "cannot locate the user's home directory",
        ))?;
        let data = if cfg!(windows) {
            env_path("LOCALAPPDATA")
                .unwrap_or_else(|| home.join("AppData/Local"))
                .join("beankeeper-desktop")
        } else if cfg!(target_os = "macos") {
            home.join("Library/Application Support/beankeeper-desktop")
        } else {
            env_path("XDG_DATA_HOME")
                .unwrap_or_else(|| home.join(".local/share"))
                .join("beankeeper-desktop")
        };
        let data = env_path("BEANKEEPER_SERVICE_DATA_DIR").unwrap_or(data);
        let state = crate::product::state_dir()
            .map_err(|_| ServiceError::Configuration("cannot locate Desktop state"))?;
        let logs = if cfg!(target_os = "macos") {
            home.join("Library/Logs/beankeeper-desktop")
        } else {
            state.join("logs")
        };
        let logs = env_path("BEANKEEPER_SERVICE_LOG_DIR").unwrap_or(logs);
        let definition = if cfg!(windows) {
            state.join("service-task.ps1")
        } else if cfg!(target_os = "macos") {
            home.join("Library/LaunchAgents/ai.govcraft.beankeeper-desktop.plist")
        } else {
            env_path("XDG_CONFIG_HOME")
                .unwrap_or_else(|| home.join(".config"))
                .join("systemd/user/beankeeper-desktop.service")
        };
        for path in [&home, &data, &state, &logs, &definition] {
            if !path.is_absolute() {
                return Err(ServiceError::Configuration(
                    "service paths must be absolute",
                ));
            }
        }
        Ok(Self {
            home,
            data,
            state,
            logs,
            definition,
        })
    }
    fn config(&self) -> PathBuf {
        self.state.join("service.json")
    }
    pub fn runtime(&self) -> PathBuf {
        self.state.join("service-runtime.json")
    }
    pub fn log(&self) -> PathBuf {
        self.logs.join("service.log")
    }
    fn prepare(&self) -> Result<(), ServiceError> {
        for path in [&self.data, &self.state, &self.logs] {
            fs::create_dir_all(path)?;
            keychain::restrict_directory(path)?;
        }
        Ok(())
    }
}
fn env_path(name: &str) -> Option<PathBuf> {
    std::env::var_os(name)
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Config {
    executable: PathBuf,
    books: PathBuf,
    port: u16,
}
impl Config {
    fn load(paths: &Paths) -> Result<Option<Self>, ServiceError> {
        if !paths.config().try_exists()? {
            return Ok(None);
        }
        let config: Self = read_private(&paths.config())?;
        if !config.executable.is_absolute() || !config.books.is_absolute() || config.port == 0 {
            return Err(ServiceError::Configuration(
                "invalid service paths or port; repair service.json",
            ));
        }
        Ok(Some(config))
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Runtime {
    bind: SocketAddr,
    pid: u32,
    books: PathBuf,
}

/// A snapshot contains no credentials or recovery material.
#[derive(Debug, Serialize)]
pub struct Status {
    /// Whether a service registration is installed.
    pub installed: bool,
    /// Whether the managed server accepts this ledger's agent credential.
    pub running: bool,
    /// Running Desktop version; absent when stopped.
    pub version: Option<String>,
    /// Discovered local MCP endpoint; absent when stopped.
    pub mcp_endpoint: Option<String>,
    /// Optional browser view for the owner.
    pub desktop_url: Option<String>,
    /// Ledger location, preserved on uninstall.
    pub books: PathBuf,
    /// Private server diagnostics.
    pub log: PathBuf,
    /// Operating system service registration.
    pub service_definition: PathBuf,
    /// PID of the managed server, absent when stopped.
    pub pid: Option<u32>,
    /// Named failure when the advertised service cannot be reached or authenticated.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_code: Option<&'static str>,
    /// Plain-language status.
    pub message: String,
}

/// Read the runtime endpoint and authenticate the expected ledger before reporting health.
///
/// # Errors
/// Returns unsafe/corrupt state errors; a stopped service is a successful status result.
pub fn status() -> Result<Status, ServiceError> {
    status_at(&Paths::discover()?)
}
fn status_at(paths: &Paths) -> Result<Status, ServiceError> {
    let config = Config::load(paths)?;
    let mut result = Status {
        installed: config.is_some() && paths.definition.try_exists()?,
        running: false,
        version: None,
        mcp_endpoint: None,
        desktop_url: None,
        pid: None,
        books: config.map_or_else(|| paths.data.join("books.db"), |config| config.books),
        log: paths.log(),
        service_definition: paths.definition.clone(),
        error_code: None,
        message: ServiceError::NotInstalled.to_string(),
    };
    if !result.installed {
        return Ok(result);
    }
    match authenticated_endpoint(paths, &result.books) {
        Ok(Some(AuthenticatedEndpoint(endpoint))) => {
            result.running = true;
            result.desktop_url = Some(endpoint.desktop_url().into());
            result.mcp_endpoint = Some(endpoint.url.into());
            result.version = Some(endpoint.version);
            result.pid = Some(endpoint.pid);
            result.message = "Beankeeper is running locally. Your agent can work with your books; opening Desktop is optional.".into();
        }
        Ok(None) => result.message = "Beankeeper is stopped. Run bk service start.".into(),
        Err(
            error @ (ServiceError::EndpointConnect
            | ServiceError::EndpointIdentity
            | ServiceError::EndpointMcp(_)),
        ) => {
            result.error_code = Some(error.code());
            result.message = error.to_string();
        }
        Err(error) => return Err(error),
    }
    Ok(result)
}

/// An endpoint from the private descriptor whose health response matched its process.
pub(crate) struct Endpoint {
    url: Url,
    token_file: PathBuf,
    version: String,
    pid: u32,
}

impl Endpoint {
    pub(crate) fn url(&self) -> &Url {
        &self.url
    }

    pub(crate) fn token_file(&self) -> &Path {
        &self.token_file
    }

    fn desktop_url(&self) -> Url {
        let mut url = self.url.clone();
        url.set_path("/");
        url
    }
}

fn endpoint_at(paths: &Paths, books: &Path) -> Result<Option<Endpoint>, ServiceError> {
    let runtime: Runtime = match read_private(&paths.runtime()) {
        Ok(runtime) => runtime,
        Err(ServiceError::Io(error)) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    if runtime.books != books
        || runtime.bind.ip() != Ipv4Addr::LOCALHOST
        || runtime.bind.port() == 0
        || runtime.pid == 0
    {
        return Err(ServiceError::Configuration(
            "invalid service runtime descriptor",
        ));
    }
    let mut url = Url::parse(&crate::server::local_url(runtime.bind))
        .map_err(|_| ServiceError::Configuration("invalid service runtime address"))?;
    url.set_path("/health");
    let response = Client::builder()
        .no_proxy()
        .redirect(Policy::none())
        .timeout(Duration::from_secs(2))
        .build()
        .and_then(|client| client.get(url.clone()).send())
        .map_err(|_| ServiceError::EndpointConnect)?;
    let pid = response
        .headers()
        .get("x-beankeeper-desktop-pid")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u32>().ok());
    let version = response
        .headers()
        .get("x-beankeeper-desktop-version")
        .and_then(|value| value.to_str().ok())
        .filter(|version| !version.is_empty())
        .map(str::to_owned)
        .ok_or(ServiceError::EndpointIdentity)?;
    if !response.status().is_success() || pid != Some(runtime.pid) {
        return Err(ServiceError::EndpointIdentity);
    }
    let mut body = String::new();
    response
        .take(64)
        .read_to_string(&mut body)
        .map_err(|_| ServiceError::EndpointConnect)?;
    if body != crate::ui::HEALTH {
        return Err(ServiceError::EndpointIdentity);
    }
    url.set_path("/mcp");
    Ok(Some(Endpoint {
        url,
        token_file: runtime.books.with_extension("agent-token"),
        version,
        pid: runtime.pid,
    }))
}

/// A live bridge endpoint only needs its private descriptor and HTTP health.
/// The OS manager is needed for lifecycle changes, including restarting a service.
pub(crate) fn connection() -> Result<Endpoint, ServiceError> {
    let paths = Paths::discover()?;
    let config = Config::load(&paths)?.ok_or(ServiceError::NotInstalled)?;
    if !paths.definition.try_exists()? {
        return Err(ServiceError::NotInstalled);
    }
    if let Some(endpoint) = endpoint_at(&paths, &config.books)? {
        Ok(endpoint)
    } else {
        UserSessionEnv::discover()?;
        Err(ServiceError::NotReady)
    }
}

struct AuthenticatedEndpoint(Endpoint);

fn authenticated_endpoint(
    paths: &Paths,
    books: &Path,
) -> Result<Option<AuthenticatedEndpoint>, ServiceError> {
    endpoint_at(paths, books)?
        .map(|endpoint| {
            bridge::probe_service(&endpoint, bridge::Probe::Authentication)
                .map_err(|error| ServiceError::EndpointMcp(Box::new(error)))?;
            Ok(AuthenticatedEndpoint(endpoint))
        })
        .transpose()
}

/// Check session availability for registration when a managed service exists.
/// Healthy endpoints work without a session bus in the client process.
pub(crate) fn registration_session() -> Result<(), ServiceError> {
    let Err(error) = UserSessionEnv::discover() else {
        return Ok(());
    };
    let paths = Paths::discover()?;
    if let Some(config) = Config::load(&paths)?
        && paths.definition.try_exists()?
        && matches!(
            endpoint_at(&paths, &config.books),
            Ok(None) | Err(ServiceError::EndpointConnect)
        )
    {
        return Err(error);
    }
    Ok(())
}

/// Dispatch a service command. Global ledger flags are used only for first install.
///
/// # Errors
/// Returns actionable service-manager, configuration and provisioning errors.
pub fn run(options: &ServiceOptions, mut cli: Cli, explicit_db: bool) -> Result<(), ServiceError> {
    if matches!(options.command, ServiceCommand::Mcp) {
        return mcp::serve();
    }
    let paths = Paths::discover()?;
    if matches!(options.command, ServiceCommand::Run) {
        return host(&paths);
    }
    if let ServiceCommand::Status { json } = options.command {
        let status = status_at(&paths)?;
        if json {
            println!(
                "{}",
                serde_json::to_string(&status)
                    .map_err(|_| ServiceError::Configuration("cannot serialize service status"))?
            );
        } else {
            println!(
                "{}\nBooks: {}\nLog: {}",
                status.message,
                status.books.display(),
                status.log.display()
            );
            if let Some(endpoint) = status.mcp_endpoint {
                println!("MCP: {endpoint}");
            }
        }
        return Ok(());
    }
    paths.prepare()?;
    let _lock = lock(&paths.state.join("service-operation.lock"))?;
    let previous = Config::load(&paths)?;
    match options.command {
        ServiceCommand::Install {
            file_store,
            no_agents,
        } => install(
            &paths,
            &mut cli,
            previous.as_ref(),
            InstallMode::Command {
                storage: if file_store {
                    ServiceSecretStorage::PrivateFile
                } else {
                    ServiceSecretStorage::NativeWithFileFallback
                },
                explicit_db,
                no_agents,
            },
        )?,
        ServiceCommand::InstallExtension => {
            // An explicit service installation racing the extension takes
            // precedence. Reuse it and leave its registration untouched.
            if previous.is_none() {
                install(&paths, &mut cli, None, InstallMode::Extension)?;
            }
        }
        ServiceCommand::Start => {
            let config = require_installed(previous.as_ref())?;
            manager::start(&paths, &UserSessionEnv::discover()?)?;
            wait_ready(&paths, &config.books)?;
            println!("Beankeeper started.");
        }
        ServiceCommand::Stop => {
            if previous.is_some() {
                manager::stop(&paths, &UserSessionEnv::discover()?)?;
                // A native manager can terminate the host before it removes
                // its descriptor, notably when Windows stops the task.
                remove_if_exists(&paths.runtime())?;
            }
            println!("Beankeeper stopped; login startup is disabled until bk service start.");
        }
        ServiceCommand::Uninstall => {
            if previous.is_some() {
                manager::uninstall(&paths, &UserSessionEnv::discover()?)?;
                remove_if_exists(&paths.config())?;
                remove_if_exists(&paths.runtime())?;
            }
            println!(
                "Service removed. Books, credentials, MCP configs and recovery material were preserved."
            );
        }
        ServiceCommand::Status { .. } | ServiceCommand::Mcp | ServiceCommand::Run => unreachable!(),
    }
    Ok(())
}
#[derive(Clone, Copy)]
enum InstallMode {
    Command {
        explicit_db: bool,
        storage: ServiceSecretStorage,
        no_agents: bool,
    },
    Extension,
}

fn install(
    paths: &Paths,
    cli: &mut Cli,
    previous: Option<&Config>,
    mode: InstallMode,
) -> Result<(), ServiceError> {
    let (explicit_db, storage) = match mode {
        InstallMode::Command {
            explicit_db,
            storage,
            ..
        } => (explicit_db, storage),
        InstallMode::Extension => (false, ServiceSecretStorage::NativeRequired),
    };
    manager::check_user()?;
    let session = UserSessionEnv::discover()?;
    if let Some(previous) = previous {
        if explicit_db && fs::canonicalize(&cli.db)? != previous.books {
            return Err(ServiceError::Configuration(
                "a different ledger is already installed; uninstall the service before selecting other books",
            ));
        }
        cli.db.clone_from(&previous.books);
        manager::stop(paths, &session)?;
    } else {
        if paths.definition.symlink_metadata().is_ok() {
            return Err(ServiceError::Configuration(
                "an unmanaged service definition already exists; inspect it before installing Beankeeper hosting",
            ));
        }
        if !explicit_db {
            cli.db = paths.data.join("books.db");
        }
    }
    if !cli.db.is_absolute() {
        cli.db = std::path::absolute(&cli.db)?;
    }
    let executable = std::env::current_exe()?;
    let registration = match mode {
        InstallMode::Command {
            no_agents: false, ..
        } => ClientRegistration::Managed {
            previous_executable: previous.map_or(executable.as_path(), |config| &config.executable),
        },
        InstallMode::Command {
            no_agents: true, ..
        }
        | InstallMode::Extension => ClientRegistration::Skip,
    };
    let port = previous.map_or(DEFAULT_PORT, |config| config.port);
    setup::run_service(
        cli,
        &paths.home,
        (Ipv4Addr::LOCALHOST, port).into(),
        registration,
        storage,
    )?;
    let config = Config {
        executable,
        books: fs::canonicalize(&cli.db)?,
        port,
    };
    private_json(&paths.config(), &config)?;
    manager::install(paths, &config)?;
    manager::start(paths, &session)?;
    wait_ready(paths, &config.books)?;
    println!(
        "Beankeeper is running in the background. Recovery kit: {}. Keep an offline copy under your own control.",
        config.books.with_extension("recovery.txt").display()
    );
    Ok(())
}

fn require_installed(config: Option<&Config>) -> Result<&Config, ServiceError> {
    config.ok_or(ServiceError::Configuration(
        "service is not installed; run bk service install",
    ))
}
fn wait_ready(paths: &Paths, books: &Path) -> Result<(), ServiceError> {
    let deadline = Instant::now() + Duration::from_secs(60);
    let mut last_error = ServiceError::NotReady;
    while Instant::now() < deadline {
        match authenticated_endpoint(paths, books) {
            Ok(Some(_)) => return Ok(()),
            Ok(None) => {}
            Err(error @ (ServiceError::EndpointConnect | ServiceError::EndpointIdentity)) => {
                last_error = error;
            }
            Err(error) => return Err(error),
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    Err(last_error)
}
fn host(paths: &Paths) -> Result<(), ServiceError> {
    let config = Config::load(paths)?.ok_or(ServiceError::Configuration(
        "service registration is missing",
    ))?;
    paths.prepare()?;
    let _lock = lock(&paths.state.join("service-host.lock"))?;
    let listener = bind_listener(config.port)?;
    let bind = listener.local_addr()?;
    let runtime = Runtime {
        bind,
        pid: std::process::id(),
        books: config.books.clone(),
    };
    let mut cli = <Cli as clap::Parser>::try_parse_from(["bk", "mcp", "serve"])
        .map_err(|_| ServiceError::Configuration("cannot construct service ledger options"))?;
    cli.db = config.books;
    let _passphrase = setup::stored_passphrase(&mut cli)?;
    private_json(&paths.runtime(), &runtime)?;
    let result = crate::server::serve_listener(&cli, listener);
    let _ = remove_if_exists(&paths.runtime());
    result.map_err(Into::into)
}
fn bind_listener(preferred: u16) -> io::Result<TcpListener> {
    match TcpListener::bind((Ipv4Addr::LOCALHOST, preferred)) {
        Ok(listener) => Ok(listener),
        Err(error) if error.kind() == io::ErrorKind::AddrInUse => {
            TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        }
        Err(error) => Err(error),
    }
}
fn lock(path: &Path) -> Result<fs::File, ServiceError> {
    let file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)?;
    keychain::restrict_file(path)?;
    file.try_lock().map_err(|_| {
        ServiceError::Configuration("another service operation or host is already running")
    })?;
    Ok(file)
}
fn read_private<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T, ServiceError> {
    let bytes = PrivateFile::open(path)?.read_bytes(SERVICE_STATE_LIMIT)?;
    serde_json::from_slice(bytes.expose_secret()).map_err(|_| {
        ServiceError::Configuration(
            "invalid private service state; inspect service.json and service-runtime.json",
        )
    })
}
fn private_json(path: &Path, value: &impl Serialize) -> Result<(), ServiceError> {
    let bytes = serde_json::to_vec_pretty(value)
        .map_err(|_| ServiceError::Configuration("cannot serialize service state"))?;
    private_write(path, &bytes)
}
pub(crate) fn private_write(path: &Path, bytes: &[u8]) -> Result<(), ServiceError> {
    let parent = path.parent().ok_or(ServiceError::Configuration(
        "service file needs a parent directory",
    ))?;
    fs::create_dir_all(parent)?;
    match PrivateFile::open(path) {
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    let mut pending = tempfile::NamedTempFile::new_in(parent)?;
    keychain::restrict_file(pending.path())?;
    pending.write_all(bytes)?;
    pending.as_file().sync_all()?;
    pending.persist(path).map_err(|error| error.error)?;
    Ok(())
}
fn remove_if_exists(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        result => result,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    type TestResult = Result<(), Box<dyn Error>>;

    #[test]
    fn private_service_state_reads_allow_valid_json_and_reject_oversized_input() -> TestResult {
        let root = tempfile::tempdir()?;
        let path = root.path().join("service.json");
        private_write(&path, br#"{"port":8750}"#)?;
        let valid: serde_json::Value = read_private(&path)?;
        assert_eq!(valid["port"], 8750);
        private_write(&path, &vec![b' '; SERVICE_STATE_LIMIT + 1])?;
        assert!(matches!(
            read_private::<serde_json::Value>(&path),
            Err(ServiceError::Io(error)) if error.kind() == io::ErrorKind::FileTooLarge
        ));
        Ok(())
    }

    #[test]
    fn occupied_port_falls_back_to_ipv4_loopback_without_losing_the_listener() -> TestResult {
        let occupied = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
        let listener = bind_listener(occupied.local_addr()?.port())?;
        assert_eq!(listener.local_addr()?.ip(), Ipv4Addr::LOCALHOST);
        assert_ne!(listener.local_addr()?.port(), occupied.local_addr()?.port());
        Ok(())
    }

    #[test]
    fn service_locks_exclude_concurrent_mutations_and_release_on_drop() -> TestResult {
        let root = tempfile::tempdir()?;
        let path = root.path().join("operation.lock");
        let first = lock(&path)?;
        assert!(lock(&path).is_err());
        drop(first);
        lock(&path)?;
        Ok(())
    }

    #[test]
    fn absent_service_status_never_creates_books_or_contacts_a_manager() -> TestResult {
        let root = tempfile::tempdir()?;
        let paths = Paths {
            home: root.path().into(),
            data: root.path().join("data"),
            state: root.path().join("state"),
            logs: root.path().join("logs"),
            definition: root.path().join("definition"),
        };
        let status = status_at(&paths)?;
        assert!(!status.installed);
        assert!(!status.running);
        assert!(status.version.is_none());
        assert!(status.mcp_endpoint.is_none());
        assert!(!paths.data.exists());
        assert!(!paths.state.exists());
        Ok(())
    }

    #[test]
    fn private_state_rejects_malformed_and_relative_configuration() -> TestResult {
        let root = tempfile::tempdir()?;
        let paths = Paths {
            home: root.path().into(),
            data: root.path().join("data"),
            state: root.path().join("state"),
            logs: root.path().join("logs"),
            definition: root.path().join("definition"),
        };
        paths.prepare()?;
        let config = Config {
            executable: "relative-bk".into(),
            books: paths.data.join("books.db"),
            port: 8750,
        };
        private_json(&paths.config(), &config)?;
        assert!(Config::load(&paths).is_err());
        private_write(&paths.config(), b"invalid-json-private-value")?;
        let error = Config::load(&paths).expect_err("invalid state must fail");
        assert!(!error.to_string().contains("private-value"));
        Ok(())
    }
}
