//! A Claude stdio session can expose setup before a ledger service exists.

use std::{
    io::{self, BufRead, Write},
    thread::{self, JoinHandle},
    time::Duration,
};

use rmcp::{
    ErrorData,
    model::{
        CacheScope, CallToolRequestParams, CallToolResult, ContentBlock, DiscoverResult,
        Implementation, InitializeRequestParams, ListToolsResult, ProtocolVersion,
        ServerCapabilities, ServerConfig, Tool,
    },
};
use serde::Deserialize;
use serde_json::{Value, json};

use super::{
    BridgeError, ManagedConnection, Transport, connected_transport,
    protocol::{BridgeRequest, InlineProtocol, MethodKind},
    read_message, rpc_reply, tools_changed, write_message,
};
use crate::{
    agent_skill,
    agents::extension as agents_extension,
    product::mcp::LicenseTool,
    service::{
        self, Endpoint, ExtensionCommand, ServiceError,
        extension::{self, ExtensionRequest, Progress, Registration},
    },
    setup::AgentAccess,
};

pub(super) fn run(mut input: impl BufRead, output: &mut impl Write) -> Result<(), BridgeError> {
    let mut session = Session::default();
    let result = (|| {
        while let Some(message) = read_message(&mut input)? {
            session.handle(&message, output)?;
        }
        Ok(())
    })();
    if let Some(transport) = session.transport {
        transport.close();
    }
    result
}

pub(super) fn tools(include_status: bool) -> Vec<Tool> {
    ExtensionCommand::ALL
        .into_iter()
        .filter(|command| include_status || *command != ExtensionCommand::Status)
        .filter_map(ExtensionCommand::tool)
        .chain(agents_extension::mcp_tool())
        // `doctor` already diagnoses the extension's authenticated ledger connection.
        // `update_check` carries the shared version report on this surface so that
        // existing agents retain the connection diagnostic without a name collision.
        .chain(crate::diagnostics::tools().into_iter().filter(|tool| {
            tool.name.parse::<crate::diagnostics::Diagnostic>()
                != Ok(crate::diagnostics::Diagnostic::Doctor)
        }))
        .chain(LicenseTool::tools())
        .collect()
}

fn ledger_catalog(
    tools: Vec<Tool>,
) -> Result<std::collections::BTreeMap<std::borrow::Cow<'static, str>, Tool>, BridgeError> {
    use beankeeper_cli::operation::{Exposure, Operation};

    let access = crate::setup::ManagedAccess::default();
    let mut definitions: std::collections::BTreeMap<_, _> = tools
        .into_iter()
        .map(|tool| (tool.name.clone(), tool))
        .collect();
    let mut catalog = std::collections::BTreeMap::new();
    for operation in Operation::ALL {
        let name = match operation.exposure() {
            Exposure::Both(name) | Exposure::McpOnly(name, _) => name,
            Exposure::Grouped(_) | Exposure::CliOnly(_) => continue,
        };
        if let Some(tool) = definitions.remove(name) {
            let capability = operation
                .primary_capability()
                .map_err(|_| BridgeError::Protocol)?;
            if access.catalog_includes(capability) {
                catalog.insert(tool.name.clone(), tool);
            }
        }
    }
    // Upstream currently returns unpaired definitions. Refuse catalog drift instead
    // of silently hiding a tool that has no typed authorization mapping.
    if !definitions.is_empty() {
        return Err(BridgeError::Protocol);
    }
    Ok(catalog)
}

fn initial_tools(file_imports: bool) -> Result<Vec<Tool>, BridgeError> {
    let access = crate::setup::ManagedAccess::default();
    let mut catalog = ledger_catalog(beankeeper_cli::mcp::LedgerAccess::ReadWrite.tools())?;
    for host in crate::onboarding::DesktopTools::definitions(true) {
        if access.catalog_includes(host.capability) {
            catalog.insert(host.tool.name.clone(), host.tool);
        }
    }
    for tool in service::access_tool()
        .into_iter()
        .chain(service::open_tool())
        .chain(tools(false))
    {
        catalog.insert(tool.name.clone(), tool);
    }
    let mut tools: Vec<_> = catalog.into_values().collect();
    if file_imports {
        crate::imports::amend_tool_definitions(&mut tools);
    }
    Ok(tools)
}

#[derive(Default)]
enum Worker {
    #[default]
    Idle,
    Running(JoinHandle<Result<Registration, ServiceError>>),
    Finished(Result<Registration, ServiceError>),
}

impl Worker {
    fn start(&mut self) -> Progress<'_> {
        if !matches!(self, Self::Running(_) | Self::Finished(Ok(_))) {
            *self = match thread::Builder::new()
                .name("beankeeper-setup".into())
                .spawn(extension::setup)
            {
                Ok(worker) => Self::Running(worker),
                Err(error) => Self::Finished(Err(error.into())),
            };
        }
        self.progress().unwrap_or(Progress::Running)
    }

    fn refresh(&mut self) -> bool {
        if matches!(self, Self::Running(worker) if worker.is_finished())
            && let Self::Running(worker) = std::mem::take(self)
        {
            *self = Self::Finished(worker.join().unwrap_or(Err(ServiceError::Worker)));
            return true;
        }
        false
    }

    fn progress(&self) -> Option<Progress<'_>> {
        match self {
            Self::Idle => None,
            Self::Running(_) => Some(Progress::Running),
            Self::Finished(Ok(registration)) => Some(Progress::Ready {
                registration: Some(registration),
            }),
            Self::Finished(Err(error)) => Some(Progress::Failed { code: error.code() }),
        }
    }
}

/// Why a ledger call cannot yet be attached, keeping setup and transport failures distinct.
enum PreAttach {
    NeverSetUp,
    SetupRunning,
    SetupFailed { code: &'static str },
    ServiceUnavailable(BridgeError),
}

impl From<(&Worker, Option<BridgeError>)> for PreAttach {
    fn from((worker, unavailable): (&Worker, Option<BridgeError>)) -> Self {
        match worker {
            Worker::Running(_) => Self::SetupRunning,
            Worker::Finished(Err(error)) => Self::SetupFailed { code: error.code() },
            Worker::Idle if matches!(&unavailable, Some(BridgeError::Service(error)) if matches!(error.as_ref(), ServiceError::NotInstalled)) => {
                Self::NeverSetUp
            }
            Worker::Idle | Worker::Finished(Ok(_)) => {
                Self::ServiceUnavailable(unavailable.unwrap_or(BridgeError::Protocol))
            }
        }
    }
}

#[derive(Default)]
struct Session {
    initialize: Option<Value>,
    transport: Option<Transport>,
    worker: Worker,
}

impl Session {
    fn handle(&mut self, message: &Value, output: &mut impl Write) -> Result<(), BridgeError> {
        let Some(id) = message.get("id") else {
            if message["method"] != "notifications/initialized"
                && let Some(transport) = &mut self.transport
            {
                return transport.forward_message(message, output);
            }
            return Ok(());
        };
        if message["method"] == "initialize" {
            return self.initialize(message, id, output);
        }
        let request = match BridgeRequest::try_from(message) {
            Ok(request) => request,
            Err(error) => return rpc_reply(output, id, Err(error)),
        };
        let inline = request.inline();
        if request.kind() == MethodKind::Discover {
            let config = match server_config() {
                Ok(config) => config,
                Err(error) => return rpc_reply(output, id, Err(error)),
            };
            let discovery =
                DiscoverResult::from_server_info(ProtocolVersion::KNOWN_VERSIONS.to_vec(), config);
            return response(output, id, discovery);
        }
        if self.initialize.is_none() && inline.is_none() {
            return rpc_error(
                output,
                id,
                -32002,
                "Initialize the Beankeeper extension first.",
            );
        }
        if let MethodKind::Skill(method) = request.kind() {
            return rpc_reply(output, id, agent_skill::result(method, &message["params"]));
        }
        if request.kind() == MethodKind::Ping && inline.is_none() {
            return response(output, id, json!({}));
        }
        if request.kind() == MethodKind::ToolsCall
            && let Some(tool) = message["params"]["name"]
                .as_str()
                .and_then(LicenseTool::from_name)
        {
            let books = self
                .transport
                .as_ref()
                .and_then(|transport| transport.managed.as_ref())
                .map(|managed| managed.books.as_path());
            return response(output, id, tool.call(&message["params"], books));
        }
        if request.kind() == MethodKind::ToolsCall && Self::local_tool(message, id, output)? {
            return Ok(());
        }
        let command = (request.kind() == MethodKind::ToolsCall)
            .then(|| {
                message["params"]["name"]
                    .as_str()?
                    .parse::<ExtensionCommand>()
                    .ok()
            })
            .flatten();
        let finished = self.worker.refresh();
        if let Some(command @ ExtensionCommand::Setup) = command {
            return self.call(command, None, message, id, output);
        }
        let was_attached = self.transport.is_some();
        let unavailable = self.attach(inline).err();
        if inline.is_none() && !was_attached && self.transport.is_some() {
            tools_changed(output)?;
        }
        let status_is_local = self.transport.is_none()
            || matches!(self.worker, Worker::Running(_) | Worker::Finished(Err(_)))
            || finished;
        match command {
            Some(command @ ExtensionCommand::Doctor) => {
                self.call(command, unavailable.as_ref(), message, id, output)
            }
            Some(command @ ExtensionCommand::Status) if status_is_local => {
                self.call(command, unavailable.as_ref(), message, id, output)
            }
            _ => self.forward(&request, id, output, unavailable),
        }
    }

    fn local_tool(
        message: &Value,
        id: &Value,
        output: &mut impl Write,
    ) -> Result<bool, BridgeError> {
        if message["params"]["name"] == agents_extension::TOOL {
            let result = CallToolRequestParams::deserialize(&message["params"]).map_or_else(
                |_| {
                    failure(
                        agents_extension::ARGUMENTS_ERROR,
                        "Use the extension tool's arguments.",
                    )
                },
                agents_extension::mcp_call,
            );
            return response(output, id, result).map(|()| true);
        }
        if let Some(
            operation @ (crate::diagnostics::Diagnostic::UpdateCheck
            | crate::diagnostics::Diagnostic::UpdateStatus),
        ) = message["params"]["name"]
            .as_str()
            .and_then(|name| name.parse::<crate::diagnostics::Diagnostic>().ok())
        {
            let params = CallToolRequestParams::deserialize(&message["params"])
                .map_err(|_| BridgeError::Protocol)?;
            return response(output, id, crate::diagnostics::call(operation, params))
                .map(|()| true);
        }
        Ok(false)
    }

    fn initialize(
        &mut self,
        message: &Value,
        id: &Value,
        output: &mut impl Write,
    ) -> Result<(), BridgeError> {
        if self.initialize.is_some() {
            return rpc_error(
                output,
                id,
                -32600,
                "The Beankeeper extension is already initialized.",
            );
        }
        let Ok(request) = InitializeRequestParams::deserialize(&message["params"]) else {
            return rpc_error(output, id, -32602, "Invalid initialize arguments.");
        };
        let latest = ProtocolVersion::LATEST_WITH_INITIALIZE;
        let protocol = if ProtocolVersion::known_up_to(&latest).contains(&request.protocol_version)
        {
            request.protocol_version
        } else {
            latest
        };
        let mut info = match server_config() {
            Ok(config) => config.with_protocol_version(protocol),
            Err(error) => return rpc_reply(output, id, Err(error)),
        };
        self.initialize = Some(message.clone());
        if let Some(tools) = &mut info.capabilities.tools {
            tools.list_changed = Some(true);
        }
        if let Ok(Some(upstream)) = self.attach(None)
            && let Some(instructions) = &mut info.instructions
        {
            instructions.insert(0, ' ');
            instructions.insert_str(0, &upstream);
        }
        response(output, id, info)
    }

    fn attach(
        &mut self,
        inline: Option<&InlineProtocol<'_>>,
    ) -> Result<Option<String>, BridgeError> {
        if self.transport.is_some() || matches!(self.worker, Worker::Running(_)) {
            return Ok(None);
        }
        let initialize = self.initialize.as_ref().filter(|_| inline.is_none());
        let (transport, instructions) = connect(&service::connection()?, initialize)?;
        self.transport = Some(transport);
        Ok(instructions)
    }

    fn call(
        &mut self,
        command: ExtensionCommand,
        unavailable: Option<&BridgeError>,
        message: &Value,
        id: &Value,
        output: &mut impl Write,
    ) -> Result<(), BridgeError> {
        let request = CallToolRequestParams::deserialize(&message["params"])
            .map_err(|_| ServiceError::Arguments)
            .and_then(|params| ExtensionRequest::try_from((command, params.arguments)));
        let result = match request.map(ExtensionCommand::from) {
            Ok(ExtensionCommand::Setup) => self.worker.start().result(),
            Ok(ExtensionCommand::Status) => self.progress(unavailable).result(),
            Ok(ExtensionCommand::Doctor) => self.doctor(unavailable).result(),
            Err(error) => Ok(failure(error.code(), "This tool takes no arguments.")),
        }
        .map_err(|_| BridgeError::Protocol)?;
        response(output, id, result)
    }

    fn progress(&self, unavailable: Option<&BridgeError>) -> Progress<'_> {
        if matches!(self.worker, Worker::Finished(Ok(_))) && unavailable.is_some() {
            return readiness(unavailable);
        }
        self.worker
            .progress()
            .unwrap_or_else(|| readiness(unavailable))
    }

    fn doctor(&self, unavailable: Option<&BridgeError>) -> Progress<'_> {
        if self.transport.is_none() || matches!(self.worker, Worker::Running(_)) {
            return self.progress(unavailable);
        }
        match extension::doctor() {
            Ok(()) => self.progress(None),
            Err(error) => Progress::Failed { code: error.code() },
        }
    }

    fn forward(
        &mut self,
        request: &BridgeRequest<'_>,
        id: &Value,
        output: &mut impl Write,
        unavailable: Option<BridgeError>,
    ) -> Result<(), BridgeError> {
        match request.kind() {
            MethodKind::ToolsList if self.transport.is_none() => {
                let result = ListToolsResult::with_all_items(initial_tools(
                    request
                        .inline()
                        .is_some_and(InlineProtocol::desktop_file_imports),
                )?)
                .with_ttl_ms(0)
                .with_cache_scope(CacheScope::Private);
                return response(output, id, result);
            }
            MethodKind::ToolsCall if self.transport.is_none() => {
                let result = match PreAttach::from((&self.worker, unavailable)) {
                    PreAttach::NeverSetUp => {
                        let error = BridgeError::NotSetUp;
                        failure(error.diagnostic_code(), &error.to_string())
                    }
                    PreAttach::SetupRunning => failure(
                        "EXTENSION_SETUP_REQUIRED",
                        "Beankeeper is setting up locally. Check setup_status again shortly.",
                    ),
                    PreAttach::SetupFailed { code } => Progress::Failed { code }
                        .result()
                        .map_err(|_| BridgeError::Protocol)?,
                    PreAttach::ServiceUnavailable(error) => {
                        failure(error.diagnostic_code(), &error.to_string())
                    }
                };
                return response(output, id, result);
            }
            _ => {}
        }
        if let Some(transport) = &mut self.transport {
            transport.forward_negotiated(request, output)
        } else {
            rpc_error(
                output,
                id,
                -32601,
                "Method not available on the Beankeeper extension.",
            )
        }
    }
}

fn server_config() -> Result<ServerConfig, ErrorData> {
    let skill = agent_skill::EmbeddedSkill::get()?;
    Ok(ServerConfig::new(
        ServerCapabilities::builder()
            .enable_tools()
            .enable_resources()
            .enable_skills()
            .build(),
    )
    .with_server_info(Implementation::new("beankeeper", env!("CARGO_PKG_VERSION")))
    .with_instructions(format!(
        "{} {}",
        skill.claude_setup(),
        AgentAccess::mcp_instructions()?
    )))
}

fn readiness(unavailable: Option<&BridgeError>) -> Progress<'static> {
    match unavailable {
        Some(BridgeError::Service(error)) => error.as_ref().into(),
        Some(error) => Progress::Failed {
            code: error.diagnostic_code(),
        },
        None => Progress::Ready { registration: None },
    }
}

fn connect(
    endpoint: &Endpoint,
    initialize: Option<&Value>,
) -> Result<(Transport, Option<String>), BridgeError> {
    let mut transport = connected_transport(
        endpoint.url().clone(),
        endpoint.token_file(),
        None,
        Duration::from_secs(120),
    )?;
    transport.managed = Some(ManagedConnection {
        books: endpoint.books().into(),
        token_file: endpoint.token_file().into(),
        initialize: initialize.cloned(),
    });
    transport.extension_tools = true;
    let Some(initialize) = initialize else {
        return Ok((transport, None));
    };
    let replies = transport.exchange(initialize, &mut io::sink())?;
    let instructions = replies.iter().find_map(|reply| {
        if reply.get("id") != initialize.get("id") {
            return None;
        }
        reply["result"]["instructions"].as_str().map(str::to_owned)
    });
    transport.exchange(
        &json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
        &mut io::sink(),
    )?;
    if transport.protocol.is_none() {
        return Err(BridgeError::Protocol);
    }
    Ok((transport, instructions))
}

fn response(
    output: &mut impl Write,
    id: &Value,
    result: impl serde::Serialize,
) -> Result<(), BridgeError> {
    let result = serde_json::to_value(result).map_err(|_| BridgeError::Protocol)?;
    write_message(output, &json!({"jsonrpc":"2.0","id":id,"result":result}))
}

fn rpc_error(
    output: &mut impl Write,
    id: &Value,
    code: i32,
    message: &str,
) -> Result<(), BridgeError> {
    write_message(
        output,
        &json!({"jsonrpc":"2.0","id":id,"error":{"code":code,"message":message}}),
    )
}

fn failure(code: &str, message: &str) -> CallToolResult {
    let mut result = CallToolResult::structured_error(json!({"code":code,"message":message}));
    result.content = vec![ContentBlock::text(message)];
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unattached_call(
        worker: Worker,
        error: Option<BridgeError>,
    ) -> Result<Value, Box<dyn std::error::Error>> {
        let message = json!({"jsonrpc":"2.0","id":1,"method":"tools/call",
            "params":{"name":"company_list","arguments":{}}});
        let request = BridgeRequest::try_from(&message)?;
        let mut session = Session {
            worker,
            ..Session::default()
        };
        let mut output = Vec::new();
        session.forward(&request, &json!(1), &mut output, error)?;
        let reply: Value = serde_json::from_slice(&output)?;
        assert_eq!(reply["result"]["isError"], true);
        Ok(reply["result"]["structuredContent"].clone())
    }

    #[test]
    fn failed_setup_preserves_failure_on_ledger_call() -> Result<(), Box<dyn std::error::Error>> {
        let result = unattached_call(
            Worker::Finished(Err(ServiceError::Worker)),
            Some(ServiceError::NotInstalled.into()),
        )?;
        assert_eq!(result["state"], "failed");
        assert_eq!(result["code"], ServiceError::Worker.code());
        Ok(())
    }

    #[test]
    fn completed_setup_with_unreachable_service_preserves_error()
    -> Result<(), Box<dyn std::error::Error>> {
        for error in [
            BridgeError::from(ServiceError::NotReady),
            BridgeError::InvalidToken,
            BridgeError::Rejected(reqwest::StatusCode::UNAUTHORIZED),
            BridgeError::from(ServiceError::NotInstalled),
        ] {
            let code = error.diagnostic_code();
            let result = unattached_call(
                Worker::Finished(Ok(Registration::Checked(Vec::new()))),
                Some(error),
            )?;
            assert_eq!(result["code"], code);
        }
        Ok(())
    }

    #[test]
    fn completed_setup_with_http_failure_preserves_transport_code()
    -> Result<(), Box<dyn std::error::Error>> {
        let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
        let address = listener.local_addr()?;
        // Accept and close without a response, making the attach handshake fail.
        let server = thread::spawn(move || listener.accept().map(|_| ()));
        let error = reqwest::blocking::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(5))
            .build()?
            .get(format!("http://{address}/mcp"))
            .send()
            .expect_err("the service must fail the handshake");
        server.join().expect("fixture thread panicked")?;
        let result = unattached_call(
            Worker::Finished(Ok(Registration::Checked(Vec::new()))),
            Some(error.into()),
        )?;
        assert_eq!(result["code"], "AGENT_MCP_UNREACHABLE");
        Ok(())
    }

    #[test]
    fn never_set_up_is_the_only_not_set_up_state() -> Result<(), Box<dyn std::error::Error>> {
        let result = unattached_call(Worker::Idle, Some(ServiceError::NotInstalled.into()))?;
        assert_eq!(result["code"], "NOT_SET_UP");
        let worker = thread::spawn(|| Err(ServiceError::Worker));
        let result = unattached_call(Worker::Running(worker), None)?;
        assert_eq!(result["code"], "EXTENSION_SETUP_REQUIRED");
        Ok(())
    }

    #[test]
    fn every_upstream_tool_has_a_typed_capability_mapping() -> Result<(), BridgeError> {
        ledger_catalog(beankeeper_cli::mcp::LedgerAccess::ReadWrite.tools())?;
        Ok(())
    }

    #[test]
    fn unmapped_upstream_tool_is_rejected_instead_of_hidden() {
        let mut tools = beankeeper_cli::mcp::LedgerAccess::ReadWrite.tools();
        let mut unknown = tools[0].clone();
        unknown.name = "future_unmapped_tool".into();
        tools.push(unknown);
        assert!(matches!(ledger_catalog(tools), Err(BridgeError::Protocol)));
    }
}
