//! First-run provisioning shared by the extension tools and the CLI.

use std::{
    fs, io,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    str::FromStr,
};

use clap::Subcommand;
use rmcp::model::{CallToolResult, ContentBlock, JsonObject, Tool, ToolAnnotations};
use serde::{Deserialize, Serialize};

use super::{Config, Endpoint, Paths, ServiceError, UserSessionEnv, keychain, lock};
use crate::{agents, bridge, onboarding::STATUS_TOOL};

/// Manage a local installation owned by the Claude extension.
#[derive(Subcommand, Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExtensionCommand {
    /// Provision local hosting and register detected companion agents.
    Setup,
    /// Check whether the extension's local service is ready.
    Status,
    /// Verify the extension's ledger connection with an authenticated read-only call.
    Doctor,
}

impl ExtensionCommand {
    pub(crate) const ALL: [Self; 3] = [Self::Setup, Self::Status, Self::Doctor];

    fn name(self) -> &'static str {
        match self {
            Self::Setup => "setup",
            Self::Status => STATUS_TOOL,
            Self::Doctor => "doctor",
        }
    }

    pub(crate) fn tool(self) -> Option<Tool> {
        let description = match self {
            Self::Setup => {
                "When the owner asks to set up their books or create demo books, call setup to start local Beankeeper if setup_status reports not_started. It returns immediately. If it reports running, poll setup_status until ready or failed. Takes no arguments."
            }
            Self::Status => {
                "Check local setup progress, then read the books' setup status and suggested next step when ready. Takes no arguments."
            }
            Self::Doctor => {
                "Check the local Beankeeper extension and its authenticated connection. Use this if setup fails or Claude cannot reach the books. Takes no arguments."
            }
        };
        let schema = schemars::schema_for!(NoArguments).to_value();
        Some(
            Tool::new(self.name(), description, schema.as_object()?.clone()).with_annotations(
                ToolAnnotations::new()
                    .read_only(matches!(self, Self::Status | Self::Doctor))
                    .destructive(false)
                    .idempotent(true)
                    .open_world(false),
            ),
        )
    }
}

impl FromStr for ExtensionCommand {
    type Err = ServiceError;

    fn from_str(name: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .find(|command| command.name() == name)
            .ok_or(ServiceError::UnknownTool)
    }
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct NoArguments {}

pub(crate) struct ExtensionRequest {
    command: ExtensionCommand,
    _arguments: NoArguments,
}

impl TryFrom<(ExtensionCommand, Option<JsonObject>)> for ExtensionRequest {
    type Error = ServiceError;

    fn try_from(
        (command, arguments): (ExtensionCommand, Option<JsonObject>),
    ) -> Result<Self, Self::Error> {
        let arguments =
            NoArguments::deserialize(serde_json::Value::Object(arguments.unwrap_or_default()))
                .map_err(|_| ServiceError::Arguments)?;
        Ok(Self {
            command,
            _arguments: arguments,
        })
    }
}

impl From<ExtensionRequest> for ExtensionCommand {
    fn from(request: ExtensionRequest) -> Self {
        request.command
    }
}

#[derive(Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub(crate) enum Progress<'a> {
    NotStarted,
    Running,
    Ready {
        #[serde(skip_serializing_if = "Option::is_none")]
        registration: Option<&'a Registration>,
    },
    Failed {
        code: &'static str,
    },
}

impl From<&ServiceError> for Progress<'_> {
    fn from(error: &ServiceError) -> Self {
        match error {
            ServiceError::NotInstalled => Self::NotStarted,
            _ => Self::Failed { code: error.code() },
        }
    }
}

impl Progress<'_> {
    fn summary(&self) -> &'static str {
        match self {
            Self::NotStarted => {
                "Beankeeper is ready to set up. Call setup when the owner asks to set up their books or try practice books."
            }
            Self::Running => "Beankeeper is setting up locally. Check setup_status again shortly.",
            Self::Ready { .. } => {
                "Beankeeper is ready in this chat. If you connected other apps, reopen those apps to load their connections."
            }
            Self::Failed {
                code: "SESSION_ENV_UNAVAILABLE",
            } => {
                "Beankeeper could not reach your Linux desktop session. Open Claude from your signed-in desktop and try again."
            }
            Self::Failed {
                code: "SERVICE_NOT_READY",
            } => {
                "Beankeeper hosting is installed but unavailable. If it was stopped deliberately, it needs to be started again before you can use your books."
            }
            Self::Failed { .. } => {
                "Beankeeper could not finish setup. Make sure your computer's credential store is unlocked, then try again. If it still fails, share the error code with Beankeeper support."
            }
        }
    }

    fn report(&self) -> Report<'_> {
        Report {
            progress: self,
            claude_restart_required: false,
            summary: self.summary(),
        }
    }

    pub(crate) fn result(&self) -> Result<CallToolResult, serde_json::Error> {
        let report = self.report();
        let payload = serde_json::to_value(&report)?;
        let mut result = if matches!(self, Self::Failed { .. }) {
            CallToolResult::structured_error(payload)
        } else {
            CallToolResult::structured(payload)
        };
        result.content = vec![ContentBlock::text(report.summary)];
        Ok(result)
    }
}

#[derive(Serialize)]
struct Report<'a> {
    #[serde(flatten)]
    progress: &'a Progress<'a>,
    claude_restart_required: bool,
    summary: &'static str,
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Registration {
    Checked(Vec<agents::ExtensionRegistrationIssue>),
    Unavailable { code: &'static str },
}

impl From<Result<Vec<agents::ExtensionRegistrationIssue>, &'static str>> for Registration {
    fn from(result: Result<Vec<agents::ExtensionRegistrationIssue>, &'static str>) -> Self {
        match result {
            Ok(issues) => Self::Checked(issues),
            Err(code) => Self::Unavailable { code },
        }
    }
}

pub(super) fn run(command: ExtensionCommand, json: bool) -> Result<(), ServiceError> {
    let outcome = match command {
        ExtensionCommand::Setup => setup().map(Some),
        ExtensionCommand::Status => super::connection().map(|_| None),
        ExtensionCommand::Doctor => doctor().map(|()| None),
    };
    let progress = match &outcome {
        Ok(registration) => Progress::Ready {
            registration: registration.as_ref(),
        },
        Err(error) => error.into(),
    };
    let failed = matches!(progress, Progress::Failed { .. });
    let report = progress.report();
    if json {
        println!(
            "{}",
            serde_json::to_string(&report)
                .map_err(|_| ServiceError::Configuration("cannot serialize extension status"))?
        );
    } else if !failed {
        println!("{}", report.summary);
    }
    if failed || command == ExtensionCommand::Setup {
        outcome?;
    }
    Ok(())
}

pub(crate) fn doctor() -> Result<(), ServiceError> {
    verify(&super::connection()?)
}

fn verify(endpoint: &Endpoint) -> Result<(), ServiceError> {
    bridge::probe_service(endpoint, bridge::Probe::ReadOnlyTool)
        .map_err(|error| ServiceError::EndpointMcp(Box::new(error)))?;
    Ok(())
}

pub(crate) fn setup() -> Result<Registration, ServiceError> {
    let paths = Paths::discover()?;
    ensure_installed(&paths)?;
    let config = Config::load(&paths)?.ok_or(ServiceError::NotInstalled)?;
    verify(&super::connection()?)?;
    Ok(agents::connect_from_extension(&config.executable).into())
}

fn ensure_installed(paths: &Paths) -> Result<(), ServiceError> {
    if !needs_install(paths)? {
        return Ok(());
    }
    paths.prepare()?;
    let _lock = lock(&paths.state.join("extension-install.lock"))?;
    if !needs_install(paths)? {
        return Ok(());
    }
    let session = UserSessionEnv::discover()?;
    let executable = match Config::load(paths)? {
        Some(config) => config.executable,
        None => install_executable(&std::env::current_exe()?, paths)?,
    };
    let mut installer = Command::new(executable);
    session.apply(&mut installer);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        installer.creation_flags(CREATE_NO_WINDOW);
    }
    let status = installer
        .args(["service", "install-extension"])
        .env_remove("BEANKEEPER_PASSPHRASE_CMD")
        .env_remove("BEANKEEPER_DB")
        .env_remove("BK_AGENT_TOKEN")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .status()?;
    if !status.success() {
        return Err(ServiceError::ExtensionInstall);
    }
    Ok(())
}

fn needs_install(paths: &Paths) -> Result<bool, ServiceError> {
    Ok(Config::load(paths)?.is_none() || paths.extension_pending().try_exists()?)
}

fn install_executable(source: &Path, paths: &Paths) -> Result<PathBuf, ServiceError> {
    let directory = paths.state.join("claude-extension");
    fs::create_dir_all(&directory)?;
    keychain::restrict_directory(&directory)?;
    let binary = directory.join(if cfg!(windows) { "bk.exe" } else { "bk" });
    let mut pending = tempfile::NamedTempFile::new_in(&directory)?;
    keychain::restrict_file(pending.path())?;
    io::copy(&mut fs::File::open(source)?, pending.as_file_mut())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        pending
            .as_file()
            .set_permissions(fs::Permissions::from_mode(0o700))?;
    }
    pending.as_file().sync_all()?;
    pending.persist(&binary).map_err(|error| error.error)?;
    Ok(binary)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn service_binary_is_independent_of_the_extension_cache()
    -> Result<(), Box<dyn std::error::Error>> {
        let root = tempfile::tempdir()?;
        let paths = Paths {
            home: root.path().join("home"),
            data: root.path().join("data"),
            state: root.path().join("state"),
            logs: root.path().join("logs"),
            definition: root.path().join("service-definition"),
        };
        let source = root.path().join("claude-cache");
        fs::write(&source, b"fixture executable")?;
        let installed = install_executable(&source, &paths)?;
        fs::remove_file(source)?;
        assert!(installed.starts_with(&paths.state));
        assert_eq!(fs::read(&installed)?, b"fixture executable");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(fs::metadata(installed)?.permissions().mode() & 0o777, 0o700);
        }
        Ok(())
    }
}
