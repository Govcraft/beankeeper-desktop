use axum::{
    Router,
    http::{StatusCode, header},
    response::{Html, IntoResponse, Response},
    routing::get,
};

const SHELL: &str = include_str!("../../../ui/index.html");
const STYLE: &str = include_str!("../../../ui/style.css");
pub(crate) const HEALTH: &str = "beankeeper-desktop-v1";

fn page(title: &str, description: &str, path: &str) -> Html<String> {
    Html(
        SHELL
            .replace("{{title}}", title)
            .replace("{{description}}", description)
            .replace(
                &format!("href=\"{path}\" data-view"),
                &format!("href=\"{path}\" aria-current=\"page\""),
            ),
    )
}

pub(crate) fn router() -> Router {
    Router::new()
        .route(
            "/",
            get(|| async {
                page(
                    "Beankeeper is running",
                    "Ask your agent to work with your books. Your local workspace is ready.",
                    "/",
                )
            }),
        )
        .route(
            "/transactions",
            get(|| async {
                page(
                    "Transactions",
                    "Your transaction register will appear here in a future update.",
                    "/transactions",
                )
            }),
        )
        .route(
            "/reports",
            get(|| async {
                page(
                    "Reports",
                    "Your financial reports will appear here in a future update.",
                    "/reports",
                )
            }),
        )
        .route(
            "/review",
            get(|| async {
                page(
                    "Review queue",
                    "Human review of agent proposals will be available in a future update.",
                    "/review",
                )
            }),
        )
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
