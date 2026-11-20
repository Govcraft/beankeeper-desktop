use std::time::Instant;

use axum::{
    Form, Router,
    body::{Body, to_bytes},
    extract::{FromRequest, Query, Request, State},
    http::{HeaderMap, HeaderValue, Method, StatusCode, header},
    middleware::{self, Next},
    response::{Html, IntoResponse, Redirect, Response},
    routing::{get, post},
};
use serde::Deserialize;

use super::{AgentKey, PairingError, PairingState, secret::Secret};

impl IntoResponse for PairingError {
    fn into_response(self) -> Response {
        let status = match self {
            Self::Unauthorized => StatusCode::UNAUTHORIZED,
            Self::InvalidLink => StatusCode::GONE,
            Self::Capacity => StatusCode::TOO_MANY_REQUESTS,
            Self::Unavailable | Self::Entropy(_) => StatusCode::INTERNAL_SERVER_ERROR,
        };
        (status, self.to_string()).into_response()
    }
}

#[derive(Deserialize)]
struct PairQuery {
    token: Option<String>,
    invite: Option<String>,
}

#[derive(Deserialize)]
struct Csrf {
    csrf: String,
}

#[derive(Deserialize)]
struct Association {
    invite: String,
}

pub(crate) fn router(state: PairingState) -> Router {
    Router::new()
        .route("/api/session/logout", post(logout))
        .route("/api/session/pairing", post(new_link))
        .route("/api/session/associate", post(associate))
        .route_layer(middleware::from_fn_with_state(state.clone(), human_guard))
        .route("/pair", get(pair))
        .with_state(state)
}

async fn pair(
    State(state): State<PairingState>,
    headers: HeaderMap,
    Query(query): Query<PairQuery>,
) -> Result<Response, PairingError> {
    if headers.contains_key(header::AUTHORIZATION) {
        return Err(PairingError::Unauthorized);
    }
    match (query.token, query.invite) {
        (Some(token), None) => {
            let authority = headers
                .get(header::HOST)
                .and_then(|host| host.to_str().ok())
                .ok_or(PairingError::Unauthorized)?;
            let cookie = state.redeem(&token, authority, Instant::now())?;
            let mut response = Redirect::to("/").into_response();
            response.headers_mut().insert(
                header::SET_COOKIE,
                HeaderValue::from_str(&cookie).map_err(|_| PairingError::Unavailable)?,
            );
            Ok(response)
        }
        (None, Some(invite)) => {
            let now = Instant::now();
            let session = state.session(&headers, now);
            let registry = state.lock(now)?;
            let link = registry
                .links
                .get(&Secret::digest(&invite))
                .filter(|link| link.agent.is_some())
                .ok_or(PairingError::InvalidLink)?;
            let session = match session {
                Ok(session) => session,
                Err(PairingError::Unauthorized) => {
                    // Strict cookies may be withheld on the first navigation from
                    // an external site. A same-origin link allows a paired browser
                    // to continue, without weakening its cookie policy.
                    return Ok((StatusCode::UNAUTHORIZED, crate::ui::message(
                        "Connect your agent",
                        &format!("<p>If this browser is already paired, continue to confirm the agent connection. Otherwise open the terminal's one-time pairing URL first, then return here.</p><p><a href=\"/pair?invite={}\">Continue in this browser</a></p>", link.secret.expose())
                    )).into_response());
                }
                Err(error) => return Err(error),
            };
            // Only generated hex strings are interpolated, never query input.
            Ok(crate::ui::message("Connect your agent", &format!(
                "<p>Confirm that the agent which gave you this link may navigate this browser workspace. This grants no approval rights to the agent.</p><form method=\"post\" action=\"/api/session/associate\"><input type=\"hidden\" name=\"csrf\" value=\"{}\"><input type=\"hidden\" name=\"invite\" value=\"{}\"><button type=\"submit\">Connect agent to this browser</button></form>",
                session.csrf.expose(), link.secret.expose()
            )).into_response())
        }
        _ => Err(PairingError::InvalidLink),
    }
}

/// Applied to all browser routes, so future cookie writes inherit both checks.
pub(crate) async fn human_guard(
    State(state): State<PairingState>,
    mut request: Request,
    next: Next,
) -> Response {
    let result = async {
        let now = Instant::now();
        let session = state.session(request.headers(), now)?;
        if !matches!(
            *request.method(),
            Method::GET | Method::HEAD | Method::OPTIONS
        ) {
            let csrf = if let Some(token) = request.headers().get("x-csrf-token") {
                if request.headers().get_all("x-csrf-token").iter().count() != 1 {
                    return Err(PairingError::Unauthorized);
                }
                token
                    .to_str()
                    .map_err(|_| PairingError::Unauthorized)?
                    .to_owned()
            } else {
                let (parts, body) = request.into_parts();
                let bytes = to_bytes(body, 8192)
                    .await
                    .map_err(|_| PairingError::Unauthorized)?;
                request = Request::from_parts(parts, Body::from(bytes.clone()));
                let mut form_request = Request::new(Body::from(bytes));
                *form_request.headers_mut() = request.headers().clone();
                *form_request.method_mut() = request.method().clone();
                Form::<Csrf>::from_request(form_request, &())
                    .await
                    .map_err(|_| PairingError::Unauthorized)?
                    .0
                    .csrf
            };
            state.authorize_write(request.headers(), &csrf, now)?;
        }
        request.extensions_mut().insert(session);
        Ok::<_, PairingError>(request)
    }
    .await;
    match result {
        Ok(request) => next.run(request).await,
        Err(PairingError::Unauthorized) => (
            StatusCode::UNAUTHORIZED,
            crate::ui::message("Pair this browser", "<p>Open the one-time pairing URL printed in the terminal by <code>bk serve</code>. If it has expired or was already used, create a new link in a paired browser or restart the server.</p><p>After pairing, reopen your agent's connection link to connect it to this browser.</p>"),
        ).into_response(),
        Err(error) => error.into_response(),
    }
}

async fn logout(
    State(state): State<PairingState>,
    headers: HeaderMap,
) -> Result<Response, PairingError> {
    state
        .lock(Instant::now())?
        .sessions
        .remove(&Secret::digest(state.cookie(&headers)?));
    let mut response = Redirect::to("/").into_response();
    response.headers_mut().insert(
        header::SET_COOKIE,
        HeaderValue::from_str(&format!(
            "{}=; Path=/; HttpOnly; SameSite=Strict; Max-Age=0",
            state.cookie_name
        ))
        .map_err(|_| PairingError::Unavailable)?,
    );
    Ok(response)
}

async fn new_link(State(state): State<PairingState>) -> Result<Html<String>, PairingError> {
    let url = state.browser_link(Instant::now())?;
    Ok(crate::ui::message(
        "Pair another browser",
        &format!(
            "<p>This one-time link expires in five minutes. Open it in the browser you want to pair. Keep it private.</p><p><a href=\"{url}\">{url}</a></p><p><a href=\"/\">Return to workspace</a></p>"
        ),
    ))
}

async fn associate(
    State(state): State<PairingState>,
    headers: HeaderMap,
    Form(form): Form<Association>,
) -> Result<Redirect, PairingError> {
    state.associate(&headers, &form.invite, Instant::now())?;
    Ok(Redirect::to("/"))
}

/// Observe successful upstream authentication without copying verification code.
/// Cookie credentials are stripped before entering the MCP transport.
pub(crate) async fn mcp_guard(
    State((state, fallback)): State<(PairingState, Option<AgentKey>)>,
    mut request: Request,
    next: Next,
) -> Response {
    let agent = match request.headers().get(header::AUTHORIZATION) {
        Some(value) => value
            .to_str()
            .ok()
            .and_then(|value| value.split_once(' '))
            .filter(|(scheme, _)| scheme.eq_ignore_ascii_case("bearer"))
            .map(|(_, token)| AgentKey::from_verified_token(token)),
        None => fallback,
    };
    request.headers_mut().remove(header::COOKIE);
    let mut response = next.run(request).await;
    // http_router verifies credentials before it dispatches any MCP request.
    // Failed authentication, including revocation, can never mint an invite.
    if response.status().is_success()
        && let Some(agent) = agent
        && let Ok(sessions) = state.paired_sessions(&agent, Instant::now())
        && sessions.is_empty()
        && let Ok(url) = state.agent_pairing_url(agent, Instant::now())
        && let Ok(value) = HeaderValue::from_str(&url)
    {
        response
            .headers_mut()
            .insert("beankeeper-pairing-url", value);
    }
    response
}
