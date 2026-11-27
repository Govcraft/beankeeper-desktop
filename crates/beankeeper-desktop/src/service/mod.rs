//! Per-user background hosting. No ledger schema or accounting operations live here.

pub(crate) mod extension;
mod manager;
mod mcp;
mod operation;
mod session;

use std::{
    error::Error,
    fmt, fs,
    io::{self, Write},
    net::{Ipv4Addr, SocketAddr, TcpListener},
    num::NonZeroU32,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use beankeeper_cli::{cli::Cli, passphrase};
use clap::{Args, Subcommand};
use reqwest::Url;
use secrecy::ExposeSecret;
use serde::{Deserialize, Serialize};

use crate::{
    bridge, browser,
    instance::{self, BooksPath, Health, InstanceId, ListenerAddress, LiveListener},
    private_file::PrivateFile,
    setup::{
        self, AccessOptions, AgentAccess, ClientRegistration, ServiceSecretStorage, keychain,
        managed::AccessChange,
    },
};

pub use crate::instance::PortWarning;
pub use extension::ExtensionCommand;
pub(crate) use mcp::local_call;
pub use operation::InstallOptions;
use operation::ServiceOperation;
pub(crate) use operation::{ACCESS_TOOL, OPEN_TOOL, access_tool, open_tool};
use session::UserSessionEnv;

use crate::folders::{Directory, FolderPlan, WriteDenied};

const SERVICE_STATE_LIMIT: usize = 64 * 1024;

/// Lifecycle controls run with the current OS user's authority.
#[derive(Args, Debug)]
pub struct ServiceOptions {
    /// Service operation.
    #[command(subcommand)]
    pub command: ServiceCommand,
}

/// Available per-user lifecycle operations.
#[derive(Subcommand, Clone, Copy, Debug)]
pub enum ServiceCommand {
    /// Create encrypted books if needed, register login startup and start the service.
    Install(InstallOptions),
    /// Internal first-launch install for an MCP bundle that manages its own client registration.
    #[command(hide = true)]
    InstallExtension,
    /// Set up or diagnose the Claude extension's local service.
    Extension {
        /// Extension operation.
        #[command(subcommand)]
        command: ExtensionCommand,
    },
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
    /// Open the installed service in the default browser.
    Open,
    /// Change the managed agent's access with the local owner's authority.
    AccessSet {
        /// Requested agent access.
        #[command(flatten)]
        access: AccessOptions,
        /// Emit structured service status after the change.
        #[arg(long)]
        json: bool,
    },
    /// Serve lifecycle tools over local stdio MCP, even when the HTTP server is stopped.
    Mcp,
    /// Internal service entry point.
    #[command(hide = true)]
    Run,
}

/// Secret-free service-manager failure categories, independent of localized messages.
#[derive(Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ManagerFailure {
    /// The selected session socket could not be reached.
    Connection,
    /// The OS denied access to the selected socket or command.
    PermissionDenied,
    /// The manager returned an unsuccessful operation status.
    Operation,
}
impl fmt::Display for ManagerFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Connection => "Cannot connect to the OS service manager. The agent should use the owner's active session bus, then retry the same operation.",
            Self::PermissionDenied => "Access to the OS service manager was denied. The agent should restore access to the owner's session socket and retry.",
            Self::Operation => "The OS service manager could not complete the unit operation. The agent should inspect the private service log and manager status, then retry.",
        })
    }
}

/// Service lifecycle failures with stable machine-readable codes.
#[derive(Debug)]
pub enum ServiceError {
    /// Filesystem or process execution failure.
    Io(io::Error),
    /// A denied write, retaining its affected directory and OS failure kind.
    WriteDenied(crate::folders::WriteDenied),
    /// Invalid or conflicting service configuration.
    Configuration(&'static str),
    /// An OS manager command failed. Secret-bearing output is never included.
    Manager(ManagerFailure),
    /// This Linux process cannot locate the owner's session bus.
    SessionEnvUnavailable,
    /// No managed background service is installed.
    NotInstalled,
    /// Installed hosting is stopped.
    NotRunning,
    /// The local MCP name is outside the lifecycle tool catalog.
    UnknownTool,
    /// MCP arguments do not match the selected operation's schema.
    Arguments,
    /// A lifecycle worker exited before returning a result.
    Worker,
    /// The advertised loopback endpoint could not be reached.
    EndpointConnect,
    /// The responding endpoint did not match the service descriptor.
    EndpointIdentity,
    /// The service health response contains an invalid semantic version.
    Version,
    /// A fresh browser link could not be obtained using local OS-user authority.
    BrowserPairing,
    /// The OS could not launch its default browser.
    BrowserLaunch,
    /// No live Desktop instance matches the selected books and optional address.
    BooksNotServed(PathBuf),
    /// More than one live instance serves the same books.
    MultipleInstances,
    /// The endpoint was reached, but opening an authenticated MCP session failed.
    EndpointMcp(Box<crate::bridge::BridgeError>),
    /// Encrypted provisioning failed.
    Setup(Box<setup::SetupError>),
    /// The extension's private installer failed.
    ExtensionInstall,
    /// Desktop hosting failed.
    Desktop(Box<crate::DesktopError>),
    /// A live authenticated server did not become ready in time.
    NotReady,
}

impl ServiceError {
    pub(crate) fn at(directory: Directory, error: io::Error) -> Self {
        WriteDenied::at(directory, &error).map_or_else(|| Self::Io(error), Self::WriteDenied)
    }
    pub(crate) fn structured(&self, resume: &str) -> serde_json::Value {
        let mut value = serde_json::json!({"code": self.code(), "message": self.to_string()});
        if let Self::WriteDenied(error) = self
            && let (Some(object), Some(details)) =
                (value.as_object_mut(), error.details(resume).as_object())
        {
            object.extend(details.clone());
        }
        if let Self::Manager(reason) = self {
            value["reason"] = serde_json::json!(reason);
        }
        value
    }
    /// Stable error name returned by lifecycle MCP tools.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::Io(_) => "SERVICE_IO",
            Self::WriteDenied(_) => "SERVICE_WRITE_DENIED",
            Self::Configuration(_) => "SERVICE_CONFIGURATION",
            Self::Manager(_) => "SERVICE_MANAGER",
            Self::SessionEnvUnavailable => "SESSION_ENV_UNAVAILABLE",
            Self::NotInstalled => "SERVICE_NOT_INSTALLED",
            Self::NotRunning => "SERVICE_NOT_RUNNING",
            Self::UnknownTool => "SERVICE_UNKNOWN_TOOL",
            Self::Arguments => "SERVICE_ARGUMENTS",
            Self::Worker => "SERVICE_INTERNAL",
            Self::EndpointConnect => "SERVICE_ENDPOINT_UNREACHABLE",
            Self::EndpointIdentity => "SERVICE_ENDPOINT_IDENTITY",
            Self::Version => "SERVICE_VERSION_INVALID",
            Self::BrowserPairing => "DESKTOP_BROWSER_PAIRING_UNAVAILABLE",
            Self::BrowserLaunch => "DESKTOP_BROWSER_LAUNCH_FAILED",
            Self::BooksNotServed(_) => "DESKTOP_BOOKS_NOT_SERVED",
            Self::MultipleInstances => "DESKTOP_MULTIPLE_INSTANCES",
            Self::EndpointMcp(error) => error.diagnostic_code(),
            Self::Setup(_) => "SERVICE_SETUP",
            Self::ExtensionInstall => "EXTENSION_INSTALL_FAILED",
            Self::Desktop(_) => "SERVICE_HOST",
            Self::NotReady => "SERVICE_NOT_READY",
        }
    }
}
impl fmt::Display for ServiceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(f, "service I/O failed: {error}"),
            Self::WriteDenied(error) => error.fmt(f),
            Self::Configuration(message) => f.write_str(message),
            Self::Manager(error) => error.fmt(f),
            Self::SessionEnvUnavailable => f.write_str("Cannot find your Linux session bus. The agent should use the owner's active login session and its DBUS_SESSION_BUS_ADDRESS. When that address works, leave XDG_RUNTIME_DIR unset for the service-manager child. If no bus address is available, the agent can use the owner's XDG_RUNTIME_DIR with its systemd/private socket. Retry the same operation; it picks up where it stopped."),
            Self::NotInstalled => f.write_str("Beankeeper is not installed as a background service. Run bk service install."),
            Self::NotRunning => f.write_str("Beankeeper is stopped. Run bk service start."),
            Self::UnknownTool => f.write_str("List the service lifecycle tools to find the supported operation names."),
            Self::Arguments => f.write_str("Use the arguments listed in the service tool schema."),
            Self::Worker => f.write_str("Service operation could not complete. Run service_status to inspect the result."),
            Self::EndpointConnect => f.write_str("Cannot connect to the published Desktop endpoint. Run bk service status --json and inspect the private service log."),
            Self::EndpointIdentity => f.write_str("The published endpoint did not identify itself as the expected Beankeeper process. Run bk service status --json and inspect the private service log."),
            Self::Version => f.write_str("The service returned an invalid semantic version."),
            Self::BrowserPairing => f.write_str("Could not request a fresh browser link for this Desktop instance. Check that Desktop is running under your OS account, then try again."),
            Self::BrowserLaunch => f.write_str("Could not open your default browser. Check your browser settings and try again."),
            Self::BooksNotServed(books) => write!(f, "No matching Desktop instance is serving {}. Each instance serves one books file. Start Desktop for these books, or answer without a detail link.", books.display()),
            Self::MultipleInstances => f.write_str("Multiple Desktop instances serve these books. Select the intended listener with --bind."),
            Self::EndpointMcp(error) => error.fmt(f),
            Self::Setup(error) => write!(f, "{error}"),
            Self::ExtensionInstall => f.write_str("Beankeeper could not set up local hosting for the extension. Check the extension log and unlock your OS keychain before trying again."),
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
        match error {
            setup::SetupError::WriteDenied(error) => Self::WriteDenied(error),
            error => Self::Setup(Box::new(error)),
        }
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
    fn extension_pending(&self) -> PathBuf {
        self.state.join("extension-install.pending")
    }
    pub fn runtime(&self) -> PathBuf {
        self.state.join("service-runtime.json")
    }
    pub fn log(&self) -> PathBuf {
        self.logs.join("service.log")
    }
    fn directories(&self) -> [Directory; 3] {
        [&self.data, &self.state, &self.logs].map(|path| Directory::new(path))
    }
    fn prepare(&self) -> Result<(), ServiceError> {
        for directory in self.directories() {
            directory
                .prepare()
                .map_err(|error| ServiceError::at(directory, error))?;
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

/// Find the installed service's registered books, if any.
///
/// # Errors
/// Returns invalid service path or registration errors.
pub fn managed_books() -> Result<Option<PathBuf>, ServiceError> {
    let paths = Paths::discover()?;
    Ok(Config::load(&paths)?
        .filter(|_| paths.definition.is_file())
        .map(|config| config.books))
}

pub(crate) fn default_books() -> Result<PathBuf, ServiceError> {
    Ok(service_ledger_options()?.db)
}

pub(crate) fn managed_executable() -> Result<Option<PathBuf>, ServiceError> {
    let paths = Paths::discover()?;
    Ok(Config::load(&paths)?.map(|config| config.executable))
}

pub(crate) fn managed_books_match(books: &Path) -> Result<bool, ServiceError> {
    Ok(managed_books()?
        .is_some_and(|managed| fs::canonicalize(books).is_ok_and(|books| books == managed)))
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Runtime {
    bind: ListenerAddress,
    pid: NonZeroU32,
    books: BooksPath,
    #[serde(default)]
    id: Option<InstanceId>,
}

/// A snapshot contains no credentials or recovery material.
#[derive(Debug, Serialize)]
pub struct Status {
    /// Whether a service registration is installed.
    pub installed: bool,
    /// Whether the managed server accepts this ledger's agent credential.
    pub running: bool,
    /// Running Desktop version; absent when stopped.
    pub version: Option<semver::Version>,
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
    /// A competing Beankeeper listener on the default port, when present.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub warning: Option<PortWarning>,
    /// Plain-language status.
    pub message: String,
    /// Verified grants of the credential used by the managed bridge, when available.
    pub agent_access: Option<AgentAccess>,
    /// Shared local license status, including trial expiry and remaining days.
    pub license: crate::product::LicenseResponse,
}

impl Status {
    fn plain_text(&self) -> String {
        let message = format!("{}\n{}", self.message, self.license);
        match &self.agent_access {
            Some(access) => format!("{message}\n{}", access.note()),
            None => message,
        }
    }
}

enum ServiceReply {
    Status(Box<Status>),
    Opened,
    Plan(FolderPlan),
}

impl ServiceReply {
    const OPENED_MESSAGE: &str =
        "Opened Beankeeper Desktop in your default browser. If asked, select Pair this browser.";
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
    let books = config.as_ref().map_or_else(
        || paths.data.join("books.db"),
        |config| config.books.clone(),
    );
    let license = crate::product::report_for_books(&books, &paths.state);
    let mut result = Status {
        installed: config.is_some() && paths.definition.try_exists()?,
        running: false,
        version: None,
        mcp_endpoint: None,
        desktop_url: None,
        pid: None,
        books,
        log: paths.log(),
        service_definition: paths.definition.clone(),
        error_code: None,
        warning: None,
        message: ServiceError::NotInstalled.to_string(),
        agent_access: None,
        license,
    };
    let endpoint = if result.installed {
        authenticated_endpoint(paths, &result.books, bridge::Probe::AgentAccess)
    } else {
        Ok(None)
    };
    let mut address = None;
    match endpoint {
        Ok(Some(AuthenticatedEndpoint { endpoint, access })) => {
            address = Some((&endpoint.address).into());
            result.running = true;
            result.desktop_url = Some(endpoint.desktop_url().into());
            result.mcp_endpoint = Some(endpoint.url.into());
            result.version = Some(endpoint.version);
            result.pid = Some(endpoint.pid);
            result.agent_access = access;
            result.message = "Beankeeper is running locally. Opening Desktop is optional. To pair this browser, ask your assistant to open Beankeeper Desktop or use bk open.".into();
        }
        Ok(None) if result.installed => {
            result.error_code = Some(ServiceError::NotRunning.code());
            result.message = ServiceError::NotRunning.to_string();
        }
        Ok(None) => {}
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
    result.warning = instance::default_port_warning(&paths.state, &result.books, address);
    Ok(result)
}

pub(crate) fn port_warning() -> Option<PortWarning> {
    let paths = Paths::discover().ok()?;
    let config = Config::load(&paths).ok()?;
    let books = config.map_or_else(|| paths.data.join("books.db"), |config| config.books);
    let address = endpoint_at(&paths, &books)
        .ok()
        .flatten()
        .map(|endpoint| (&endpoint.address).into());
    instance::default_port_warning(&paths.state, &books, address)
}

/// An endpoint from the private descriptor whose health response matched its process.
pub(crate) struct Endpoint {
    address: LiveListener,
    url: Url,
    books: PathBuf,
    token_file: PathBuf,
    version: semver::Version,
    pid: u32,
}

impl Endpoint {
    pub(crate) fn url(&self) -> &Url {
        &self.url
    }

    pub(crate) fn books(&self) -> &Path {
        &self.books
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
    if runtime.books.as_ref() != books || SocketAddr::from(runtime.bind).ip() != Ipv4Addr::LOCALHOST
    {
        return Err(ServiceError::Configuration(
            "invalid service runtime descriptor",
        ));
    }
    let health = Health::probe(runtime.bind)?;
    let address = health
        .verify(runtime.bind, runtime.pid, runtime.id.as_ref())
        .ok_or(ServiceError::EndpointIdentity)?;
    Ok(Some(Endpoint {
        address,
        url: runtime.bind.url("/mcp")?,
        books: runtime.books.as_ref().into(),
        token_file: runtime.books.as_ref().with_extension("agent-token"),
        version: health.version.parse().map_err(|_| ServiceError::Version)?,
        pid: runtime.pid.get(),
    }))
}

/// Select the books that an optional Desktop link should display.
#[derive(Clone, Copy, Debug)]
pub enum LinkBooks<'a> {
    /// Use the books explicitly selected by the caller.
    Explicit(&'a Path),
    /// Prefer managed books, falling back to the upstream CLI default path.
    ManagedOrDefault(&'a Path),
}

impl<'a> LinkBooks<'a> {
    fn config(self, paths: &Paths) -> Result<Option<Config>, ServiceError> {
        match self {
            Self::Explicit(_) => Ok(Config::load(paths).ok().flatten()),
            Self::ManagedOrDefault(_) => Config::load(paths),
        }
    }

    fn path(self, config: Option<&'a Config>) -> &'a Path {
        match self {
            Self::Explicit(path) => path,
            Self::ManagedOrDefault(default) => {
                config.map_or(default, |config| config.books.as_path())
            }
        }
    }
}

pub(crate) fn link_address(
    target: LinkBooks<'_>,
    bind: Option<ListenerAddress>,
) -> Result<LiveListener, ServiceError> {
    let paths = Paths::discover()?;
    let config = target.config(&paths)?;
    let requested = target.path(config.as_ref());
    let books = match BooksPath::try_from(requested) {
        Ok(books) => books,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Err(ServiceError::BooksNotServed(std::path::absolute(
                requested,
            )?));
        }
        Err(error) => return Err(error.into()),
    };
    if config
        .as_ref()
        .is_some_and(|config| config.books == books.as_ref())
        && let Ok(Some(endpoint)) = endpoint_at(&paths, books.as_ref())
        && bind.is_none_or(|bind| bind == (&endpoint.address).into())
    {
        return Ok(endpoint.address);
    }
    instance::discover(&paths.state, &books, bind)?
        .ok_or_else(|| ServiceError::BooksNotServed(books.into()))
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

pub(crate) fn browser_listener(
    paths: &Paths,
    expected_books: Option<&Path>,
) -> Result<LiveListener, ServiceError> {
    let config = Config::load(paths)?.ok_or(ServiceError::NotInstalled)?;
    if !paths.definition.try_exists()? {
        return Err(ServiceError::NotInstalled);
    }
    if expected_books.is_some_and(|books| books != config.books) {
        return Err(ServiceError::Configuration(
            "The local Desktop service is now hosting different books. Reconnect before opening it.",
        ));
    }
    endpoint_at(paths, &config.books)?
        .map(|endpoint| endpoint.address)
        .ok_or(ServiceError::NotRunning)
}

struct AuthenticatedEndpoint {
    endpoint: Endpoint,
    access: Option<AgentAccess>,
}

fn authenticated_endpoint(
    paths: &Paths,
    books: &Path,
    probe: bridge::Probe,
) -> Result<Option<AuthenticatedEndpoint>, ServiceError> {
    endpoint_at(paths, books)?
        .map(|endpoint| {
            let access = bridge::probe_service(&endpoint, probe)
                .map_err(|error| ServiceError::EndpointMcp(Box::new(error)))?;
            Ok(AuthenticatedEndpoint { endpoint, access })
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
pub fn run(options: &ServiceOptions, cli: Cli, explicit_db: bool) -> Result<(), ServiceError> {
    match options.command {
        ServiceCommand::Mcp => return mcp::serve(),
        ServiceCommand::Run => return host(&Paths::discover()?),
        ServiceCommand::Extension { command } => return extension::run(command, cli.is_json()),
        _ => {}
    }
    let operation = ServiceOperation::try_from(options.command)?;
    let json = cli.is_json()
        || matches!(
            options.command,
            ServiceCommand::Status { json: true } | ServiceCommand::AccessSet { json: true, .. }
        );
    let resume = match operation {
        ServiceOperation::Install(options) => {
            options.resume_for(explicit_db.then_some(cli.db.as_path()))
        }
        _ => "bk service status --json".into(),
    };
    let reply = match execute(operation, Some((cli, explicit_db)), None) {
        Ok(reply) => reply,
        Err(error) => {
            if json {
                println!(
                    "{}",
                    serde_json::to_value(crate::output::Envelope::<_, (), _> {
                        ok: false,
                        meta: crate::output::Metadata::new("service"),
                        data: None,
                        error: Some(error.structured(&resume))
                    })
                    .map_err(|_| ServiceError::Configuration("cannot serialize service error"))?
                );
            }
            return Err(error);
        }
    };
    if let ServiceReply::Plan(plan) = reply {
        if json {
            println!(
                "{}",
                serde_json::to_value(crate::output::Envelope::<_, _, ()> {
                    ok: true,
                    meta: crate::output::Metadata::new("service.install"),
                    data: Some(plan),
                    error: None
                })
                .map_err(|_| ServiceError::Configuration("cannot serialize folder plan"))?
            );
        } else {
            println!("{plan}");
        }
        return Ok(());
    }
    let ServiceReply::Status(status) = reply else {
        if json {
            println!(
                "{}",
                serde_json::json!({"opened":true,"message":ServiceReply::OPENED_MESSAGE})
            );
        } else {
            println!("{}", ServiceReply::OPENED_MESSAGE);
        }
        return Ok(());
    };
    if json {
        println!(
            "{}",
            serde_json::to_string(&status)
                .map_err(|_| ServiceError::Configuration("cannot serialize service status"))?
        );
        return Ok(());
    }
    print_status(options, *status)
}

fn print_status(options: &ServiceOptions, status: Status) -> Result<(), ServiceError> {
    match options.command {
        ServiceCommand::Status { json: true } | ServiceCommand::AccessSet { json: true, .. } => {
            println!(
                "{}",
                serde_json::to_string(&status)
                    .map_err(|_| ServiceError::Configuration("cannot serialize service status"))?
            );
        }
        ServiceCommand::Status { json: false } | ServiceCommand::AccessSet { json: false, .. } => {
            println!(
                "{}\nBooks: {}\nLog: {}",
                status.plain_text(),
                status.books.display(),
                status.log.display()
            );
            if let Some(endpoint) = status.mcp_endpoint {
                println!("MCP: {endpoint}");
            }
            if let Some(warning) = status.warning {
                eprintln!("{}", warning.message());
            }
        }
        ServiceCommand::Install(_) | ServiceCommand::InstallExtension => println!(
            "Encrypted ledger: {}\n{}\nRecovery kit: {}. Keep an offline copy under your own control.",
            status.books.display(),
            status.message,
            status.books.with_extension("recovery.txt").display()
        ),
        ServiceCommand::Start => println!("Beankeeper started."),
        ServiceCommand::Stop => {
            println!("Beankeeper stopped; login startup is disabled until bk service start.");
        }
        ServiceCommand::Uninstall => println!(
            "Service removed. Books, credentials, MCP configs and recovery material were preserved."
        ),
        ServiceCommand::Mcp
        | ServiceCommand::Run
        | ServiceCommand::Open
        | ServiceCommand::Extension { .. } => {}
    }
    Ok(())
}

fn install_plan(
    paths: &Paths,
    ledger: Option<&(Cli, bool)>,
    options: InstallOptions,
) -> Result<FolderPlan, ServiceError> {
    let mut directories = paths.directories().to_vec();
    directories.push(setup::temporary_secret_directory());
    directories.push(Directory::containing(&paths.definition));
    if cfg!(target_os = "linux") {
        directories.push(Directory::containing(&manager::linux_artifact(paths)));
    }
    let explicit = ledger
        .filter(|(_, explicit)| *explicit)
        .map(|(cli, _)| cli.db.as_path());
    let books = explicit.map_or_else(
        || {
            Config::load(paths).map(|config| {
                config.map_or_else(|| paths.data.join("books.db"), |config| config.books)
            })
        },
        |path| std::path::absolute(path).map_err(Into::into),
    )?;
    directories.push(Directory::containing(&books));
    if !options.no_agents {
        directories.extend(
            setup::config::service_config_paths(&paths.home)
                .iter()
                .map(|path| Directory::containing(path)),
        );
    }
    FolderPlan::try_from((directories, options.resume_for(explicit))).map_err(Into::into)
}

fn execute(
    operation: ServiceOperation,
    ledger: Option<(Cli, bool)>,
    expected_books: Option<&Path>,
) -> Result<ServiceReply, ServiceError> {
    let paths = Paths::discover()?;
    match operation {
        ServiceOperation::Status => return status_reply(&paths),
        ServiceOperation::Open => {
            browser::open_managed(&paths, expected_books)?;
            return Ok(ServiceReply::Opened);
        }
        _ => {}
    }
    if let ServiceOperation::Install(options) = operation
        && options.plan
    {
        return install_plan(&paths, ledger.as_ref(), options).map(ServiceReply::Plan);
    }
    paths.prepare()?;
    let _lock = lock(&paths.state.join("service-operation.lock"))?;
    let previous = Config::load(&paths)?;
    match operation {
        ServiceOperation::Install(options) => {
            install(
                &paths,
                ledger,
                previous.as_ref(),
                InstallMode::Command(options),
            )?;
            remove_if_exists(&paths.extension_pending())?;
        }
        ServiceOperation::InstallExtension => {
            // An explicit service installation racing the extension takes
            // precedence. Reuse it and leave its registration untouched.
            if previous.is_none() {
                install(&paths, ledger, None, InstallMode::Extension)?;
            } else if let Some(config) = previous.as_ref()
                && paths.extension_pending().try_exists()?
            {
                finish_extension_install(&paths, config)?;
            }
            remove_if_exists(&paths.extension_pending())?;
        }
        ServiceOperation::Start => {
            let config = previous.as_ref().ok_or(ServiceError::NotInstalled)?;
            manager::start(&paths, &UserSessionEnv::discover()?)?;
            wait_ready(&paths, &config.books)?;
        }
        ServiceOperation::Stop => {
            if previous.is_some() {
                manager::stop(&paths, &UserSessionEnv::discover()?)?;
                // A native manager can terminate the host before it removes
                // its descriptor, notably when Windows stops the task.
                remove_if_exists(&paths.runtime())?;
                remove_if_exists(&paths.extension_pending())?;
            }
        }
        ServiceOperation::Uninstall => {
            if previous.is_some() {
                manager::uninstall(&paths, &UserSessionEnv::discover()?)?;
                remove_if_exists(&paths.config())?;
                remove_if_exists(&paths.runtime())?;
                remove_if_exists(&paths.extension_pending())?;
            }
        }
        ServiceOperation::AccessSet(access) => {
            let config = previous.as_ref().ok_or(ServiceError::NotInstalled)?;
            if expected_books.is_some_and(|books| books != config.books)
                || matches!(ledger, Some((cli, true)) if fs::canonicalize(&cli.db)? != config.books)
            {
                return Err(ServiceError::Configuration(
                    "access-set applies to the installed service's books",
                ));
            }
            let mut cli = service_ledger_options()?;
            cli.db.clone_from(&config.books);
            let _passphrase = setup::stored_passphrase(&mut cli)?;
            change_access(&cli, AccessChange::Set(access))?;
            let mut status = status_at(&paths)?;
            status.agent_access.get_or_insert_with(|| access.into());
            return Ok(ServiceReply::Status(Box::new(status)));
        }
        ServiceOperation::Status | ServiceOperation::Open => {}
    }
    status_reply(&paths)
}

fn status_reply(paths: &Paths) -> Result<ServiceReply, ServiceError> {
    status_at(paths).map(|status| ServiceReply::Status(Box::new(status)))
}

#[derive(Clone, Copy)]
enum InstallMode {
    Command(InstallOptions),
    Extension,
}

impl InstallMode {
    fn storage(self) -> ServiceSecretStorage {
        match self {
            Self::Command(options) => options.storage(),
            Self::Extension => ServiceSecretStorage::NativeRequired,
        }
    }

    fn registration(self, previous_executable: &Path) -> ClientRegistration<'_> {
        match self {
            Self::Command(options) => options.registration(previous_executable),
            Self::Extension => ClientRegistration::Skip,
        }
    }
}

fn install(
    paths: &Paths,
    ledger: Option<(Cli, bool)>,
    previous: Option<&Config>,
    mode: InstallMode,
) -> Result<(), ServiceError> {
    let (mut cli, explicit_db) = match ledger {
        Some(options) => options,
        None => (service_ledger_options()?, false),
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
    let previous_executable = previous.map_or(executable.as_path(), |config| &config.executable);
    let registration = mode.registration(previous_executable);
    let adopting = previous.is_none() && cli.db.try_exists()?;
    let port = previous.map_or_else(
        || SocketAddr::from(ListenerAddress::default()).port(),
        |config| config.port,
    );
    setup::run_service(
        &mut cli,
        &paths.home,
        (Ipv4Addr::LOCALHOST, port).into(),
        registration,
        mode.storage(),
        &paths.state,
    )?;
    if adopting && cli.db.with_extension("agent-token").try_exists()? {
        cli.passphrase.passphrase_file = None;
        let _passphrase = setup::stored_passphrase(&mut cli)?;
        change_access(&cli, AccessChange::Preserve)?;
    }
    let config = Config {
        executable,
        books: fs::canonicalize(&cli.db)?,
        port,
    };
    if matches!(mode, InstallMode::Extension) {
        private_write(&paths.extension_pending(), b"")?;
    }
    private_json(&paths.config(), &config)?;
    start_install(paths, &config, &session)
}

fn finish_extension_install(paths: &Paths, config: &Config) -> Result<(), ServiceError> {
    if matches!(
        authenticated_endpoint(paths, &config.books, bridge::Probe::Authentication),
        Ok(Some(_))
    ) {
        return Ok(());
    }
    start_install(paths, config, &UserSessionEnv::discover()?)
}

fn start_install(
    paths: &Paths,
    config: &Config,
    session: &UserSessionEnv,
) -> Result<(), ServiceError> {
    manager::install(paths, config)?;
    manager::start(paths, session)?;
    wait_ready(paths, &config.books)
}

fn service_ledger_options() -> Result<Cli, ServiceError> {
    <Cli as clap::Parser>::try_parse_from(["bk", "mcp", "serve"])
        .map_err(|_| ServiceError::Configuration("cannot construct service ledger options"))
}

fn change_access(cli: &Cli, change: AccessChange) -> Result<(), ServiceError> {
    let passphrase = passphrase::resolve_passphrase(
        cli.passphrase.passphrase_fd,
        cli.passphrase.passphrase_file.as_deref(),
        false,
    )
    .map_err(setup::SetupError::from)?;
    let books =
        crate::instance::BooksPath::try_from(cli.db.as_path()).map_err(setup::SetupError::from)?;
    let db = crate::backups::LocalBackups::new(books, passphrase)
        .open_protected(beankeeper_cli::mcp::LedgerTarget::default())
        .map_err(setup::SetupError::from)?;
    match setup::managed::reconcile(&db, &cli.db, change) {
        Err(setup::SetupError::Ledger(
            error @ beankeeper_cli::error::CliError::WriteDenied { .. },
        )) if !matches!(change, AccessChange::Set(_)) => {
            // Let the existing credential keep reading until automatic rotation
            // can run on a licensed copy.
            eprintln!("Managed agent access rotation was deferred. {error}");
            Ok(())
        }
        result => result.map_err(Into::into),
    }
}

/// Restart the running managed service after replacing its own executable.
/// # Errors
/// Returns service registration, manager or readiness errors.
pub fn restart_updated(executable: &Path) -> Result<bool, ServiceError> {
    let paths = Paths::discover()?;
    let Some(config) = Config::load(&paths)? else {
        return Ok(false);
    };
    if config.executable != executable || endpoint_at(&paths, &config.books)?.is_none() {
        return Ok(false);
    }
    let _lock = lock(&paths.state.join("service-operation.lock"))?;
    let session = UserSessionEnv::discover()?;
    manager::stop(&paths, &session)?;
    remove_if_exists(&paths.runtime())?;
    manager::start(&paths, &session)?;
    wait_ready(&paths, &config.books)?;
    Ok(true)
}

fn wait_ready(paths: &Paths, books: &Path) -> Result<(), ServiceError> {
    let deadline = Instant::now() + Duration::from_secs(60);
    let mut last_error = ServiceError::NotReady;
    while Instant::now() < deadline {
        match authenticated_endpoint(paths, books, bridge::Probe::Authentication) {
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
    let mut config = Config::load(paths)?.ok_or(ServiceError::Configuration(
        "service registration is missing",
    ))?;
    paths.prepare()?;
    let _lock = lock(&paths.state.join("service-host.lock"))?;
    let listener = bind_listener(config.port)?;
    let bound = listener.local_addr()?;
    let bind = ListenerAddress::try_from(bound).map_err(ServiceError::Configuration)?;
    let id = InstanceId::default();
    let runtime = Runtime {
        bind,
        pid: NonZeroU32::try_from(std::process::id())
            .map_err(|_| ServiceError::EndpointIdentity)?,
        books: BooksPath::try_from(config.books.as_path())?,
        id: Some(id.clone()),
    };
    let mut cli = service_ledger_options()?;
    cli.db.clone_from(&config.books);
    let _passphrase = setup::stored_passphrase(&mut cli)?;
    change_access(&cli, AccessChange::Migrate)?;
    if bound.port() != config.port {
        config.port = bound.port();
        private_json(&paths.config(), &config)?;
    }
    private_json(&paths.runtime(), &runtime)?;
    let result = crate::server::serve_listener(&cli, listener, id);
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
pub(crate) fn lock(path: &Path) -> Result<fs::File, ServiceError> {
    lock_at(path).map_err(|error| match error {
        ServiceError::Io(error) => ServiceError::at(Directory::containing(path), error),
        error => error,
    })
}
fn lock_at(path: &Path) -> Result<fs::File, ServiceError> {
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
pub(crate) fn read_private<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T, ServiceError> {
    let bytes = PrivateFile::open(path)?.read_bytes(SERVICE_STATE_LIMIT)?;
    serde_json::from_slice(bytes.expose_secret()).map_err(|_| {
        ServiceError::Configuration(
            "invalid private service state; inspect service.json and service-runtime.json",
        )
    })
}
pub(crate) fn private_json(path: &Path, value: &impl Serialize) -> Result<(), ServiceError> {
    let bytes = serde_json::to_vec_pretty(value)
        .map_err(|_| ServiceError::Configuration("cannot serialize service state"))?;
    private_write(path, &bytes)
}
pub(crate) fn private_write(path: &Path, bytes: &[u8]) -> Result<(), ServiceError> {
    private_write_at(path, bytes).map_err(|error| match error {
        ServiceError::Io(error) => ServiceError::at(Directory::containing(path), error),
        error => error,
    })
}
fn private_write_at(path: &Path, bytes: &[u8]) -> Result<(), ServiceError> {
    let parent = Directory::containing(path);
    parent.create()?;
    match PrivateFile::open(path) {
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    let mut pending = tempfile::NamedTempFile::new_in(parent.as_ref())?;
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
