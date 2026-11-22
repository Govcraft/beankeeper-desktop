use std::{
    future::{Future, ready},
    time::Instant,
};

use axum::{
    Form, Router,
    body::{Body, to_bytes},
    extract::{FromRequest, FromRequestParts, Query, Request, State},
    handler::Handler,
    http::{HeaderMap, HeaderValue, Method, StatusCode, header, request::Parts},
    middleware::{self, Next},
    response::{Html, IntoResponse, Redirect, Response},
    routing::{MethodRouter, get, post},
};
use serde::Deserialize;

use crate::{boundary::SameOrigin, markup::markup};

use super::{
    AgentKey, HumanSession, HumanWrite, LinkKind, PairingError, PairingState, secret::Secret,
};

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

impl<S: Sync> FromRequestParts<S> for HumanSession {
    type Rejection = PairingError;

    fn from_request_parts(
        parts: &mut Parts,
        _: &S,
    ) -> impl Future<Output = Result<Self, Self::Rejection>> + Send {
        ready(
            parts
                .extensions
                .remove::<Self>()
                .ok_or(PairingError::Unauthorized),
        )
    }
}

impl<S: Sync> FromRequestParts<S> for HumanWrite {
    type Rejection = PairingError;

    fn from_request_parts(
        parts: &mut Parts,
        _: &S,
    ) -> impl Future<Output = Result<Self, Self::Rejection>> + Send {
        ready(
            parts
                .extensions
                .remove::<Self>()
                .ok_or(PairingError::Unauthorized),
        )
    }
}

impl<S: Sync> FromRequestParts<S> for SameOrigin {
    type Rejection = PairingError;

    fn from_request_parts(
        parts: &mut Parts,
        _: &S,
    ) -> impl Future<Output = Result<Self, Self::Rejection>> + Send {
        ready(
            parts
                .extensions
                .remove::<Self>()
                .ok_or(PairingError::Unauthorized),
        )
    }
}

/// Axum handler arguments for protected POST routes. The first tuple entry is
/// axum's marker; the first request extractor must carry write authorization.
pub(crate) trait HumanPostArgs {}

impl<M> HumanPostArgs for (M, HumanWrite) {}
impl<M, A> HumanPostArgs for (M, HumanWrite, A) {}
impl<M, A, B> HumanPostArgs for (M, HumanWrite, A, B) {}
impl<M, A, B, C> HumanPostArgs for (M, HumanWrite, A, B, C) {}

pub(crate) fn human_post<H, T, S>(handler: H) -> MethodRouter<S>
where
    H: Handler<T, S>,
    T: HumanPostArgs + 'static,
    S: Clone + Send + Sync + 'static,
{
    post(handler)
}

#[derive(Deserialize)]
struct PairQuery {
    token: Option<String>,
    invite: Option<String>,
}

#[derive(Deserialize)]
struct BrowserPairing {
    token: String,
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
        .route("/api/session/logout", human_post(logout))
        .route("/api/session/pairing", human_post(new_link))
        .route("/api/session/associate", human_post(associate))
        .route_layer(middleware::from_fn_with_state(state.clone(), human_guard))
        .route(super::PAIR_ROUTE, get(pair).post(pair_browser))
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
            let registry = state.lock(Instant::now())?;
            let link = registry
                .links
                .get(&Secret::digest(&token))
                .filter(|link| matches!(link.kind, LinkKind::Browser))
                .ok_or(PairingError::InvalidLink)?;
            // Displaying a link must never consume it: browsers and link handlers
            // may issue speculative or duplicate GETs before the human sees it.
            Ok(crate::ui::message("Pair this browser", &markup!("<p>Pair this browser to open your local Beankeeper workspace. This one-time link expires five minutes after it was created.</p><form method=\"post\" action=\"/pair\"><input type=\"hidden\" name=\"token\" value=\"{}\"><button type=\"submit\">Pair this browser</button></form>", link.secret.expose())).into_response())
        }
        (None, Some(invite)) => {
            let now = Instant::now();
            let session = state.session(&headers, now);
            let registry = state.lock(now)?;
            let link = registry
                .links
                .get(&Secret::digest(&invite))
                .filter(|link| matches!(link.kind, LinkKind::AgentInvite(_)))
                .ok_or(PairingError::InvalidLink)?;
            let session = match session {
                Ok(session) => session,
                Err(PairingError::Unauthorized) => {
                    // Strict cookies may be withheld on the first navigation from
                    // an external site. A same-origin link allows a paired browser
                    // to continue, without weakening its cookie policy.
                    return Ok((StatusCode::UNAUTHORIZED, crate::ui::message(
                        "Connect your agent",
                        &markup!("<p>If this browser is already paired, continue to confirm the agent connection. Otherwise open the terminal's one-time pairing URL first, then return here.</p><p><a href=\"/pair?invite={}\">Continue in this browser</a></p>", link.secret.expose())
                    )).into_response());
                }
                Err(error) => return Err(error),
            };
            Ok(crate::ui::message("Connect your agent", &markup!("<p>Confirm that the agent which gave you this link may navigate this browser workspace. This grants no approval rights to the agent.</p><form method=\"post\" action=\"/api/session/associate\"><input type=\"hidden\" name=\"csrf\" value=\"{}\"><input type=\"hidden\" name=\"invite\" value=\"{}\"><button type=\"submit\">Connect agent to this browser</button></form>", session.csrf_token(), link.secret.expose())).into_response())
        }
        _ => Err(PairingError::InvalidLink),
    }
}

async fn pair_browser(
    State(state): State<PairingState>,
    origin: SameOrigin,
    headers: HeaderMap,
    Form(form): Form<BrowserPairing>,
) -> Result<Response, PairingError> {
    if headers.contains_key(header::AUTHORIZATION) {
        return Err(PairingError::Unauthorized);
    }
    let cookie = state.redeem(&form.token, &origin, Instant::now())?;
    let destination = state.take_destination(&headers, Instant::now())?;
    let mut response = Redirect::to(&destination).into_response();
    response.headers_mut().insert(
        header::SET_COOKIE,
        HeaderValue::from_str(&cookie).map_err(|_| PairingError::Unavailable)?,
    );
    response.headers_mut().append(
        header::SET_COOKIE,
        HeaderValue::from_str(&state.clear_destination_cookie())
            .map_err(|_| PairingError::Unavailable)?,
    );
    Ok(response)
}

/// Authenticates browser routes and provides the proof required by their handlers.
pub(crate) async fn human_guard(
    State(state): State<PairingState>,
    mut request: Request,
    next: Next,
) -> Response {
    let destination = if matches!(*request.method(), Method::GET | Method::HEAD)
        && !request.headers().contains_key(header::AUTHORIZATION)
    {
        crate::links::parse_path(&request.uri().to_string())
            .and_then(|address| address.path())
            .ok()
    } else {
        None
    };
    let request_headers = request.headers().clone();
    let result = async {
        let now = Instant::now();
        let session = state.session(request.headers(), now)?;
        if matches!(
            *request.method(),
            Method::GET | Method::HEAD | Method::OPTIONS
        ) {
            request.extensions_mut().insert(session);
        } else {
            let origin = request
                .extensions()
                .get::<SameOrigin>()
                .cloned()
                .ok_or(PairingError::Unauthorized)?;
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
            let write = state.authorize_write(session, &origin, &csrf, Instant::now())?;
            request.extensions_mut().insert(write);
        }
        Ok::<_, PairingError>(request)
    }
    .await;
    match result {
        Ok(request) => next.run(request).await,
        Err(PairingError::Unauthorized) => {
            let content = destination.as_ref().map_or_else(
                || markup!("<p>Open the one-time pairing URL printed by <code>bk serve</code>. Use a fresh link if it has expired or was already used.</p>"),
                |path| markup!("<p>Open the one-time pairing URL printed in the terminal by <code>bk serve</code>, then select <strong>Pair this browser</strong>. Your requested view will open automatically. Pairing links expire after five minutes; use a fresh link if needed.</p><p>If already paired, <a href=\"{}\">continue to your requested view</a>.</p>", path),
            );
            let mut response = (
                StatusCode::UNAUTHORIZED,
                crate::ui::message("Pair this browser", &content),
            )
                .into_response();
            if let Some(path) = destination {
                match state.remember_destination(&request_headers, path, Instant::now()) {
                    Ok(cookie) if !cookie.is_empty() => {
                        let Ok(cookie) = HeaderValue::from_str(&cookie) else {
                            return PairingError::Unavailable.into_response();
                        };
                        response.headers_mut().append(header::SET_COOKIE, cookie);
                    }
                    Ok(_) => {}
                    Err(error) => return error.into_response(),
                }
            }
            response
        }
        Err(error) => error.into_response(),
    }
}

async fn logout(
    write: HumanWrite,
    State(state): State<PairingState>,
) -> Result<Response, PairingError> {
    state
        .lock(Instant::now())?
        .sessions
        .remove(&write.session().cookie_key);
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

async fn new_link(
    _write: HumanWrite,
    State(state): State<PairingState>,
) -> Result<Html<String>, PairingError> {
    let url = state.browser_link(Instant::now())?;
    Ok(crate::ui::message(
        "Pair another browser",
        &markup!(
            "<p>This one-time link expires in five minutes. Open it in the browser you want to pair. Keep it private.</p><p><a href=\"{}\">{}</a></p><p><a href=\"/\">Return to workspace</a></p>",
            url,
            url
        ),
    ))
}

async fn associate(
    write: HumanWrite,
    State(state): State<PairingState>,
    Form(form): Form<Association>,
) -> Result<Redirect, PairingError> {
    state.associate(&write, &form.invite, Instant::now())?;
    Ok(Redirect::to("/"))
}

pub(crate) type McpPairing = (
    PairingState,
    Option<secrecy::SecretString>,
    std::sync::Arc<std::sync::Mutex<beankeeper_cli::db::Db>>,
);

/// Observe successful upstream authentication without copying verification code.
/// Cookie credentials are stripped before entering the MCP transport.
pub(crate) async fn mcp_guard(
    State((state, fallback, db)): State<McpPairing>,
    mut request: Request,
    next: Next,
) -> Response {
    let agent = match request.headers().get(header::AUTHORIZATION) {
        Some(value) => value
            .to_str()
            .ok()
            .and_then(|value| value.split_once(' '))
            .filter(|(scheme, _)| scheme.eq_ignore_ascii_case("bearer"))
            .map(|(_, token)| secrecy::SecretString::from(token.to_owned())),
        None => fallback,
    };
    request.headers_mut().remove(header::COOKIE);
    let mut response = next.run(request).await;
    // http_router verifies credentials before it dispatches any MCP request.
    // Failed authentication, including revocation, can never mint an invite.
    let agent = if response.status().is_success() {
        tokio::task::spawn_blocking(move || {
            let token = agent?;
            let db = db.lock().ok()?;
            // Reuse the public upstream verifier only to derive D3 routing identity.
            // Authentication and grant decisions remain inside the MCP router.
            let identity = beankeeper_cli::identity::verify(
                db.read(),
                secrecy::ExposeSecret::expose_secret(&token),
            )
            .ok()?;
            Some(AgentKey::from_verified_identity(&identity))
        })
        .await
        .ok()
        .flatten()
    } else {
        None
    };
    if let Some(agent) = agent
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::boundary::{LocalBoundary, LocalOrigin};

    #[tokio::test]
    async fn protected_post_requires_the_middleware_proof() -> Result<(), Box<dyn std::error::Error>>
    {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let bound = listener.local_addr()?;
        let app = Router::new().route(
            "/protected",
            human_post(|_: HumanWrite| async { StatusCode::NO_CONTENT }),
        );
        let server = tokio::spawn(async move { axum::serve(listener, app).await });
        let result = reqwest::Client::new()
            .post(format!("http://{bound}/protected"))
            .send()
            .await;
        server.abort();
        assert_eq!(result?.status(), StatusCode::UNAUTHORIZED);
        Ok(())
    }

    #[tokio::test]
    async fn token_expiring_after_the_confirmation_page_cannot_be_redeemed()
    -> Result<(), Box<dyn std::error::Error>> {
        let (state, url) = PairingState::new("127.0.0.1:8750".parse()?)?;
        let token = url.split_once('=').ok_or("pairing token")?.1;
        let mut headers = HeaderMap::new();
        headers.insert(header::HOST, "127.0.0.1:8750".parse()?);
        headers.insert(header::ORIGIN, "http://127.0.0.1:8750".parse()?);
        let Some(LocalOrigin::Same(origin)) = LocalBoundary::new(8750).check(&headers) else {
            return Err("local same-origin request".into());
        };
        let page = pair(
            State(state.clone()),
            headers.clone(),
            Query(PairQuery {
                token: Some(token.into()),
                invite: None,
            }),
        )
        .await?;
        assert_eq!(page.status(), StatusCode::OK);
        assert!(page.headers().get(header::SET_COOKIE).is_none());
        // Move the deadline instead of sleeping five minutes.
        let now = Instant::now();
        state
            .lock(now)?
            .links
            .get_mut(&Secret::digest(token))
            .ok_or("unconsumed link")?
            .expires = now;
        let response = pair_browser(
            State(state.clone()),
            origin,
            headers,
            Form(BrowserPairing {
                token: token.into(),
            }),
        )
        .await
        .expect_err("expired POST must fail")
        .into_response();
        assert_eq!(response.status(), StatusCode::GONE);
        assert!(response.headers().get(header::SET_COOKIE).is_none());
        let body = to_bytes(response.into_body(), 8192).await?;
        let body = std::str::from_utf8(&body)?;
        assert!(body.contains("Pairing link expired or already used"));
        assert!(!body.contains(token));
        assert!(state.lock(Instant::now())?.sessions.is_empty());
        Ok(())
    }
}

#[cfg(test)]
mod destination_tests {
    use super::*;

    #[tokio::test]
    async fn full_destination_registry_still_serves_pairing_and_remembers_the_new_view()
    -> Result<(), Box<dyn std::error::Error>> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let bound = listener.local_addr()?;
        let (state, _) = PairingState::new(bound)?;
        let now = Instant::now();
        let mut oldest_cookie = None;
        for index in 0..super::super::MAX_ENTRIES {
            let cookie = state.remember_destination(
                &HeaderMap::new(),
                "/settings".into(),
                now.checked_sub(std::time::Duration::from_secs(u64::try_from(
                    super::super::MAX_ENTRIES - index,
                )?))
                .ok_or("fixture timestamp")?,
            )?;
            if oldest_cookie.is_none() {
                oldest_cookie = Some(cookie);
            }
        }
        let app = Router::new()
            .route("/accounts", get(|| async { "protected view" }))
            .route_layer(middleware::from_fn_with_state(state.clone(), human_guard));
        let server = tokio::spawn(async move { axum::serve(listener, app).await });
        let response = reqwest::Client::new()
            .get(format!("http://{bound}/accounts?company=acme"))
            .send()
            .await?;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        let cookie = response
            .headers()
            .get(header::SET_COOKIE)
            .ok_or("destination cookie")?
            .to_str()?
            .to_owned();
        let body = response.text().await?;
        assert!(body.contains("Pair this browser"));
        assert!(body.contains("Your requested view will open automatically"));
        assert!(!body.contains("Too many pairing links"));
        assert!(!body.contains("protected view"));
        {
            let registry = state.lock(Instant::now())?;
            assert_eq!(registry.destinations.len(), super::super::MAX_ENTRIES);
            assert!(registry.sessions.is_empty());
        }
        let mut headers = HeaderMap::new();
        headers.insert(
            header::COOKIE,
            oldest_cookie
                .ok_or("oldest cookie")?
                .split(';')
                .next()
                .ok_or("cookie")?
                .parse()?,
        );
        assert_eq!(state.take_destination(&headers, Instant::now())?, "/");
        headers.insert(
            header::COOKIE,
            cookie.split(';').next().ok_or("cookie")?.parse()?,
        );
        assert_eq!(
            state.take_destination(&headers, Instant::now())?,
            "/accounts?company=acme"
        );
        server.abort();
        Ok(())
    }

    #[test]
    fn destinations_expire_are_consumed_and_never_authorize_a_session()
    -> Result<(), Box<dyn std::error::Error>> {
        let (state, _) = PairingState::new("127.0.0.1:8750".parse()?)?;
        let now = Instant::now();
        let cookie =
            state.remember_destination(&HeaderMap::new(), "/accounts?company=acme".into(), now)?;
        let mut headers = HeaderMap::new();
        headers.insert(header::HOST, "127.0.0.1:8750".parse()?);
        headers.insert(
            header::COOKIE,
            cookie.split(';').next().ok_or("cookie")?.parse()?,
        );
        assert!(matches!(
            state.session(&headers, now),
            Err(PairingError::Unauthorized)
        ));
        assert_eq!(
            state.take_destination(&headers, now)?,
            "/accounts?company=acme"
        );
        assert_eq!(state.take_destination(&headers, now)?, "/");
        let cookie = state.remember_destination(&HeaderMap::new(), "/settings".into(), now)?;
        headers.insert(
            header::COOKIE,
            cookie.split(';').next().ok_or("cookie")?.parse()?,
        );
        assert_eq!(
            state.take_destination(&headers, now + super::super::LINK_LIFETIME)?,
            "/"
        );
        headers.insert(header::COOKIE, "bk_desktop_8750_return=forged".parse()?);
        assert_eq!(state.take_destination(&headers, now)?, "/");
        Ok(())
    }
}
