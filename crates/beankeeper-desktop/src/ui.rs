use axum::{
    Extension, Router,
    http::{StatusCode, header},
    middleware,
    response::{Html, IntoResponse, Response},
    routing::get,
};

use crate::pairing::{self, PairingState, Session};

const SHELL: &str = include_str!("../../../ui/index.html");
const STYLE: &str = include_str!("../../../ui/style.css");
const SESSION_CONTROLS: &str = include_str!("../../../ui/session.html");
pub(crate) const HEALTH: &str = "beankeeper-desktop-v1";

fn page(title: &str, description: &str, path: &str, session: &Session) -> Html<String> {
    Html(
        SHELL
            .replace("{{title}}", title)
            .replace("{{description}}", description)
            .replace("{{content}}", &session_controls(session))
            .replace(
                &format!("href=\"{path}\" data-view"),
                &format!("href=\"{path}\" aria-current=\"page\""),
            ),
    )
}

fn session_controls(session: &Session) -> String {
    let connection = if session.agent.is_some() {
        "Your agent is connected to this browser."
    } else {
        "Your browser session is active. Open your agent's connection link here to let it navigate this workspace."
    };
    SESSION_CONTROLS
        .replace("{{csrf}}", session.csrf.expose())
        .replace("{{connection}}", connection)
}

pub(crate) fn message(title: &str, content: &str) -> Html<String> {
    Html(
        SHELL
            .replace("{{title}}", title)
            .replace("{{description}}", "")
            .replace("{{content}}", content),
    )
}

pub(crate) fn router(state: PairingState) -> Router {
    Router::new()
        .route(
            "/",
            get(|Extension(session): Extension<Session>| async move {
                page(
                    "Beankeeper is running",
                    "Ask your agent to work with your books. Your local workspace is ready.",
                    "/",
                    &session,
                )
            }),
        )
        .route(
            "/transactions",
            get(|Extension(session): Extension<Session>| async move {
                page(
                    "Transactions",
                    "Your transaction register will appear here in a future update.",
                    "/transactions",
                    &session,
                )
            }),
        )
        .route(
            "/reports",
            get(|Extension(session): Extension<Session>| async move {
                page(
                    "Reports",
                    "Your financial reports will appear here in a future update.",
                    "/reports",
                    &session,
                )
            }),
        )
        .route(
            "/review",
            get(|Extension(session): Extension<Session>| async move {
                page(
                    "Review queue",
                    "Human review of agent proposals will be available in a future update.",
                    "/review",
                    &session,
                )
            }),
        )
        .route_layer(middleware::from_fn_with_state(state, pairing::human_guard))
        .route(
            "/assets/style.css",
            get(|| async { ([(header::CONTENT_TYPE, "text/css; charset=utf-8")], STYLE) }),
        )
        .route("/health", get(|| async { HEALTH }))
        .fallback(not_found)
}

async fn not_found() -> Response {
    (StatusCode::NOT_FOUND, "Page not found").into_response()
}
