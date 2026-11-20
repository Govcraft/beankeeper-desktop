use std::sync::{Arc, Mutex};

use axum::{
    Extension, Router,
    extract::{Query, State},
    http::{StatusCode, Uri, header},
    middleware,
    response::{Html, IntoResponse, Response},
    routing::get,
};
use beankeeper_cli::db::Db;

use crate::{
    pairing::{self, PairingState, Session},
    views::{self, ViewQuery},
};

const SHELL: &str = include_str!("../../../ui/index.html");
const STYLE: &str = include_str!("../../../ui/style.css");
const SESSION_CONTROLS: &str = include_str!("../../../ui/session.html");
pub(crate) const HEALTH: &str = "beankeeper-desktop-v1";

pub(crate) fn escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

pub(crate) fn page(title: &str, description: &str, path: &str, content: &str) -> Html<String> {
    Html(
        SHELL
            .replace("{{title}}", &escape(title))
            .replace("{{description}}", &escape(description))
            .replace(
                &format!("href=\"{path}\" data-view"),
                &format!("href=\"{path}\" aria-current=\"page\""),
            )
            .replace("{{content}}", content),
    )
}

pub(crate) fn session_controls(session: &Session) -> String {
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

#[derive(Clone)]
struct ViewState {
    db: Arc<Mutex<Db>>,
    company: Option<String>,
}

pub(crate) fn router(db: Db, company: Option<String>, pairing: PairingState) -> Router {
    let data = Router::new()
        .route("/", get(view))
        .route("/accounts", get(view))
        .route("/transactions", get(view))
        .route("/transactions/{id}", get(view))
        .route(
            "/reports",
            get(|Extension(session): Extension<Session>| async move {
                page(
                    "Reports",
                    "Financial reports will be available in a future update.",
                    "/reports",
                    &session_controls(&session),
                )
            }),
        )
        .with_state(ViewState {
            db: Arc::new(Mutex::new(db)),
            company,
        });
    data.route_layer(middleware::from_fn_with_state(
        pairing,
        pairing::human_guard,
    ))
    .route(
        "/assets/style.css",
        get(|| async { ([(header::CONTENT_TYPE, "text/css; charset=utf-8")], STYLE) }),
    )
    .route("/health", get(|| async { HEALTH }))
    .fallback(|| async { (StatusCode::NOT_FOUND, "Page not found") })
}

async fn view(
    State(state): State<ViewState>,
    Extension(session): Extension<Session>,
    uri: Uri,
    query: Result<Query<ViewQuery>, axum::extract::rejection::QueryRejection>,
) -> Response {
    let mut query = match query {
        Ok(Query(query)) => query,
        Err(_) => {
            return (
                StatusCode::BAD_REQUEST,
                page(
                    "Invalid filters",
                    "Check your URL filters and try again.",
                    "",
                    "<p><a href=\"/transactions\">Reset register filters</a></p>",
                ),
            )
                .into_response();
        }
    };
    if query.company.is_none() {
        query.company = state.company;
    }
    let db = state.db;
    let path = uri.path().to_owned();
    let result = tokio::task::spawn_blocking(move || {
        let db = db.lock().map_err(|_| ())?;
        // Keep all reads for this page in one consistent upstream SQLite snapshot.
        let _snapshot = db.conn().unchecked_transaction().map_err(|_| ())?;
        Ok::<_, ()>(views::render(&db, &path, &query))
    })
    .await;
    match result {
        Ok(Ok(Ok(view))) => page(
            view.title,
            view.description,
            view.nav,
            &format!("{}{}", view.content, session_controls(&session)),
        )
        .into_response(),
        Ok(Ok(Err(error))) => {
            if error.status().is_server_error() {
                eprintln!("Ledger view failed: {error:?}");
            }
            (error.status(), page("Unable to display this view", &error.to_string(), "", "<p><a href=\"/\">Return to dashboard</a> or <a href=\"/transactions\">reset register filters</a>.</p>")).into_response()
        }
        _ => (
            StatusCode::INTERNAL_SERVER_ERROR,
            page(
                "Ledger unavailable",
                "The ledger could not be read. Try again or check the server terminal.",
                "",
                "",
            ),
        )
            .into_response(),
    }
}
