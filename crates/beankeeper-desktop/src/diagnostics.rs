//! Shared CLI and MCP extension diagnostics, independent of ledger availability.

use crate::{agents::extension_status::Observation, product, updates};
use rmcp::model::{CallToolRequestParams, CallToolResult, ContentBlock, Tool, ToolAnnotations};
use serde::{Deserialize, Serialize};
use std::{fmt, process::ExitCode, str::FromStr};

/// Read-only diagnostic operations available to both shell and chat agents.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Diagnostic {
    /// Inspect the locally installed Claude extension.
    Doctor,
    /// Read cached Desktop release status without network access.
    UpdateStatus,
    /// Check Desktop releases and inspect the locally installed Claude extension.
    UpdateCheck,
}

/// Named failures at the diagnostic tool boundary.
#[derive(Debug, PartialEq, Eq)]
pub enum DiagnosticError {
    /// The requested tool is not a diagnostic operation.
    UnknownTool,
    /// Diagnostics accept no arguments.
    Arguments,
}

impl fmt::Display for DiagnosticError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::UnknownTool => "Unknown diagnostic tool.",
            Self::Arguments => "This tool takes no arguments.",
        })
    }
}

impl std::error::Error for DiagnosticError {}

impl FromStr for Diagnostic {
    type Err = DiagnosticError;
    fn from_str(name: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .find(|operation| operation.as_ref() == name)
            .ok_or(DiagnosticError::UnknownTool)
    }
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct NoArguments {}

#[derive(Serialize)]
struct Report {
    installed_version: &'static str,
    extension: Observation,
    #[serde(skip_serializing_if = "Option::is_none")]
    desktop_updates: Option<DesktopUpdates>,
}

#[derive(Serialize)]
#[serde(untagged)]
enum DesktopUpdates {
    Status(updates::UpdateStatus),
    Unavailable {
        code: UpdateCheckErrorCode,
        message: String,
    },
}

#[derive(Serialize)]
enum UpdateCheckErrorCode {
    #[serde(rename = "DESKTOP_UPDATE_CHECK_UNAVAILABLE")]
    Unavailable,
}

impl From<product::ProductError> for DesktopUpdates {
    fn from(error: product::ProductError) -> Self {
        Self::Unavailable {
            code: UpdateCheckErrorCode::Unavailable,
            message: format!("Could not check Desktop updates: {error}"),
        }
    }
}

impl fmt::Display for DesktopUpdates {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Status(status) => status.fmt(f),
            Self::Unavailable { message, .. } => f.write_str(message),
        }
    }
}

impl AsRef<str> for Diagnostic {
    fn as_ref(&self) -> &str {
        match self {
            Self::Doctor => "doctor",
            Self::UpdateCheck => "update_check",
            Self::UpdateStatus => "update_status",
        }
    }
}

impl fmt::Display for Diagnostic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_ref())
    }
}

impl Diagnostic {
    /// Complete diagnostic catalog shared by CLI and MCP adapters.
    pub const ALL: [Self; 3] = [Self::Doctor, Self::UpdateStatus, Self::UpdateCheck];

    fn description(self) -> &'static str {
        match self {
            Self::Doctor => {
                "Read the installed Claude extension version and compare it with this Desktop version. A stale extension never blocks the books. Ask agents_extension to open an upgrade when the owner requests it."
            }
            Self::UpdateStatus => {
                "Read cached Desktop update status without network access. Mention an available update once in plain words, offer help, and continue the owner's work without blocking it. Use update_check only when a fresh check is requested."
            }
            Self::UpdateCheck => {
                "Check signed Desktop releases and report the same Claude extension status as doctor. Takes no arguments."
            }
        }
    }

    fn report(self) -> Report {
        let desktop_updates = match self {
            Self::Doctor => None,
            Self::UpdateStatus => Some(
                product::state_dir()
                    .and_then(|dir| {
                        Ok(updates::cached_status(
                            &dir.join("updates.json"),
                            product::update_checks(),
                        )?)
                    })
                    .map_or_else(DesktopUpdates::from, DesktopUpdates::Status),
            ),
            Self::UpdateCheck => Some(
                product::state_dir()
                    .and_then(|dir| {
                        Ok(updates::check(
                            &dir.join("updates.json"),
                            product::update_checks(),
                            updates::now(),
                        )?)
                    })
                    .map_or_else(DesktopUpdates::from, DesktopUpdates::Status),
            ),
        };
        Report {
            installed_version: env!("CARGO_PKG_VERSION"),
            extension: Observation::discover(),
            desktop_updates,
        }
    }
}

impl fmt::Display for Report {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(updates) = &self.desktop_updates {
            writeln!(f, "Installed Desktop: {}.", self.installed_version)?;
            writeln!(f, "{updates}")?;
        }
        self.extension.fmt(f)
    }
}

/// Print the same typed diagnostic report returned to an MCP caller.
pub fn run(operation: Diagnostic, json: bool) -> ExitCode {
    let report = operation.report();
    if json {
        match serde_json::to_string(&report) {
            Ok(text) => println!("{text}"),
            Err(error) => {
                eprintln!("DIAGNOSTIC_OUTPUT: {error}");
                return ExitCode::FAILURE;
            }
        }
    } else {
        println!("{report}");
    }
    ExitCode::SUCCESS
}

pub(crate) fn tools() -> Vec<Tool> {
    let schema = schemars::schema_for!(NoArguments).to_value();
    Diagnostic::ALL
        .into_iter()
        .filter_map(|operation| {
            Some(
                Tool::new(
                    operation.to_string(),
                    operation.description(),
                    schema.as_object()?.clone(),
                )
                .with_annotations(
                    ToolAnnotations::new()
                        .read_only(true)
                        .destructive(false)
                        .open_world(operation == Diagnostic::UpdateCheck),
                ),
            )
        })
        .collect()
}

pub(crate) fn call(operation: Diagnostic, request: CallToolRequestParams) -> CallToolResult {
    let Ok(request) = DiagnosticRequest::try_from((operation, request.arguments)) else {
        return CallToolResult::structured_error(
            serde_json::json!({"code":"DIAGNOSTIC_ARGUMENTS", "message":"This tool takes no arguments."}),
        );
    };
    let report = request.report();
    let mut result = CallToolResult::structured(serde_json::json!(report));
    result.content = vec![ContentBlock::text(report.to_string())];
    result
}

struct DiagnosticRequest {
    operation: Diagnostic,
    _arguments: NoArguments,
}

impl TryFrom<(Diagnostic, Option<rmcp::model::JsonObject>)> for DiagnosticRequest {
    type Error = DiagnosticError;
    fn try_from(
        (operation, arguments): (Diagnostic, Option<rmcp::model::JsonObject>),
    ) -> Result<Self, Self::Error> {
        let arguments =
            NoArguments::deserialize(serde_json::Value::Object(arguments.unwrap_or_default()))
                .map_err(|_| DiagnosticError::Arguments)?;
        Ok(Self {
            operation,
            _arguments: arguments,
        })
    }
}

impl DiagnosticRequest {
    fn report(self) -> Report {
        self.operation.report()
    }
}

/// Inform the owner using the authenticated version just installed, even before restart.
pub fn after_update(version: semver::Version) {
    println!("{}", Observation::at_version(version));
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn catalog_names_round_trip_and_match_tools() {
        let tools = tools();
        assert_eq!(tools.len(), Diagnostic::ALL.len());
        for (operation, tool) in Diagnostic::ALL.into_iter().zip(tools) {
            assert_eq!(operation.as_ref().parse(), Ok(operation));
            assert_eq!(operation.to_string(), tool.name);
        }
        assert!("unknown".parse::<Diagnostic>().is_err());
    }

    #[test]
    fn cached_status_is_read_only_and_local() {
        let tool = tools()
            .into_iter()
            .find(|tool| tool.name == "update_status");
        assert!(tool.is_some());
        if let Some(tool) = tool {
            let annotations = tool.annotations.unwrap_or_default();
            assert_eq!(annotations.read_only_hint, Some(true));
            assert_eq!(annotations.open_world_hint, Some(false));
            assert_eq!(annotations.destructive_hint, Some(false));
        }
    }

    #[test]
    fn diagnostic_arguments_are_validated_into_a_request() {
        assert!(DiagnosticRequest::try_from((Diagnostic::Doctor, None)).is_ok());
        let invalid = serde_json::Map::from_iter([("unexpected".into(), serde_json::json!(true))]);
        assert!(DiagnosticRequest::try_from((Diagnostic::UpdateCheck, Some(invalid))).is_err());
    }
}
