use std::net::SocketAddr;

use beankeeper_cli::{
    capability::Capability,
    db::{Db, audit::receipts::AuditCursor, companies::CompanySlug},
    error::CliError,
    mcp::{HostTool, HostToolEffect},
};
use rmcp::model::{
    CallToolRequestParams, CallToolResult, ContentBlock, JsonObject, Tool, ToolAnnotations,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Value, json};

use crate::instance::BooksPath;

use super::{Report, acknowledge, book};

pub(crate) const SINCE_TOOL: &str = "changes_since";
pub(crate) const MARK_TOOL: &str = "changes_mark_seen";

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct SinceInput {
    /// Book slug. Defaults to the configured book or the single book on this ledger.
    #[schemars(with = "Option<String>")]
    company: Option<CompanySlug>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct MarkInput {
    /// Book slug returned by `changes_since`.
    #[schemars(with = "String")]
    company: CompanySlug,
    /// The through cursor returned by the page that was delivered.
    #[schemars(with = "u64", range(max = i64::MAX))]
    through: AuditCursor,
}

pub(crate) fn definitions() -> [HostTool; 2] {
    [
        definition::<SinceInput>(
            SINCE_TOOL,
            "Read one page of grouped receipts since the owner was last informed. Tell the owner what changed in plain words and offer the undo links. After delivering the summary, call changes_mark_seen if available with the returned company and through. If has_more is true, repeat to deliver the next page.",
            HostToolEffect::ReadOnly,
        ),
        definition::<MarkInput>(
            MARK_TOOL,
            "After delivering a changes_since page to the owner, advance the local last-seen marker using its company and through. This does not change the ledger.",
            HostToolEffect::Mutation,
        ),
    ]
}

fn definition<T: JsonSchema>(
    name: &'static str,
    description: &'static str,
    effect: HostToolEffect,
) -> HostTool {
    HostTool {
        tool: Tool::new(
            name,
            description,
            schemars::schema_for!(T)
                .to_value()
                .as_object()
                .cloned()
                .unwrap_or_default(),
        )
        .with_annotations(
            ToolAnnotations::new()
                .read_only(effect == HostToolEffect::ReadOnly)
                .destructive(false)
                .idempotent(true),
        ),
        capability: Capability::Read,
        effect,
    }
}

fn decode<T: DeserializeOwned>(arguments: Option<JsonObject>) -> Result<T, CliError> {
    serde_json::from_value(Value::Object(arguments.unwrap_or_default()))
        .map_err(|error| CliError::Validation(error.to_string()))
}

pub(crate) fn call(
    request: CallToolRequestParams,
    db: &Db,
    books: &BooksPath,
    company: Option<CompanySlug>,
    bound: SocketAddr,
) -> CallToolResult {
    match execute(request, db, books, company, bound) {
        Ok(result) => result,
        Err(error) => failure(&error),
    }
}

pub(crate) fn failure(error: &CliError) -> CallToolResult {
    let text = error.to_string();
    let mut result =
        CallToolResult::structured_error(json!({"code": error.error_code(), "message": text}));
    result.content = vec![ContentBlock::text(text)];
    result
}

fn execute(
    request: CallToolRequestParams,
    db: &Db,
    books: &BooksPath,
    company: Option<CompanySlug>,
    bound: SocketAddr,
) -> Result<CallToolResult, CliError> {
    match request.name.as_ref() {
        SINCE_TOOL => {
            let input: SinceInput = decode(request.arguments)?;
            let company = book(db, input.company, company)?;
            let report = Report::read(db, books, company, None)?;
            structured(&report.data(Some(bound)))
        }
        MARK_TOOL => {
            let input: MarkInput = decode(request.arguments)?;
            structured(&acknowledge(db, books, input.company, input.through)?)
        }
        _ => Err(CliError::Usage("unknown changes tool".into())),
    }
}

fn structured(data: &(impl Serialize + std::fmt::Display)) -> Result<CallToolResult, CliError> {
    let value = serde_json::to_value(data)
        .map_err(|error| CliError::General(format!("cannot encode changes: {error}")))?;
    let mut result = CallToolResult::structured(value);
    result.content = vec![ContentBlock::text(data.to_string())];
    Ok(result)
}
