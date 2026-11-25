use std::{
    error::Error,
    fmt,
    fs::File,
    io::{self, Read},
    process::ExitCode,
    thread,
    time::{Duration, Instant},
};

use beankeeper_cli::cli::{Cli, InputSource};
use clap::Args;
use reqwest::{Url, blocking::Client, header::HeaderValue, redirect::Policy};
use rmcp::{
    model::{CallToolResult, GetTaskResult, TaskPayload},
    transport::common::http_header::{
        HEADER_MCP_METHOD, HEADER_MCP_NAME, HEADER_MCP_PROTOCOL_VERSION,
    },
};
use secrecy::ExposeSecret;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{
    bridge::{self, BridgeError},
    instance::ListenerAddress,
    output::{Envelope, Metadata},
    server,
    service::{self, LinkBooks, ServiceError},
};

use super::{
    META_CAPABILITIES, TOOL,
    protocol::{
        ImportCode, ImportMethod, ImportReply, META_VERSION, PollInterval, RpcCall, TaskMethod,
        ToolCall, VERSION,
    },
    request::{ImportFields, ReadyImport},
    tasks::{ImportTaskId, TASKS},
    upload::{MAX_BYTES, StatementText},
};

/// Import a statement through the running Desktop's audited ledger import.
#[derive(Args, Debug)]
pub struct ImportOptions {
    /// Local statement file, or '-' for standard input.
    file: InputSource,
    #[command(flatten)]
    fields: ImportFields,
    /// Select the running Desktop listener for these books.
    #[arg(long)]
    bind: Option<ListenerAddress>,
}

impl TryFrom<&InputSource> for StatementText {
    type Error = CommandError;

    fn try_from(source: &InputSource) -> Result<Self, Self::Error> {
        match source {
            InputSource::Path(path) => {
                Self::try_from(File::open(path)?.take((MAX_BYTES + 1) as u64))
            }
            InputSource::Stdin => Self::try_from(io::stdin().lock().take((MAX_BYTES + 1) as u64)),
        }
    }
}

impl<R: Read> TryFrom<io::Take<R>> for StatementText {
    type Error = CommandError;

    fn try_from(mut input: io::Take<R>) -> Result<Self, Self::Error> {
        let mut bytes = Vec::new();
        input.read_to_end(&mut bytes)?;
        let text = String::from_utf8(bytes).map_err(|_| CommandError::InvalidSource)?;
        Self::try_from(text.into_boxed_str()).map_err(|_| CommandError::InvalidSource)
    }
}

#[derive(Debug)]
pub(super) enum CommandError {
    Io(io::Error),
    InvalidSource,
    Service(ServiceError),
    Transport(BridgeError),
    ResultUnavailable,
    Rejected { code: ImportCode, message: String },
}

impl CommandError {
    fn code(&self) -> &ImportCode {
        match self {
            Self::InvalidSource => &ImportCode::Validation,
            Self::Io(_) => &ImportCode::Io,
            Self::Service(_) => &ImportCode::BooksNotServed,
            Self::Transport(BridgeError::Rejected(reqwest::StatusCode::UNAUTHORIZED)) => {
                &ImportCode::Unauthorized
            }
            Self::Transport(BridgeError::Rejected(reqwest::StatusCode::FORBIDDEN)) => {
                &ImportCode::Forbidden
            }
            Self::Transport(_) | Self::ResultUnavailable => &ImportCode::Unavailable,
            Self::Rejected { code, .. } => code,
        }
    }
}

impl fmt::Display for CommandError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => error.fmt(f),
            Self::Service(error) => error.fmt(f),
            Self::Transport(error) => error.fmt(f),
            Self::InvalidSource => f.write_str("Use a nonempty UTF-8 statement of at most 1 MiB."),
            Self::ResultUnavailable => f.write_str("The statement result is unavailable. If you retry, reuse the same import reference."),
            Self::Rejected { message, .. } => f.write_str(message),
        }
    }
}

impl Error for CommandError {}

impl From<io::Error> for CommandError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<BridgeError> for CommandError {
    fn from(error: BridgeError) -> Self {
        Self::Transport(error)
    }
}

impl From<reqwest::Error> for CommandError {
    fn from(error: reqwest::Error) -> Self {
        Self::Transport(BridgeError::from(error))
    }
}

struct Connection {
    client: Client,
    url: Url,
    authorization: HeaderValue,
}

impl Connection {
    fn connect(
        cli: &Cli,
        options: &ImportOptions,
        target: LinkBooks<'_>,
    ) -> Result<Self, CommandError> {
        let (url, authorization) = if let Some(token) = &cli.agent_token {
            let address =
                service::link_address(target, options.bind).map_err(CommandError::Service)?;
            let url = Url::parse(&format!("{}mcp", server::local_url(address.into())))
                .map_err(|_| BridgeError::InvalidUrl)?;
            (url, bridge::bearer_header(token.expose_secret())?)
        } else {
            let endpoint = service::connection().map_err(CommandError::Service)?;
            let address = service::link_address(LinkBooks::Explicit(endpoint.books()), None)
                .map_err(CommandError::Service)?;
            let requested =
                service::link_address(target, options.bind).map_err(CommandError::Service)?;
            if std::net::SocketAddr::from(address) != std::net::SocketAddr::from(requested) {
                return Err(CommandError::Rejected {
                    code: ImportCode::Unauthorized,
                    message: "Supply an agent credential for the selected Desktop listener.".into(),
                });
            }
            (
                endpoint.url().clone(),
                bridge::authorization(endpoint.token_file())?,
            )
        };
        let client = Client::builder()
            .no_proxy()
            .redirect(Policy::none())
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_mins(2))
            .build()?;
        Ok(Self {
            client,
            url,
            authorization,
        })
    }

    fn request(
        &self,
        method: ImportMethod,
        name: &str,
        params: impl Serialize,
    ) -> Result<Value, CommandError> {
        let id = json!(1);
        let call = RpcCall::from((&id, &method, params));
        let response = self
            .client
            .post(self.url.clone())
            .header(reqwest::header::AUTHORIZATION, self.authorization.clone())
            .header("accept", "application/json, text/event-stream")
            .header(HEADER_MCP_PROTOCOL_VERSION, VERSION.as_str())
            .header(HEADER_MCP_METHOD, method.as_ref())
            .header(HEADER_MCP_NAME, name)
            .json(&call)
            .send()?;
        Ok(bridge::response_result(response)?)
    }

    fn import(
        &self,
        arguments: &ReadyImport,
        show_progress: bool,
    ) -> Result<CallToolResult, CommandError> {
        let metadata = Self::metadata();
        let params = ToolCall::from((arguments, &metadata, None));
        let result = self.request(ImportMethod::CallTool, TOOL, params)?;
        match ImportReply::deserialize(result).map_err(|_| CommandError::ResultUnavailable)? {
            ImportReply::Complete(result) => Ok(result),
            ImportReply::Task(task) => {
                let id: ImportTaskId = task
                    .task
                    .task_id
                    .parse()
                    .map_err(|_| CommandError::ResultUnavailable)?;
                self.poll(&id, show_progress)
            }
            ImportReply::Upload(_upload) => Err(CommandError::ResultUnavailable),
        }
    }

    fn metadata() -> Value {
        json!({
            META_VERSION: VERSION,
            "io.modelcontextprotocol/clientInfo": {"name":"bk import","version":env!("CARGO_PKG_VERSION")},
            META_CAPABILITIES: {"extensions":{ TASKS: {} }}
        })
    }

    fn poll(&self, id: &ImportTaskId, show_progress: bool) -> Result<CallToolResult, CommandError> {
        let limit = Instant::now() + super::RETENTION;
        let mut previous = None;
        let id = id.to_string();
        let params = json!({"taskId": id, "_meta": Self::metadata()});
        while Instant::now() < limit {
            let response = self.request(ImportMethod::Task(TaskMethod::Get), &id, &params)?;
            let result: GetTaskResult =
                serde_json::from_value(response).map_err(|_| CommandError::ResultUnavailable)?;
            match result.task.payload {
                TaskPayload::Working => {
                    let task = result.task.task;
                    if show_progress
                        && previous != task.status_message
                        && let Some(message) = &task.status_message
                    {
                        eprintln!("{message}");
                    }
                    previous = task.status_message;
                    thread::sleep(PollInterval::from(task.poll_interval_ms).into());
                }
                TaskPayload::Completed { result } => {
                    return serde_json::from_value(Value::Object(result))
                        .map_err(|_| CommandError::ResultUnavailable);
                }
                _ => return Err(CommandError::ResultUnavailable),
            }
        }
        Err(CommandError::ResultUnavailable)
    }
}

fn execute(
    options: &ImportOptions,
    cli: &Cli,
    target: LinkBooks<'_>,
) -> Result<Value, CommandError> {
    let statement = StatementText::try_from(&options.file)?;
    let connection = Connection::connect(cli, options, target)?;
    let result = connection.import(&(&options.fields, statement).into(), !cli.verbosity.quiet)?;
    let data = result
        .structured_content
        .ok_or(CommandError::ResultUnavailable)?;
    if result.is_error == Some(true) {
        return Err(CommandError::Rejected {
            code: data["code"]
                .as_str()
                .map(ImportCode::from)
                .unwrap_or_default(),
            message: data["message"]
                .as_str()
                .unwrap_or("Statement import failed.")
                .into(),
        });
    }
    Ok(data)
}

/// Print the import receipt and its counts, using a JSON envelope when selected.
pub fn run(options: &ImportOptions, cli: &Cli, target: LinkBooks<'_>) -> ExitCode {
    let result = execute(options, cli, target);
    let meta = Metadata::new("import");
    match result {
        Ok(data) => {
            if cli.is_json() {
                let envelope = Envelope {
                    ok: true,
                    meta,
                    data: Some(data),
                    error: None::<Value>,
                };
                println!("{}", json!(envelope));
            } else {
                let counts = &data["counts"];
                println!(
                    "Recorded {} and skipped {} duplicates. {} pending. {} need categorization. Receipt {}.",
                    counts["recorded"],
                    counts["skipped_duplicates"],
                    counts["pending"],
                    counts["uncertain"]
                        .as_u64()
                        .map_or_else(|| "Unknown how many".into(), |count| count.to_string()),
                    data["receipt_id"]
                );
            }
            ExitCode::SUCCESS
        }
        Err(error) => {
            if cli.is_json() {
                println!(
                    "{}",
                    json!({"ok":false,"meta":meta,"error":{"code":error.code(),"message":error.to_string()}})
                );
            } else {
                eprintln!("{error}");
            }
            ExitCode::FAILURE
        }
    }
}
