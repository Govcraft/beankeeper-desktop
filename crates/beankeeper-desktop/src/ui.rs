use std::sync::{Arc, Mutex};

use axum::{
    Router,
    extract::{Query, State},
    http::{StatusCode, Uri, header},
    middleware,
    response::{Html, IntoResponse, Response},
    routing::get,
};
use beankeeper_cli::db::Db;

use crate::{
    markup::{Attribute, Markup, markup, write_markup},
    pairing::{self, HumanSession, PairingState},
    reports::{self, ReportQuery, ReportResponse},
    views::{self, filters::ViewQuery},
};

const SHELL: &str = include_str!("../../../ui/index.html");
const STYLE: &str = include_str!("../../../ui/style.css");
const SESSION_CONTROLS: &str = include_str!("../../../ui/session.html");
pub(crate) const HEALTH: &str = "beankeeper-desktop-v1";

pub(crate) fn page(title: &str, description: &str, path: &str, content: Markup) -> Html<String> {
    let mut navigation = Markup::default();
    for (href, label) in [
        ("/", "Dashboard"),
        ("/accounts", "Accounts"),
        ("/transactions", "Transactions"),
        ("/reports", "Reports"),
        ("/reconciliation", "Reconciliation"),
        ("/review", "Review queue"),
        ("/settings", "License and updates"),
    ] {
        write_markup!(
            navigation,
            "      <a href=\"{}\" {}>{}</a>\n",
            href,
            Attribute::CurrentPage(path == href),
            label,
        );
    }
    let title = Markup::escaped(title);
    markup!(SHELL, title, navigation, title, description, content).into_html()
}

pub(crate) fn session_controls(session: &HumanSession) -> Markup {
    let connection = if session.agent().is_some() {
        "Your agent is connected to this browser."
    } else {
        "Your browser session is active. Open your agent's connection link here to let it navigate this workspace."
    };
    let csrf = session.csrf_token();
    markup!(SESSION_CONTROLS, connection, csrf, csrf)
}

pub(crate) fn message(title: &str, content: Markup) -> Html<String> {
    page(title, "", "", content)
}

#[derive(Clone)]
struct ViewState {
    db: Arc<Mutex<Db>>,
    company: Option<String>,
}

pub(crate) fn router(db: Db, company: Option<String>, pairing: PairingState) -> Router {
    let data = Router::new()
        .route("/", get(view))
        .route(
            "/settings",
            get(|session: HumanSession| async move {
                let status = tokio::task::spawn_blocking(crate::product::status_text)
                    .await
                    .unwrap_or_else(|_| "Desktop status unavailable.".into());
                page(
                    "License and updates",
                    "Offline license and last update check",
                    "/settings",
                    markup!("<pre>{}</pre>{}", status, session_controls(&session)),
                )
            }),
        )
        .route("/accounts", get(view))
        .route("/transactions", get(view))
        .route("/transactions/{id}", get(view))
        .route("/reports", get(report))
        .route("/reports/{id}", get(report))
        .route("/reconciliation", get(report))
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

async fn report(
    State(state): State<ViewState>,
    session: HumanSession,
    uri: Uri,
    query: Result<Query<ReportQuery>, axum::extract::rejection::QueryRejection>,
) -> Response {
    let mut query = match query {
        Ok(Query(query)) => query,
        Err(_) => {
            return (
                StatusCode::BAD_REQUEST,
                page(
                    "Invalid report filters",
                    "Check the URL filters and try again.",
                    "/reports",
                    markup!("<p><a href=\"/reports\">Choose a report</a></p>"),
                ),
            )
                .into_response();
        }
    };
    if query.filters.company.is_none() {
        query.filters.company = state.company;
    }
    let result = tokio::task::spawn_blocking(move || {
        let query = match query.parse(uri.path()) {
            Ok(query) => query,
            Err(error) => return Ok(Err(error)),
        };
        let db = state.db.lock().map_err(|_| ())?;
        let _snapshot = db.conn().unchecked_transaction().map_err(|_| ())?;
        Ok::<_, ()>(reports::render(&db, uri.path(), &query))
    })
    .await;
    match result {
        Ok(Ok(Ok(ReportResponse::Page(view)))) => page(
            view.title,
            view.description,
            view.nav,
            with_session(view.content, &session),
        )
        .into_response(),
        Ok(Ok(Ok(ReportResponse::Download {
            body,
            content_type,
            filename,
        }))) => (
            [
                (header::CONTENT_TYPE, content_type.to_owned()),
                (
                    header::CONTENT_DISPOSITION,
                    format!("attachment; filename=\"{filename}\""),
                ),
            ],
            body,
        )
            .into_response(),
        Ok(Ok(Err(error))) => {
            if error.status().is_server_error() {
                eprintln!("Report view failed: {error:?}");
            }
            (
                error.status(),
                page(
                    "Unable to display this report",
                    &error.to_string(),
                    "/reports",
                    markup!(
                        "<p><a href=\"/reports\">Choose a report</a> and check your filters.</p>"
                    ),
                ),
            )
                .into_response()
        }
        _ => (
            StatusCode::INTERNAL_SERVER_ERROR,
            page(
                "Ledger unavailable",
                "The ledger could not be read. Check the server terminal and try again.",
                "/reports",
                Markup::default(),
            ),
        )
            .into_response(),
    }
}

async fn view(
    State(state): State<ViewState>,
    session: HumanSession,
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
                    markup!(
                        "<p><a href=\"/transactions\">Reset register filters</a></p>{}",
                        session_controls(&session)
                    ),
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
        let query = match query.parse() {
            Ok(query) => query,
            Err(error) => return Ok(Err(error)),
        };
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
            with_session(view.content, &session),
        )
        .into_response(),
        Ok(Ok(Err(error))) => {
            if error.status().is_server_error() {
                eprintln!("Ledger view failed: {error:?}");
            }
            (error.status(), page("Unable to display this view", &error.to_string(), "", markup!("<p><a href=\"/\">Return to dashboard</a> or <a href=\"/transactions\">reset register filters</a>.</p>{}", session_controls(&session)))).into_response()
        }
        _ => (
            StatusCode::INTERNAL_SERVER_ERROR,
            page(
                "Ledger unavailable",
                "The ledger could not be read. Try again or check the server terminal.",
                "",
                session_controls(&session),
            ),
        )
            .into_response(),
    }
}

fn with_session(mut content: Markup, session: &HumanSession) -> Markup {
    content.append(&session_controls(session));
    content
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn page_text_cannot_replace_template_slots() {
        let Html(body) = page(
            "Title {{description}}",
            "Café & bread {{content}}",
            "/reports",
            markup!("<p>{}</p>", "Daily summary"),
        );
        assert!(body.contains("<h1>Title {{description}}</h1>"));
        assert!(body.contains("<p class=\"intro\">Café &amp; bread {{content}}</p>"));
        assert!(body.contains("<a href=\"/reports\" aria-current=\"page\">"));
        assert!(body.contains("<p>Daily summary</p>"));
    }
}
