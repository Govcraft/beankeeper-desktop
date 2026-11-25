//! Read-only link generation on the authenticated ledger connection.

use std::net::SocketAddr;

use beankeeper_cli::{
    capability::Capability,
    mcp::{HostTool, HostToolEffect},
};
use rmcp::model::{CallToolResult, ContentBlock, Tool, ToolAnnotations};
use serde_json::{Value, json};

use super::{LinkError, ViewAddress, ViewLink};

pub(crate) const NAME: &str = "ui_get_link";

pub(crate) fn definition() -> HostTool {
    HostTool {
        tool: Tool::new(
            NAME,
            "Return an optional Desktop detail URL for the books on this ledger connection. For other books, use their own ledger connection or answer in words without a link. Offer the link after answering in words, when a view helps. Accepts view_id and optional state, including financial_summary's desktop_view.link. The link contains no pairing credential. If the owner wants help opening it, use service_open on the local service connection when available, or suggest bk open.",
            schemars::schema_for!(ViewAddress)
                .to_value()
                .as_object()
                .cloned()
                .unwrap_or_default(),
        )
        .with_annotations(ToolAnnotations::new().read_only(true)),
        capability: Capability::Read,
        effect: HostToolEffect::ReadOnly,
    }
}

pub(crate) fn call(arguments: Value, bound: SocketAddr) -> CallToolResult {
    let result = (|| {
        let address = ViewAddress::try_from(arguments)?;
        let link = ViewLink::try_from((&address, bound))?;
        let text = format!("Optional Desktop detail\n{}", link.url);
        Ok::<_, LinkError>((text, link.payload()?))
    })();
    let (text, mut result) = match result {
        Ok((text, payload)) => (text, CallToolResult::structured(payload)),
        Err(error) => {
            let text = error.to_string();
            let result =
                CallToolResult::structured_error(json!({"code":"VALIDATION","message":text}));
            (text, result)
        }
    };
    result.content = vec![ContentBlock::text(text)];
    result
}
