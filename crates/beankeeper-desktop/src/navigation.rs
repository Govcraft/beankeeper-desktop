//! Desktop-only navigation. Upstream owns all MCP authentication and grants.

use std::{
    collections::{HashMap, VecDeque},
    convert::Infallible,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use axum::{
    Form, Json, Router,
    extract::{Query, State},
    http::{HeaderMap, StatusCode},
    middleware,
    response::{IntoResponse, Response, Sse, sse::Event},
    routing::get,
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

use crate::{
    links::{LocalViewUrl, ViewAddress},
    pairing::{
        self, AgentKey, BrowserSessionId, HumanSession, HumanWrite, PairingError, PairingState,
    },
};

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
#[schemars(deny_unknown_fields)]
struct Show {
    #[serde(flatten)]
    address: ViewAddress,
    tab_id: Option<TabId>,
}

impl Show {
    fn url(&self) -> Result<LocalViewUrl, &'static str> {
        self.address.local_url()
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
    url: LocalViewUrl,
    title: String,
    targets: Vec<String>,
    seen: Instant,
    pending: VecDeque<(AgentKey, UiEvent)>,
    connected: bool,
    epoch: u64,
    highlight: Option<String>,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum UiEvent {
    Navigate { url: LocalViewUrl },
    Highlight { target: String },
    Heartbeat,
}

#[derive(Serialize)]
struct EventFrame {
    #[serde(flatten)]
    event: UiEvent,
    agent_associated: bool,
}

#[derive(Clone, Copy, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
enum NavigationFailure {
    Validation,
    ViewUnavailableOrValidation,
    AmbiguousTab,
    NoConnectedTab,
    TabBusy,
}

#[derive(Clone, Copy)]
enum NavigationTool {
    ShowView,
    Highlight,
    Status,
    GetLink,
}

impl NavigationTool {
    const ALL: [Self; 4] = [Self::ShowView, Self::Highlight, Self::Status, Self::GetLink];

    fn name(self) -> &'static str {
        match self {
            Self::ShowView => "ui_show_view",
            Self::Highlight => "ui_highlight",
            Self::Status => "ui_status",
            Self::GetLink => "ui_get_link",
        }
    }

    fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|tool| tool.name() == name)
    }

    fn definition(self) -> HostTool {
        let (description, schema, capability, effect) = match self {
            Self::ShowView => (
                "Queue a Desktop view with URL-restorable filters. Use ui_status to choose a tab.",
                schemars::schema_for!(Show),
                Capability::Navigate,
                HostToolEffect::Mutation,
            ),
            Self::Highlight => (
                "Highlight an existing DOM ID from ui_status targets. A missing target is a no-op.",
                schemars::schema_for!(Highlight),
                Capability::Navigate,
                HostToolEffect::Mutation,
            ),
            Self::Status => (
                "Report paired and live tabs, confirmed view URLs, highlight targets and a pairing link.",
                schemars::schema_for!(Status),
                Capability::Navigate,
                HostToolEffect::ReadOnly,
            ),
            Self::GetLink => (
                "Return an optional Desktop detail URL after answering in words. Does not navigate, open a browser or require pairing. Uses the same view_id/state as ui_show_view.",
                schemars::schema_for!(ViewAddress),
                Capability::Read,
                HostToolEffect::ReadOnly,
            ),
        };
        HostTool {
            tool: Tool::new(
                self.name(),
                description,
                schema.to_value().as_object().cloned().unwrap_or_default(),
            ),
            capability,
            effect,
        }
    }
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
        code: NavigationFailure,
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
        tool: NavigationTool,
        arguments: Value,
        identity: &VerifiedAgent,
    ) -> Result<CallToolResult, ErrorData> {
        if let NavigationTool::GetLink = tool {
            return Ok(self.link_result(arguments));
        }
        let agent = AgentKey::from_verified_identity(identity);
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
        let parsed = match tool {
            NavigationTool::ShowView => serde_json::from_value::<Show>(arguments).map(|input| {
                input.url().map(|url| {
                    (
                        input.tab_id,
                        format!("Requested view: {}.", url.as_str()),
                        UiEvent::Navigate { url },
                    )
                })
            }),
            NavigationTool::Highlight => {
                serde_json::from_value::<Highlight>(arguments).map(|input| {
                    if input.target.is_empty() || input.target.len() > 256 {
                        Err("Highlight target must be a DOM ID of 1 to 256 bytes.")
                    } else {
                        Ok((
                            input.tab_id,
                            format!("Requested highlight: {}.", input.target),
                            UiEvent::Highlight {
                                target: input.target,
                            },
                        ))
                    }
                })
            }
            NavigationTool::Status => {
                return self.status_result(arguments, agent, &sessions, available);
            }
            NavigationTool::GetLink => unreachable!("link handled before paired-tab access"),
        };
        let (wanted, description, event) = match parsed {
            Ok(Ok(value)) => value,
            Ok(Err(message)) => {
                return self.failure_with_link(
                    &agent,
                    NavigationFailure::ViewUnavailableOrValidation,
                    message,
                );
            }
            Err(_) => {
                return self.failure_with_link(
                    &agent,
                    NavigationFailure::Validation,
                    "Invalid navigation arguments. Check the tool schema.",
                );
            }
        };
        if let Some(wanted) = wanted {
            available.retain(|(id, _)| **id == wanted);
        }
        if available.len() > 1 {
            return Ok(failure(
                NavigationFailure::AmbiguousTab,
                "Multiple tabs are connected. Call ui_status and supply tab_id.",
            ));
        }
        let Some((id, tab)) = available.into_iter().next() else {
            let mut result = self.failure_with_link(
                &agent,
                NavigationFailure::NoConnectedTab,
                &format!("{description} No connected matching tab."),
            )?;
            if let Some(payload) = &mut result.structured_content {
                payload["queued"] = json!(false);
            }
            return Ok(result);
        };
        Ok(enqueue_event(agent, event, id, tab, &description))
    }
    fn link_result(&self, arguments: Value) -> CallToolResult {
        match serde_json::from_value::<ViewAddress>(arguments) {
            Ok(address) => match address.result(self.pairing.base_url()) {
                Ok(payload) => reply(
                    format!(
                        "Optional Desktop detail: {}. No browser was opened or changed.",
                        payload["url"].as_str().unwrap_or_default()
                    ),
                    payload,
                ),
                Err(error) => failure(NavigationFailure::Validation, &error.to_string()),
            },
            Err(_) => failure(
                NavigationFailure::Validation,
                "Invalid view or filters. Check the tool schema.",
            ),
        }
    }

    fn status_result(
        &self,
        arguments: Value,
        agent: AgentKey,
        sessions: &[BrowserSessionId],
        mut available: Vec<(&TabId, &mut Tab)>,
    ) -> Result<CallToolResult, ErrorData> {
        available.sort_by(|a, b| a.0.0.cmp(&b.0.0));
        let input: Status = match serde_json::from_value(arguments) {
            Ok(input) => input,
            Err(_) => {
                return self.failure_with_link(
                    &agent,
                    NavigationFailure::Validation,
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
        Ok(reply(
            format!("{text} Pair or connect another browser: {url}"),
            json!({"paired":!sessions.is_empty(), "tabs":states, "pairing_url":url}),
        ))
    }
}

fn enqueue_event(
    agent: AgentKey,
    event: UiEvent,
    id: &TabId,
    tab: &mut Tab,
    description: &str,
) -> CallToolResult {
    if let UiEvent::Highlight { target } = &event
        && !tab.targets.contains(target)
    {
        return reply(
            format!("{description} No such row or region in the current view; nothing changed."),
            json!({"queued":false,"tab_id":id}),
        );
    }
    if tab.pending.len() >= MAX_PENDING {
        return failure(
            NavigationFailure::TabBusy,
            "Tab event queue is full. Wait for the browser to catch up.",
        );
    }
    tab.pending.push_back((agent, event));
    reply(
        format!(
            "{description} Queued for tab {}. Call ui_status to check the browser's confirmed view.",
            id.0
        ),
        json!({"queued":true,"tab_id":id}),
    )
}

fn reply(text: String, payload: Value) -> CallToolResult {
    let mut result = CallToolResult::structured(payload);
    result.content = vec![rmcp::model::ContentBlock::text(text)];
    result
}

fn failure(code: NavigationFailure, message: &str) -> CallToolResult {
    let mut result = reply(message.into(), json!({"code":code,"message":message}));
    result.is_error = Some(true);
    result
}

impl HostToolHandler for Navigation {
    fn tools(&self) -> Vec<HostTool> {
        NavigationTool::ALL
            .into_iter()
            .map(NavigationTool::definition)
            .collect()
    }

    fn call_tool(
        &self,
        request: CallToolRequestParams,
        identity: VerifiedAgent,
    ) -> impl std::future::Future<Output = Result<CallToolResult, ErrorData>> + Send {
        std::future::ready(match NavigationTool::parse(&request.name) {
            Some(tool) => self.call(
                tool,
                Value::Object(request.arguments.unwrap_or_default()),
                &identity,
            ),
            None => Err(ErrorData::invalid_params("Unknown Desktop tool", None)),
        })
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TabUpdate {
    #[serde(rename = "csrf")]
    _csrf: serde::de::IgnoredAny,
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
        .route("/api/ui/tab", pairing::human_post(update))
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

async fn update(
    write: HumanWrite,
    State(state): State<Navigation>,
    Form(input): Form<TabUpdate>,
) -> Response {
    let session_id = write.session().id();
    let Ok(url) = LocalViewUrl::try_from(input.url) else {
        return (StatusCode::BAD_REQUEST, "Invalid tab state").into_response();
    };
    if input.targets.len() > 4096 {
        return (StatusCode::BAD_REQUEST, "Invalid tab state").into_response();
    }
    let Ok(mut tabs) = state.tabs.lock() else {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    };
    tabs.retain(|_, tab| tab.seen.elapsed() < TAB_LIFETIME);
    let id = input
        .tab_id
        .filter(|id| tabs.get(id).is_some_and(|tab| &tab.session == session_id))
        .unwrap_or_else(TabId::new);
    if tabs.len() >= MAX_TABS && !tabs.contains_key(&id) {
        return StatusCode::TOO_MANY_REQUESTS.into_response();
    }
    let tab = tabs.entry(id.clone()).or_insert_with(|| Tab {
        session: session_id.clone(),
        url: url.clone(),
        title: String::new(),
        targets: Vec::new(),
        seen: Instant::now(),
        pending: VecDeque::new(),
        connected: false,
        epoch: 0,
        highlight: None,
    });
    tab.url = url;
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
    session: HumanSession,
    headers: HeaderMap,
    Query(input): Query<Events>,
) -> Result<Response, PairingError> {
    let epoch = {
        let mut tabs = state.tabs.lock().map_err(|_| PairingError::Unavailable)?;
        let tab = tabs
            .get_mut(&input.tab_id)
            .filter(|tab| &tab.session == session.id())
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
                .filter(|tab| &tab.session == live.id() && tab.epoch == lease.epoch)?;
            tab.seen = Instant::now();
            let frame = EventFrame {
                event: next_event(&mut tab.pending, live.agent()),
                agent_associated: live.agent().is_some(),
            };
            let _keep_lease = &lease;
            let event = Event::default().event("ui").json_data(frame).ok()?;
            Some(Ok::<_, Infallible>(event))
        })
        .take_while(Option::is_some)
        .map(|event| event.unwrap_or_else(|| Ok(Event::default())));
    Ok(Sse::new(stream).into_response())
}

fn next_event(pending: &mut VecDeque<(AgentKey, UiEvent)>, agent: Option<&AgentKey>) -> UiEvent {
    std::iter::from_fn(|| pending.pop_front())
        .find(|(owner, _)| agent == Some(owner))
        .map_or(UiEvent::Heartbeat, |(_, event)| event)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reassociation_discards_pending_events_from_the_previous_agent()
    -> Result<(), Box<dyn std::error::Error>> {
        let old = AgentKey::for_test("old");
        let current = AgentKey::for_test("current");
        let mut pending = VecDeque::from([
            (
                old,
                UiEvent::Navigate {
                    url: "/review".to_owned().try_into()?,
                },
            ),
            (
                current.clone(),
                UiEvent::Highlight {
                    target: "main".to_owned(),
                },
            ),
        ]);
        assert_eq!(
            next_event(&mut pending, Some(&current)),
            UiEvent::Highlight {
                target: "main".to_owned()
            }
        );
        assert!(pending.is_empty());
        pending.push_back((
            current,
            UiEvent::Navigate {
                url: "/accounts".to_owned().try_into()?,
            },
        ));
        assert_eq!(next_event(&mut pending, None), UiEvent::Heartbeat);
        assert!(pending.is_empty());
        Ok(())
    }

    #[test]
    fn navigation_schema_adds_tab_selection_to_the_shared_address_contract()
    -> Result<(), Box<dyn std::error::Error>> {
        let schema = schemars::schema_for!(Show).to_value();
        assert_eq!(schema["type"], "object");
        assert_eq!(schema["unevaluatedProperties"], false);
        assert!(schema["properties"]["tab_id"].is_object());
        let variants = schema["oneOf"].as_array().ok_or("view alternatives")?;
        assert_eq!(variants.len(), 16);
        for variant in variants {
            // tab_id is declared by the outer object and must remain allowed.
            assert_ne!(variant["additionalProperties"], false);
        }
        let input = json!({"view_id":"accounts","tab_id":"tab_01h455vb4pex5vsknk084sn02q"});
        let show: Show = serde_json::from_value(input)?;
        assert_eq!(show.url()?.as_str(), "/accounts");
        assert!(
            serde_json::from_value::<Show>(json!({"view_id":"accounts","injected":true})).is_err()
        );
        Ok(())
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
            assert_eq!(show.url()?.as_str(), expected);
            assert!(LocalViewUrl::try_from(expected.to_owned()).is_ok());
        }
        for id in [
            "../approve",
            "x/reject",
            "..",
            "transaction_01h455vb4pex5vsknk084sn02q",
        ] {
            assert!(
                serde_json::from_value::<Show>(
                    json!({"view_id":"review_detail","state":{"proposal_id":id}}),
                )
                .is_err()
            );
        }
        Ok(())
    }
}
