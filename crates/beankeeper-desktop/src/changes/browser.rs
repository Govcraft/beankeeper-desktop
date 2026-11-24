use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
};

use axum::{
    Router,
    extract::{Path, Query, State},
    http::StatusCode,
    middleware,
    response::{IntoResponse, Response},
    routing::get,
};
use beankeeper::types::Money;
use beankeeper_cli::{
    db::{
        Actor, Db,
        audit::receipts::{
            AuditCursor, AuditReceipt, PostingSnapshot, ReceiptChange, ReceiptEvent, ReceiptId,
            ReceiptUndoOutcome, UndoOutcome, undo_change,
        },
        companies::{CompanySlug, list_companies},
    },
    error::CliError,
    identity::{self, Granted, OwnerUndo},
};
use secrecy::ExposeSecret;
use serde::Deserialize;

use crate::{
    instance::BooksPath,
    markup::{Markup, markup, write_markup},
    money,
    pairing::{self, HumanSession, HumanWrite, PairingState},
    private_file::{BEARER_FILE_LIMIT, PrivateFile},
    setup::OwnerCredential,
    ui,
    views::encode,
};

use super::{PATH, Report, ReportRange, UndoLink, book, description};

const TITLE: &str = "Changes since you last looked";

#[derive(Clone)]
struct Browser(Arc<Mutex<Books>>);

struct Books {
    db: Db,
    path: BooksPath,
    company: Option<CompanySlug>,
    owner_file: PathBuf,
}

pub(crate) fn router(
    db: Db,
    path: BooksPath,
    company: Option<CompanySlug>,
    owner_file: PathBuf,
    pairing: PairingState,
) -> Router {
    Router::new()
        .route("/changes/{id}/undo", pairing::human_post(undo))
        .route(PATH, get(view))
        .route_layer(middleware::from_fn_with_state(
            pairing,
            pairing::human_guard,
        ))
        .with_state(Browser(Arc::new(Mutex::new(Books {
            db,
            path,
            company,
            owner_file,
        }))))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ViewQuery {
    company: Option<CompanySlug>,
    after: Option<AuditCursor>,
    through: Option<AuditCursor>,
}

impl TryFrom<&ViewQuery> for Option<ReportRange> {
    type Error = CliError;

    fn try_from(query: &ViewQuery) -> Result<Self, Self::Error> {
        match (query.after, query.through) {
            (None, None) => Ok(None),
            (Some(after), Some(through)) => ReportRange::try_from((after, through)).map(Some),
            _ => Err(CliError::Validation(
                "after and through must be supplied together".into(),
            )),
        }
    }
}

async fn view(
    State(state): State<Browser>,
    session: HumanSession,
    Query(query): Query<ViewQuery>,
) -> Response {
    run(state, move |books| {
        let range = Option::<ReportRange>::try_from(&query)?;
        if query.company.is_none() && books.company.is_none() {
            let companies = list_companies(books.db.read())?;
            if companies.len() != 1 {
                let mut content = markup!("<p>Choose a book to see its changes.</p><ul>");
                if companies.is_empty() {
                    content = markup!("<p>There are no books yet.</p>");
                } else {
                    for company in companies {
                        write_markup!(
                            content,
                            "<li><a href=\"{}?company={}\">{}</a></li>",
                            PATH,
                            encode(company.slug.as_str()),
                            company.name
                        );
                    }
                    write_markup!(content, "</ul>");
                }
                return Ok(ui::page(TITLE, "", PATH, &content).into_response());
            }
        }
        let company = book(&books.db, query.company, books.company.clone())?;
        let report = Report::read(&books.db, &books.path, company, range)?;
        let can_undo = books.owner_file.is_file();
        Ok(render(&report, &session, can_undo))
    })
    .await
}

#[derive(Deserialize)]
struct UndoQuery {
    company: CompanySlug,
    after: AuditCursor,
    through: AuditCursor,
}

async fn undo(
    write: HumanWrite,
    State(state): State<Browser>,
    Path(receipt): Path<ReceiptId>,
    Query(query): Query<UndoQuery>,
) -> Response {
    run(state, move |books| {
        let range = ReportRange::try_from((query.after, query.through))?;
        let return_to = format!("{}#receipt-{receipt}", range.path(&query.company));
        let outcome = books.undo(&write, &query.company, receipt);
        Ok(undo_response(outcome, &return_to))
    })
    .await
}

impl Books {
    fn undo(
        &self,
        _write: &HumanWrite,
        company: &CompanySlug,
        receipt: ReceiptId,
    ) -> Result<ReceiptUndoOutcome, CliError> {
        let owner = PrivateFile::open(&self.owner_file)
            .and_then(|file| file.read_secret(BEARER_FILE_LIMIT))
            .map_err(|_| {
                CliError::Forbidden("Undo requires Desktop's private owner credential.".into())
            })?;
        let owner = OwnerCredential::try_from(identity::verify(
            self.db.read(),
            owner.expose_secret().trim(),
        )?)?;
        let actor = Actor::try_from(owner)?;
        Granted::<OwnerUndo>::with_db(&self.db, &actor, |grant| {
            undo_change(grant, company, receipt)
        })
    }
}

async fn run(
    state: Browser,
    operation: impl FnOnce(&Books) -> Result<Response, CliError> + Send + 'static,
) -> Response {
    let result = tokio::task::spawn_blocking(move || {
        let books = state
            .0
            .lock()
            .map_err(|_| CliError::General("changes connection unavailable".into()))?;
        operation(&books)
    })
    .await;
    match result {
        Ok(Ok(response)) => response,
        Ok(Err(error)) => error_response(&error, PATH),
        Err(_) => error_response(&CliError::General("changes worker failed".into()), PATH),
    }
}

fn undo_response(outcome: Result<ReceiptUndoOutcome, CliError>, return_to: &str) -> Response {
    let message = match outcome {
        Ok(ReceiptUndoOutcome::Single(UndoOutcome::Reversed { reversal, .. })) => format!(
            "Recorded reversal transaction {reversal}. The original is still in your books."
        ),
        Ok(ReceiptUndoOutcome::Single(UndoOutcome::AnnotationsRestored {
            transaction, ..
        })) => {
            format!("Restored the earlier annotations on transaction {transaction}.")
        }
        Ok(ReceiptUndoOutcome::Single(UndoOutcome::Archived { account, .. })) => {
            format!("Archived account {account}. Its history is still in your books.")
        }
        // Onboarding and any future undo kinds share the receipt-backed message.
        Ok(outcome) => format!(
            "Undid that change. Receipt {} records the undo and preserves the original history.",
            outcome.receipt()
        ),
        Err(error) => return error_response(&error, return_to),
    };
    ui::page("Change undone", "", PATH, &feedback(&message, return_to)).into_response()
}

fn feedback(message: &str, return_to: &str) -> Markup {
    markup!(
        "<p>{}</p><p><a href=\"{}\">Return to changes</a></p>",
        message,
        return_to
    )
}

fn error_response(error: &CliError, return_to: &str) -> Response {
    let (status, message) = match error {
        CliError::Unauthorized(_) | CliError::Forbidden(_) => {
            (StatusCode::FORBIDDEN, error.to_string())
        }
        CliError::ReceiptUndo(_) => (StatusCode::CONFLICT, error.to_string()),
        CliError::NotFound(_) => (StatusCode::NOT_FOUND, error.to_string()),
        CliError::Validation(_) => (StatusCode::BAD_REQUEST, error.to_string()),
        _ => {
            eprintln!("Changes view failed: {error}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Desktop couldn't load or apply this change.".into(),
            )
        }
    };
    (
        status,
        ui::page(
            "Changes unavailable",
            "",
            PATH,
            &feedback(&message, return_to),
        ),
    )
        .into_response()
}

fn render(report: &Report, session: &HumanSession, can_undo: bool) -> Response {
    let mut html = markup!("<p>{}</p>", report.summary());
    if !can_undo && report.receipts().next().is_some() {
        write_markup!(html, "<p>Undo is unavailable in this Desktop session.</p>");
    }
    for receipt in report.receipts() {
        let undo = if can_undo {
            UndoLink::try_from(receipt)
                .ok()
                .map(|target| target.action(report))
        } else {
            None
        };
        render_receipt(&mut html, receipt, undo.as_deref(), session);
    }
    if report.has_more {
        write_markup!(
            html,
            "<p>More changes are waiting. Ask your agent to summarize the next page after it marks these changes seen.</p>"
        );
    }
    ui::page(TITLE, "", PATH, &html).into_response()
}

impl UndoLink {
    fn action(self, report: &Report) -> String {
        format!(
            "{PATH}/{}/undo?company={}&after={}&through={}",
            self.0,
            encode(report.company.as_str()),
            report.range.after,
            report.range.through,
        )
    }
}

fn render_receipt(
    html: &mut Markup,
    receipt: &AuditReceipt,
    action: Option<&str>,
    session: &HumanSession,
) {
    let change = receipt.change();
    let receipt_id = receipt.id();
    write_markup!(
        html,
        "<article class=\"change-receipt\" id=\"receipt-{}\"><h2>{}</h2><dl>",
        receipt_id,
        description(receipt)
    );
    let time = change.changed_at();
    write_markup!(
        html,
        "<dt>Recorded</dt><dd><time datetime=\"{}\">{} UTC</time></dd><dt>By</dt><dd>{}</dd>",
        time.to_rfc3339(),
        time.format("%Y-%m-%d %H:%M"),
        change.actor().as_str()
    );
    if let Some(agent) = change.agent_uri() {
        write_markup!(
            html,
            "<dt>Verified identity</dt><dd>{}</dd>",
            agent.as_str()
        );
    }
    if let Some(reference) = receipt.source_reference() {
        write_markup!(html, "<dt>Source</dt><dd>{}</dd>", reference);
    }
    if let Some(original) = change.undo_of() {
        write_markup!(html, "<dt>Undo of receipt</dt><dd>{}</dd>", original);
    }
    write_markup!(html, "</dl>");
    render_event(html, change);
    if let Some(posting) = receipt.posting() {
        render_event(html, posting);
    }
    if let Some(action) = action {
        write_markup!(
            html,
            "<form method=\"post\" action=\"{}\"><input type=\"hidden\" name=\"csrf\" value=\"{}\"><button type=\"submit\" aria-label=\"Undo receipt {}\">Undo</button></form>",
            action,
            session.csrf_token(),
            receipt_id
        );
    }
    write_markup!(html, "</article>");
}

fn render_event(html: &mut Markup, event: &ReceiptEvent) {
    match event.summary() {
        ReceiptChange::PostingCreated { after } => render_posting(html, after),
        ReceiptChange::AccountCreated { after } => {
            write_markup!(
                html,
                "<p>Before: No account. After: {} ({})</p>",
                after.name,
                after.account_type
            );
        }
        ReceiptChange::AccountDeleted { before } => {
            write_markup!(
                html,
                "<p>Before: {} ({}). After: Account removed.</p>",
                before.name,
                before.account_type
            );
        }
        ReceiptChange::AccountArchived => {
            write_markup!(html, "<p>Before: Active. After: Archived.</p>");
        }
        ReceiptChange::PostingReversed => {
            if let Some(id) = event.related_posting() {
                write_markup!(html, "<p>Reversal transaction {}</p>", id);
            }
        }
        _ => {}
    }
}

fn render_posting(html: &mut Markup, posting: &PostingSnapshot) {
    write_markup!(
        html,
        "<p>{} · {} · {}</p><ul>",
        posting.description,
        posting.date,
        posting.currency.code()
    );
    for entry in &posting.entries {
        let amount = money::ledger(Money::from_minor(entry.amount.into(), posting.currency));
        write_markup!(
            html,
            "<li>{} {} to {}</li>",
            entry.direction,
            amount,
            entry.account_code
        );
    }
    write_markup!(html, "</ul>");
}
