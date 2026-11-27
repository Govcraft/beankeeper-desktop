//! User-level registration for supported agents and managed-service diagnostics.

mod adapters;
mod config;
pub(crate) mod extension;
pub(crate) mod extension_status;

use std::{
    env,
    error::Error,
    fmt, fs,
    io::{self, Write},
    path::PathBuf,
    process::ExitCode,
};

use clap::{Args, Subcommand};
use serde::Serialize;

use crate::{
    bridge,
    output::{Envelope, Metadata},
    service,
};
use adapters::{Agent, ExtensionRegistration, Locations};
use config::{Change, Registration};

/// Detect, register or verify a user's local agent connections.
#[derive(Args, Debug)]
pub struct AgentOptions {
    #[command(subcommand)]
    command: AgentCommand,
}

#[derive(Subcommand, Debug)]
enum AgentCommand {
    /// List all supported clients and their local configuration state.
    Detect,
    /// Add Beankeeper to installed agents and install the skill where supported.
    Connect(Selection),
    /// Remove Beankeeper's managed entries and skill, retaining other settings.
    Disconnect(Selection),
    /// Verify client configuration and make an authenticated read-only MCP call.
    Doctor(Selection),
    /// Verify and open the latest signed Claude Desktop extension (requires cosign).
    Extension(extension::Options),
}

#[derive(Args, Debug)]
struct Selection {
    /// Apply to all detected clients, instead of selecting a single client.
    #[arg(long, conflicts_with = "agent")]
    all: bool,
    /// Select one client. Use `chatgpt-codex` for local Codex in the `ChatGPT` app.
    /// Without a selection, checks all detected clients.
    #[arg(value_enum)]
    agent: Option<Agent>,
}

#[derive(Clone, Copy, Serialize)]
enum Operation {
    #[serde(rename = "agents.detect")]
    Detect,
    #[serde(rename = "agents.connect")]
    Connect,
    #[serde(rename = "agents.disconnect")]
    Disconnect,
    #[serde(rename = "agents.doctor")]
    Doctor,
}

#[derive(Debug)]
enum AgentError {
    Home,
    Version,
    Binary(io::Error),
    InvalidBinary(PathBuf),
    ClaudeProfileAmbiguous,
    Invalid(PathBuf),
    Conflict(PathBuf),
    ServerDisabled(&'static str),
    RegistrationConflict {
        path: PathBuf,
        server: &'static str,
        reason: config::ServerConflict,
    },
    Io(PathBuf, io::Error),
}

impl AgentError {
    fn code(&self) -> &'static str {
        match self {
            Self::Home => "AGENT_HOME_UNAVAILABLE",
            Self::Version => "AGENT_VERSION_INVALID",
            Self::Binary(_) | Self::InvalidBinary(_) => "AGENT_BINARY_PATH",
            Self::ClaudeProfileAmbiguous => "AGENT_CLAUDE_PROFILE_AMBIGUOUS",
            Self::Invalid(_) => "AGENT_INVALID_CONFIG",
            Self::Conflict(_) | Self::RegistrationConflict { .. } => "AGENT_CONFIG_CONFLICT",
            Self::ServerDisabled(_) => "AGENT_MCP_SERVER_DISABLED",
            Self::Io(..) => "AGENT_CONFIG_IO",
        }
    }
}

impl fmt::Display for AgentError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Home => {
                f.write_str("Beankeeper needs the desktop user's absolute home directory.")
            }
            Self::Version => f.write_str("Desktop has an invalid semantic version."),
            Self::Binary(error) => write!(f, "cannot locate the installed bk executable: {error}"),
            Self::InvalidBinary(path) => write!(
                f,
                "MCP clients need an absolute Unicode executable path: {}",
                path.display()
            ),
            Self::ClaudeProfileAmbiguous => f.write_str(
                "Several Claude Desktop profiles were found. Set CLAUDE_USER_DATA_DIR to the absolute path of the profile Claude Desktop is using, then retry.",
            ),
            Self::Invalid(path) => write!(
                f,
                "invalid or unsupported agent config file {}; repair it before retrying",
                path.display()
            ),
            Self::Conflict(path) => write!(
                f,
                "a different Beankeeper entry or skill already exists in {}; inspect it before retrying",
                path.display()
            ),
            Self::ServerDisabled(server) => write!(
                f,
                "Codex has the {server} connection turned off. Reconnect it only if the owner asks."
            ),
            Self::RegistrationConflict { path, server, reason } => {
                write!(f, "{}: the `{server}` entry {reason}", path.display())
            }
            Self::Io(path, error) => write!(f, "cannot update or read {}: {error}", path.display()),
        }
    }
}

impl Error for AgentError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Binary(error) | Self::Io(_, error) => Some(error),
            _ => None,
        }
    }
}

#[derive(Serialize)]
struct Failure {
    code: &'static str,
    message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    technical_detail: Option<&'static str>,
}

impl Failure {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            technical_detail: None,
        }
    }
}

impl From<AgentError> for Failure {
    fn from(error: AgentError) -> Self {
        Self::new(error.code(), error.to_string())
    }
}

#[derive(Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
enum State {
    Unsupported,
    NotInstalled,
    Available,
    PartiallyConfigured,
    Configured,
    Disconnected,
    ExtensionManaged,
    ExtensionDisabled,
    ExtensionBlocked,
    Error,
}

#[derive(Serialize)]
struct ClientStatus {
    agent: Agent,
    name: &'static str,
    state: State,
    #[serde(skip_serializing_if = "Option::is_none")]
    config: Option<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    skill_installed: Option<bool>,
    message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<Failure>,
}

impl ClientStatus {
    fn new(agent: Agent, state: State, message: impl Into<String>) -> Self {
        Self {
            agent,
            name: agent.label(),
            state,
            config: None,
            skill_installed: None,
            message: message.into(),
            error: None,
        }
    }

    fn failure(mut self, error: Failure) -> Self {
        self.message.clone_from(&error.message);
        self.error = Some(error);
        self
    }
}

#[derive(Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
enum ServiceCheck {
    Ready { endpoint: String },
    Unavailable { error: Failure },
}

impl ServiceCheck {
    fn unavailable(code: &'static str, message: impl Into<String>) -> Self {
        Self::Unavailable {
            error: Failure::new(code, message),
        }
    }

    fn probe() -> Self {
        let endpoint = match service::connection() {
            Ok(endpoint) => endpoint,
            Err(error) => return Self::unavailable(error.code(), error.to_string()),
        };
        if let Err(error) = bridge::probe_service(&endpoint, bridge::Probe::ReadOnlyTool) {
            return Self::unavailable(error.diagnostic_code(), error.to_string());
        }
        Self::Ready {
            endpoint: endpoint.url().to_string(),
        }
    }
}

#[derive(Clone, Copy, Serialize)]
#[serde(into = "&'static str")]
struct ClaudeOverlap;

impl ClaudeOverlap {
    fn detect(agents: &[ClientStatus], inspect: impl Fn(Agent) -> State) -> Option<Self> {
        let state = |agent| {
            agents
                .iter()
                .find(|status| status.agent == agent)
                .map_or_else(|| inspect(agent), |status| status.state)
        };
        (matches!(state(Agent::ClaudeDesktop), State::ExtensionManaged)
            && matches!(
                state(Agent::ClaudeCode),
                State::Configured | State::PartiallyConfigured
            ))
        .then_some(Self)
    }
}

impl From<ClaudeOverlap> for &'static str {
    fn from(_: ClaudeOverlap) -> Self {
        "Claude Desktop may also show Claude Code's Beankeeper connections under Local dev. In Claude Desktop, you only need the Beankeeper extension."
    }
}

#[derive(Serialize)]
struct Report {
    agents: Vec<ClientStatus>,
    #[serde(skip_serializing_if = "Option::is_none")]
    claude_overlap: Option<ClaudeOverlap>,
    #[serde(skip_serializing_if = "Option::is_none")]
    service: Option<ServiceCheck>,
    #[serde(skip_serializing_if = "Option::is_none")]
    warning: Option<crate::instance::PortWarning>,
    #[serde(skip_serializing_if = "Option::is_none")]
    keyring: Option<Failure>,
}

type Output = Envelope<Operation, Report, Failure>;

/// Run the requested local agent operation and print its result.
#[must_use = "return this status to the CLI caller"]
pub fn run(options: AgentOptions, json: bool, quiet: bool) -> ExitCode {
    let (operation, selected) = match options.command {
        AgentCommand::Detect => (Operation::Detect, None),
        AgentCommand::Connect(selection) => (Operation::Connect, selected_agent(&selection)),
        AgentCommand::Disconnect(selection) => (Operation::Disconnect, selected_agent(&selection)),
        AgentCommand::Doctor(selection) => (Operation::Doctor, selected_agent(&selection)),
        AgentCommand::Extension(options) => return extension::run(options, json, quiet),
    };
    let output_meta = Metadata::new(operation);
    let output = match perform(operation, selected) {
        Ok(data) => {
            let ok = data.keyring.is_none()
                && data.agents.iter().all(|item| item.error.is_none())
                && !matches!(data.service, Some(ServiceCheck::Unavailable { .. }));
            let error = (!ok).then(|| Failure::new(
                "AGENT_CHECKS_FAILED",
                "One or more agent operations need attention. See the per-client or service result.",
            ));
            Output {
                ok,
                meta: output_meta,
                data: Some(data),
                error,
            }
        }
        Err(error) => Output {
            ok: false,
            meta: output_meta,
            data: None,
            error: Some(error.into()),
        },
    };
    let result = if json {
        write_json(&output, &mut io::stdout().lock())
    } else if !quiet || !output.ok {
        write_text(&output, &mut io::stdout().lock(), &mut io::stderr().lock())
    } else {
        Ok(())
    };
    if let Err(error) = result {
        eprintln!("AGENT_OUTPUT: {error}");
        return ExitCode::FAILURE;
    }
    if output.ok {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

fn selected_agent(selection: &Selection) -> Option<Agent> {
    if selection.all { None } else { selection.agent }
}

fn write_json(output: &Output, writer: &mut impl Write) -> io::Result<()> {
    serde_json::to_writer(&mut *writer, output).map_err(io::Error::other)?;
    writeln!(writer)
}

fn write_text(output: &Output, writer: &mut impl Write, errors: &mut impl Write) -> io::Result<()> {
    if let Some(data) = &output.data {
        for item in &data.agents {
            if let Some(error) = &item.error {
                writeln!(errors, "{}: {}: {}", item.name, error.code, error.message)?;
            } else {
                writeln!(writer, "{}: {}", item.name, item.message)?;
            }
        }
        if let Some(overlap) = data.claude_overlap {
            writeln!(writer, "{}", <&str>::from(overlap))?;
        }
        match &data.service {
            Some(ServiceCheck::Ready { endpoint }) => writeln!(
                writer,
                "Beankeeper: read-only MCP call succeeded at {endpoint}."
            )?,
            Some(ServiceCheck::Unavailable { error }) => {
                writeln!(errors, "Beankeeper: {}: {}", error.code, error.message)?;
            }
            None => {}
        }
        if let Some(error) = &data.keyring {
            writeln!(errors, "Beankeeper: {}: {}", error.code, error.message)?;
            if let Some(detail) = error.technical_detail {
                writeln!(errors, "Technical detail: {detail}")?;
            }
        }
        if let Some(warning) = &data.warning {
            writeln!(errors, "{}", warning.message())?;
        }
    } else if let Some(error) = &output.error {
        writeln!(errors, "{}: {}", error.code, error.message)?;
    }
    Ok(())
}

fn perform(operation: Operation, selected: Option<Agent>) -> Result<Report, AgentError> {
    let binary = env::current_exe().map_err(AgentError::Binary)?;
    perform_for(operation, selected, &binary, RegistrationSource::Command)
}

#[derive(Clone, Copy)]
enum RegistrationSource {
    Command,
    ClaudeExtension,
}

#[derive(Serialize)]
pub(crate) struct ExtensionRegistrationIssue {
    agent: Agent,
    code: &'static str,
}

pub(crate) fn connect_from_extension(
    binary: &std::path::Path,
) -> Result<Vec<ExtensionRegistrationIssue>, &'static str> {
    let report = perform_for(
        Operation::Connect,
        None,
        binary,
        RegistrationSource::ClaudeExtension,
    )
    .map_err(|error| error.code())?;
    Ok(report
        .agents
        .into_iter()
        .filter_map(|status| {
            status.error.map(|error| ExtensionRegistrationIssue {
                agent: status.agent,
                code: error.code,
            })
        })
        .collect())
}

fn perform_for(
    operation: Operation,
    selected: Option<Agent>,
    binary: &std::path::Path,
    source: RegistrationSource,
) -> Result<Report, AgentError> {
    let locations = Locations::discover()
        .filter(|locations| locations.home.is_absolute())
        .ok_or(AgentError::Home)?;
    let binary = binary
        .to_str()
        .filter(|_| binary.is_absolute())
        .ok_or_else(|| AgentError::InvalidBinary(binary.into()))?;
    let _lock = matches!(operation, Operation::Connect | Operation::Disconnect)
        .then(mutation_lock)
        .transpose()?;
    let agents: Vec<_> = Agent::ALL
        .into_iter()
        .filter(|agent| {
            !matches!(
                (source, agent),
                (RegistrationSource::ClaudeExtension, Agent::ClaudeDesktop)
            )
        })
        .filter(|agent| selected.is_none_or(|selected| selected == *agent))
        .map(|agent| check_agent(agent, &locations, binary, operation, selected.is_some()))
        .collect();
    let claude_overlap = if matches!(source, RegistrationSource::Command)
        && !matches!(operation, Operation::Disconnect)
        && matches!(
            selected,
            None | Some(Agent::ClaudeDesktop | Agent::ClaudeCode)
        ) {
        ClaudeOverlap::detect(&agents, |agent| {
            check_agent(agent, &locations, binary, Operation::Detect, false).state
        })
    } else {
        None
    };
    let service = match operation {
        Operation::Doctor => Some(ServiceCheck::probe()),
        Operation::Connect => service::registration_session()
            .err()
            .map(|error| ServiceCheck::unavailable(error.code(), error.to_string())),
        Operation::Detect | Operation::Disconnect => None,
    };
    let warning = if matches!(operation, Operation::Doctor) {
        service::port_warning()
    } else {
        None
    };
    let keyring = if matches!(operation, Operation::Doctor)
        && matches!(service, Some(ServiceCheck::Unavailable { .. }))
    {
        keyring_check(&locations.home)
    } else {
        None
    };
    Ok(Report {
        agents,
        claude_overlap,
        service,
        warning,
        keyring,
    })
}

// Read only: no probe credential is written, even before first setup.
fn keyring_check(home: &std::path::Path) -> Option<Failure> {
    use crate::setup::keychain::{OsKeychain, SecretStore};
    let error = OsKeychain
        .load(&home.join(".beankeeper-keyring-diagnostic"))
        .err()?;
    Some(crate::setup::SetupError::from(error).into())
}

impl From<crate::setup::SetupError> for Failure {
    fn from(error: crate::setup::SetupError) -> Self {
        let code = match &error {
            crate::setup::SetupError::KeyringLocked { .. } => "KEYRING_LOCKED",
            crate::setup::SetupError::NoKeyringProvider { .. } => "KEYRING_NO_PROVIDER",
            _ => "KEYRING_UNAVAILABLE",
        };
        Self {
            code,
            message: error.to_string(),
            technical_detail: error.technical_detail(),
        }
    }
}

fn mutation_lock() -> Result<fs::File, AgentError> {
    let state = crate::product::state_dir().map_err(|_| AgentError::Home)?;
    let path = state.join("agent-registration.lock");
    let error_at = |error| AgentError::Io(path.clone(), error);
    fs::create_dir_all(&state).map_err(error_at)?;
    crate::setup::keychain::restrict_directory(&state).map_err(error_at)?;
    if fs::symlink_metadata(&path).is_ok_and(|meta| !meta.is_file()) {
        return Err(AgentError::Invalid(path));
    }
    let mut options = fs::OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options.open(&path).map_err(error_at)?;
    crate::setup::keychain::restrict_file(&path).map_err(error_at)?;
    file.lock().map_err(error_at)?;
    Ok(file)
}

fn check_agent(
    agent: Agent,
    locations: &Locations,
    binary: &str,
    operation: Operation,
    explicit: bool,
) -> ClientStatus {
    let adapter = match agent.adapter(locations) {
        Ok(adapter) => adapter,
        Err(error) => return ClientStatus::new(agent, State::Error, "").failure(error.into()),
    };
    let Some(adapter) = adapter else {
        let message = agent.unsupported_message();
        let status = ClientStatus::new(agent, State::Unsupported, message);
        return if explicit {
            status.failure(Failure::new("AGENT_UNSUPPORTED", message))
        } else {
            status
        };
    };
    if !(adapter.installed() || explicit && matches!(operation, Operation::Connect)) {
        let message = "No local installation or configuration was found.";
        let status = ClientStatus::new(agent, State::NotInstalled, message);
        return if explicit {
            status.failure(Failure::new("AGENT_NOT_INSTALLED", message))
        } else {
            status
        };
    }
    match adapter.extension_registration() {
        Ok(Some(registration)) => return extension_status(agent, operation, registration),
        Ok(None) => {}
        Err(error) => return ClientStatus::new(agent, State::Error, "").failure(error.into()),
    }
    let outcome = match operation {
        Operation::Connect => adapter.change(binary, Change::Connect).map(|()| {
            ClientStatus::new(
                agent,
                State::Configured,
                match agent {
                    Agent::Codex => "Beankeeper is registered in Codex. If this chat still lacks the tools, reload Codex and open a new local chat.",
                    _ => "Beankeeper is registered. Restart the agent client to load it.",
                },
            )
        }),
        Operation::Disconnect => adapter.change(binary, Change::Disconnect).map(|()| {
            ClientStatus::new(
                agent,
                State::Disconnected,
                "Managed Beankeeper entries were removed or already absent.",
            )
        }),
        Operation::Detect | Operation::Doctor => adapter.registration(binary).and_then(|state| {
            Ok(registration_status(
                agent,
                operation,
                state,
                adapter.skill_available()?,
                &adapter,
            ))
        }),
    };
    let mut status = match outcome {
        Ok(status) => status,
        Err(error) => ClientStatus::new(agent, State::Error, "").failure(error.into()),
    };
    status.config = Some(adapter.config);
    status
}

fn extension_status(
    agent: Agent,
    operation: Operation,
    registration: ExtensionRegistration,
) -> ClientStatus {
    let (state, message, problem) = match registration {
        ExtensionRegistration::Enabled => (
            State::ExtensionManaged,
            "The Beankeeper extension manages this connection.",
            None,
        ),
        ExtensionRegistration::Disabled => (
            State::ExtensionDisabled,
            "Enable the Beankeeper extension in Claude Desktop Settings, then retry.",
            Some("AGENT_EXTENSION_DISABLED"),
        ),
        ExtensionRegistration::Blocked => (
            State::ExtensionBlocked,
            "Ask your Claude workspace administrator to allow the Beankeeper extension.",
            Some("AGENT_EXTENSION_BLOCKED"),
        ),
    };
    let status = ClientStatus::new(agent, state, message);
    match (operation, problem) {
        (Operation::Disconnect, _) => status.failure(Failure::new(
            "AGENT_EXTENSION_MANAGED",
            "Remove the Beankeeper extension in Claude Desktop Settings to disconnect it.",
        )),
        (Operation::Doctor, Some(code)) => status.failure(Failure::new(code, message)),
        _ => status,
    }
}

fn registration_status(
    agent: Agent,
    operation: Operation,
    registration: Registration,
    skill_available: bool,
    adapter: &adapters::Adapter,
) -> ClientStatus {
    let (state, message) = match registration {
        Registration::Missing => (
            State::Available,
            "Detected. Run bk agents connect to register Beankeeper.",
        ),
        Registration::Partial => (
            State::PartiallyConfigured,
            "One Beankeeper entry is missing. Run bk agents connect to repair it.",
        ),
        Registration::Owned if !skill_available => (
            State::PartiallyConfigured,
            "The Beankeeper skill is missing. Run bk agents connect to repair it.",
        ),
        Registration::Owned => (
            State::Configured,
            match agent {
                Agent::Codex => {
                    "The Codex profile is configured. Call company_list in the local chat to verify that Codex has loaded Beankeeper."
                }
                _ => "The managed MCP configuration is present.",
            },
        ),
    };
    let incomplete = !matches!(state, State::Configured);
    let mut result = ClientStatus::new(agent, state, message);
    result.skill_installed = adapter.skill.as_ref().map(|_| skill_available);
    if incomplete && matches!(operation, Operation::Doctor) {
        result = result.failure(Failure::new("AGENT_NOT_CONFIGURED", message));
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn doctor_keeps_keyring_details_separate_from_owner_message() -> Result<(), Box<dyn Error>> {
        for (error, code) in [
            (
                crate::setup::keychain::KeychainError::Locked,
                "KEYRING_LOCKED",
            ),
            (
                crate::setup::keychain::KeychainError::NoProvider,
                "KEYRING_NO_PROVIDER",
            ),
        ] {
            let failure = Failure::from(crate::setup::SetupError::from(error));
            let wire = serde_json::to_value(&failure)?;
            assert_eq!(wire["code"], code);
            assert!(wire["technical_detail"].is_string());
            assert!(!failure.message.contains("keyring::Error"));
            assert!(!failure.message.contains("terminal"));
            assert_ne!(wire["message"], wire["technical_detail"]);
        }
        Ok(())
    }
}
