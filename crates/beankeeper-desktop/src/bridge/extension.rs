//! A Claude stdio session can expose setup before a ledger service exists.

use std::{
    io::{self, BufRead, Write},
    thread::{self, JoinHandle},
    time::Duration,
};

use rmcp::model::{
    CallToolRequestParams, CallToolResult, ContentBlock, Implementation, InitializeRequestParams,
    ListToolsResult, ProtocolVersion, ServerCapabilities, ServerConfig, Tool,
};
use serde::Deserialize;
use serde_json::{Value, json};

use super::{
    BridgeError, ManagedConnection, Transport, connected_transport, read_message, tools_changed,
    write_message,
};
use crate::{
    service::{
        self, Endpoint, ExtensionCommand, ServiceError,
        extension::{self, ExtensionRequest, Progress, Registration},
    },
    setup::AgentAccess,
};

const INSTRUCTIONS: &str = "For ‘Set up my books’ or ‘Create demo books’, call setup_status first. +    If it reports not_started, call setup. While running, poll setup_status until ready or failed. +    If setup fails, use doctor and explain the error in plain words. Once ready, refresh the tool list in this chat. +    For real books, call setup_status again for the next onboarding step. For practice books, call demo_start +    with empty arguments and use the practice tools in this chat. Never ask for a terminal, token or passphrase.";

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
        .collect()
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
        if self.initialize.is_none() {
            return rpc_error(
                output,
                id,
                -32002,
                "Initialize the Beankeeper extension first.",
            );
        }
        if message["method"] == "ping" {
            return response(output, id, json!({}));
        }
        let command = (message["method"] == "tools/call")
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
        let unavailable = self.attach().err();
        if !was_attached && self.transport.is_some() {
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
            _ => self.forward(message, id, output, unavailable.as_ref()),
        }
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
        self.initialize = Some(message.clone());
        let upstream = self.attach().ok().flatten().unwrap_or_default();
        let info = ServerConfig::new(
            ServerCapabilities::builder()
                .enable_tools()
                .enable_tool_list_changed()
                .build(),
        )
        .with_protocol_version(protocol)
        .with_server_info(Implementation::new("beankeeper", env!("CARGO_PKG_VERSION")))
        .with_instructions(format!(
            "{upstream} {INSTRUCTIONS} {}",
            AgentAccess::mcp_instructions()
        ));
        response(output, id, info)
    }

    fn attach(&mut self) -> Result<Option<String>, BridgeError> {
        if self.transport.is_some() || matches!(self.worker, Worker::Running(_)) {
            return Ok(None);
        }
        let initialize = self.initialize.as_ref().ok_or(BridgeError::Protocol)?;
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
        message: &Value,
        id: &Value,
        output: &mut impl Write,
        unavailable: Option<&BridgeError>,
    ) -> Result<(), BridgeError> {
        match message["method"].as_str() {
            Some("tools/list") if self.transport.is_none() => {
                return response(output, id, ListToolsResult::with_all_items(tools(true)));
            }
            Some("tools/call") if self.transport.is_none() => {
                let result = match self.progress(unavailable) {
                    progress @ Progress::Failed { .. } => {
                        progress.result().map_err(|_| BridgeError::Protocol)?
                    }
                    _ => failure(
                        "EXTENSION_SETUP_REQUIRED",
                        "Call setup to set up Beankeeper, then check setup_status.",
                    ),
                };
                return response(output, id, result);
            }
            _ => {}
        }
        if let Some(transport) = &mut self.transport {
            transport.forward_message(message, output)
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
    initialize: &Value,
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
        initialize: Some(initialize.clone()),
    });
    transport.extension_tools = true;
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
