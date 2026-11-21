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
use serde_json::{Value, json};

use super::ServiceError;

struct Lifecycle;
const TOOLS: [(&str, &str); 5] = [
    (
        "service_install",
        "Install and start per-user Beankeeper hosting. Creates encrypted local books on first use, a private owner recovery kit, and read-only ledger MCP configuration. Reinstall preserves books.",
    ),
    (
        "service_uninstall",
        "Remove background hosting and login startup. Preserve books, credentials, recovery kit and MCP configuration.",
    ),
    (
        "service_start",
        "Start installed Beankeeper hosting and enable login startup.",
    ),
    (
        "service_stop",
        "Stop Beankeeper hosting and persistently disable login startup and restart until service_start.",
    ),
    (
        "service_status",
        "Report authenticated running status, version, local MCP endpoint, books path and optional Desktop link. No secrets are returned.",
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
                .map(|(name, description)| {
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
        let Some(operation) = request
            .name
            .strip_prefix("service_")
            .filter(|_| TOOLS.iter().any(|(name, _)| *name == request.name))
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
        let operation = operation.to_owned();
        let result = tokio::task::spawn_blocking(move || execute(&operation)).await;
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
fn execute(operation: &str) -> Result<Value, ServiceError> {
    if operation != "status" {
        let output = Command::new(std::env::current_exe()?)
            .args(["service", operation])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .output()?;
        if !output.status.success() {
            return Err(ServiceError::Manager(
                "service operation failed; run bk service status --json and inspect the private service log, or run the same lifecycle command in a terminal for diagnostics",
            ));
        }
    }
    serde_json::to_value(super::status()?)
        .map_err(|_| ServiceError::Configuration("cannot serialize service status"))
}
fn reply(payload: Value) -> CallToolResult {
    let message = payload["message"]
        .as_str()
        .unwrap_or("Service operation completed.")
        .to_owned();
    let mut result = CallToolResult::structured(payload);
    result.content = vec![ContentBlock::text(message)];
    result
}
fn failure(code: &str, message: &str) -> CallToolResult {
    let mut result = reply(json!({"code":code,"message":message}));
    result.is_error = Some(true);
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
