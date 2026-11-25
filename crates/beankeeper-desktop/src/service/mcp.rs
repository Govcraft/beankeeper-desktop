//! Local OS-authority lifecycle channel, separate from the ledger's bearer grants.

use rmcp::{
    ErrorData, RoleServer, ServerHandler, ServiceExt,
    model::{
        CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, Implementation,
        ListToolsResult, PaginatedRequestParams, ServerCapabilities, ServerConfig,
    },
    service::RequestContext,
};
use std::path::Path;

use serde::Deserialize;
use serde_json::json;

use super::{ServiceError, ServiceOperation, ServiceReply, execute};
use crate::setup::AgentAccess;

struct Lifecycle {
    instructions: &'static str,
}

impl ServerHandler for Lifecycle {
    fn get_info(&self) -> ServerConfig {
        let mut info = ServerConfig::default();
        info.capabilities = ServerCapabilities::builder().enable_tools().build();
        info.server_info = Implementation::new("beankeeper-service", env!("CARGO_PKG_VERSION"));
        info.instructions = Some(format!(
            "This connection manages the local per-user service. {}",
            self.instructions
        ));
        info
    }
    fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> impl std::future::Future<Output = Result<ListToolsResult, ErrorData>> + Send + '_ {
        std::future::ready(Ok(ListToolsResult::with_all_items(
            ServiceOperation::all()
                .into_iter()
                .filter_map(ServiceOperation::tool)
                .collect(),
        )))
    }
    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        let operation = match ServiceOperation::try_from(request) {
            Ok(operation) => operation,
            Err(error) => return Ok(failure(&error).into()),
        };
        let result = tokio::task::spawn_blocking(move || execute(operation, None, None)).await;
        Ok(match result {
            Ok(Ok(status)) => reply(status),
            Ok(Err(error)) => failure(&error),
            Err(_) => failure(&ServiceError::Worker),
        }
        .into())
    }
}

pub(crate) fn local_call(params: &serde_json::Value, books: &Path) -> CallToolResult {
    let Ok(request) = CallToolRequestParams::deserialize(params) else {
        return failure(&ServiceError::Arguments);
    };
    match ServiceOperation::try_from(request) {
        Ok(operation @ (ServiceOperation::AccessSet(_) | ServiceOperation::Open)) => {
            match execute(operation, None, Some(books)) {
                Ok(status) => reply(status),
                Err(error) => failure(&error),
            }
        }
        Ok(_) => failure(&ServiceError::UnknownTool),
        Err(error) => failure(&error),
    }
}

fn reply(reply: ServiceReply) -> CallToolResult {
    let ServiceReply::Status(status) = reply else {
        let message = ServiceReply::OPENED_MESSAGE;
        let mut result = CallToolResult::structured(json!({"opened": true, "message": message}));
        result.content = vec![ContentBlock::text(message)];
        return result;
    };
    let mut message = status.plain_text();
    if let Some(warning) = &status.warning {
        message.push('\n');
        message.push_str(warning.message());
    }
    let Ok(payload) = serde_json::to_value(status) else {
        let error = ServiceError::Configuration("cannot serialize service status");
        return failure(&error);
    };
    let mut result = CallToolResult::structured(payload);
    result.content = vec![ContentBlock::text(message)];
    result
}
fn failure(error: &ServiceError) -> CallToolResult {
    let message = error.to_string();
    let mut result =
        CallToolResult::structured_error(json!({"code": error.code(), "message": message}));
    result.content = vec![ContentBlock::text(message)];
    result
}

pub(super) fn serve() -> Result<(), ServiceError> {
    let instructions = AgentAccess::mcp_instructions()
        .map_err(|_| ServiceError::Configuration("invalid embedded agent skill guidance"))?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?;
    runtime.block_on(async {
        let service = Lifecycle { instructions }
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
