use std::{fmt, str::FromStr, time::Duration};

use rmcp::model::{CallToolResult, CreateTaskResult, ProtocolVersion};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{TOOL, request::ReadyImport, upload::UploadRequired};

pub(super) const META_VERSION: &str = "io.modelcontextprotocol/protocolVersion";
pub(super) const VERSION: ProtocolVersion = ProtocolVersion::STANDARD_HEADERS;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum ImportMethod {
    Discover,
    ListTools,
    CallTool,
    Task(TaskMethod),
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum TaskMethod {
    Get,
    Cancel,
    Update,
}

impl AsRef<str> for ImportMethod {
    fn as_ref(&self) -> &str {
        match self {
            Self::Discover => "server/discover",
            Self::ListTools => "tools/list",
            Self::CallTool => "tools/call",
            Self::Task(TaskMethod::Get) => "tasks/get",
            Self::Task(TaskMethod::Cancel) => "tasks/cancel",
            Self::Task(TaskMethod::Update) => "tasks/update",
        }
    }
}

impl FromStr for ImportMethod {
    type Err = ();

    fn from_str(wire: &str) -> Result<Self, Self::Err> {
        [
            Self::Discover,
            Self::ListTools,
            Self::CallTool,
            Self::Task(TaskMethod::Get),
            Self::Task(TaskMethod::Cancel),
            Self::Task(TaskMethod::Update),
        ]
        .into_iter()
        .find(|method| method.as_ref() == wire)
        .ok_or(())
    }
}

#[derive(Serialize)]
pub(super) struct RpcCall<'a, T> {
    jsonrpc: &'static str,
    id: &'a Value,
    method: &'a str,
    params: T,
}

impl<'a, T> From<(&'a Value, &'a ImportMethod, T)> for RpcCall<'a, T> {
    fn from((id, method, params): (&'a Value, &'a ImportMethod, T)) -> Self {
        Self {
            jsonrpc: "2.0",
            id,
            method: method.as_ref(),
            params,
        }
    }
}

#[derive(Serialize)]
pub(super) struct ToolCall<'a> {
    name: &'static str,
    arguments: &'a ReadyImport,
    #[serde(rename = "_meta")]
    metadata: &'a Value,
    #[serde(rename = "inputResponses", skip_serializing_if = "Option::is_none")]
    input_responses: Option<&'a Value>,
}

impl<'a> From<(&'a ReadyImport, &'a Value, Option<&'a Value>)> for ToolCall<'a> {
    fn from(
        (arguments, metadata, input_responses): (&'a ReadyImport, &'a Value, Option<&'a Value>),
    ) -> Self {
        Self {
            name: TOOL,
            arguments,
            metadata,
            input_responses,
        }
    }
}

#[derive(Deserialize)]
#[serde(untagged)]
pub(super) enum ImportReply {
    Task(CreateTaskResult),
    Complete(CallToolResult),
    Upload(UploadRequired),
}

pub(super) struct PollInterval(u64);

impl Default for PollInterval {
    fn default() -> Self {
        Self(500)
    }
}

impl From<Option<u64>> for PollInterval {
    fn from(milliseconds: Option<u64>) -> Self {
        milliseconds.map_or_else(Self::default, Self)
    }
}

impl From<PollInterval> for u64 {
    fn from(interval: PollInterval) -> Self {
        interval.0
    }
}

impl From<PollInterval> for Duration {
    fn from(interval: PollInterval) -> Self {
        Self::from_millis(interval.into())
    }
}

#[derive(Debug, Default)]
pub(super) enum ImportCode {
    Validation,
    Busy,
    Unavailable,
    Unauthorized,
    Forbidden,
    Io,
    BooksNotServed,
    #[default]
    Failed,
    Upstream(Box<str>),
}

impl From<&str> for ImportCode {
    fn from(code: &str) -> Self {
        [
            Self::Validation,
            Self::Busy,
            Self::Unavailable,
            Self::Unauthorized,
            Self::Forbidden,
            Self::Io,
            Self::BooksNotServed,
            Self::Failed,
        ]
        .into_iter()
        .find(|known| known.as_ref() == code)
        .unwrap_or_else(|| Self::Upstream(code.into()))
    }
}

impl AsRef<str> for ImportCode {
    fn as_ref(&self) -> &str {
        match self {
            Self::Validation => "VALIDATION",
            Self::Busy => "IMPORT_BUSY",
            Self::Unavailable => "IMPORT_UNAVAILABLE",
            Self::Unauthorized => "UNAUTHORIZED",
            Self::Forbidden => "FORBIDDEN",
            Self::Io => "IO",
            Self::BooksNotServed => "DESKTOP_BOOKS_NOT_SERVED",
            Self::Failed => "IMPORT_FAILED",
            Self::Upstream(code) => code,
        }
    }
}

impl fmt::Display for ImportCode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_ref())
    }
}

impl Serialize for ImportCode {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}
