//! Local CLI and MCP handoff for the signed Claude Desktop extension.

use std::{
    env, fmt,
    path::{Path, PathBuf},
    process::ExitCode,
};

use clap::Args;
use rmcp::model::{CallToolRequestParams, CallToolResult, ContentBlock, Tool, ToolAnnotations};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::json;

use super::{
    Agent, AgentError, Failure, Locations,
    config::{Change, KnownExecutables},
    mutation_lock,
};
use crate::{
    output::{Envelope, Metadata},
    product, service,
    updates::{
        UpdateError,
        extension::{self, PreparedBundle},
    },
};

pub(crate) const TOOL: &str = "agents_extension";
pub(crate) const ARGUMENTS_ERROR: &str = "AGENT_EXTENSION_ARGUMENTS";

#[derive(Args, Clone, Copy, Debug, Default, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub(super) struct Options {
    /// Print the verified path without opening it or editing Claude's configuration.
    #[arg(long)]
    path_only: bool,
}

impl Options {
    fn apply(self, bundle: &PreparedBundle) -> Result<Handoff, ExtensionError> {
        if self.path_only {
            return Ok(Handoff::Prepared);
        }
        let locations = Locations::discover()
            .filter(|locations| locations.home.is_absolute())
            .ok_or(AgentError::Home)?;
        let adapter = Agent::ClaudeDesktop
            .adapter(&locations)?
            .ok_or(AgentError::Home)?;
        let current = env::current_exe().map_err(AgentError::Binary)?;
        let managed = service::managed_executable().ok().flatten();
        let known = KnownExecutables {
            current: current
                .to_str()
                .filter(|_| current.is_absolute())
                .ok_or_else(|| AgentError::InvalidBinary(current.clone()))?,
            managed_service: managed.as_deref().and_then(Path::to_str),
        };
        let _lock = mutation_lock()?;
        handoff(&adapter, known, || {
            bundle
                .open()
                .map_err(|_| ExtensionError::Launch(bundle.path().into()))
        })
    }
}

#[derive(Clone, Copy)]
enum Handoff {
    Prepared,
    Opened { removed_stdio: bool },
}

impl Handoff {
    fn message(self) -> &'static str {
        match self {
            Self::Prepared => "The signed Beankeeper extension is verified and ready to open.",
            Self::Opened {
                removed_stdio: false,
            } => {
                "Opened the verified Beankeeper extension. Select Install in Claude Desktop to finish."
            }
            Self::Opened {
                removed_stdio: true,
            } => {
                "Opened the verified Beankeeper extension and removed the duplicate local Beankeeper connection. Select Install in Claude Desktop to finish."
            }
        }
    }
}

fn handoff(
    adapter: &super::adapters::Adapter,
    known: KnownExecutables<'_>,
    open: impl FnOnce() -> Result<(), ExtensionError>,
) -> Result<Handoff, ExtensionError> {
    let removal = adapter.stage_config(known, Change::Disconnect)?;
    open()?;
    if let Some(removal) = &removal {
        removal.commit()?;
    }
    Ok(Handoff::Opened {
        removed_stdio: removal.is_some(),
    })
}

#[derive(Serialize)]
struct Report {
    version: String,
    target: &'static str,
    path: PathBuf,
    opened: bool,
    removed_stdio: bool,
    message: &'static str,
}

fn execute(options: Options) -> Result<Report, ExtensionError> {
    let state = product::state_dir().map_err(|_| ExtensionError::State)?;
    let bundle = extension::prepare(&state)?;
    let handoff = options.apply(&bundle)?;
    Ok(Report {
        version: bundle.version(),
        target: bundle.target(),
        path: bundle.path().into(),
        opened: matches!(handoff, Handoff::Opened { .. }),
        removed_stdio: matches!(
            handoff,
            Handoff::Opened {
                removed_stdio: true
            }
        ),
        message: handoff.message(),
    })
}

#[derive(Debug)]
enum ExtensionError {
    Arguments,
    State,
    Release(UpdateError),
    Config(AgentError),
    Launch(PathBuf),
}

impl ExtensionError {
    fn code(&self) -> &'static str {
        match self {
            Self::Arguments => ARGUMENTS_ERROR,
            Self::State | Self::Release(UpdateError::Io(_)) => "AGENT_EXTENSION_IO",
            Self::Release(UpdateError::Network | UpdateError::Http(_)) => {
                "AGENT_EXTENSION_DOWNLOAD"
            }
            Self::Release(UpdateError::Signature) => "AGENT_EXTENSION_SIGNATURE",
            Self::Release(UpdateError::Invalid(_) | UpdateError::Json(_)) => {
                "AGENT_EXTENSION_INVALID_RELEASE"
            }
            Self::Config(error) => error.code(),
            Self::Launch(_) => "AGENT_EXTENSION_LAUNCH",
        }
    }
}

impl fmt::Display for ExtensionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Arguments => f.write_str("Use path_only with a boolean value, or omit it."),
            Self::State => {
                f.write_str("Beankeeper could not locate the local Desktop state folder.")
            }
            Self::Release(error) => error.fmt(f),
            Self::Config(error) => error.fmt(f),
            Self::Launch(path) => write!(
                f,
                "The operating system could not open the verified extension. Open {} from Claude Desktop.",
                path.display()
            ),
        }
    }
}

impl std::error::Error for ExtensionError {}

impl From<UpdateError> for ExtensionError {
    fn from(error: UpdateError) -> Self {
        Self::Release(error)
    }
}

impl From<AgentError> for ExtensionError {
    fn from(error: AgentError) -> Self {
        Self::Config(error)
    }
}

impl From<ExtensionError> for Failure {
    fn from(error: ExtensionError) -> Self {
        Self::new(error.code(), error.to_string())
    }
}

pub(super) fn run(options: Options, json: bool, quiet: bool) -> ExitCode {
    let result = execute(options).map_err(Failure::from);
    let ok = result.is_ok();
    if json {
        let (data, error) = match result {
            Ok(data) => (Some(data), None),
            Err(error) => (None, Some(error)),
        };
        let envelope = Envelope {
            ok,
            meta: Metadata::new("agents.extension"),
            data,
            error,
        };
        match serde_json::to_string(&envelope) {
            Ok(line) => println!("{line}"),
            Err(error) => {
                eprintln!("AGENT_OUTPUT: {error}");
                return ExitCode::FAILURE;
            }
        }
    } else {
        match result {
            Ok(report) if !quiet && report.opened => println!("{}", report.message),
            Ok(report) if !quiet => println!("{}", report.path.display()),
            Ok(_) => {}
            Err(error) => eprintln!("{}: {}", error.code, error.message),
        }
    }
    if ok {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

pub(crate) fn mcp_tool() -> Option<Tool> {
    let schema = schemars::schema_for!(Options).to_value();
    Some(
        Tool::new(
            TOOL,
            "When the owner asks to install or upgrade the Beankeeper extension for Claude Desktop, verify the newest signed release and open the local installer. The owner finishes in Claude Desktop. Set path_only to true to prepare the verified local file without opening it or editing Claude's settings.",
            schema.as_object()?.clone(),
        )
        .with_annotations(ToolAnnotations::new().open_world(true)),
    )
}

pub(crate) fn mcp_call(request: CallToolRequestParams) -> CallToolResult {
    let arguments = serde_json::Value::Object(request.arguments.unwrap_or_default());
    let result = serde_json::from_value::<Options>(arguments)
        .map_err(|_| ExtensionError::Arguments)
        .and_then(execute)
        .map_err(Failure::from);
    let (mut response, message) = match result {
        Ok(report) => (
            CallToolResult::structured(json!(report)),
            report.message.into(),
        ),
        Err(error) => (
            CallToolResult::structured_error(json!(error)),
            error.message,
        ),
    };
    response.content = vec![ContentBlock::text(message)];
    response
}

#[cfg(test)]
mod tests {
    use std::fs;

    use serde_json::Value;

    use super::*;
    use crate::agents::adapters::{Adapter, ConfigFormat};

    const ORIGINAL: &str = r#"{
        "developer": true,
        "mcpServers": {
            "beankeeper": {"command":"/bin/bk", "args":["mcp","bridge","--service"]},
            "beankeeper-service": {"command":"/bin/bk", "args":["service","mcp"]},
            "other": {"command":"other", "args":[]}
        }
    }"#;

    fn fixture(directory: &Path, configuration: &str) -> std::io::Result<Adapter> {
        let config = directory.join("claude_desktop_config.json");
        fs::write(&config, configuration)?;
        Ok(Adapter {
            config,
            format: ConfigFormat::Json,
            skill: None,
            app_dir: directory.into(),
            program: None,
            extension_registry: None,
        })
    }

    #[test]
    fn opening_replaces_only_the_managed_stdio_entries_and_keeps_a_backup()
    -> Result<(), Box<dyn std::error::Error>> {
        let home = tempfile::tempdir()?;
        let adapter = fixture(home.path(), ORIGINAL)?;
        let known = KnownExecutables {
            current: "/extension/server/bk",
            managed_service: Some("/bin/bk"),
        };
        let result = handoff(&adapter, known, || {
            assert_eq!(
                fs::read_to_string(&adapter.config).as_deref().ok(),
                Some(ORIGINAL)
            );
            Ok(())
        })?;
        assert!(matches!(
            result,
            Handoff::Opened {
                removed_stdio: true
            }
        ));
        let backup = home
            .path()
            .join("claude_desktop_config.json.beankeeper-backup");
        assert_eq!(fs::read_to_string(&backup)?, ORIGINAL);
        let edited: Value = serde_json::from_slice(&fs::read(&adapter.config)?)?;
        assert_eq!(
            edited,
            json!({"developer": true, "mcpServers": {"other": {"command":"other", "args":[]}}})
        );
        let result = handoff(&adapter, known, || Ok(()))?;
        assert!(matches!(
            result,
            Handoff::Opened {
                removed_stdio: false
            }
        ));
        assert_eq!(fs::read_to_string(backup)?, ORIGINAL);
        Ok(())
    }

    #[test]
    fn launcher_failure_leaves_the_existing_connection_in_place()
    -> Result<(), Box<dyn std::error::Error>> {
        let home = tempfile::tempdir()?;
        let adapter = fixture(home.path(), ORIGINAL)?;
        let outcome = handoff(&adapter, "/bin/bk".into(), || {
            Err(ExtensionError::Launch(home.path().join("verified.mcpb")))
        });
        assert!(matches!(outcome, Err(ExtensionError::Launch(_))));
        assert_eq!(fs::read_to_string(&adapter.config)?, ORIGINAL);
        assert!(
            !home
                .path()
                .join("claude_desktop_config.json.beankeeper-backup")
                .exists()
        );
        Ok(())
    }

    #[test]
    fn custom_config_conflicts_are_preserved_before_launch()
    -> Result<(), Box<dyn std::error::Error>> {
        let home = tempfile::tempdir()?;
        let custom = ORIGINAL.replace(
            r#""args":["service","mcp"]"#,
            r#""args":["service","mcp"], "env":{"CUSTOM":"yes"}"#,
        );
        let adapter = fixture(home.path(), &custom)?;
        let outcome = handoff(&adapter, "/bin/bk".into(), || {
            panic!("conflicting settings must be detected before opening")
        });
        assert!(matches!(
            outcome,
            Err(ExtensionError::Config(
                AgentError::RegistrationConflict { .. }
            ))
        ));
        assert_eq!(fs::read_to_string(&adapter.config)?, custom);
        Ok(())
    }
}
