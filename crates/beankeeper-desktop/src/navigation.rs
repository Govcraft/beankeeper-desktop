//! Desktop-only navigation. Upstream owns all MCP authentication and grants.

use std::{
    collections::{HashMap, VecDeque},
    convert::Infallible,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use axum::{
    Extension, Form, Json, Router,
    extract::{Query, State},
    http::{HeaderMap, StatusCode},
    middleware,
    response::{IntoResponse, Response, Sse, sse::Event},
    routing::{get, post},
};
use beankeeper_cli::{
    capability::Capability,
    identity::VerifiedAgent,
    mcp::{HostTool, HostToolEffect, HostToolHandler},
};
use mti::prelude::*;
use rmcp::{
    ErrorData,
    model::{CallToolRequestParams, CallToolResult, Tool},
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio_stream::{StreamExt, wrappers::IntervalStream};

use crate::pairing::{self, AgentKey, BrowserSessionId, PairingError, PairingState, Session};

const TAB_LIFETIME: Duration = Duration::from_secs(3);
const MAX_TABS: usize = 128;
const MAX_PENDING: usize = 32;

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(transparent)]
struct TabId(#[schemars(with = "String")] MagicTypeId);

impl TabId {
    fn new() -> Self {
        Self("tab".create_type_id::<V7>())
    }
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct Show {
    view_id: crate::links::View,
    #[serde(default)]
    state: crate::links::AddressState,
    tab_id: Option<TabId>,
}

impl Show {
    fn url(&self) -> Result<String, &'static str> {
        crate::links::ViewAddress {
            view_id: self.view_id,
            state: self.state.clone(),
        }
        .path()
    }
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct Highlight {
    target: String,
    tab_id: Option<TabId>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct Status {
    tab_id: Option<TabId>,
}

struct Tab {
    session: BrowserSessionId,
    url: String,
    title: String,
    targets: Vec<String>,
    seen: Instant,
    pending: VecDeque<(AgentKey, Value)>,
    connected: bool,
    epoch: u64,
    highlight: Option<String>,
}

#[derive(Clone)]
pub(crate) struct Navigation {
    pairing: PairingState,
    cancellation: tokio_util::sync::CancellationToken,
    tabs: Arc<Mutex<HashMap<TabId, Tab>>>,
}

impl Navigation {
    pub(crate) fn new(
        pairing: PairingState,
        cancellation: tokio_util::sync::CancellationToken,
    ) -> Self {
        Self {
            pairing,
            cancellation,
            tabs: Arc::default(),
        }
    }

    fn failure_with_link(
        &self,
        agent: &AgentKey,
        code: &str,
        message: &str,
    ) -> Result<CallToolResult, ErrorData> {
        let url = self
            .pairing
            .agent_pairing_url(agent.clone(), Instant::now())
            .map_err(|_| ErrorData::internal_error("Pairing link unavailable", None))?;
        let mut result = failure(
            code,
            &format!("{message} Nothing was displayed. Connect a paired browser: {url}"),
        );
        if let Some(payload) = &mut result.structured_content {
            payload["pairing_url"] = json!(url);
        }
        Ok(result)
    }

    fn call(
        &self,
        name: &str,
        arguments: Value,
        identity: VerifiedAgent,
    ) -> Result<CallToolResult, ErrorData> {
        if name == "ui_get_link" {
            return Ok(
                match serde_json::from_value::<crate::links::ViewAddress>(arguments) {
                    Ok(address) => match address.result(self.pairing.base_url()) {
                        Ok(payload) => reply(
                            format!(
                                "Optional Desktop detail: {}. No browser was opened or changed.",
                                payload["url"].as_str().unwrap_or_default()
                            ),
                            payload,
                        ),
                        Err(error) => failure("VALIDATION", &error.to_string()),
                    },
                    Err(_) => failure(
                        "VALIDATION",
                        "Invalid view or filters. Check the tool schema.",
                    ),
                },
            );
        }
        let agent = AgentKey::from_verified_identity(&identity);
        let sessions = self
            .pairing
            .paired_sessions(&agent, Instant::now())
            .map_err(|_| ErrorData::internal_error("Pairing unavailable", None))?;
        let mut tabs = self
            .tabs
            .lock()
            .map_err(|_| ErrorData::internal_error("Tabs unavailable", None))?;
        tabs.retain(|_, tab| tab.seen.elapsed() < TAB_LIFETIME);
        let mut available: Vec<_> = tabs
            .iter_mut()
            .filter(|(_, tab)| tab.connected && sessions.contains(&tab.session))
            .collect();
        available.sort_by(|a, b| a.0.0.cmp(&b.0.0));
        let parsed = match name {
            "ui_show_view" => serde_json::from_value::<Show>(arguments).map(|input| {
                input.url().map(|url| {
                    (
                        input.tab_id,
                        json!({"kind":"navigate", "url":url}),
                        format!("Requested view: {url}."),
                    )
                })
            }),
            "ui_highlight" => serde_json::from_value::<Highlight>(arguments).map(|input| {
                if input.target.is_empty() || input.target.len() > 256 {
                    Err("Highlight target must be a DOM ID of 1 to 256 bytes.")
                } else {
                    Ok((
                        input.tab_id,
                        json!({"kind":"highlight", "target": input.target}),
                        format!("Requested highlight: {}.", input.target),
                    ))
                }
            }),
            "ui_status" => {
                let input: Status = match serde_json::from_value(arguments) {
                    Ok(input) => input,
                    Err(_) => {
                        return self.failure_with_link(
                            &agent,
                            "VALIDATION",
                            "Invalid ui_status arguments.",
                        );
                    }
                };
                let states: Vec<_> = available.iter().filter(|(id, _)| input.tab_id.as_ref().is_none_or(|wanted| wanted == *id)).map(|(id, tab)| json!({"tab_id":id, "url":tab.url, "title":tab.title, "highlight":tab.highlight, "targets":tab.targets, "pending":tab.pending.len()})).collect();
                let text = if states.is_empty() {
                    "No connected matching tab. Nothing is displayed.".to_owned()
                } else {
                    format!(
                        "{} connected tab(s): {}",
                        states.len(),
                        Value::Array(states.clone())
                    )
                };
                let url = self
                    .pairing
                    .agent_pairing_url(agent, Instant::now())
                    .map_err(|_| ErrorData::internal_error("Pairing link unavailable", None))?;
                return Ok(reply(
                    format!("{text} Pair or connect another browser: {url}"),
                    json!({"paired":!sessions.is_empty(), "tabs":states, "pairing_url":url}),
                ));
            }
            _ => return Err(ErrorData::invalid_params("Unknown Desktop tool", None)),
        };
        let (wanted, event, description) = match parsed {
            Ok(Ok(value)) => value,
            Ok(Err(message)) => {
                return self.failure_with_link(&agent, "VIEW_UNAVAILABLE_OR_VALIDATION", message);
            }
            Err(_) => {
                return self.failure_with_link(
                    &agent,
                    "VALIDATION",
                    "Invalid navigation arguments. Check the tool schema.",
                );
            }
        };
        if let Some(wanted) = wanted {
            available.retain(|(id, _)| **id == wanted);
        }
        if available.len() > 1 {
            return Ok(failure(
                "AMBIGUOUS_TAB",
                "Multiple tabs are connected. Call ui_status and supply tab_id.",
            ));
        }
        let Some((id, tab)) = available.into_iter().next() else {
            let mut result = self.failure_with_link(
                &agent,
                "NO_CONNECTED_TAB",
                &format!("{description} No connected matching tab."),
            )?;
            if let Some(payload) = &mut result.structured_content {
                payload["queued"] = json!(false);
            }
            return Ok(result);
        };
        if event["kind"] == "highlight"
            && !tab
                .targets
                .iter()
                .any(|target| event["target"].as_str() == Some(target))
        {
            return Ok(reply(
                format!(
                    "{description} No such row or region in the current view; nothing changed."
                ),
                json!({"queued":false,"tab_id":id}),
            ));
        }
        if tab.pending.len() >= MAX_PENDING {
            return Ok(failure(
                "TAB_BUSY",
                "Tab event queue is full. Wait for the browser to catch up.",
            ));
        }
        tab.pending.push_back((agent, event));
        Ok(reply(
            format!(
                "{description} Queued for tab {}. Call ui_status to check the browser's confirmed view.",
                id.0
            ),
            json!({"queued":true,"tab_id":id}),
        ))
    }
}

fn reply(text: String, payload: Value) -> CallToolResult {
    let mut result = CallToolResult::structured(payload);
    result.content = vec![rmcp::model::ContentBlock::text(text)];
    result
}

fn failure(code: &str, message: &str) -> CallToolResult {
    let mut result = reply(message.into(), json!({"code":code,"message":message}));
    result.is_error = Some(true);
    result
}

impl HostToolHandler for Navigation {
    fn tools(&self) -> Vec<HostTool> {
        let mut tools: Vec<_> = [("ui_show_view", "Queue a real Desktop view with URL-restorable filters, including financial reports, tax summaries and reconciliation. Use a stable report view_id or reports with state.report_kind. No ledger data is returned. Use ui_status to choose among tabs.", schemars::schema_for!(Show).to_value(), HostToolEffect::Mutation),
         ("ui_highlight", "Highlight an existing DOM ID from ui_status targets. A missing target is a no-op.", schemars::schema_for!(Highlight).to_value(), HostToolEffect::Mutation),
         ("ui_status", "Report paired and live tabs, confirmed view URLs, highlight targets and pairing link. Requires ui/navigate.", schemars::schema_for!(Status).to_value(), HostToolEffect::ReadOnly)]
        .into_iter().map(|(name, description, schema, effect)| HostTool { tool: Tool::new(name, description, schema.as_object().cloned().unwrap_or_default()), capability: Capability::Navigate, effect }).collect();
        tools.push(HostTool { tool: Tool::new("ui_get_link", "Return an optional Desktop detail URL after answering in words. Does not navigate, open a browser or require pairing. Uses the same view_id/state as ui_show_view. Requires ledger/read.", schemars::schema_for!(crate::links::ViewAddress).to_value().as_object().cloned().unwrap_or_default()), capability: Capability::Read, effect: HostToolEffect::ReadOnly });
        tools
    }

    fn call_tool(
        &self,
        request: CallToolRequestParams,
        identity: VerifiedAgent,
    ) -> impl std::future::Future<Output = Result<CallToolResult, ErrorData>> + Send {
        std::future::ready(self.call(
            &request.name,
            Value::Object(request.arguments.unwrap_or_default()),
            identity,
        ))
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TabUpdate {
    csrf: String,
    tab_id: Option<TabId>,
    url: String,
    #[serde(default)]
    title: String,
    targets: String,
    highlight: Option<String>,
}

#[derive(Deserialize)]
struct Events {
    tab_id: TabId,
}

pub(crate) fn router(navigation: Navigation, pairing: PairingState) -> Router {
    Router::new()
        .route("/api/ui/tab", post(update))
        .route("/api/ui/events", get(events))
        .with_state(navigation)
        .route_layer(middleware::from_fn_with_state(
            pairing,
            pairing::human_guard,
        ))
        .route(
            "/assets/navigation.js",
            get(|| async {
                (
                    [("content-type", "text/javascript; charset=utf-8")],
                    include_str!("../../../ui/navigation.js"),
                )
            }),
        )
}

fn local_view(url: &str) -> bool {
    let Some(path) = url.split('?').next() else {
        return false;
    };
    url.len() <= 4096
        && !url.contains(['#', '\\', '\r', '\n'])
        && (matches!(
            path,
            "/" | "/accounts"
                | "/transactions"
                | "/review"
                | "/reports"
                | "/reconciliation"
                | "/settings"
        ) || path
            .strip_prefix("/reports/")
            .is_some_and(|id| crate::reports::VIEW_IDS.contains(&id))
            || ["/transactions/", "/review/"].iter().any(|prefix| {
                path.strip_prefix(prefix).is_some_and(|id| {
                    !id.is_empty() && id.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_')
                })
            }))
}

async fn update(
    State(state): State<Navigation>,
    Extension(session): Extension<Session>,
    Form(input): Form<TabUpdate>,
) -> Response {
    // human_guard already checked this field before the form extractor.
    let _csrf = input.csrf;
    if !local_view(&input.url) || input.targets.len() > 4096 {
        return (StatusCode::BAD_REQUEST, "Invalid tab state").into_response();
    }
    let Ok(mut tabs) = state.tabs.lock() else {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    };
    tabs.retain(|_, tab| tab.seen.elapsed() < TAB_LIFETIME);
    let id = input
        .tab_id
        .filter(|id| tabs.get(id).is_some_and(|tab| tab.session == session.id))
        .unwrap_or_else(TabId::new);
    if tabs.len() >= MAX_TABS && !tabs.contains_key(&id) {
        return StatusCode::TOO_MANY_REQUESTS.into_response();
    }
    let tab = tabs.entry(id.clone()).or_insert_with(|| Tab {
        session: session.id,
        url: String::new(),
        title: String::new(),
        targets: Vec::new(),
        seen: Instant::now(),
        pending: VecDeque::new(),
        connected: false,
        epoch: 0,
        highlight: None,
    });
    tab.url = input.url;
    tab.title = input.title.chars().take(256).collect();
    tab.targets = input
        .targets
        .lines()
        .take(128)
        .filter(|target| !target.is_empty() && target.len() <= 256)
        .map(str::to_owned)
        .collect();
    tab.highlight = input
        .highlight
        .filter(|target| tab.targets.contains(target));
    tab.seen = Instant::now();
    Json(json!({"tab_id":id})).into_response()
}

// A dropped or replaced HTTP stream immediately stops being a live target.
struct StreamLease {
    tabs: Arc<Mutex<HashMap<TabId, Tab>>>,
    id: TabId,
    epoch: u64,
}

impl Drop for StreamLease {
    fn drop(&mut self) {
        if let Ok(mut tabs) = self.tabs.lock()
            && let Some(tab) = tabs.get_mut(&self.id)
            && tab.epoch == self.epoch
        {
            tab.connected = false;
        }
    }
}

async fn events(
    State(state): State<Navigation>,
    Extension(session): Extension<Session>,
    headers: HeaderMap,
    Query(input): Query<Events>,
) -> Result<Response, PairingError> {
    let epoch = {
        let mut tabs = state.tabs.lock().map_err(|_| PairingError::Unavailable)?;
        let tab = tabs
            .get_mut(&input.tab_id)
            .filter(|tab| tab.session == session.id)
            .ok_or(PairingError::Unauthorized)?;
        tab.connected = true;
        tab.epoch = tab.epoch.wrapping_add(1);
        tab.epoch
    };
    let lease = StreamLease {
        tabs: state.tabs.clone(),
        id: input.tab_id.clone(),
        epoch,
    };
    let stream = IntervalStream::new(tokio::time::interval(Duration::from_millis(250)))
        .map(move |_| {
            if state.cancellation.is_cancelled() {
                return None;
            }
            // Revalidate D3 authority during the stream, including logout and expiry.
            let live = state.pairing.session(&headers, Instant::now()).ok()?;
            let mut tabs = state.tabs.lock().ok()?;
            let tab = tabs
                .get_mut(&input.tab_id)
                .filter(|tab| tab.session == live.id && tab.epoch == lease.epoch)?;
            tab.seen = Instant::now();
            let mut event = next_event(&mut tab.pending, live.agent.as_ref());
            event["agent_associated"] = json!(live.agent.is_some());
            let _keep_lease = &lease;
            Some(Ok::<_, Infallible>(
                Event::default().event("ui").data(event.to_string()),
            ))
        })
        .take_while(Option::is_some)
        .map(|event| event.unwrap_or_else(|| Ok(Event::default())));
    Ok(Sse::new(stream).into_response())
}

fn next_event(pending: &mut VecDeque<(AgentKey, Value)>, agent: Option<&AgentKey>) -> Value {
    std::iter::from_fn(|| pending.pop_front())
        .find(|(owner, _)| agent == Some(owner))
        .map(|(_, event)| event)
        .unwrap_or_else(|| json!({"kind":"heartbeat"}))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reassociation_discards_pending_events_from_the_previous_agent() {
        let old = AgentKey::for_test("old");
        let current = AgentKey::for_test("current");
        let mut pending = VecDeque::from([
            (old, json!({"kind":"navigate","url":"/review"})),
            (current.clone(), json!({"kind":"highlight","target":"main"})),
        ]);
        assert_eq!(
            next_event(&mut pending, Some(&current))["kind"],
            "highlight"
        );
        assert!(pending.is_empty());
        pending.push_back((current, json!({"kind":"navigate","url":"/accounts"})));
        assert_eq!(next_event(&mut pending, None)["kind"], "heartbeat");
        assert!(pending.is_empty());
    }

    #[test]
    fn view_addresses_use_real_routes_and_validate_detail_identifiers()
    -> Result<(), Box<dyn std::error::Error>> {
        for (input, expected) in [
            (json!({"view_id":"dashboard"}), "/"),
            (
                json!({"view_id":"accounts","state":{"company":"a&b"}}),
                "/accounts?company=a%26b",
            ),
            (
                json!({"view_id":"transaction","state":{"transaction_id":42,"company":"acme"}}),
                "/transactions/42?company=acme",
            ),
            (
                json!({"view_id":"review_detail","state":{"proposal_id":"proposal_01h455vb4pex5vsknk084sn02q"}}),
                "/review/proposal_01h455vb4pex5vsknk084sn02q",
            ),
        ] {
            let show: Show = serde_json::from_value(input)?;
            assert_eq!(show.url()?, expected);
            assert!(local_view(expected));
        }
        for id in [
            "../approve",
            "x/reject",
            "..",
            "transaction_01h455vb4pex5vsknk084sn02q",
        ] {
            let show: Show = serde_json::from_value(
                json!({"view_id":"review_detail","state":{"proposal_id":id}}),
            )?;
            assert!(show.url().is_err());
        }
        Ok(())
    }
}
