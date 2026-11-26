use std::{future, net::SocketAddr};

use beankeeper_cli::{
    cli::Cli,
    error::CliError,
    identity::VerifiedAgent,
    mcp::{
        HostTool, HostToolHandler, HttpLedgerTargets, StreamableHttpServerConfig,
        http_router_with_ledger_targets,
    },
};
use rmcp::{
    ErrorData,
    model::{CallToolRequestParams, CallToolResult},
};

use crate::instance::ListenerAddress;

#[derive(Clone, Copy)]
pub(super) struct HttpBind(SocketAddr);

impl Default for HttpBind {
    fn default() -> Self {
        Self(ListenerAddress::default().into())
    }
}

impl TryFrom<(Option<SocketAddr>, bool)> for HttpBind {
    type Error = CliError;

    fn try_from((bind, allow_remote): (Option<SocketAddr>, bool)) -> Result<Self, CliError> {
        let bind = bind.map(Self).unwrap_or_default();
        if !bind.0.ip().is_loopback() && !allow_remote {
            return Err(CliError::Usage(
                "non-loopback HTTP binds require --allow-remote (plaintext HTTP, no TLS)".into(),
            ));
        }
        Ok(bind)
    }
}

impl HttpBind {
    // The upstream standalone runner accepts a switchable registry only.
    // Use its router factory to preserve replay for a single prepared ledger.
    pub(super) fn serve(
        self,
        cli: &Cli,
        targets: HttpLedgerTargets,
        read_only: bool,
    ) -> Result<(), CliError> {
        let mut config = StreamableHttpServerConfig::default().enforce_origin_validation();
        if self.0.ip().is_unspecified() {
            // A remote wildcard bind has no fixed Host.
            config = config.disable_allowed_hosts();
        } else {
            config.allowed_hosts.push(self.0.ip().to_string());
        }
        let cancellation = config.cancellation_token.clone();
        let app = http_router_with_ledger_targets(cli, targets, read_only, config, NoHostTools)?;
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()?
            .block_on(async move {
                let listener = tokio::net::TcpListener::bind(self.0).await?;
                let bound = listener.local_addr()?;
                eprintln!("MCP Streamable HTTP listening on http://{bound}/mcp");
                axum::serve(listener, app)
                    .with_graceful_shutdown(async move {
                        let _ = tokio::signal::ctrl_c().await;
                        cancellation.cancel();
                    })
                    .await
            })?;
        Ok(())
    }
}

struct NoHostTools;

impl HostToolHandler for NoHostTools {
    fn tools(&self) -> Vec<HostTool> {
        Vec::new()
    }

    fn call_tool(
        &self,
        _request: CallToolRequestParams,
        _identity: VerifiedAgent,
    ) -> impl Future<Output = Result<CallToolResult, ErrorData>> + Send {
        future::ready(Err(ErrorData::invalid_params("unknown host tool", None)))
    }
}
