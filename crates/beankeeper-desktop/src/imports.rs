//! Desktop's MCP transport for the existing audited statement import.

pub mod command;
mod protocol;
mod request;
mod tasks;
mod upload;

use std::{
    str::FromStr,
    sync::{Arc, Mutex, MutexGuard},
    time::Duration,
};

use axum::{
    Json, Router,
    body::{Body, to_bytes},
    extract::{Path, Request, State},
    http::{HeaderMap, Method, StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::put,
};
use beankeeper_cli::{
    db::Db,
    error::CliError,
    identity,
    mcp::{LEDGER_TARGET_HEADER, LEDGER_TARGET_META_KEY},
    operation::Operation,
};
use rmcp::{
    model::{CallToolResult, ProtocolVersion, TaskAckResult},
    transport::common::http_header::{
        HEADER_MCP_METHOD, HEADER_MCP_NAME, HEADER_MCP_PROTOCOL_VERSION,
    },
};
use secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Serialize, de::value::StrDeserializer};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

use crate::entitlement::LedgerAccess;
use crate::pairing::{AgentKey, Secret};
use protocol::{ImportCode, ImportMethod, META_VERSION, RpcCall, TaskMethod, ToolCall, VERSION};
use request::ImportRequest;
use tasks::{ImportTaskId, TASKS, Tasks};
pub(crate) use upload::FILES;
use upload::{UPLOAD_ROUTE, Uploads};

const TOOL: &str = "ledger_setup_statement_import";
pub(crate) const META_CAPABILITIES: &str = "io.modelcontextprotocol/clientCapabilities";
const MAX_RESPONSE: usize = 16 * 1024 * 1024;
const RETENTION: Duration = Duration::from_mins(15);

type Principal = AgentKey;

#[derive(Clone, PartialEq, Eq, Hash)]
struct Ticket([u8; 32]);

impl Ticket {
    fn generate() -> Result<(Self, Secret), ImportError> {
        let secret = Secret::generate().map_err(|_| ImportError::Unavailable)?;
        Ok((Self::from(&secret), secret))
    }
}

impl From<&Secret> for Ticket {
    fn from(secret: &Secret) -> Self {
        Self(Sha256::digest(secret.expose().as_bytes()).into())
    }
}

impl FromStr for Ticket {
    type Err = ImportError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let secret: Secret = value.parse().map_err(|_| ImportError::Unknown)?;
        Ok(Self::from(&secret))
    }
}

#[derive(Debug)]
enum ImportError {
    Validation(&'static str),
    MissingCapability(&'static str),
    Unknown,
    Busy,
    Unavailable,
    Authorization(CliError),
}

impl ImportError {
    fn respond(self, id: &Value) -> Response {
        match self {
            Self::Validation(message) => reply(id, tool_failure(&ImportCode::Validation, message)),
            Self::Busy => reply(
                id,
                tool_failure(
                    &ImportCode::Busy,
                    "Statement import capacity is full. Try again shortly.",
                ),
            ),
            Self::Unavailable => reply(
                id,
                tool_failure(
                    &ImportCode::Unavailable,
                    "Statement import state is unavailable.",
                ),
            ),
            Self::Unknown => rpc_error(
                id,
                -32602,
                "Unknown, expired, or mismatched statement upload or task.",
                &Value::Null,
            ),
            Self::MissingCapability(extension) => {
                let settings = if extension == FILES {
                    json!({"upload": {}})
                } else {
                    json!({})
                };
                rpc_error(
                    id,
                    -32021,
                    "Missing required client capability.",
                    &json!({"requiredCapabilities":{"extensions":{extension:settings}}}),
                )
            }
            Self::Authorization(error @ CliError::Unauthorized(_)) => (
                StatusCode::UNAUTHORIZED,
                [(header::WWW_AUTHENTICATE, "Bearer")],
                Json(json!({"code":ImportCode::Unauthorized,"message":error.to_string()})),
            )
                .into_response(),
            Self::Authorization(error) => reply(
                id,
                tool_failure(&ImportCode::from(error.error_code()), &error.to_string()),
            ),
        }
    }
}

fn rpc_error(id: &Value, code: i32, message: &str, data: &Value) -> Response {
    Json(json!({"jsonrpc":"2.0","id":id,"error":{"code":code,"message":message,"data":data}}))
        .into_response()
}

fn reply(id: &Value, result: impl Serialize) -> Response {
    no_store(Json(json!({"jsonrpc":"2.0","id":id,"result":result})))
}

fn no_store(response: impl IntoResponse) -> Response {
    ([(header::CACHE_CONTROL, "no-store")], response).into_response()
}

fn tool_failure(code: &ImportCode, message: &str) -> CallToolResult {
    CallToolResult::structured_error(json!({"code":code,"message":message}))
}

#[derive(Clone)]
struct Imports {
    uploads: Arc<Mutex<Uploads>>,
    tasks: Arc<Mutex<Tasks>>,
    db: Arc<Mutex<Db>>,
    access: LedgerAccess,
    fallback: Option<SecretString>,
    request_limit: usize,
    transfers: Arc<tokio::sync::Semaphore>,
}

fn locked<T>(mutex: &Mutex<T>) -> Result<MutexGuard<'_, T>, ImportError> {
    mutex.lock().map_err(|_| ImportError::Unavailable)
}

#[derive(Clone, Copy)]
enum ImportAccess {
    Tool,
    Transfer,
}

impl ImportAccess {
    fn fallback(self, token: Option<&SecretString>) -> Option<&SecretString> {
        match self {
            Self::Tool => token,
            Self::Transfer => None,
        }
    }

    fn authorize(self, ledger: &LedgerAccess) -> Result<(), CliError> {
        match self {
            Self::Tool => Ok(()),
            Self::Transfer => ledger.ordinary_write(),
        }
    }
}

impl Imports {
    async fn authorize(
        &self,
        headers: &HeaderMap,
        import: ImportAccess,
    ) -> Result<Principal, ImportError> {
        let token = bearer(headers, import.fallback(self.fallback.as_ref()))?;
        let db = Arc::clone(&self.db);
        let access = self.access.clone();
        tokio::task::spawn_blocking(move || {
            let db = locked(&db)?;
            let conn = db.read();
            let identity = identity::authorize_operation(
                conn,
                Some(token.expose_secret()),
                Operation::LedgerSetupStatementImport.name(),
            )
            .map_err(ImportError::Authorization)?
            .ok_or_else(missing_bearer)?;
            import
                .authorize(&access)
                .map_err(ImportError::Authorization)?;
            Ok(AgentKey::from_verified_identity(&identity))
        })
        .await
        .map_err(|_| ImportError::Unavailable)?
    }
}

fn missing_bearer() -> ImportError {
    ImportError::Authorization(CliError::Unauthorized(
        "HTTP requires a valid Authorization: Bearer agent token".into(),
    ))
}

fn bearer(
    headers: &HeaderMap,
    fallback: Option<&SecretString>,
) -> Result<SecretString, ImportError> {
    let mut values = headers.get_all(header::AUTHORIZATION).iter();
    let Some(value) = values.next() else {
        return fallback.cloned().ok_or_else(missing_bearer);
    };
    if values.next().is_some() {
        return Err(missing_bearer());
    }
    let (_, token) = value
        .to_str()
        .ok()
        .and_then(|value| value.split_once(' '))
        .filter(|(scheme, token)| {
            scheme.eq_ignore_ascii_case("Bearer")
                && !token.is_empty()
                && !token.bytes().any(|byte| byte.is_ascii_whitespace())
        })
        .ok_or_else(missing_bearer)?;
    Ok(SecretString::from(token))
}

/// Add transport extensions around the upstream authenticated import tool.
pub(crate) fn layer(
    mcp: Router,
    db: Arc<Mutex<Db>>,
    access: LedgerAccess,
    fallback: Option<SecretString>,
    request_limit: usize,
) -> Router {
    let state = Imports {
        uploads: Arc::default(),
        tasks: Arc::default(),
        db,
        access,
        fallback,
        request_limit,
        transfers: Arc::new(tokio::sync::Semaphore::new(4)),
    };
    mcp.layer(middleware::from_fn_with_state(state.clone(), intercept))
        .merge(
            Router::new()
                .route(UPLOAD_ROUTE, put(transfer))
                .with_state(state),
        )
}

async fn transfer(
    State(state): State<Imports>,
    Path(transfer): Path<String>,
    request: Request,
) -> Response {
    let Ok(transfer) = transfer.parse::<Ticket>() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let principal = match state
        .authorize(request.headers(), ImportAccess::Transfer)
        .await
    {
        Ok(principal) => principal,
        Err(ImportError::Authorization(CliError::Unauthorized(_))) => {
            return StatusCode::UNAUTHORIZED.into_response();
        }
        Err(ImportError::Authorization(error)) => {
            return (
                StatusCode::FORBIDDEN,
                Json(beankeeper_cli::output::json::EnvelopeError::from(&error)),
            )
                .into_response();
        }
        Err(_) => return StatusCode::SERVICE_UNAVAILABLE.into_response(),
    };
    let Ok(size) =
        locked(&state.uploads).and_then(|mut uploads| uploads.expected_size(&transfer, &principal))
    else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let Ok(_permit) = state.transfers.try_acquire() else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    let Ok(Ok(bytes)) =
        tokio::time::timeout(Duration::from_secs(30), to_bytes(request.into_body(), size)).await
    else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    match locked(&state.uploads).and_then(|mut uploads| uploads.stage(&transfer, &principal, bytes))
    {
        Ok(()) => StatusCode::NO_CONTENT,
        Err(ImportError::Validation(_)) => StatusCode::BAD_REQUEST,
        Err(ImportError::Unknown) => StatusCode::NOT_FOUND,
        Err(_) => StatusCode::SERVICE_UNAVAILABLE,
    }
    .into_response()
}

/// The header selects a candidate before reading the request body.
fn extension_method(request: &Request) -> Option<(ImportMethod, ProtocolVersion)> {
    let headers = request.headers();
    if request.method() != Method::POST || headers.contains_key(LEDGER_TARGET_HEADER) {
        return None;
    }
    let version = headers.get(HEADER_MCP_PROTOCOL_VERSION)?.to_str().ok()?;
    let version =
        ProtocolVersion::deserialize(StrDeserializer::<serde::de::value::Error>::new(version))
            .ok()?;
    if version != VERSION {
        return None;
    }
    let method: ImportMethod = headers
        .get(HEADER_MCP_METHOD)?
        .to_str()
        .ok()?
        .parse()
        .ok()?;
    if method == ImportMethod::CallTool
        && headers.get(HEADER_MCP_NAME).is_none_or(|name| name != TOOL)
    {
        return None;
    }
    Some((method, version))
}

async fn intercept(State(state): State<Imports>, request: Request, next: Next) -> Response {
    let Some((method, version)) = extension_method(&request) else {
        return next.run(request).await;
    };
    let (parts, body) = request.into_parts();
    let Ok(bytes) = to_bytes(body, state.request_limit).await else {
        return StatusCode::PAYLOAD_TOO_LARGE.into_response();
    };
    let request = Request::from_parts(parts, Body::from(bytes.clone()));
    let Ok(message) = serde_json::from_slice::<Value>(&bytes) else {
        return next.run(request).await;
    };
    let Some(id) = message.get("id").filter(|id| id.is_string() || id.is_i64()) else {
        return next.run(request).await;
    };
    let params = &message["params"];
    let metadata = &params["_meta"];
    if message["jsonrpc"] != "2.0"
        || message["method"].as_str() != Some(method.as_ref())
        || metadata.get(LEDGER_TARGET_META_KEY).is_some()
    {
        return next.run(request).await;
    }
    let capabilities = &metadata[META_CAPABILITIES]["extensions"];
    let files =
        capabilities[FILES]["upload"].is_object() && capabilities[FILES].get("revision").is_none();
    let tasks = capabilities[TASKS].is_object();
    let versions_agree =
        || ProtocolVersion::deserialize(&metadata[META_VERSION]).is_ok_and(|body| body == version);
    match method {
        ImportMethod::Discover => discovery(request, next, id).await,
        ImportMethod::ListTools if files => tool_list(request, next, id).await,
        ImportMethod::CallTool if params["name"] == TOOL && versions_agree() => state
            .import(id, params, request, next, files, tasks)
            .await
            .unwrap_or_else(|error| error.respond(id)),
        ImportMethod::Task(method) if versions_agree() => state
            .task_request(method, params, request.headers(), tasks)
            .await
            .map_or_else(|error| error.respond(id), |result| reply(id, result)),
        ImportMethod::ListTools | ImportMethod::CallTool | ImportMethod::Task(_) => {
            next.run(request).await
        }
    }
}

async fn discovery(request: Request, next: Next, id: &Value) -> Response {
    amend(request, next, id, |result| {
        let extensions = &mut result["capabilities"]["extensions"];
        extensions[FILES] = json!({"upload": {}});
        extensions[TASKS] = json!({});
    })
    .await
}

async fn tool_list(request: Request, next: Next, id: &Value) -> Response {
    amend(request, next, id, |result| {
        if let Some(tool) = result["tools"]
            .as_array_mut()
            .and_then(|tools| tools.iter_mut().find(|tool| tool["name"] == TOOL))
            && let Some(schema) = tool["inputSchema"].as_object_mut()
        {
            schema.entry("properties").or_insert_with(|| json!({}))["file"] = upload::schema();
            if let Some(required) = schema.get_mut("required").and_then(Value::as_array_mut) {
                required.retain(|field| field != "content");
            }
            schema.insert(
                "oneOf".into(),
                json!([{"required":["file"]},{"required":["content"]}]),
            );
        }
    })
    .await
}

async fn amend(
    request: Request,
    next: Next,
    id: &Value,
    adjust: impl FnOnce(&mut Value),
) -> Response {
    let response = next.run(request).await;
    if !response.status().is_success() {
        return response;
    }
    match read_rpc(response, id).await {
        Ok(mut message) => {
            if let Some(result) = message.get_mut("result") {
                adjust(result);
            }
            no_store(Json(message))
        }
        Err(error) => error.respond(id),
    }
}

async fn read_rpc(response: Response, id: &Value) -> Result<Value, ImportError> {
    let (parts, body) = response.into_parts();
    let bytes = to_bytes(body, MAX_RESPONSE)
        .await
        .map_err(|_| ImportError::Unavailable)?;
    let sse = parts
        .headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.starts_with("text/event-stream"));
    let response = if sse {
        std::str::from_utf8(&bytes).ok().and_then(|text| {
            text.lines()
                .filter_map(|line| line.strip_prefix("data:"))
                .filter_map(|data| serde_json::from_str::<Value>(data).ok())
                .find(|message| &message["id"] == id)
        })
    } else {
        serde_json::from_slice(&bytes).ok()
    };
    response.ok_or(ImportError::Unavailable)
}

impl Imports {
    async fn task_request(
        &self,
        method: TaskMethod,
        params: &Value,
        headers: &HeaderMap,
        supported: bool,
    ) -> Result<Value, ImportError> {
        let task_id = params["taskId"].as_str().ok_or(ImportError::Unknown)?;
        if headers
            .get(HEADER_MCP_NAME)
            .is_none_or(|name| name != task_id)
        {
            return Err(ImportError::Unknown);
        }
        if !supported {
            return Err(ImportError::MissingCapability(TASKS));
        }
        let principal = self.authorize(headers, ImportAccess::Tool).await?;
        let id: ImportTaskId = task_id.parse()?;
        let mut tasks = locked(&self.tasks)?;
        let result = match method {
            TaskMethod::Get => serde_json::to_value(tasks.get(&id, &principal)?),
            TaskMethod::Cancel => {
                tasks.cancel(&id, &principal)?;
                serde_json::to_value(TaskAckResult::new())
            }
            TaskMethod::Update => {
                tasks.get(&id, &principal)?;
                serde_json::to_value(TaskAckResult::new())
            }
        };
        result.map_err(|_| ImportError::Unavailable)
    }

    async fn import(
        &self,
        id: &Value,
        params: &Value,
        request: Request,
        next: Next,
        files: bool,
        tasks: bool,
    ) -> Result<Response, ImportError> {
        let request_state = params
            .get("requestState")
            .map(|state| {
                state
                    .as_str()
                    .ok_or(ImportError::Unknown)?
                    .parse::<Ticket>()
            })
            .transpose()?;
        let arguments = params["arguments"]
            .as_object()
            .ok_or(ImportError::Validation("Expected statement arguments."))?;
        // File transfers require an explicit credential, including malformed file requests.
        let import = if arguments.contains_key("file") {
            ImportAccess::Transfer
        } else {
            ImportAccess::Tool
        };
        let principal = self.authorize(request.headers(), import).await?;
        let arguments = ImportRequest::try_from(arguments)?;
        let arguments = match (arguments, request_state) {
            (ImportRequest::Inline(arguments), None) => arguments,
            (ImportRequest::Inline(_), Some(_)) => return Err(ImportError::Unknown),
            (ImportRequest::File(_), _) if !files => {
                return Err(ImportError::MissingCapability(FILES));
            }
            (ImportRequest::File(arguments), Some(ticket)) => {
                locked(&self.uploads)?.confirm(&ticket, &principal, &arguments)?
            }
            (ImportRequest::File(arguments), None) => {
                let result = locked(&self.uploads)?.issue(principal, arguments)?;
                let uploads = Arc::downgrade(&self.uploads);
                tokio::spawn(async move {
                    tokio::time::sleep(RETENTION).await;
                    if let Some(uploads) = uploads.upgrade()
                        && let Ok(mut uploads) = uploads.lock()
                    {
                        uploads.prune(std::time::Instant::now());
                    }
                });
                return Ok(reply(id, result));
            }
        };
        let large = arguments.content().uses_background_task();
        let destination = arguments.summary_scope();
        let call = ToolCall::from((&arguments, &params["_meta"], params.get("inputResponses")));
        let call = RpcCall::from((id, &ImportMethod::CallTool, call));
        let body = serde_json::to_vec(&call).map_err(|_| ImportError::Unavailable)?;
        let (mut parts, _) = request.into_parts();
        parts.headers.remove(header::CONTENT_LENGTH);
        let request = Request::from_parts(parts, Body::from(body));
        if large && tasks {
            let (task, handle) = locked(&self.tasks)?.enqueue(principal)?;
            let state = self.clone();
            let response_id = id.clone();
            tokio::spawn(async move {
                if !locked(&state.tasks).is_ok_and(|mut tasks| tasks.begin(&task)) {
                    return;
                }
                let result = state
                    .execute(&response_id, request, next, destination)
                    .await;
                if let Ok(mut tasks) = locked(&state.tasks) {
                    tasks.finish(&task, task_outcome(&result));
                }
            });
            Ok(reply(id, handle))
        } else {
            Ok(no_store(Json(
                self.execute(id, request, next, destination).await,
            )))
        }
    }

    async fn execute(
        &self,
        id: &Value,
        request: Request,
        next: Next,
        destination: summary::Scope,
    ) -> Value {
        let response = next.run(request).await;
        if !response.status().is_success() {
            let (code, message) = if response.status() == StatusCode::UNAUTHORIZED {
                (
                    ImportCode::Unauthorized,
                    "The import credential is no longer valid.",
                )
            } else {
                (
                    ImportCode::Unavailable,
                    "The ledger did not return the statement import result. Retry with the same reference.",
                )
            };
            return json!({"jsonrpc":"2.0","id":id,"result":tool_failure(&code,message)});
        }
        let Ok(mut response) = read_rpc(response, id).await else {
            return json!({"jsonrpc":"2.0","id":id,"result":tool_failure(&ImportCode::Unavailable,"The ledger did not return the statement import result. Retry with the same reference.")});
        };
        if let Some(error) = response.get_mut("error") {
            let code = error["code"].as_i64().unwrap_or(-32603);
            *error = json!({"code":code,"message":"The ledger could not process the statement import request."});
            return response;
        }
        let result = &mut response["result"];
        if result["isError"] == true {
            let code = result["structuredContent"]["code"]
                .as_str()
                .map(ImportCode::from)
                .unwrap_or_default();
            // Parsers can include source rows in diagnostic text. Keep the named
            // error, and never pass source-dependent messages to the client.
            *result = json!(tool_failure(
                &code,
                "Statement import failed. Check the format, accounts, and stable reference before retrying."
            ));
        } else if let Some(data) = result.get_mut("structuredContent") {
            if let Some(message) = destination.summarize(Arc::clone(&self.db), data).await {
                result["content"] = json!([{"type":"text","text":message}]);
            }
        } else {
            return json!({"jsonrpc":"2.0","id":id,"result":tool_failure(&ImportCode::Unavailable,"The ledger did not return the statement import result. Retry with the same reference.")});
        }
        response
    }
}

fn task_outcome(response: &Value) -> Result<Map<String, Value>, Map<String, Value>> {
    if let Some(result) = response["result"].as_object() {
        Ok(result.clone())
    } else if let Some(error) = response["error"].as_object() {
        Err(error.clone())
    } else {
        Err(Map::from_iter([
            ("code".to_owned(), json!(-32603)),
            (
                "message".to_owned(),
                json!("The statement import did not return a result."),
            ),
        ]))
    }
}

mod summary;
