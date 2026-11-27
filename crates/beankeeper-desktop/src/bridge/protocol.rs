use std::borrow::Cow;

use base64::{Engine, prelude::BASE64_STANDARD};
use beankeeper_cli::mcp::{LEDGER_TARGET_META_KEY, NamedLedgerTarget};
use reqwest::{
    blocking::RequestBuilder,
    header::{HeaderValue, InvalidHeaderValue},
};
use rmcp::{
    ErrorData,
    model::{ProtocolVersion, RequestMetaObject},
    transport::common::http_header::{
        BASE64_HEADER_PREFIX, BASE64_HEADER_SUFFIX, HEADER_MCP_METHOD, HEADER_MCP_NAME,
        HEADER_MCP_PROTOCOL_VERSION,
    },
};
use serde::Deserialize;
use serde_json::Value;

use crate::agent_skill::SkillMethod;

const INLINE_VERSION: &str = "io.modelcontextprotocol/protocolVersion";
const INLINE_CAPABILITIES: &str = "io.modelcontextprotocol/clientCapabilities";

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum MethodKind {
    Discover,
    Initialize,
    Initialized,
    Ping,
    ToolsCall,
    ToolsList,
    PromptsGet,
    ResourceSubscription,
    Task,
    Skill(SkillMethod),
    Other,
}

#[derive(Clone, Copy)]
struct RequestMethod<'a> {
    wire: Option<&'a str>,
    kind: MethodKind,
}

impl<'a> From<Option<&'a str>> for RequestMethod<'a> {
    fn from(wire: Option<&'a str>) -> Self {
        let kind = match wire {
            Some("server/discover") => MethodKind::Discover,
            Some("initialize") => MethodKind::Initialize,
            Some("notifications/initialized") => MethodKind::Initialized,
            Some("ping") => MethodKind::Ping,
            Some("tools/call") => MethodKind::ToolsCall,
            Some("tools/list") => MethodKind::ToolsList,
            Some("prompts/get") => MethodKind::PromptsGet,
            Some("resources/subscribe" | "resources/unsubscribe") => {
                MethodKind::ResourceSubscription
            }
            Some("tasks/get" | "tasks/update" | "tasks/cancel") => MethodKind::Task,
            Some(method) => method.parse().map_or(MethodKind::Other, MethodKind::Skill),
            None => MethodKind::Other,
        };
        Self { wire, kind }
    }
}

impl MethodKind {
    fn name_parameter(self) -> Option<&'static str> {
        match self {
            Self::ToolsCall | Self::PromptsGet => Some("name"),
            Self::ResourceSubscription => Some("uri"),
            Self::Task => Some("taskId"),
            Self::Discover
            | Self::Initialize
            | Self::Initialized
            | Self::Ping
            | Self::ToolsList
            | Self::Skill(_)
            | Self::Other => None,
        }
    }
}

/// An MCP-Name value encoded for lossless forwarding over HTTP.
struct McpNameHeader<'a>(Cow<'a, str>);

impl<'a> From<&'a str> for McpNameHeader<'a> {
    fn from(name: &'a str) -> Self {
        let plain = name.trim() == name
            && name.bytes().all(|byte| (b' '..=b'~').contains(&byte))
            && !(name.starts_with(BASE64_HEADER_PREFIX) && name.ends_with(BASE64_HEADER_SUFFIX));
        if plain {
            Self(Cow::Borrowed(name))
        } else {
            Self(Cow::Owned(format!(
                "{BASE64_HEADER_PREFIX}{}{BASE64_HEADER_SUFFIX}",
                BASE64_STANDARD.encode(name)
            )))
        }
    }
}

impl TryFrom<McpNameHeader<'_>> for HeaderValue {
    type Error = InvalidHeaderValue;

    fn try_from(name: McpNameHeader<'_>) -> Result<Self, Self::Error> {
        Self::from_str(&name.0)
    }
}

// The legacy handshake lives in the connection. Modern requests carry their own context.
pub(super) struct InlineProtocol<'a> {
    version: ProtocolVersion,
    method: RequestMethod<'a>,
    name: Option<HeaderValue>,
    ledger_target: Option<NamedLedgerTarget>,
    file_imports: bool,
}

impl<'a> InlineProtocol<'a> {
    fn from_params(method: RequestMethod<'a>, params: &Value) -> Result<Option<Self>, ErrorData> {
        let invalid = || ErrorData::invalid_params("Invalid MCP request metadata.", None);
        let meta = match RequestMetaObject::deserialize(&params["_meta"]) {
            Ok(meta) => meta,
            Err(_) if method.kind != MethodKind::Discover => return Ok(None),
            Err(_) => return Err(invalid()),
        };
        let version = meta
            .get(INLINE_VERSION)
            .map(ProtocolVersion::deserialize)
            .transpose()
            .map_err(|_| invalid())?;
        if method.kind != MethodKind::Discover
            && version.as_ref().is_none_or(|version| {
                version < &ProtocolVersion::NO_INITIALIZE && !meta.contains_key(INLINE_CAPABILITIES)
            })
        {
            return Ok(None);
        }
        let version = version.ok_or_else(invalid)?;
        if meta.client_capabilities().is_none() {
            return Err(invalid());
        }
        if !ProtocolVersion::KNOWN_VERSIONS.contains(&version) {
            return Err(ErrorData::unsupported_protocol_version(
                version,
                ProtocolVersion::KNOWN_VERSIONS,
            ));
        }
        let ledger_target = meta
            .get(LEDGER_TARGET_META_KEY)
            .map(NamedLedgerTarget::deserialize)
            .transpose()
            .map_err(|_| ErrorData::invalid_params("Invalid Beankeeper ledger target.", None))?;
        let name = method
            .kind
            .name_parameter()
            .and_then(|key| params[key].as_str())
            .map(McpNameHeader::from)
            .map(HeaderValue::try_from)
            .transpose()
            .map_err(|_| ErrorData::invalid_params("Invalid MCP name header.", None))?;
        Ok(Some(Self {
            version,
            method,
            name,
            ledger_target,
            file_imports: crate::imports::supports_file_imports(
                &params["_meta"][INLINE_CAPABILITIES]["extensions"],
            ),
        }))
    }

    pub(super) fn desktop_file_imports(&self) -> bool {
        self.version == ProtocolVersion::STANDARD_HEADERS
            && self.ledger_target.is_none()
            && self.file_imports
    }

    pub(super) fn ledger_target(&self) -> Option<&NamedLedgerTarget> {
        self.ledger_target.as_ref()
    }

    pub(super) fn http_headers(&self, mut request: RequestBuilder) -> RequestBuilder {
        request = request.header(HEADER_MCP_PROTOCOL_VERSION, self.version.as_str());
        if self.version >= ProtocolVersion::STANDARD_HEADERS
            && let Some(method) = self.method.wire
        {
            request = request.header(HEADER_MCP_METHOD, method);
            if let Some(name) = &self.name {
                request = request.header(HEADER_MCP_NAME, name);
            }
        }
        request
    }
}

pub(super) struct BridgeRequest<'a> {
    message: &'a Value,
    kind: MethodKind,
    inline: Option<InlineProtocol<'a>>,
}

impl<'a> TryFrom<&'a Value> for BridgeRequest<'a> {
    type Error = ErrorData;

    fn try_from(message: &'a Value) -> Result<Self, Self::Error> {
        let method = RequestMethod::from(message["method"].as_str());
        Ok(Self {
            message,
            kind: method.kind,
            inline: InlineProtocol::from_params(method, &message["params"])?,
        })
    }
}

impl<'a> BridgeRequest<'a> {
    pub(super) fn message(&self) -> &'a Value {
        self.message
    }

    pub(super) fn kind(&self) -> MethodKind {
        self.kind
    }

    pub(super) fn inline(&self) -> Option<&InlineProtocol<'a>> {
        self.inline.as_ref()
    }
}

#[cfg(test)]
mod tests {
    use super::{
        BASE64_HEADER_PREFIX, BASE64_HEADER_SUFFIX, BASE64_STANDARD, BridgeRequest, Engine,
        HEADER_MCP_METHOD, HEADER_MCP_NAME, HEADER_MCP_PROTOCOL_VERSION, HeaderValue,
        McpNameHeader, ProtocolVersion,
    };
    use rmcp::model::ErrorCode;
    use serde_json::json;

    #[test]
    fn mcp_name_keeps_printable_ascii() -> Result<(), reqwest::header::InvalidHeaderValue> {
        let wire = HeaderValue::try_from(McpNameHeader::from("company_list"))?;
        assert_eq!(wire, "company_list");
        Ok(())
    }

    #[test]
    fn mcp_name_wraps_non_ascii_and_literal_wrappers() -> Result<(), Box<dyn std::error::Error>> {
        let sentinel = format!("{BASE64_HEADER_PREFIX}YQ=={BASE64_HEADER_SUFFIX}");
        for original in ["café", sentinel.as_str(), " padded ", "line\nbreak"] {
            let wire = HeaderValue::try_from(McpNameHeader::from(original))?;
            let encoded = wire
                .to_str()?
                .strip_prefix(BASE64_HEADER_PREFIX)
                .and_then(|value| value.strip_suffix(BASE64_HEADER_SUFFIX))
                .ok_or("MCP name is missing its base64 wrapper")?;
            assert_eq!(BASE64_STANDARD.decode(encoded)?, original.as_bytes());
        }
        Ok(())
    }

    #[test]
    fn inline_metadata_distinguishes_legacy_modern_and_invalid_requests()
    -> Result<(), rmcp::ErrorData> {
        let mut message = json!({
            "method": "tools/list",
            "params": {"_meta": {"io.modelcontextprotocol/protocolVersion": "2025-11-25"}}
        });
        assert!(BridgeRequest::try_from(&message)?.inline().is_none());

        let meta = &mut message["params"]["_meta"];
        meta["io.modelcontextprotocol/protocolVersion"] = json!(ProtocolVersion::NO_INITIALIZE);
        assert!(BridgeRequest::try_from(&message).is_err());

        message["params"]["_meta"]["io.modelcontextprotocol/clientCapabilities"] = json!({});
        assert!(BridgeRequest::try_from(&message)?.inline().is_some());

        message["params"]["_meta"]["io.modelcontextprotocol/protocolVersion"] = json!("2099-01-01");
        assert_eq!(
            BridgeRequest::try_from(&message)
                .err()
                .map(|error| error.code),
            Some(ErrorCode::UNSUPPORTED_PROTOCOL_VERSION)
        );

        message["params"]["_meta"]["io.modelcontextprotocol/protocolVersion"] = json!(42);
        assert!(BridgeRequest::try_from(&message).is_err());
        Ok(())
    }

    #[test]
    fn inline_headers_use_the_method_specific_parameter() -> Result<(), Box<dyn std::error::Error>>
    {
        let client = reqwest::blocking::Client::new();
        for (method, expected) in [
            ("tools/call", Some("the name")),
            ("prompts/get", Some("the name")),
            ("resources/subscribe", Some("skill://example")),
            ("resources/unsubscribe", Some("skill://example")),
            ("tasks/get", Some("task_1")),
            ("tasks/update", Some("task_1")),
            ("tasks/cancel", Some("task_1")),
            ("tools/list", None),
        ] {
            let message = json!({
                "method": method,
                "params": {
                    "name": "the name",
                    "uri": "skill://example",
                    "taskId": "task_1",
                    "_meta": {
                        "io.modelcontextprotocol/protocolVersion": ProtocolVersion::STANDARD_HEADERS,
                        "io.modelcontextprotocol/clientCapabilities": {}
                    }
                }
            });
            let request = BridgeRequest::try_from(&message).map_err(|error| error.message)?;
            let http = request
                .inline()
                .ok_or("missing inline protocol")?
                .http_headers(client.post("http://127.0.0.1/mcp"))
                .build()?;
            let headers = http.headers();
            assert_eq!(
                headers[HEADER_MCP_PROTOCOL_VERSION],
                ProtocolVersion::STANDARD_HEADERS.as_str()
            );
            assert_eq!(headers[HEADER_MCP_METHOD], method);
            assert_eq!(
                headers
                    .get(HEADER_MCP_NAME)
                    .map(HeaderValue::to_str)
                    .transpose()?,
                expected
            );
        }
        Ok(())
    }
}
