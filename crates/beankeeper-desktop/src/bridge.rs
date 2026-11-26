//! A stdio transport for clients that cannot connect directly to Desktop HTTP.
//!
//! Requests use the running Desktop process and its MCP sessions. The bridge
//! never opens a ledger, starts a second tool server, or writes credentials to
//! stdout. Current upstream tools use request/response traffic; a separate GET
//! notification stream is not needed for them.

mod extension;
mod protocol;

use std::{
    error::Error,
    fmt,
    io::{self, BufRead, Write},
    path::{Path, PathBuf},
    time::Duration,
};

use beankeeper_cli::mcp::{LEDGER_TARGET_HEADER, NamedLedgerTarget};
use clap::Args;
use protocol::{BridgeRequest, MethodKind};
use reqwest::{
    StatusCode, Url,
    blocking::{Client, Response},
    header::{AUTHORIZATION, CONTENT_TYPE, HeaderValue},
    redirect::Policy,
};
use rmcp::{
    ErrorData,
    model::{CacheScope, CallToolResult, ErrorCode, JsonObject, ListToolsResult, Tool},
};
use secrecy::{ExposeSecret, zeroize::Zeroizing};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{
    agent_skill,
    demo::{DemoError, DemoOperation, DemoReport, target_meta},
    onboarding::STATUS_TOOL,
    private_file::{BEARER_FILE_LIMIT, PrivateFile},
    product::mcp::LicenseTool,
    service::{self, Endpoint, ServiceError},
    setup::AgentAccess,
};

const MAX_MESSAGE_BYTES: u64 = 8 * 1024 * 1024;

fn exceeds_message_limit(bytes: usize) -> bool {
    u64::try_from(bytes).map_or(true, |length| length > MAX_MESSAGE_BYTES)
}

/// Connect an MCP stdio client to the already-running Desktop HTTP endpoint.
#[derive(Args, Debug, Clone)]
pub struct BridgeOptions {
    /// Local Streamable HTTP endpoint belonging to `bk serve`.
    #[arg(long, default_value = "http://127.0.0.1:8750/mcp")]
    pub url: String,
    /// Private file containing the agent bearer token; never put the token in argv.
    #[arg(long, required_unless_present = "service", conflicts_with = "service")]
    pub token_file: Option<PathBuf>,
    /// Discover the per-user background service endpoint and its private agent token.
    #[arg(long, conflicts_with = "url")]
    pub service: bool,
}

/// Transport failures without request bodies or secret credentials in diagnostics.
#[derive(Debug)]
pub enum BridgeError {
    /// The managed Desktop service is unavailable.
    Service(Box<crate::service::ServiceError>),
    /// A local-only URL was not supplied.
    InvalidUrl,
    /// The token is missing, malformed, or readable by other Unix users.
    InvalidToken,
    /// A token file or stdio stream could not be read or written.
    Io(io::Error),
    /// A bounded, proxy-free HTTP operation failed.
    Http(reqwest::Error),
    /// Desktop rejected the HTTP request.
    Rejected(StatusCode),
    /// The client or server sent malformed or oversized JSON-RPC.
    Protocol,
    /// The embedded agent skill could not supply valid MCP guidance.
    EmbeddedSkill(ErrorData),
    /// The managed service rejected the read-only diagnostic call.
    ToolFailed,
}

impl BridgeError {
    pub(crate) fn diagnostic_code(&self) -> &'static str {
        match self {
            Self::Service(error) => error.code(),
            Self::InvalidToken => "AGENT_CREDENTIAL_INVALID",
            Self::Rejected(_) => "AGENT_MCP_REJECTED",
            Self::ToolFailed => "AGENT_TOOL_FAILED",
            Self::Http(_) => "AGENT_MCP_UNREACHABLE",
            _ => "AGENT_MCP_PROTOCOL",
        }
    }
}

impl From<ServiceError> for BridgeError {
    fn from(error: ServiceError) -> Self {
        Self::Service(Box::new(error))
    }
}

impl fmt::Display for BridgeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Service(error) => write!(f, "{}: {error}", error.code()),
            Self::InvalidUrl => f.write_str("MCP bridge requires http://127.0.0.1:PORT/mcp, http://localhost:PORT/mcp or http://[::1]:PORT/mcp"),
            Self::InvalidToken => f.write_str("MCP bridge requires a nonempty private agent token file (Unix mode 0600); run `bk setup` to configure it"),
            Self::Io(error) => write!(f, "MCP bridge I/O failed: {error}"),
            Self::Http(error) => write!(f, "MCP bridge cannot reach Desktop; start `bk serve` first: {error}"),
            Self::Rejected(status @ (StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN)) => write!(f, "Desktop rejected MCP authentication (HTTP {status}); ask the owner to check or renew the agent credential"),
            Self::Rejected(status) => write!(f, "Desktop rejected the MCP bridge request (HTTP {status}); check the running server"),
            Self::Protocol => f.write_str("MCP bridge received malformed or oversized JSON-RPC"),
            Self::EmbeddedSkill(error) => f.write_str(&error.message),
            Self::ToolFailed => f.write_str("Desktop rejected the read-only MCP diagnostic. Check the agent's ledger read access."),
        }
    }
}
impl Error for BridgeError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Http(error) => Some(error),
            _ => None,
        }
    }
}
impl From<io::Error> for BridgeError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}
impl From<reqwest::Error> for BridgeError {
    fn from(error: reqwest::Error) -> Self {
        Self::Http(error)
    }
}

/// Forward newline-delimited JSON-RPC until stdin closes, then close the session.
///
/// # Errors
/// Rejects remote URLs, invalid credentials, HTTP failures, and malformed traffic.
pub fn run(options: &BridgeOptions) -> Result<(), BridgeError> {
    if options.service {
        let endpoint = service::connection().map_err(|error| match error {
            ServiceError::NotInstalled => ServiceError::NotReady,
            error => error,
        })?;
        run_connected(
            endpoint.url().clone(),
            endpoint.token_file(),
            Some(endpoint.books()),
        )
    } else {
        run_connected(
            local_url(&options.url)?,
            options
                .token_file
                .as_deref()
                .ok_or(BridgeError::InvalidToken)?,
            None,
        )
    }
}

/// Serve the Claude extension's first-run tools and bridge to the managed ledger.
///
/// # Errors
/// Returns actionable setup, service discovery, credential and transport errors.
pub fn run_extension() -> Result<(), BridgeError> {
    extension::run(io::stdin().lock(), &mut io::stdout().lock())
}

fn run_connected(
    url: Url,
    token_file: &Path,
    managed_books: Option<&Path>,
) -> Result<(), BridgeError> {
    let mut transport = connected_transport(
        url,
        token_file,
        Some(AgentAccess::mcp_instructions().map_err(BridgeError::EmbeddedSkill)?),
        Duration::from_secs(120),
    )?;
    transport.managed = managed_books.map(|books| ManagedConnection {
        books: books.into(),
        token_file: token_file.into(),
        initialize: None,
    });
    let result = transport.forward(io::stdin().lock(), &mut io::stdout().lock());
    transport.close();
    result
}

/// How far to exercise the managed service, using the same transport as clients.
#[derive(Clone, Copy)]
pub(crate) enum Probe {
    Authentication,
    ReadOnlyTool,
    AgentAccess,
}

pub(crate) fn probe_service(
    endpoint: &Endpoint,
    probe: Probe,
) -> Result<Option<AgentAccess>, BridgeError> {
    let mut transport = connected_transport(
        endpoint.url().clone(),
        endpoint.token_file(),
        None,
        Duration::from_secs(5),
    )?;
    let result = transport.probe(probe);
    transport.close();
    result
}

pub(crate) fn call_demo(
    operation: DemoOperation,
    arguments: &JsonObject,
) -> Result<CallToolResult, BridgeError> {
    let endpoint = service::connection()?;
    let mut transport = connected_transport(
        endpoint.url().clone(),
        endpoint.token_file(),
        None,
        Duration::from_secs(120),
    )?;
    let result = transport
        .initialize()
        .and_then(|()| transport.request_tool(operation.name(), arguments));
    transport.close();
    result
}

fn connected_transport(
    url: Url,
    token_file: &Path,
    instructions: Option<&'static str>,
    timeout: Duration,
) -> Result<Transport, BridgeError> {
    let client = Client::builder()
        .no_proxy()
        .redirect(Policy::none())
        .connect_timeout(Duration::from_secs(5))
        .timeout(timeout)
        .build()?;
    Ok(Transport {
        client,
        url,
        authorization: authorization(token_file)?,
        session: None,
        protocol: None,
        managed: None,
        demo: DemoSelection::default(),
        instructions,
        extension_tools: false,
    })
}

pub(crate) fn authorization(token_file: &Path) -> Result<HeaderValue, BridgeError> {
    let secret = PrivateFile::open(token_file)
        .and_then(|file| file.read_secret(BEARER_FILE_LIMIT))
        .map_err(|_| BridgeError::InvalidToken)?;
    bearer_header(secret.expose_secret().trim())
}

pub(crate) fn bearer_header(token: &str) -> Result<HeaderValue, BridgeError> {
    if token.is_empty() || token.bytes().any(|byte| byte.is_ascii_whitespace()) {
        return Err(BridgeError::InvalidToken);
    }
    let value = Zeroizing::new(format!("Bearer {token}"));
    let mut authorization = HeaderValue::from_str(&value).map_err(|_| BridgeError::InvalidToken)?;
    authorization.set_sensitive(true);
    Ok(authorization)
}

fn local_url(input: &str) -> Result<Url, BridgeError> {
    let url = Url::parse(input).map_err(|_| BridgeError::InvalidUrl)?;
    if url.scheme() != "http"
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || url.path() != "/mcp"
        || url.port_or_known_default() == Some(0)
        || !matches!(url.host_str(), Some("127.0.0.1" | "localhost" | "[::1]"))
    {
        return Err(BridgeError::InvalidUrl);
    }
    Ok(url)
}

struct ManagedConnection {
    books: PathBuf,
    token_file: PathBuf,
    initialize: Option<Value>,
}

#[derive(Clone, Default, PartialEq, Eq)]
enum DemoSelection {
    #[default]
    Startup,
    Unavailable,
    Active(NamedLedgerTarget),
}

impl DemoSelection {
    fn for_request(&self, explicit: Option<&NamedLedgerTarget>) -> Self {
        explicit.cloned().map_or_else(|| self.clone(), Self::Active)
    }

    fn begin(&mut self, operation: DemoOperation) -> bool {
        if matches!((operation, &*self), (DemoOperation::Start, Self::Startup)) {
            // A failed attempt leaves practice calls suspended until explicitly resolved.
            *self = Self::Unavailable;
            true
        } else {
            false
        }
    }

    fn apply(&mut self, operation: DemoOperation, result: &Value) -> Result<bool, BridgeError> {
        if result.is_null() || result.get("isError").and_then(Value::as_bool) == Some(true) {
            return Ok(false);
        }
        let report = DemoReport::deserialize(&result["structuredContent"])
            .map_err(|_| BridgeError::Protocol)?;
        let selection = match (operation, report) {
            (DemoOperation::Start, DemoReport::Active { target }) => Self::Active(target),
            (DemoOperation::Remove, DemoReport::Removed { .. } | DemoReport::Absent) => {
                Self::Startup
            }
            _ => return Err(BridgeError::Protocol),
        };
        let changed = *self != selection;
        *self = selection;
        Ok(changed)
    }
}

fn demo_tools() -> Vec<Tool> {
    DemoOperation::ALL
        .map(|operation| operation.definition().tool)
        .into()
}

struct Transport {
    client: Client,
    url: Url,
    authorization: HeaderValue,
    managed: Option<ManagedConnection>,
    demo: DemoSelection,
    session: Option<HeaderValue>,
    protocol: Option<String>,
    instructions: Option<&'static str>,
    extension_tools: bool,
}

fn read_message(input: &mut impl BufRead) -> Result<Option<Value>, BridgeError> {
    loop {
        let mut line = String::new();
        let read = io::Read::take(&mut *input, MAX_MESSAGE_BYTES + 1).read_line(&mut line)?;
        if read == 0 {
            return Ok(None);
        }
        if exceeds_message_limit(read) {
            return Err(BridgeError::Protocol);
        }
        if line.trim().is_empty() {
            continue;
        }
        let message: Value = serde_json::from_str(&line).map_err(|_| BridgeError::Protocol)?;
        if message.get("jsonrpc").and_then(Value::as_str) != Some("2.0") || !message.is_object() {
            return Err(BridgeError::Protocol);
        }
        return Ok(Some(message));
    }
}

fn rpc_reply(
    output: &mut impl Write,
    id: &Value,
    result: Result<Value, ErrorData>,
) -> Result<(), BridgeError> {
    let message = match result {
        Ok(result) => json!({"jsonrpc":"2.0", "id":id, "result":result}),
        Err(error) => json!({"jsonrpc":"2.0", "id":id, "error":error}),
    };
    write_message(output, &message)
}

impl Transport {
    fn forward(
        &mut self,
        mut input: impl BufRead,
        output: &mut impl Write,
    ) -> Result<(), BridgeError> {
        while let Some(message) = read_message(&mut input)? {
            self.forward_message(&message, output)?;
        }
        Ok(())
    }

    fn forward_message(
        &mut self,
        message: &Value,
        output: &mut impl Write,
    ) -> Result<(), BridgeError> {
        // A relative upload target belongs to the HTTP transport. This stdio
        // bridge has no externally reachable upload address to advertise.
        let mut sanitized;
        let message = if message["params"]["_meta"][crate::imports::META_CAPABILITIES]["extensions"]
            .get(crate::imports::FILES)
            .is_some()
        {
            sanitized = message.clone();
            if let Some(extensions) = sanitized["params"]["_meta"]
                [crate::imports::META_CAPABILITIES]["extensions"]
                .as_object_mut()
            {
                extensions.remove(crate::imports::FILES);
            }
            &sanitized
        } else {
            message
        };
        let request = match BridgeRequest::try_from(message) {
            Ok(request) => request,
            Err(error) => {
                return message
                    .get("id")
                    .map_or(Ok(()), |id| rpc_reply(output, id, Err(error)));
            }
        };
        self.forward_negotiated(&request, output)
    }

    fn local_tool(&self, message: &Value, output: &mut impl Write) -> Result<bool, BridgeError> {
        if let Some(managed) = &self.managed
            && let Some(tool) = message["params"]["name"]
                .as_str()
                .and_then(LicenseTool::from_name)
            && let Some(id) = message.get("id")
        {
            return write_message(
                output,
                &json!({"jsonrpc": "2.0", "id": id, "result": tool.call(&message["params"], Some(&managed.books))}),
            ).map(|()| true);
        }
        if self.managed.is_some()
            && let Some(operation) = message["params"]["name"]
                .as_str()
                .and_then(|name| name.parse::<crate::diagnostics::Diagnostic>().ok())
            && let Some(id) = message.get("id")
        {
            let params = rmcp::model::CallToolRequestParams::deserialize(&message["params"])
                .map_err(|_| BridgeError::Protocol)?;
            return write_message(
                output,
                &json!({"jsonrpc":"2.0","id":id,"result":crate::diagnostics::call(operation, params)}),
            ).map(|()| true);
        }
        Ok(false)
    }

    fn forward_negotiated(
        &mut self,
        request: &BridgeRequest<'_>,
        output: &mut impl Write,
    ) -> Result<(), BridgeError> {
        let message = request.message();
        let inline = request.inline();
        if let MethodKind::Skill(method) = request.kind()
            && self.instructions.is_some()
        {
            return message.get("id").map_or(Ok(()), |id| {
                let result = if self.protocol.is_some() || inline.is_some() {
                    agent_skill::result(method, &message["params"])
                } else {
                    Err(ErrorData::new(
                        ErrorCode(-32002),
                        "Initialize Beankeeper first.",
                        None,
                    ))
                };
                rpc_reply(output, id, result)
            });
        }
        let tool_request = request.kind() == MethodKind::ToolsCall;
        let tool_list = request.kind() == MethodKind::ToolsList;
        if tool_request
            && (self.protocol.is_some() || inline.is_some())
            && self.local_tool(message, output)?
        {
            return Ok(());
        }
        self.refresh_access(output)?;
        let operation = if self.managed.is_some() && tool_request {
            message["params"]["name"]
                .as_str()
                .and_then(|name| name.parse::<DemoOperation>().ok())
        } else {
            None
        };
        if let Some(operation) = operation {
            let entering_demo = self.demo.begin(operation);
            let replies = self.exchange_startup(request, output)?;
            let changed = if let Some(reply) = replies
                .iter()
                .find(|reply| reply.get("id") == message.get("id"))
            {
                self.demo.apply(operation, &reply["result"])?
            } else {
                false
            };
            if inline.is_none() && (entering_demo || changed) {
                tools_changed(output)?;
            }
            return Ok(());
        }
        if tool_request || tool_list {
            let explicit_target = inline.and_then(protocol::InlineProtocol::ledger_target);
            match self.demo.for_request(explicit_target) {
                DemoSelection::Startup => {}
                DemoSelection::Unavailable => {
                    return self.exchange_demo(request, tool_list, None, output);
                }
                DemoSelection::Active(target) => {
                    return self.exchange_demo(request, tool_list, Some(&target), output);
                }
            }
        }
        if let Some(managed) = &self.managed
            && (self.protocol.is_some() || inline.is_some())
            && tool_request
            && matches!(
                message["params"]["name"].as_str(),
                Some(service::ACCESS_TOOL | service::OPEN_TOOL)
            )
            && let Some(id) = message.get("id")
        {
            let result = service::local_call(&message["params"], &managed.books);
            write_message(
                output,
                &json!({"jsonrpc": "2.0", "id": id, "result": result}),
            )?;
            self.refresh_access(output)?;
            return Ok(());
        }
        self.exchange_startup(request, output)?;
        if request.kind() == MethodKind::Initialize
            && let Some(managed) = &mut self.managed
        {
            managed.initialize = Some(message.clone());
        }
        Ok(())
    }

    fn exchange_startup(
        &mut self,
        request: &BridgeRequest<'_>,
        output: &mut impl Write,
    ) -> Result<Vec<Value>, BridgeError> {
        match self.exchange_target(request, None, output) {
            Err(BridgeError::Rejected(StatusCode::UNAUTHORIZED))
                if self.refresh_access(output)? =>
            {
                self.exchange_target(request, None, output)
            }
            Err(error) if self.managed.is_some() && lost_session(&error) => {
                self.reconnect()?;
                self.exchange_target(request, None, output)
            }
            result => result,
        }
    }

    fn exchange_demo(
        &mut self,
        request: &BridgeRequest<'_>,
        tool_list: bool,
        target: Option<&NamedLedgerTarget>,
        output: &mut impl Write,
    ) -> Result<(), BridgeError> {
        if let Some(target) = target {
            let result = match self.exchange_target(request, Some(target), output) {
                Err(BridgeError::Rejected(StatusCode::UNAUTHORIZED))
                    if self.refresh_access(output)? =>
                {
                    self.exchange_target(request, Some(target), output)
                }
                result => result,
            };
            match result {
                Ok(_) => return Ok(()),
                Err(BridgeError::Rejected(StatusCode::NOT_FOUND)) => {}
                Err(error) => return Err(error),
            }
        }
        if let Some(id) = request.message().get("id") {
            let meta = target.map(target_meta);
            let result = if tool_list {
                let mut tools = demo_tools();
                if self.extension_tools {
                    tools.extend(extension::tools(false));
                } else {
                    tools.extend(LicenseTool::tools());
                    tools.extend(crate::diagnostics::tools());
                }
                let mut result = ListToolsResult::with_all_items(tools)
                    .with_ttl_ms(0)
                    .with_cache_scope(CacheScope::Private);
                result.meta = meta;
                serde_json::to_value(result)
            } else {
                let mut result = DemoError::Unavailable.result();
                result.meta = meta;
                serde_json::to_value(result)
            }
            .map_err(|_| BridgeError::Protocol)?;
            write_message(output, &json!({"jsonrpc":"2.0","id":id,"result":result}))?;
        }
        Ok(())
    }

    fn refresh_access(&mut self, output: &mut impl Write) -> Result<bool, BridgeError> {
        let Some(managed) = &self.managed else {
            return Ok(false);
        };
        let current = authorization(&managed.token_file)?;
        if self.authorization == current {
            return Ok(false);
        }
        self.authorization = current;
        if self.protocol.is_some() {
            tools_changed(output)?;
        }
        Ok(true)
    }

    // Retry only after an explicit missing-session response or a failed connection.
    // A request that may have reached a ledger is never replayed.
    fn reconnect(&mut self) -> Result<(), BridgeError> {
        let managed = self.managed.as_ref().ok_or(BridgeError::Protocol)?;
        let endpoint = service::connection()?;
        if endpoint.books() != managed.books {
            return Err(BridgeError::Protocol);
        }
        self.url = endpoint.url().clone();
        self.authorization = authorization(endpoint.token_file())?;
        self.session = None;
        self.protocol = None;
        if let Some(initialize) = managed.initialize.clone() {
            let mut sink = io::sink();
            self.exchange(&initialize, &mut sink)?;
            self.exchange(
                &json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
                &mut sink,
            )?;
        }
        Ok(())
    }

    fn exchange(
        &mut self,
        message: &Value,
        output: &mut impl Write,
    ) -> Result<Vec<Value>, BridgeError> {
        let request = BridgeRequest::try_from(message).map_err(|_| BridgeError::Protocol)?;
        self.exchange_target(&request, None, output)
    }

    fn exchange_target(
        &mut self,
        exchange: &BridgeRequest<'_>,
        target: Option<&NamedLedgerTarget>,
        output: &mut impl Write,
    ) -> Result<Vec<Value>, BridgeError> {
        let message = exchange.message();
        let method = exchange.kind();
        let mut request = self
            .client
            .post(self.url.clone())
            .header(AUTHORIZATION, self.authorization.clone())
            .header("Accept", "application/json, text/event-stream")
            .json(message);
        if let Some(target) = target {
            request = request.header(LEDGER_TARGET_HEADER, target.as_ref());
        }
        if let Some(protocol) = exchange.inline() {
            request = protocol.http_headers(request);
        } else {
            if let Some(session) = &self.session {
                request = request.header("Mcp-Session-Id", session);
            }
            if let Some(protocol) = &self.protocol {
                request = request.header("MCP-Protocol-Version", protocol);
            }
        }
        if self.extension_tools
            && matches!(method, MethodKind::Initialize | MethodKind::Initialized)
        {
            request = request.timeout(Duration::from_secs(5));
        }
        let response = request.send()?;
        if !response.status().is_success() {
            return Err(BridgeError::Rejected(response.status()));
        }
        if let Some(session) = response.headers().get("Mcp-Session-Id") {
            self.session = Some(session.clone());
        }
        let initial = method == MethodKind::Initialize;
        let discovery = method == MethodKind::Discover;
        let tool_list = method == MethodKind::ToolsList;
        let relay = ReplyContext {
            request_id: message.get("id"),
            instructions: if initial || discovery {
                self.instructions
            } else {
                None
            },
            managed_initialize: initial && self.managed.is_some(),
            supplemental_tools: match (tool_list, target, &self.managed) {
                (true, Some(_), _) => SupplementalTools::Demo,
                (true, None, Some(_)) => SupplementalTools::Managed,
                _ => SupplementalTools::None,
            },
            extension_tools: tool_list && self.extension_tools,
            license_tools: tool_list && self.managed.is_some() && !self.extension_tools,
        };
        let replies = relay_response(response, relay, output)?;
        if initial {
            self.protocol = replies.iter().find_map(|reply| {
                reply
                    .get("result")?
                    .get("protocolVersion")?
                    .as_str()
                    .map(str::to_owned)
            });
        }
        Ok(replies)
    }

    fn initialize(&mut self) -> Result<(), BridgeError> {
        let mut output = io::sink();
        self.exchange(
            &json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{
                "protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{
                    "name":"bk service probe", "version":env!("CARGO_PKG_VERSION")
                }
            }}),
            &mut output,
        )?;
        if self.session.is_none() || self.protocol.is_none() {
            return Err(BridgeError::Protocol);
        }
        Ok(())
    }

    fn request_tool(
        &mut self,
        tool: &str,
        arguments: &JsonObject,
    ) -> Result<CallToolResult, BridgeError> {
        let mut output = io::sink();
        self.exchange(
            &json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
            &mut output,
        )?;
        let replies = self.exchange(
            &json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{
                "name":tool, "arguments":arguments
            }}),
            &mut output,
        )?;
        let reply = replies
            .iter()
            .find(|reply| reply.get("id").and_then(Value::as_u64) == Some(2))
            .ok_or(BridgeError::Protocol)?;
        let result = reply.get("result").ok_or(BridgeError::ToolFailed)?;
        CallToolResult::deserialize(result).map_err(|_| BridgeError::Protocol)
    }

    fn probe(&mut self, probe: Probe) -> Result<Option<AgentAccess>, BridgeError> {
        self.initialize()?;
        let tool = match probe {
            Probe::Authentication => return Ok(None),
            Probe::ReadOnlyTool => "company_list",
            Probe::AgentAccess => STATUS_TOOL,
        };
        let result = self.request_tool(tool, &JsonObject::new())?;
        if result.is_error == Some(true) {
            return Err(BridgeError::ToolFailed);
        }
        if matches!(probe, Probe::AgentAccess) {
            #[derive(Deserialize)]
            struct SetupAccess {
                agent_access: Option<AgentAccess>,
            }
            let payload = result.structured_content.ok_or(BridgeError::Protocol)?;
            return serde_json::from_value::<SetupAccess>(payload)
                .map(|status| status.agent_access)
                .map_err(|_| BridgeError::Protocol);
        }
        // Array-shaped company lists can travel as text in some MCP versions.
        if result.structured_content.is_none() && result.content.is_empty() {
            return Err(BridgeError::ToolFailed);
        }
        Ok(None)
    }

    fn close(&self) {
        if let Some(session) = &self.session {
            let mut request = self
                .client
                .delete(self.url.clone())
                .header(AUTHORIZATION, self.authorization.clone())
                .header("Mcp-Session-Id", session)
                .timeout(Duration::from_secs(2));
            if let Some(protocol) = &self.protocol {
                request = request.header("MCP-Protocol-Version", protocol);
            }
            let _ = request.send();
        }
    }
}

fn lost_session(error: &BridgeError) -> bool {
    match error {
        BridgeError::Rejected(StatusCode::NOT_FOUND) => true,
        BridgeError::Http(error) => error.is_connect(),
        _ => false,
    }
}

#[derive(Clone, Copy, Default)]
enum SupplementalTools {
    #[default]
    None,
    Managed,
    Demo,
}

#[derive(Clone, Copy, Default)]
struct ReplyContext<'a> {
    request_id: Option<&'a Value>,
    instructions: Option<&'static str>,
    managed_initialize: bool,
    supplemental_tools: SupplementalTools,
    extension_tools: bool,
    license_tools: bool,
}

fn relay_response(
    response: Response,
    context: ReplyContext<'_>,
    output: &mut impl Write,
) -> Result<Vec<Value>, BridgeError> {
    if response.status() == StatusCode::ACCEPTED || response.status() == StatusCode::NO_CONTENT {
        return Ok(Vec::new());
    }
    let sse = response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.starts_with("text/event-stream"));
    let mut reader = io::BufReader::new(response);
    if sse {
        return relay_sse(&mut reader, context, output);
    }
    let mut contents = Vec::new();
    io::Read::read_to_end(
        &mut io::Read::take(reader, MAX_MESSAGE_BYTES + 1),
        &mut contents,
    )?;
    if exceeds_message_limit(contents.len()) {
        return Err(BridgeError::Protocol);
    }
    let message = emit(&contents, context, output)?;
    Ok(vec![message])
}

pub(crate) fn response_result(response: Response) -> Result<Value, BridgeError> {
    if !response.status().is_success() {
        return Err(BridgeError::Rejected(response.status()));
    }
    let id = json!(1);
    let context = ReplyContext {
        request_id: Some(&id),
        ..ReplyContext::default()
    };
    relay_response(response, context, &mut io::sink())?
        .into_iter()
        .find(|message| message["id"] == id)
        .and_then(|mut message| message.get_mut("result").map(Value::take))
        .ok_or(BridgeError::Protocol)
}

fn emit(
    contents: &[u8],
    context: ReplyContext<'_>,
    output: &mut impl Write,
) -> Result<Value, BridgeError> {
    let mut message: Value = serde_json::from_slice(contents).map_err(|_| BridgeError::Protocol)?;
    if message.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
        return Err(BridgeError::Protocol);
    }
    if let Some(instructions) = context.instructions
        && context
            .request_id
            .is_some_and(|id| message.get("id") == Some(id))
        && let Some(result) = message.get_mut("result").and_then(Value::as_object_mut)
    {
        let value = result
            .entry("instructions")
            .or_insert_with(|| Value::String(String::new()));
        let Value::String(existing) = value else {
            return Err(BridgeError::Protocol);
        };
        if !existing.is_empty() {
            existing.push(' ');
        }
        existing.push_str(instructions);
        if let Some(capabilities) = result.get_mut("capabilities") {
            agent_skill::advertise(capabilities);
        }
    }
    if context
        .request_id
        .is_some_and(|id| message.get("id") == Some(id))
        && let Some(result) = message.get_mut("result")
    {
        if let Some(extensions) = result
            .get_mut("capabilities")
            .and_then(|capabilities| capabilities.get_mut("extensions"))
            .and_then(Value::as_object_mut)
        {
            extensions.remove(crate::imports::FILES);
        }
        if context.managed_initialize
            && let Some(tools) = result
                .get_mut("capabilities")
                .and_then(|capabilities| capabilities.get_mut("tools"))
                .and_then(Value::as_object_mut)
        {
            tools.insert("listChanged".into(), json!(true));
        }
        if result.get("nextCursor").is_none_or(Value::is_null)
            && let Some(tools) = result.get_mut("tools").and_then(Value::as_array_mut)
        {
            let supplemental = match context.supplemental_tools {
                SupplementalTools::Demo => demo_tools(),
                SupplementalTools::Managed => service::access_tool()
                    .into_iter()
                    .chain(service::open_tool())
                    .collect(),
                SupplementalTools::None => Vec::new(),
            };
            if context.license_tools || context.extension_tools {
                tools.retain(|tool| tool["name"] != LicenseTool::STATUS);
            }
            for tool in supplemental
                .into_iter()
                .chain(
                    context
                        .extension_tools
                        .then(|| extension::tools(false))
                        .into_iter()
                        .flatten(),
                )
                .chain(
                    context
                        .license_tools
                        .then(|| {
                            LicenseTool::tools()
                                .chain(crate::diagnostics::tools())
                                .collect::<Vec<_>>()
                        })
                        .into_iter()
                        .flatten(),
                )
            {
                tools.push(serde_json::to_value(tool).map_err(|_| BridgeError::Protocol)?);
            }
        }
    }
    write_message(output, &message)?;
    Ok(message)
}

fn tools_changed(output: &mut impl Write) -> Result<(), BridgeError> {
    write_message(
        output,
        &json!({"jsonrpc": "2.0", "method":"notifications/tools/list_changed"}),
    )
}

fn write_message(output: &mut impl Write, message: &Value) -> Result<(), BridgeError> {
    serde_json::to_writer(&mut *output, message).map_err(|_| BridgeError::Protocol)?;
    output.write_all(b"\n")?;
    output.flush()?;
    Ok(())
}

fn relay_sse(
    reader: &mut impl BufRead,
    context: ReplyContext<'_>,
    output: &mut impl Write,
) -> Result<Vec<Value>, BridgeError> {
    let mut data = String::new();
    let mut replies = Vec::new();
    loop {
        let mut line = String::new();
        let read = io::Read::take(&mut *reader, MAX_MESSAGE_BYTES + 1).read_line(&mut line)?;
        if exceeds_message_limit(read) {
            return Err(BridgeError::Protocol);
        }
        let line = line.trim_end_matches(['\r', '\n']);
        if line.is_empty() {
            if !data.is_empty() {
                let message = emit(data.as_bytes(), context, output)?;
                let completed = context.request_id.is_some_and(|id| {
                    message.get("id") == Some(id)
                        && (message.get("result").is_some() || message.get("error").is_some())
                });
                replies.push(message);
                data.clear();
                if completed {
                    return Ok(replies);
                }
            }
        } else if let Some(value) = line.strip_prefix("data:") {
            if !data.is_empty() {
                data.push('\n');
            }
            data.push_str(value.strip_prefix(' ').unwrap_or(value));
            if exceeds_message_limit(data.len()) {
                return Err(BridgeError::Protocol);
            }
        }
        if read == 0 {
            if context.request_id.is_some()
                && !replies
                    .iter()
                    .any(|reply| reply.get("id") == context.request_id)
            {
                return Err(BridgeError::Protocol);
            }
            return Ok(replies);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    type TestResult = Result<(), Box<dyn Error>>;

    fn reply(request_id: Option<&Value>) -> ReplyContext<'_> {
        ReplyContext {
            request_id,
            ..ReplyContext::default()
        }
    }

    #[test]
    fn credentials_never_go_to_nonlocal_urls() -> TestResult {
        for url in [
            "https://example.com/mcp",
            "http://127.0.0.2:8750/mcp",
            "http://user:password@127.0.0.1:8750/mcp",
            "http://localhost:8750/mcp?token=secret",
            "http://localhost:8750/other",
            "http://localhost:0/mcp",
            "http://evil.example/mcp",
        ] {
            assert!(
                matches!(local_url(url), Err(BridgeError::InvalidUrl)),
                "{url}"
            );
        }
        for url in [
            "http://127.0.0.1:8750/mcp",
            "http://localhost:8750/mcp",
            "http://[::1]:8750/mcp",
        ] {
            local_url(url)?;
        }
        Ok(())
    }

    #[test]
    fn sse_handles_comments_multiline_events_and_protocol_errors() -> TestResult {
        let data = b": keepalive\nevent: message\ndata: {\ndata: \"jsonrpc\":\"2.0\",\"method\":\"notifications/message\"}\n\ndata: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{}}\n\n";
        let mut output = Vec::new();
        let replies = relay_sse(&mut &data[..], reply(Some(&Value::from(1))), &mut output)?;
        assert_eq!(replies.len(), 2);
        assert_eq!(String::from_utf8(output)?.lines().count(), 2);
        assert!(matches!(
            relay_sse(
                &mut &b"data: not-json\n\n"[..],
                reply(Some(&Value::from(1))),
                &mut Vec::new()
            ),
            Err(BridgeError::Protocol)
        ));
        assert!(matches!(
            relay_sse(&mut &b""[..], reply(Some(&Value::from(1))), &mut Vec::new()),
            Err(BridgeError::Protocol)
        ));
        Ok(())
    }

    #[test]
    fn forwards_session_and_protocol_and_closes_session_on_eof() -> TestResult {
        use std::io::Read;
        let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
        let url = Url::parse(&format!("http://{}/mcp", listener.local_addr()?))?;
        let server = std::thread::spawn(move || -> io::Result<()> {
            for index in 0..4 {
                let (mut stream, _) = listener.accept()?;
                stream.set_read_timeout(Some(Duration::from_secs(5)))?;
                let mut reader = io::BufReader::new(stream.try_clone()?);
                let mut first = String::new();
                reader.read_line(&mut first)?;
                let mut headers = String::new();
                let mut length = 0;
                loop {
                    let mut line = String::new();
                    reader.read_line(&mut line)?;
                    if line == "\r\n" {
                        break;
                    }
                    if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                        length = value.trim().parse::<usize>().map_err(io::Error::other)?;
                    }
                    headers.push_str(&line.to_ascii_lowercase());
                }
                let mut body = vec![0; length];
                reader.read_exact(&mut body)?;
                assert!(headers.contains("authorization: bearer test-secret"));
                if index > 0 {
                    assert!(headers.contains("mcp-session-id: test-session"));
                    assert!(headers.contains("mcp-protocol-version: 2025-11-25"));
                }
                let (status, content_type, extra, body) = match index {
                    0 => (
                        "200 OK",
                        "application/json",
                        "Mcp-Session-Id: test-session\r\n",
                        "{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"protocolVersion\":\"2025-11-25\"}}",
                    ),
                    1 => ("202 Accepted", "application/json", "", ""),
                    2 => (
                        "200 OK",
                        "text/event-stream",
                        "",
                        "data: {\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{\"tools\":[]}}\n\n",
                    ),
                    _ => {
                        assert!(first.starts_with("DELETE "));
                        ("200 OK", "application/json", "", "")
                    }
                };
                write!(
                    stream,
                    "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\n{extra}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )?;
            }
            Ok(())
        });
        let mut authorization = HeaderValue::from_static("Bearer test-secret");
        authorization.set_sensitive(true);
        let mut transport = Transport {
            client: Client::builder()
                .no_proxy()
                .timeout(Duration::from_secs(5))
                .build()?,
            url,
            authorization,
            managed: None,
            demo: DemoSelection::default(),
            session: None,
            protocol: None,
            instructions: None,
            extension_tools: false,
        };
        let input = b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{}}\n{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"tools/list\"}\n";
        let mut output = Vec::new();
        transport.forward(&input[..], &mut output)?;
        transport.close();
        server.join().map_err(|_| "HTTP server panicked")??;
        let output = String::from_utf8(output)?;
        let replies: Vec<Value> = output
            .lines()
            .map(serde_json::from_str)
            .collect::<Result<_, _>>()?;
        assert_eq!(replies.len(), 2);
        assert_eq!(replies[1]["result"]["tools"], serde_json::json!([]));
        assert!(!output.contains("test-secret"));
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn token_with_public_permissions_is_rejected_before_network_access() -> TestResult {
        use std::{fs, os::unix::fs::PermissionsExt};
        let dir = tempfile::tempdir()?;
        let token_file = dir.path().join("token");
        fs::write(&token_file, "agent://private-secret")?;
        fs::set_permissions(&token_file, fs::Permissions::from_mode(0o644))?;
        let error = run(&BridgeOptions {
            service: false,
            url: "http://127.0.0.1:1/mcp".into(),
            token_file: Some(token_file),
        })
        .err()
        .ok_or("expected error")?;
        assert!(matches!(error, BridgeError::InvalidToken));
        assert!(!error.to_string().contains("private-secret"));
        Ok(())
    }
}
