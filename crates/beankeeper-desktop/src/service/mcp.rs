//! Local OS-authority lifecycle channel, separate from the ledger's bearer grants.

use std::process::{Command, Stdio};

use rmcp::{
    ErrorData, RoleServer, ServerHandler, ServiceExt,
    model::{
        CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, Implementation,
        ListToolsResult, PaginatedRequestParams, ServerCapabilities, ServerConfig, Tool,
    },
    service::RequestContext,
};
use serde_json::json;

use super::{Config, Paths, ServiceError, Status, UserSessionEnv};

struct Lifecycle;

#[derive(Clone, Copy)]
enum Operation {
    Install,
    Uninstall,
    Start,
    Stop,
}

impl Operation {
    fn cli_argument(self) -> &'static str {
        match self {
            Self::Install => "install",
            Self::Uninstall => "uninstall",
            Self::Start => "start",
            Self::Stop => "stop",
        }
    }
}

const TOOLS: [(&str, &str, Option<Operation>); 5] = [
    (
        "service_install",
        "Install and start per-user Beankeeper hosting. Creates encrypted local books on first use, a private owner recovery kit, and a bookkeeper credential that posts directly. Reinstall preserves books and grants.",
        Some(Operation::Install),
    ),
    (
        "service_uninstall",
        "Remove background hosting and login startup. Preserve books, credentials, recovery kit and MCP configuration.",
        Some(Operation::Uninstall),
    ),
    (
        "service_start",
        "Start installed Beankeeper hosting and enable login startup.",
        Some(Operation::Start),
    ),
    (
        "service_stop",
        "Stop Beankeeper hosting and persistently disable login startup and restart until service_start.",
        Some(Operation::Stop),
    ),
    (
        "service_status",
        "Report authenticated running status, version, the agent's grant and write mode, local MCP endpoint, books path and optional Desktop link. No secrets are returned.",
        None,
    ),
];

impl ServerHandler for Lifecycle {
    fn get_info(&self) -> ServerConfig {
        let mut info = ServerConfig::default();
        info.capabilities = ServerCapabilities::builder().enable_tools().build();
        info.server_info = Implementation::new("beankeeper-service", env!("CARGO_PKG_VERSION"));
        info.instructions = Some("Local per-user service management only. Answer in plain words using the structured status. Desktop is optional. Recovery material stays under the owner's control; never read or show recovery, passphrase or owner-token files.".into());
        info
    }
    fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> impl std::future::Future<Output = Result<ListToolsResult, ErrorData>> + Send + '_ {
        std::future::ready(Ok(ListToolsResult::with_all_items(
            TOOLS
                .into_iter()
                .map(|(name, description, _)| {
                    Tool::new(
                        name,
                        description,
                        json!({"type":"object","properties":{},"additionalProperties":false})
                            .as_object()
                            .cloned()
                            .unwrap_or_default(),
                    )
                })
                .collect(),
        )))
    }
    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        let Some((_, _, operation)) = TOOLS.into_iter().find(|(name, _, _)| *name == request.name)
        else {
            return Err(ErrorData::invalid_params("Unknown service tool", None));
        };
        if request
            .arguments
            .as_ref()
            .is_some_and(|arguments| !arguments.is_empty())
        {
            return Ok(failure(
                "SERVICE_ARGUMENTS",
                "Service lifecycle tools accept no arguments.",
            )
            .into());
        }
        let result = tokio::task::spawn_blocking(move || execute(operation)).await;
        Ok(match result {
            Ok(Ok(status)) => reply(status),
            Ok(Err(error)) => failure(error.code(), &error.to_string()),
            Err(_) => failure(
                "SERVICE_INTERNAL",
                "Service operation could not complete. Run service_status to inspect the result.",
            ),
        }
        .into())
    }
}
fn execute(operation: Option<Operation>) -> Result<Status, ServiceError> {
    if let Some(operation) = operation {
        let mut command = Command::new(std::env::current_exe()?);
        // Preflight here so the session error reaches the agent as a named MCP
        // result. The subprocess still isolates CLI output from the stdio wire.
        if matches!(operation, Operation::Install) || Config::load(&Paths::discover()?)?.is_some() {
            UserSessionEnv::discover()?.apply(&mut command);
        }
        let status = command
            .args(["service", operation.cli_argument()])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()?;
        if !status.success() {
            return Err(ServiceError::Manager(
                "service operation failed; run bk service status --json and inspect the private service log, or run the same lifecycle command in a terminal for diagnostics",
            ));
        }
    }
    super::status()
}
fn reply(status: Status) -> CallToolResult {
    let mut message = status.plain_text();
    if let Some(warning) = &status.warning {
        message.push('\n');
        message.push_str(warning.message());
    }
    let Ok(payload) = serde_json::to_value(status) else {
        let error = ServiceError::Configuration("cannot serialize service status");
        return failure(error.code(), &error.to_string());
    };
    let mut result = CallToolResult::structured(payload);
    result.content = vec![ContentBlock::text(message)];
    result
}
fn failure(code: &str, message: &str) -> CallToolResult {
    let mut result = CallToolResult::structured_error(json!({"code": code, "message": message}));
    result.content = vec![ContentBlock::text(message)];
    result
}

pub(super) fn serve() -> Result<(), ServiceError> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?;
    runtime.block_on(async {
        let service = Lifecycle
            .serve(rmcp::transport::stdio())
            .await
            .map_err(|_| ServiceError::Configuration("cannot initialize service MCP transport"))?;
        service
            .waiting()
            .await
            .map_err(|_| ServiceError::Configuration("service MCP transport failed"))?;
        Ok(())
    })
}
