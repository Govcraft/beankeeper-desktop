//! Shared CLI and MCP extension diagnostics, independent of ledger availability.

use crate::{agents::extension_status::Observation, product, updates};
use rmcp::model::{CallToolRequestParams, CallToolResult, ContentBlock, Tool, ToolAnnotations};
use serde::{Deserialize, Serialize};
use std::{fmt, process::ExitCode, str::FromStr};

/// Read-only diagnostic operations available to both shell and chat agents.
#[derive(Clone, Copy)]
pub enum Diagnostic {
    /// Inspect the locally installed Claude extension.
    Doctor,
    /// Check Desktop releases and inspect the locally installed Claude extension.
    UpdateCheck,
}

/// Named failures at the diagnostic tool boundary.
#[derive(Debug)]
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
        match name {
            "doctor" => Ok(Self::Doctor),
            "update_check" => Ok(Self::UpdateCheck),
            _ => Err(DiagnosticError::UnknownTool),
        }
    }
}

#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct NoArguments {}

#[derive(Serialize)]
struct Report {
    extension: Observation,
    #[serde(skip_serializing_if = "Option::is_none")]
    desktop_updates: Option<String>,
}

impl Diagnostic {
    fn report(self) -> Report {
        let desktop_updates = matches!(self, Self::UpdateCheck).then(|| {
            product::state_dir()
                .and_then(|dir| {
                    Ok(updates::check(
                        &dir.join("updates.json"),
                        product::update_checks(),
                        updates::now(),
                    )?)
                })
                .unwrap_or_else(|error| format!("Could not check Desktop updates: {error}"))
        });
        Report {
            extension: Observation::default(),
            desktop_updates,
        }
    }
}

impl fmt::Display for Report {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(updates) = &self.desktop_updates {
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
    [
        ("doctor", "Read the installed Claude extension version and compare it with this Desktop version. A stale extension never blocks the books. Ask agents_extension to open an upgrade when the owner requests it."),
        ("update_check", "Check signed Desktop releases and report the same Claude extension status as doctor. Takes no arguments."),
    ].into_iter().filter_map(|(name, description)| Some(
        Tool::new(name, description, schema.as_object()?.clone())
            .with_annotations(ToolAnnotations::new().read_only(true).destructive(false))
    )).collect()
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

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn diagnostic_arguments_are_validated_into_a_request() {
        assert!(DiagnosticRequest::try_from((Diagnostic::Doctor, None)).is_ok());
        let invalid = serde_json::Map::from_iter([("unexpected".into(), serde_json::json!(true))]);
        assert!(DiagnosticRequest::try_from((Diagnostic::UpdateCheck, Some(invalid))).is_err());
    }
}

/// Inform the owner using the authenticated version just installed, even before restart.
pub fn after_update(version: semver::Version) {
    println!("{}", Observation::at_version(version));
}
