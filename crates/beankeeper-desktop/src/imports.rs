//! Desktop's MCP transport for the existing audited statement import.

pub mod command;
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
use rmcp::model::{CallToolResult, TaskAckResult};
use secrecy::{ExposeSecret, SecretString};
use serde::Serialize;
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

use crate::pairing::{AgentKey, Secret};
use tasks::{ImportTaskId, TASKS, Tasks};
pub(crate) use upload::FILES;
use upload::{FileSpec, UPLOAD_ROUTE, Uploads};

const TOOL: &str = "ledger_setup_statement_import";
pub(crate) const META_CAPABILITIES: &str = "io.modelcontextprotocol/clientCapabilities";
const META_VERSION: &str = "io.modelcontextprotocol/protocolVersion";
const VERSION: &str = "2026-07-28";
const MAX_RESPONSE: usize = 16 * 1024 * 1024;
const RETENTION: Duration = Duration::from_mins(15);
const TASK_THRESHOLD: usize = 64 * 1024;

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
            Self::Validation(message) => reply(id, tool_failure("VALIDATION", message)),
            Self::Busy => reply(
                id,
                tool_failure(
                    "IMPORT_BUSY",
                    "Statement import capacity is full. Try again shortly.",
                ),
            ),
            Self::Unavailable => reply(
                id,
                tool_failure(
                    "IMPORT_UNAVAILABLE",
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
                Json(json!({"code":error.error_code(),"message":error.to_string()})),
            )
                .into_response(),
            Self::Authorization(error) => {
                reply(id, tool_failure(error.error_code(), &error.to_string()))
            }
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

fn tool_failure(code: &str, message: &str) -> CallToolResult {
    CallToolResult::structured_error(json!({"code":code,"message":message}))
}

#[derive(Clone)]
struct Imports {
    uploads: Arc<Mutex<Uploads>>,
    tasks: Arc<Mutex<Tasks>>,
    db: Arc<Mutex<Db>>,
    fallback: Option<SecretString>,
    request_limit: usize,
    transfers: Arc<tokio::sync::Semaphore>,
}

fn locked<T>(mutex: &Mutex<T>) -> Result<MutexGuard<'_, T>, ImportError> {
    mutex.lock().map_err(|_| ImportError::Unavailable)
}

impl Imports {
    async fn authorize(
        &self,
        headers: &HeaderMap,
        allow_fallback: bool,
    ) -> Result<Principal, ImportError> {
        let token = bearer(
            headers,
            allow_fallback.then_some(self.fallback.as_ref()).flatten(),
        )?;
        let db = Arc::clone(&self.db);
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

/// Add the extensions outside the upstream SDK, which owns the authenticated tool and its ledger write.
pub(crate) fn layer(
    mcp: Router,
    db: Arc<Mutex<Db>>,
    fallback: Option<SecretString>,
    request_limit: usize,
) -> Router {
    let state = Imports {
        uploads: Arc::default(),
        tasks: Arc::default(),
        db,
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
) -> StatusCode {
    let Ok(transfer) = transfer.parse::<Ticket>() else {
        return StatusCode::NOT_FOUND;
    };
    let principal = match state.authorize(request.headers(), false).await {
        Ok(principal) => principal,
        Err(ImportError::Authorization(CliError::Unauthorized(_))) => {
            return StatusCode::UNAUTHORIZED;
        }
        Err(ImportError::Authorization(_)) => return StatusCode::FORBIDDEN,
        Err(_) => return StatusCode::SERVICE_UNAVAILABLE,
    };
    let Ok(size) =
        locked(&state.uploads).and_then(|mut uploads| uploads.expected_size(&transfer, &principal))
    else {
        return StatusCode::NOT_FOUND;
    };
    let Ok(_permit) = state.transfers.try_acquire() else {
        return StatusCode::SERVICE_UNAVAILABLE;
    };
    let Ok(Ok(bytes)) =
        tokio::time::timeout(Duration::from_secs(30), to_bytes(request.into_body(), size)).await
    else {
        return StatusCode::BAD_REQUEST;
    };
    match locked(&state.uploads).and_then(|mut uploads| uploads.stage(&transfer, &principal, bytes))
    {
        Ok(()) => StatusCode::NO_CONTENT,
        Err(ImportError::Validation(_)) => StatusCode::BAD_REQUEST,
        Err(ImportError::Unknown) => StatusCode::NOT_FOUND,
        Err(_) => StatusCode::SERVICE_UNAVAILABLE,
    }
}

async fn intercept(State(state): State<Imports>, request: Request, next: Next) -> Response {
    if request.method() != Method::POST
        || request.headers().contains_key(LEDGER_TARGET_HEADER)
        || request
            .headers()
            .get("mcp-protocol-version")
            .is_none_or(|version| version != VERSION)
        || !matches!(
            request
                .headers()
                .get("mcp-method")
                .and_then(|method| method.to_str().ok()),
            Some(
                "server/discover"
                    | "tools/list"
                    | "tools/call"
                    | "tasks/get"
                    | "tasks/cancel"
                    | "tasks/update"
            )
        )
        || (request
            .headers()
            .get("mcp-method")
            .is_some_and(|method| method == "tools/call")
            && request
                .headers()
                .get("mcp-name")
                .is_none_or(|name| name != TOOL))
    {
        return next.run(request).await;
    }
    let (parts, body) = request.into_parts();
    let Ok(bytes) = to_bytes(body, state.request_limit).await else {
        return StatusCode::PAYLOAD_TOO_LARGE.into_response();
    };
    let request = Request::from_parts(parts, Body::from(bytes.clone()));
    let Ok(mut message) = serde_json::from_slice::<Value>(&bytes) else {
        return next.run(request).await;
    };
    let Some(id) = message
        .get("id")
        .filter(|id| id.is_string() || id.is_i64())
        .cloned()
    else {
        return next.run(request).await;
    };
    let method = message["method"].as_str().unwrap_or_default();
    if message["jsonrpc"] != "2.0"
        || request
            .headers()
            .get("mcp-method")
            .is_none_or(|header| header != method)
        || message["params"]["_meta"]
            .get(LEDGER_TARGET_META_KEY)
            .is_some()
    {
        return next.run(request).await;
    }
    let params = &message["params"];
    if method.starts_with("tasks/")
        && request
            .headers()
            .get("mcp-name")
            .and_then(|name| name.to_str().ok())
            != params["taskId"].as_str()
    {
        return ImportError::Unknown.respond(&id);
    }
    let capabilities = &params["_meta"][META_CAPABILITIES]["extensions"];
    let files =
        capabilities[FILES]["upload"].is_object() && capabilities[FILES].get("revision").is_none();
    let tasks = capabilities[TASKS].is_object();
    match method {
        "server/discover" => discovery(request, next, &id).await,
        "tools/list" if files => tool_list(request, next, &id).await,
        "tools/call" if params["name"] == TOOL && params["_meta"][META_VERSION] == VERSION => {
            let (parts, _) = request.into_parts();
            let result = state
                .import(&id, &mut message, parts, next, files, tasks)
                .await;
            result.unwrap_or_else(|error| error.respond(&id))
        }
        "tasks/get" | "tasks/cancel" | "tasks/update"
            if params["_meta"][META_VERSION] == VERSION =>
        {
            state
                .task_request(method, params, request.headers(), tasks)
                .await
                .map_or_else(|error| error.respond(&id), |result| reply(&id, result))
        }
        _ => next.run(request).await,
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
        method: &str,
        params: &Value,
        headers: &HeaderMap,
        supported: bool,
    ) -> Result<Value, ImportError> {
        if !supported {
            return Err(ImportError::MissingCapability(TASKS));
        }
        let principal = self.authorize(headers, true).await?;
        let id: ImportTaskId = params["taskId"]
            .as_str()
            .ok_or(ImportError::Unknown)?
            .parse()?;
        let mut tasks = locked(&self.tasks)?;
        match method {
            "tasks/get" => serde_json::to_value(tasks.get(&id, &principal)?)
                .map_err(|_| ImportError::Unavailable),
            "tasks/cancel" => {
                tasks.cancel(&id, &principal)?;
                serde_json::to_value(TaskAckResult::new()).map_err(|_| ImportError::Unavailable)
            }
            _ => {
                tasks.get(&id, &principal)?;
                serde_json::to_value(TaskAckResult::new()).map_err(|_| ImportError::Unavailable)
            }
        }
    }

    async fn import(
        &self,
        id: &Value,
        message: &mut Value,
        mut parts: axum::http::request::Parts,
        next: Next,
        files: bool,
        tasks: bool,
    ) -> Result<Response, ImportError> {
        let params = &mut message["params"];
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
            .as_object_mut()
            .ok_or(ImportError::Validation("Expected statement arguments."))?;
        let principal = self
            .authorize(&parts.headers, !arguments.contains_key("file"))
            .await?;
        if let Some(file) = arguments.get("file") {
            if arguments.contains_key("content") {
                return Err(ImportError::Validation(
                    "Supply exactly one statement source.",
                ));
            }
            if !files {
                return Err(ImportError::MissingCapability(FILES));
            }
            let mut uploads = locked(&self.uploads)?;
            let binding = arguments.clone();
            if let Some(ticket) = request_state {
                *arguments = uploads.confirm(&ticket, &principal, &binding)?;
            } else {
                let file = FileSpec::try_from(file)?;
                let result = uploads.issue(principal, binding, file)?;
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
        } else if request_state.is_some() {
            return Err(ImportError::Unknown);
        }
        let large = arguments
            .get("content")
            .and_then(Value::as_str)
            .is_some_and(|text| text.len() >= TASK_THRESHOLD);
        let destination = ImportDestination {
            company: arguments.get("company").cloned().unwrap_or_default(),
            suspense: arguments.get("suspense").cloned().unwrap_or_default(),
        };
        if let Some(params) = params.as_object_mut() {
            params.remove("requestState");
        }
        parts.headers.remove(header::CONTENT_LENGTH);
        let request = Request::from_parts(parts, Body::from(message.to_string()));
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
        destination: ImportDestination,
    ) -> Value {
        let response = next.run(request).await;
        if !response.status().is_success() {
            let (code, message) = if response.status() == StatusCode::UNAUTHORIZED {
                ("UNAUTHORIZED", "The import credential is no longer valid.")
            } else {
                (
                    "IMPORT_UNAVAILABLE",
                    "The ledger did not return the statement import result. Retry with the same reference.",
                )
            };
            return json!({"jsonrpc":"2.0","id":id,"result":tool_failure(code,message)});
        }
        let Ok(mut response) = read_rpc(response, id).await else {
            return json!({"jsonrpc":"2.0","id":id,"result":tool_failure("IMPORT_UNAVAILABLE","The ledger did not return the statement import result. Retry with the same reference.")});
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
                .unwrap_or("IMPORT_FAILED");
            // Parsers can include source rows in diagnostic text. Keep the named
            // error, and never pass source-dependent messages to the client.
            *result = json!(tool_failure(
                code,
                "Statement import failed. Check the format, accounts, and stable reference before retrying."
            ));
        } else if let Some(data) = result.get_mut("structuredContent") {
            if let Some(scope) = destination.summary_scope()
                && let Some(message) = scope.summarize(Arc::clone(&self.db), data).await
            {
                result["content"] = json!([{"type":"text","text":message}]);
            }
        } else {
            return json!({"jsonrpc":"2.0","id":id,"result":tool_failure("IMPORT_UNAVAILABLE","The ledger did not return the statement import result. Retry with the same reference.")});
        }
        response
    }
}

// Only used for read-back after upstream accepts and commits the typed import.
struct ImportDestination {
    company: Value,
    suspense: Value,
}

impl ImportDestination {
    fn summary_scope(&self) -> Option<summary::Scope> {
        summary::Scope::from_completed_import(&self.company, &self.suspense)
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
