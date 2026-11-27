//! Installation-wide license tools on the local OS-authority MCP channels.

use std::path::{Path, PathBuf};

use rmcp::model::{CallToolRequestParams, CallToolResult, ContentBlock, Tool, ToolAnnotations};
use schemars::JsonSchema;
use secrecy::SecretString;
use serde::{Deserialize, de::DeserializeOwned};
use serde_json::{Value, json};

use super::{
    BooksSelection, LicenseRequest, ProductError,
    activation::{ActivationInputs, Storefront},
    report::{LicenseErrorCode, LicenseResponse},
    returns::{self, ReturnConfirmation, ReturnRequest, ReturnResponse},
};
#[derive(Clone, Copy)]
pub(crate) enum LicenseTool {
    Status,
    Trial,
    Activate,
    Return,
    Remove,
}

impl LicenseTool {
    pub(crate) const STATUS: &str = "license_status";
    const REMOVE: &str = "license_remove";
    const RETURN: &str = "license_return";
    const ACTIVATE: &str = "license_activate";

    pub(crate) fn from_name(name: &str) -> Option<Self> {
        match name {
            "license_start_trial" => Some(Self::Trial),
            Self::STATUS => Some(Self::Status),
            Self::ACTIVATE => Some(Self::Activate),
            Self::RETURN => Some(Self::Return),
            Self::REMOVE => Some(Self::Remove),
            _ => None,
        }
    }

    pub(crate) fn tools() -> impl Iterator<Item = Tool> {
        [
            Self::Status,
            Self::Trial,
            Self::Activate,
            Self::Return,
            Self::Remove,
        ]
        .into_iter()
        .filter_map(Self::tool)
    }

    fn tool(self) -> Option<Tool> {
        let (name, description, schema) = match self {
            Self::Trial => (
                "license_start_trial",
                "Start the email-free 30-day trial directly. Tell the owner what happened; never show tokens. Mention days remaining on the first status check in a session, when asked, and at seven or fewer days, without repeating a countdown in every reply.",
                schemars::schema_for!(NoArguments),
            ),
            Self::Status => (
                Self::STATUS,
                "Check the Desktop license and whether the selected books are writable. Tell the owner in plain words. Offer to activate a license they already have and continue any work Desktop permits.",
                schemars::schema_for!(NoArguments),
            ),
            Self::Activate => (
                Self::ACTIVATE,
                "Activate the license the owner provided for these books. Use exactly one signed license, local license file, or claim code. Activate directly and tell the owner what happened. Keep license text, codes and private recovery information out of replies.",
                schemars::schema_for!(ActivateArguments),
            ),
            Self::Remove => (
                Self::REMOVE,
                "Remove a license from this computer without a refund or server-side return. Act directly, then tell the owner that books without another active license become read-only. Books and recovery material are never changed. Undo by activating the same license again.",
                schemars::schema_for!(RemoveArguments),
            ),
            Self::Return => (
                Self::RETURN,
                returns::POLICY,
                schemars::schema_for!(ReturnArguments),
            ),
        };
        let annotations = ToolAnnotations::new()
            .read_only(matches!(self, Self::Status))
            .destructive(matches!(self, Self::Return | Self::Remove))
            .open_world(matches!(self, Self::Trial | Self::Activate | Self::Return));
        Some(
            Tool::new(name, description, schema.to_value().as_object()?.clone())
                .with_annotations(annotations),
        )
    }

    pub(crate) async fn call_with_context(
        self,
        request: CallToolRequestParams,
        context: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> CallToolResult {
        if !matches!(self, Self::Return) {
            return tokio::task::spawn_blocking(move || self.call_request(request, None))
                .await
                .unwrap_or_else(|_| worker_failure());
        }
        let arguments = Value::Object(request.arguments.unwrap_or_default());
        let mut arguments = match decode::<ReturnArguments>(arguments) {
            Ok(arguments) => arguments,
            Err(error) => return failure(&error),
        };
        if let Some(response) =
            ReturnRequest::new(arguments.license_id.clone(), arguments.confirm).trial_response()
        {
            return return_reply(&response);
        }
        if !arguments.confirm.is_confirmed()
            && context
                .peer
                .supported_elicitation_modes()
                .contains(&rmcp::service::ElicitationMode::Form)
        {
            let Ok(schema) = rmcp::model::ElicitationSchema::builder()
                .required_bool("confirm")
                .build()
            else {
                return worker_failure();
            };
            let elicitation = rmcp::model::ElicitRequestParams::FormElicitationParams {
                meta: None,
                message: returns::POLICY.to_owned(),
                requested_schema: schema,
            };
            let result = context.peer.create_elicitation(elicitation).await;
            arguments.confirm = result
                .ok()
                .filter(|result| result.action == rmcp::model::ElicitationAction::Accept)
                .and_then(|result| result.content)
                .and_then(|content| serde_json::from_value::<ConfirmationAnswer>(content).ok())
                .map_or_else(ReturnConfirmation::default, |answer| answer.confirm);
        }
        tokio::task::spawn_blocking(move || return_reply(&arguments.into_request().execute()))
            .await
            .unwrap_or_else(|_| worker_failure())
    }

    pub(crate) fn call(self, params: &Value, books: Option<&Path>) -> CallToolResult {
        CallToolRequestParams::deserialize(params).map_or_else(
            |_| failure(&ProductError::ActivationInput),
            |request| self.call_request(request, books),
        )
    }

    pub(crate) fn call_request(
        self,
        request: CallToolRequestParams,
        books: Option<&Path>,
    ) -> CallToolResult {
        self.execute(request, books)
            .unwrap_or_else(|error| failure(&error))
    }

    fn execute(
        self,
        request: CallToolRequestParams,
        books: Option<&Path>,
    ) -> Result<CallToolResult, ProductError> {
        let arguments = Value::Object(request.arguments.unwrap_or_default());
        let request = match self {
            Self::Remove => {
                let arguments = decode::<RemoveArguments>(arguments)?;
                let target = BooksSelection::try_from((books, books.is_some().into()))?;
                let response = super::removal::remove(arguments.license_id, &target)?;
                let mut result = CallToolResult::structured(json!(response));
                result.content = vec![ContentBlock::text(response.message())];
                return Ok(result);
            }
            Self::Return => {
                return Ok(return_reply(
                    &decode::<ReturnArguments>(arguments)?
                        .into_request()
                        .execute(),
                ));
            }
            Self::Trial => {
                decode::<NoArguments>(arguments)?;
                LicenseRequest::Trial
            }
            Self::Status => {
                decode::<NoArguments>(arguments)?;
                LicenseRequest::Status
            }
            Self::Activate => LicenseRequest::Activate(
                ActivationInputs::from(decode::<ActivateArguments>(arguments)?).try_into()?,
            ),
        };
        let target = BooksSelection::try_from((books, books.is_some().into()))?;
        let report = request.run(&target)?;
        let settings_url = target.settings_url();
        Ok(reply(&report.response(settings_url.as_deref())))
    }
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct RemoveArguments {
    /// Installed license to remove locally.
    #[schemars(with = "String")]
    license_id: crate::license::LicenseId,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ConfirmationAnswer {
    confirm: ReturnConfirmation,
}

/// Shared typed inputs for return tools and service elicitation.
#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct ReturnArguments {
    /// Select an installed license when more than one is present.
    #[schemars(with = "Option<String>")]
    license_id: Option<crate::license::LicenseId>,
    /// Explicitly request the return and refund after reading the policy.
    #[serde(default)]
    pub(crate) confirm: ReturnConfirmation,
}

impl ReturnArguments {
    pub(crate) fn into_request(self) -> ReturnRequest {
        ReturnRequest::new(self.license_id, self.confirm)
    }
}

pub(crate) fn return_reply(response: &ReturnResponse) -> CallToolResult {
    let payload = json!(response);
    let mut result = if response.is_error() {
        CallToolResult::structured_error(payload)
    } else {
        CallToolResult::structured(payload)
    };
    result.content = vec![ContentBlock::text(response.message())];
    result
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct NoArguments {}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct ActivateArguments {
    /// The signed license beginning with v4.public, as provided by the owner.
    #[schemars(with = "Option<String>")]
    license: Option<SecretString>,
    /// Local path to the saved license file, including an email attachment.
    file_path: Option<PathBuf>,
    /// The single-use claim or checkout code provided by the owner.
    #[schemars(with = "Option<String>")]
    claim_code: Option<SecretString>,
    /// Storefront when known. Stripe checkout codes and Etsy claim codes work without this field.
    storefront: Option<Storefront>,
}

impl From<ActivateArguments> for ActivationInputs {
    fn from(args: ActivateArguments) -> Self {
        Self {
            pasted: args.license,
            file: args.file_path,
            claim: args.claim_code,
            storefront: args.storefront,
            ..Self::default()
        }
    }
}

fn decode<T: DeserializeOwned>(arguments: Value) -> Result<T, ProductError> {
    serde_json::from_value(arguments).map_err(|_| ProductError::ActivationInput)
}

fn reply(response: &LicenseResponse) -> CallToolResult {
    let payload = json!(response);
    let mut result = if response.is_error() {
        CallToolResult::structured_error(payload)
    } else {
        CallToolResult::structured(payload)
    };
    result.content = vec![ContentBlock::text(response.message())];
    result
}

pub(crate) fn worker_failure() -> CallToolResult {
    let error = ProductError::LicenseWorkerFailed;
    let code = LicenseErrorCode::from(&error);
    let message = error.to_string();
    let mut result = CallToolResult::structured_error(json!({"code": code, "message": message}));
    result.content = vec![ContentBlock::text(message)];
    result
}

fn failure(error: &ProductError) -> CallToolResult {
    reply(&LicenseResponse::from(error))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worker_failure_is_a_named_mcp_error() {
        let result = worker_failure();
        let message = "Desktop could not complete the license request.";
        assert_eq!(result.is_error, Some(true));
        assert_eq!(
            result.structured_content,
            Some(json!({"code": "LICENSE_WORKER_FAILED", "message": message}))
        );
        assert_eq!(result.content, vec![ContentBlock::text(message)]);
    }
}
