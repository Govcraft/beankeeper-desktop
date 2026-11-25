//! Typed lifecycle requests and their local MCP exposure.

use std::{path::Path, str::FromStr};

use clap::Args;
use rmcp::model::{CallToolRequestParams, Tool, ToolAnnotations};
use schemars::JsonSchema;
use serde::{Deserialize, de::DeserializeOwned};

use super::{ServiceCommand, ServiceError};
use crate::setup::{AccessOptions, ClientRegistration, ManagedAccess, ServiceSecretStorage};

pub(crate) const ACCESS_TOOL: &str = "access_set";
pub(crate) const OPEN_TOOL: &str = "service_open";

/// Options shared by the CLI and the `service_install` tool.
#[derive(Args, Clone, Copy, Debug, Default, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct InstallOptions {
    /// Use a private passphrase file instead of the native secret store for new books.
    #[arg(long)]
    file_store: bool,
    /// Leave all MCP client configurations unchanged.
    #[arg(long)]
    no_agents: bool,
}

impl InstallOptions {
    pub(super) fn storage(self) -> ServiceSecretStorage {
        if self.file_store {
            ServiceSecretStorage::PrivateFile
        } else {
            ServiceSecretStorage::default()
        }
    }

    pub(super) fn registration(self, previous_executable: &Path) -> ClientRegistration<'_> {
        if self.no_agents {
            ClientRegistration::Skip
        } else {
            ClientRegistration::Managed {
                previous_executable,
            }
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub(super) enum ServiceOperation {
    Install(InstallOptions),
    Uninstall,
    Start,
    Stop,
    Status,
    Open,
    AccessSet(ManagedAccess),
    InstallExtension,
}

#[derive(Clone, Copy)]
enum CliOnlyReason {
    // The bundle runs its private executable under the OS user's authority. It
    // owns registration and requires the native secret store on first launch.
    BundleFirstLaunch,
}

enum Exposure {
    Mcp {
        name: &'static str,
        description: &'static str,
    },
    CliOnly(CliOnlyReason),
}

impl ServiceOperation {
    pub(super) fn all() -> [Self; 8] {
        [
            Self::Install(InstallOptions::default()),
            Self::Uninstall,
            Self::Start,
            Self::Stop,
            Self::Status,
            Self::Open,
            Self::AccessSet(ManagedAccess::default()),
            Self::InstallExtension,
        ]
    }

    fn exposure(self) -> Exposure {
        match self {
            Self::Install(_) => Exposure::Mcp {
                name: "service_install",
                description: "Install and start per-user Beankeeper hosting. Creates encrypted local books on first use, a private owner recovery kit, and a bookkeeper credential that posts directly. Reinstall preserves books and explicit access choices.",
            },
            Self::Uninstall => Exposure::Mcp {
                name: "service_uninstall",
                description: "Remove background hosting and login startup. Preserve books, credentials, recovery kit and MCP configuration.",
            },
            Self::Start => Exposure::Mcp {
                name: "service_start",
                description: "Start installed Beankeeper hosting and enable login startup.",
            },
            Self::Stop => Exposure::Mcp {
                name: "service_stop",
                description: "Stop Beankeeper hosting and persistently disable login startup and restart until service_start.",
            },
            Self::Status => Exposure::Mcp {
                name: "service_status",
                description: "Report authenticated running status, version, the agent's grant and write mode, local MCP endpoint, books path and optional Desktop link. At the owner's request, use service_open to help pair their browser.",
            },
            Self::Open => Exposure::Mcp {
                name: OPEN_TOOL,
                description: "At the owner's request, open Beankeeper Desktop in their default browser. If prompted, select Pair this browser. Desktop restores the view the owner just tried to open.",
            },
            Self::AccessSet(_) => Exposure::Mcp {
                name: ACCESS_TOOL,
                description: "Change managed agent access at the owner's request. Use read-only to turn off posting, or bookkeeper with direct or propose-only writes. Tell the owner the resulting access in plain words. On the ledger connection, correct your own work in chat with audit_undo by receipt ID or txn_reverse by transaction ID when listed. In propose-only mode both return pending proposals for sign-off. Never make offsetting entries by hand or send the owner to a screen to undo, tag, rename, or enter data. Desktop links are for viewing only.",
            },
            Self::InstallExtension => Exposure::CliOnly(CliOnlyReason::BundleFirstLaunch),
        }
    }

    pub(super) fn tool(self) -> Option<Tool> {
        match self.exposure() {
            Exposure::Mcp { name, description } => {
                let schema = match self {
                    Self::Install(_) => schemars::schema_for!(InstallOptions),
                    Self::AccessSet(_) => schemars::schema_for!(AccessOptions),
                    _ => schemars::schema_for!(NoArguments),
                };
                let tool = Tool::new(name, description, schema.to_value().as_object()?.clone());
                Some(if matches!(self, Self::Open) {
                    tool.with_annotations(
                        ToolAnnotations::new().destructive(false).open_world(false),
                    )
                } else {
                    tool
                })
            }
            Exposure::CliOnly(CliOnlyReason::BundleFirstLaunch) => None,
        }
    }
}

impl TryFrom<ServiceCommand> for ServiceOperation {
    type Error = ServiceError;

    fn try_from(command: ServiceCommand) -> Result<Self, Self::Error> {
        Ok(match command {
            ServiceCommand::Install(options) => Self::Install(options),
            ServiceCommand::Uninstall => Self::Uninstall,
            ServiceCommand::Start => Self::Start,
            ServiceCommand::Stop => Self::Stop,
            ServiceCommand::Status { .. } => Self::Status,
            ServiceCommand::Open => Self::Open,
            ServiceCommand::AccessSet { access, .. } => ManagedAccess::try_from(access)
                .map(Self::AccessSet)
                .map_err(|_| ServiceError::Arguments)?,
            ServiceCommand::InstallExtension => Self::InstallExtension,
            ServiceCommand::Mcp | ServiceCommand::Run => {
                return Err(ServiceError::Configuration(
                    "service entry point is not a lifecycle operation",
                ));
            }
        })
    }
}

impl FromStr for ServiceOperation {
    type Err = ServiceError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::all()
            .into_iter()
            .find(|operation| matches!(operation.exposure(), Exposure::Mcp { name, .. } if value == name))
            .ok_or(ServiceError::UnknownTool)
    }
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct NoArguments {}

impl TryFrom<CallToolRequestParams> for ServiceOperation {
    type Error = ServiceError;

    fn try_from(request: CallToolRequestParams) -> Result<Self, Self::Error> {
        let operation = request.name.parse()?;
        let arguments = serde_json::Value::Object(request.arguments.unwrap_or_default());
        match operation {
            Self::Install(_) => decode(arguments).map(Self::Install),
            Self::AccessSet(_) => ManagedAccess::try_from(decode::<AccessOptions>(arguments)?)
                .map(Self::AccessSet)
                .map_err(|_| ServiceError::Arguments),
            _ => {
                decode::<NoArguments>(arguments)?;
                Ok(operation)
            }
        }
    }
}

pub(crate) fn access_tool() -> Option<Tool> {
    ServiceOperation::AccessSet(ManagedAccess::default()).tool()
}

pub(crate) fn open_tool() -> Option<Tool> {
    ServiceOperation::Open.tool()
}

fn decode<T: DeserializeOwned>(arguments: serde_json::Value) -> Result<T, ServiceError> {
    serde_json::from_value(arguments).map_err(|_| ServiceError::Arguments)
}

#[cfg(test)]
mod tests {
    use std::mem::discriminant;

    use clap::FromArgMatches;

    use super::*;

    #[test]
    fn lifecycle_operations_have_cli_and_mcp_paths_or_a_typed_reason()
    -> Result<(), Box<dyn std::error::Error>> {
        // Discover actual clap commands so adding a CLI operation without an MCP
        // entry or an explicit exposure decision fails this parity check.
        let command = crate::cli::command();
        let service = command
            .find_subcommand("service")
            .ok_or("missing service CLI")?;
        let mut operations = ServiceOperation::all().to_vec();
        for subcommand in service.get_subcommands() {
            let name = subcommand.get_name();
            let mut arguments = vec!["bk", "service", name];
            if name == "access-set" {
                arguments.extend(["--preset", "read-only"]);
            }
            let matches = command.clone().try_get_matches_from(arguments)?;
            let options = super::super::ServiceOptions::from_arg_matches(
                matches
                    .subcommand_matches("service")
                    .ok_or("missing service matches")?,
            )?;
            if matches!(options.command, ServiceCommand::Mcp | ServiceCommand::Run) {
                continue;
            }
            let operation = ServiceOperation::try_from(options.command)?;
            let index = operations
                .iter()
                .position(|planned| discriminant(planned) == discriminant(&operation))
                .ok_or("CLI operation missing from lifecycle catalog")?;
            operations.remove(index);
            match operation.exposure() {
                Exposure::Mcp { name, .. } => {
                    assert_eq!(
                        discriminant(&name.parse::<ServiceOperation>()?),
                        discriminant(&operation)
                    );
                    assert_eq!(operation.tool().ok_or("missing tool schema")?.name, name);
                }
                Exposure::CliOnly(CliOnlyReason::BundleFirstLaunch) => {
                    assert!(matches!(operation, ServiceOperation::InstallExtension));
                    assert!(operation.tool().is_none());
                }
            }
        }
        assert!(
            operations.is_empty(),
            "Operations without CLI paths: {operations:?}"
        );
        Ok(())
    }
}
