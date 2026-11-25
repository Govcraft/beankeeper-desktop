use std::{
    error::Error,
    fmt,
    fs::File,
    io::{self, Read},
    process::ExitCode,
    str::FromStr,
    thread,
    time::{Duration, Instant},
};

use beankeeper::types::{AccountCode, Currency};
use beankeeper_cli::{
    cli::{Cli, InputSource},
    commands::setup::{SetupReference, StatementFormat},
    db::{companies::CompanySlug, holds::HoldMatchOptions},
};
use clap::Args;
use reqwest::{Url, blocking::Client, header::HeaderValue, redirect::Policy};
use rmcp::model::{CallToolResult, CreateTaskResult, GetTaskResult, TaskPayload};
use secrecy::ExposeSecret;
use serde::Serialize;
use serde_json::{Value, json};

use crate::{
    bridge::{self, BridgeError},
    instance::ListenerAddress,
    output::{Envelope, Metadata},
    server,
    service::{self, LinkBooks, ServiceError},
};

use super::{META_CAPABILITIES, META_VERSION, TOOL, VERSION, tasks::TASKS, upload::MAX_BYTES};

/// Import a statement through the running Desktop's audited ledger import.
#[derive(Args, Debug)]
pub struct ImportOptions {
    /// Local statement file, or '-' for standard input.
    file: InputSource,
    /// Book that owns the bank account.
    #[arg(long = "book")]
    company: CompanySlug,
    /// Stable import reference. Reuse it when retrying the same statement.
    #[arg(long)]
    reference: SetupReference,
    /// Bank account code.
    #[arg(long)]
    bank: AccountCode,
    /// Account to use for entries without a category rule.
    #[arg(long)]
    suspense: AccountCode,
    /// Statement currency.
    #[arg(long)]
    currency: Currency,
    /// Source format. Use ofx for OFX or QFX.
    #[arg(long, value_enum)]
    statement_format: StatementFormat,
    /// Select the running Desktop listener for these books.
    #[arg(long)]
    bind: Option<ListenerAddress>,
    /// Optional CSV column mapping as a JSON object.
    #[arg(long)]
    csv: Option<CsvMappingInput>,
    #[command(flatten)]
    matching: HoldMatchOptions,
}

#[derive(Serialize)]
struct ImportArguments<'a> {
    company: &'a CompanySlug,
    reference: String,
    bank: &'a AccountCode,
    suspense: &'a AccountCode,
    currency: &'static str,
    format: StatementFormat,
    content: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    csv: Option<&'a CsvMappingInput>,
    #[serde(flatten)]
    matching: HoldMatchOptions,
}

#[derive(Clone, Debug, Serialize)]
#[serde(transparent)]
struct CsvMappingInput(serde_json::Map<String, Value>);

impl FromStr for CsvMappingInput {
    type Err = &'static str;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        serde_json::from_str(value)
            .map(Self)
            .map_err(|_| "Expected a JSON object with CSV mapping fields.")
    }
}

struct LocalStatement(String);

impl TryFrom<&InputSource> for LocalStatement {
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

impl<R: Read> TryFrom<io::Take<R>> for LocalStatement {
    type Error = CommandError;

    fn try_from(mut input: io::Take<R>) -> Result<Self, Self::Error> {
        let mut bytes = Vec::new();
        input.read_to_end(&mut bytes)?;
        if bytes.is_empty() || bytes.len() > MAX_BYTES {
            return Err(CommandError::InvalidSource);
        }
        let text = String::from_utf8(bytes).map_err(|_| CommandError::InvalidSource)?;
        Ok(Self(text))
    }
}

impl<'a> From<(&'a ImportOptions, LocalStatement)> for ImportArguments<'a> {
    fn from((options, statement): (&'a ImportOptions, LocalStatement)) -> Self {
        Self {
            company: &options.company,
            reference: options.reference.to_string(),
            bank: &options.bank,
            suspense: &options.suspense,
            currency: options.currency.code(),
            format: options.statement_format,
            content: statement.0,
            csv: options.csv.as_ref(),
            matching: options.matching,
        }
    }
}

#[derive(Debug)]
enum CommandError {
    Io(io::Error),
    InvalidSource,
    Service(ServiceError),
    Transport(BridgeError),
    ResultUnavailable,
    Rejected { code: String, message: String },
}

impl CommandError {
    fn code(&self) -> &str {
        match self {
            Self::InvalidSource => "VALIDATION",
            Self::Io(_) => "IO",
            Self::Service(_) => "DESKTOP_BOOKS_NOT_SERVED",
            Self::Transport(BridgeError::Rejected(reqwest::StatusCode::UNAUTHORIZED)) => {
                "UNAUTHORIZED"
            }
            Self::Transport(BridgeError::Rejected(reqwest::StatusCode::FORBIDDEN)) => "FORBIDDEN",
            Self::Transport(_) | Self::ResultUnavailable => "IMPORT_UNAVAILABLE",
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
                    code: "UNAUTHORIZED".into(),
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

    fn request(&self, method: &str, mut params: Value) -> Result<Value, CommandError> {
        params["_meta"] = json!({
            META_VERSION: VERSION,
            "io.modelcontextprotocol/clientInfo": {"name":"bk import","version":env!("CARGO_PKG_VERSION")},
            META_CAPABILITIES: {"extensions":{ TASKS: {} }}
        });
        let mut request = self
            .client
            .post(self.url.clone())
            .header(reqwest::header::AUTHORIZATION, self.authorization.clone())
            .header("accept", "application/json, text/event-stream")
            .header("mcp-protocol-version", VERSION)
            .header("mcp-method", method);
        let name = if method == "tools/call" {
            TOOL
        } else {
            params["taskId"]
                .as_str()
                .ok_or(CommandError::ResultUnavailable)?
        };
        request = request.header("mcp-name", name);
        let response = request
            .json(&json!({"jsonrpc":"2.0","id":1,"method":method,"params":params}))
            .send()?;
        Ok(bridge::response_result(response)?)
    }

    fn import(
        &self,
        arguments: &ImportArguments<'_>,
        show_progress: bool,
    ) -> Result<CallToolResult, CommandError> {
        let result = self.request("tools/call", json!({"name":TOOL,"arguments":arguments}))?;
        if result["resultType"] == "task" {
            let task: CreateTaskResult =
                serde_json::from_value(result).map_err(|_| CommandError::ResultUnavailable)?;
            self.poll(&task.task.task_id, show_progress)
        } else {
            serde_json::from_value(result).map_err(|_| CommandError::ResultUnavailable)
        }
    }

    fn poll(&self, id: &str, show_progress: bool) -> Result<CallToolResult, CommandError> {
        let limit = Instant::now() + super::RETENTION;
        let mut previous = None;
        while Instant::now() < limit {
            let response = self.request("tasks/get", json!({"taskId": id}))?;
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
                    thread::sleep(Duration::from_millis(task.poll_interval_ms.unwrap_or(500)));
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
    let statement = LocalStatement::try_from(&options.file)?;
    let connection = Connection::connect(cli, options, target)?;
    let result = connection.import(&(options, statement).into(), !cli.verbosity.quiet)?;
    let data = result
        .structured_content
        .ok_or(CommandError::ResultUnavailable)?;
    if result.is_error == Some(true) {
        return Err(CommandError::Rejected {
            code: data["code"].as_str().unwrap_or("IMPORT_FAILED").into(),
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
