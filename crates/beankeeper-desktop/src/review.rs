//! Browser-only proposal review using the upstream atomic ledger operations.

use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

use axum::{
    Form, Router,
    extract::{Path, Query, State},
    http::StatusCode,
    middleware,
    response::{IntoResponse, Redirect, Response},
    routing::get,
};
use beankeeper::types::{Currency, Money};
use beankeeper_cli::{
    capability::Capability,
    commands::verify,
    db::{
        self, Actor, Db,
        proposals::{self, Proposal, ProposalId, ProposalState, Resolution, ResolvedBy},
        spend_limits::Cap,
    },
    error::CliError,
    identity::{self, IssueRequest},
};
use chrono::{DateTime, SecondsFormat, Utc};
use secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Deserializer};

use crate::{
    markup::{Attribute, Markup, markup, write_markup},
    money,
    pairing::{self, HumanSession, HumanWrite, PairingState},
    ui,
    views::encode,
};

#[derive(Clone)]
struct ReviewState(Arc<Mutex<Review>>);

struct Review {
    db: Db,
    company: Option<CompanySlug>,
    // Never sourced from request bearer credentials or exposed to a client.
    authority: ApprovalAuthority,
    verification: Option<Verification>,
}

enum ApprovalAuthority {
    PolicyOff,
    Credential(SecretString),
}

impl ApprovalAuthority {
    fn for_launch(db: &Db) -> Result<Self, CliError> {
        if !db::policy::get(db.conn())?.enforce {
            return Ok(Self::PolicyOff);
        }

        let capabilities =
            [Capability::Read, Capability::Approve].map(|capability| capability.path().to_owned());
        let issued = identity::issue(
            db.conn(),
            &Actor::new("desktop-local-human"),
            &IssueRequest {
                // Upstream generates a unique TypeID from this name prefix.
                name: "desktop_human",
                capabilities: &capabilities,
                ttl: Duration::from_hours(12),
                public_key: None,
                generate_key: true,
            },
        )?;
        let issued: IssuedApprovalCredential = serde_json::from_value(issued).map_err(|_| {
            CliError::General("human approval credential could not be created".into())
        })?;
        Ok(Self::Credential(issued.token))
    }

    fn actor(&self, db: &Db) -> Result<Actor, CliError> {
        let actor = Actor::new("desktop-local-human");
        match self {
            Self::Credential(token) => {
                Ok(actor.with_identity(identity::verify(db.conn(), token.expose_secret())?))
            }
            Self::PolicyOff => Ok(actor),
        }
    }
}

#[derive(Deserialize)]
struct IssuedApprovalCredential {
    #[serde(deserialize_with = "deserialize_approval_token")]
    token: SecretString,
}

fn deserialize_approval_token<'de, D: Deserializer<'de>>(
    input: D,
) -> Result<SecretString, D::Error> {
    let token = String::deserialize(input)?;
    if token.is_empty() {
        return Err(serde::de::Error::custom("approval credential is empty"));
    }
    Ok(SecretString::from(token))
}

enum Verification {
    Passed { at: DateTime<Utc> },
    Failed { at: DateTime<Utc>, error: CliError },
}

impl Verification {
    fn check(db: &Db) -> Self {
        let at = Utc::now();
        match verify::execute_json(db) {
            Ok(_) => Self::Passed { at },
            Err(error) => Self::Failed { at, error },
        }
    }

    fn at(&self) -> DateTime<Utc> {
        match self {
            Self::Passed { at } | Self::Failed { at, .. } => *at,
        }
    }
}

impl Review {
    fn decide(
        &mut self,
        _write: &HumanWrite,
        company: &CompanySlug,
        id: &ProposalId,
        decision: Decision,
    ) -> Result<Response, CliError> {
        let actor = self.authority.actor(&self.db)?;
        let proposal = proposals::get(self.db.conn(), company.as_str(), id.as_str())?;
        if actor.agent_uri() == Some(proposal.proposer_uri.as_str()) {
            return Err(CliError::Forbidden(
                "a proposer cannot approve or reject as the local human".into(),
            ));
        }

        // A failed fresh check must not leave an older intact result visible.
        self.verification = None;
        // Full verification and resolution share one upstream savepoint.
        let conn = self.db.conn();
        db::audit::atomically(conn, || {
            verify::execute_json(&self.db)?;
            match decision {
                Decision::Approve => {
                    db::audit::approve_proposal(conn, &actor, company.as_str(), id.as_str())?;
                }
                Decision::Reject { reason } => {
                    db::audit::reject_proposal(
                        conn,
                        &actor,
                        company.as_str(),
                        id.as_str(),
                        reason.as_ref().map(NonEmptyReason::as_str),
                    )?;
                }
            }
            Ok(())
        })?;
        Ok(Redirect::to(&format!(
            "/review/{id}?company={}",
            encode(company.as_str())
        ))
        .into_response())
    }
}

/// The decrypted local trust root delegates approval to this launch only.
/// This credential never becomes an MCP fallback or a browser credential.
pub(crate) fn router(
    db: Db,
    company: Option<String>,
    pairing: PairingState,
) -> Result<Router, CliError> {
    let authority = ApprovalAuthority::for_launch(&db)?;
    let state = ReviewState(Arc::new(Mutex::new(Review {
        db,
        company: company.map(CompanySlug),
        authority,
        verification: None,
    })));
    Ok(Router::new()
        .route(crate::links::REVIEW, get(queue))
        .route(crate::links::REVIEW_ROUTE, get(detail))
        .route("/review/{id}/approve", pairing::human_post(approve))
        .route("/review/{id}/reject", pairing::human_post(reject))
        .route("/review/verify", pairing::human_post(recheck))
        .route_layer(middleware::from_fn_with_state(
            pairing,
            pairing::human_guard,
        ))
        .with_state(state))
}

#[derive(Deserialize)]
#[serde(transparent)]
struct CompanySlug(String);

impl CompanySlug {
    fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Default, Deserialize)]
struct ReviewQuery {
    company: Option<CompanySlug>,
}

#[derive(Deserialize)]
struct ApproveForm {
    company: CompanySlug,
}

#[derive(Deserialize)]
struct RejectForm {
    company: CompanySlug,
    reason: Option<String>,
}

enum Decision {
    Approve,
    Reject { reason: Option<NonEmptyReason> },
}

struct NonEmptyReason(String);

impl NonEmptyReason {
    fn new(value: String) -> Option<Self> {
        (!value.trim().is_empty()).then_some(Self(value))
    }

    fn as_str(&self) -> &str {
        &self.0
    }
}

fn failure(error: &CliError, session: Option<&HumanSession>) -> Response {
    let status = match error {
        CliError::NotFound(_) => StatusCode::NOT_FOUND,
        CliError::Validation(_) => StatusCode::CONFLICT,
        CliError::Unauthorized(_) | CliError::Forbidden(_) => StatusCode::FORBIDDEN,
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    };
    let message = match status {
        StatusCode::CONFLICT => {
            "The proposal changed or cannot be posted. Reload its detail and check the ledger before trying again."
        }
        StatusCode::NOT_FOUND => "This proposal or company was not found.",
        StatusCode::FORBIDDEN => {
            "Human approval authority is unavailable or expired. Restart the local server and pair again."
        }
        _ => {
            "The ledger could not complete this request. Check the server log and re-run verification."
        }
    };
    eprintln!("Browser review failed: {error}");
    (
        status,
        ui::page(
            "Review unavailable",
            message,
            "/review",
            &markup!(
                "<p><a href=\"/review\">Return to review queue</a></p>{}",
                session.map(ui::session_controls).unwrap_or_default()
            ),
        ),
    )
        .into_response()
}

async fn run<F>(state: ReviewState, session: Option<HumanSession>, operation: F) -> Response
where
    F: FnOnce(&mut Review) -> Result<Response, CliError> + Send + 'static,
{
    match tokio::task::spawn_blocking(move || {
        let mut review = state
            .0
            .lock()
            .map_err(|_| CliError::General("review state unavailable".into()))?;
        operation(&mut review)
    })
    .await
    {
        Ok(Ok(response)) => response,
        Ok(Err(error)) => failure(&error, session.as_ref()),
        Err(_) => failure(
            &CliError::General("review worker failed".into()),
            session.as_ref(),
        ),
    }
}

async fn queue(
    State(state): State<ReviewState>,
    session: HumanSession,
    Query(query): Query<ReviewQuery>,
) -> Response {
    run(state, Some(session.clone()), move |review| {
        render(review, &query, None, &session)
    })
    .await
}

async fn detail(
    State(state): State<ReviewState>,
    session: HumanSession,
    Path(id): Path<String>,
    Query(query): Query<ReviewQuery>,
) -> Response {
    run(state, Some(session.clone()), move |review| {
        render(review, &query, Some(&id), &session)
    })
    .await
}

async fn approve(
    write: HumanWrite,
    State(state): State<ReviewState>,
    Path(id): Path<String>,
    Form(form): Form<ApproveForm>,
) -> Response {
    resolve(write, state, id, form.company, Decision::Approve).await
}

async fn reject(
    write: HumanWrite,
    State(state): State<ReviewState>,
    Path(id): Path<String>,
    Form(form): Form<RejectForm>,
) -> Response {
    let reason = form.reason.and_then(NonEmptyReason::new);
    resolve(write, state, id, form.company, Decision::Reject { reason }).await
}

async fn resolve(
    write: HumanWrite,
    state: ReviewState,
    id: String,
    company: CompanySlug,
    decision: Decision,
) -> Response {
    run(state, None, move |review| {
        review.decide(&write, &company, &id.parse()?, decision)
    })
    .await
}

async fn recheck(
    _write: HumanWrite,
    State(state): State<ReviewState>,
    Form(query): Form<ReviewQuery>,
) -> Response {
    run(state, None, move |review| {
        review.verification = Some(Verification::check(&review.db));
        let suffix = query
            .company
            .as_ref()
            .map(|company| format!("?company={}", encode(company.as_str())))
            .unwrap_or_default();
        Ok(Redirect::to(&format!("/review{suffix}")).into_response())
    })
    .await
}

fn render(
    review: &mut Review,
    query: &ReviewQuery,
    id: Option<&str>,
    session: &HumanSession,
) -> Result<Response, CliError> {
    let verification = review
        .verification
        .get_or_insert_with(|| Verification::check(&review.db));
    let actor = review.authority.actor(&review.db)?;
    let _snapshot = review.db.conn().unchecked_transaction()?;
    let companies = db::list_companies(review.db.conn())?;
    let selected_company = query
        .company
        .as_ref()
        .map(CompanySlug::as_str)
        .filter(|value| !value.is_empty())
        .or_else(|| review.company.as_ref().map(CompanySlug::as_str))
        .or_else(|| companies.first().map(|company| company.slug.as_str()));
    let mut html = verification_panel(verification, session, selected_company);
    let mut proposal_html = Markup::default();
    let result = (|| -> Result<(), CliError> {
        let html = &mut proposal_html;
        if let Some(company) = selected_company {
            if !companies.iter().any(|item| item.slug == company) {
                return Err(CliError::NotFound("company not found".into()));
            }
            write_markup!(
                html,
                "<form method=\"get\" action=\"/review\"><label>Company<select name=\"company\">"
            );
            for item in &companies {
                write_markup!(
                    html,
                    "<option value=\"{}\"{}>{} ({})</option>",
                    &item.slug,
                    Attribute::Selected(item.slug == company),
                    &item.name,
                    &item.slug
                );
            }
            write_markup!(
                html,
                "</select></label><button type=\"submit\">Show queue</button></form>"
            );
            if let Some(id) = id {
                let id: ProposalId = id
                    .parse()
                    .map_err(|_| CliError::NotFound("proposal not found".into()))?;
                let proposal = proposals::get(review.db.conn(), company, id.as_str())?;
                proposal_detail(review, &proposal, session, html)?;
            } else {
                let pending = proposals::list(review.db.conn(), company, &actor, false)?;
                write_markup!(html, "<section><h2>Pending proposals</h2>");
                if pending.is_empty() {
                    write_markup!(
                        html,
                        "<p>No pending proposals. Ask your agent to propose a posting when work needs your review.</p>"
                    );
                }
                for proposal in &pending {
                    write_markup!(
                        html,
                        "<article><h3><a href=\"/review/{}?company={}\">{}</a></h3>",
                        proposal.id,
                        encode(company),
                        &proposal.payload.description
                    );
                    summary(review, proposal, html)?;
                    write_markup!(html, "</article>");
                }
                write_markup!(html, "</section>");
            }
        } else {
            write_markup!(
                html,
                "<p>No companies yet. Ask your agent to create your books.</p>"
            );
        }
        Ok(())
    })();
    if let Err(error) = result {
        if matches!(error, CliError::NotFound(_) | CliError::Validation(_)) {
            return Err(error);
        }
        eprintln!("Browser proposal data failed: {error}");
        write_markup!(
            html,
            "<section><h2>Proposal data unavailable</h2><p>The ledger could not supply trustworthy proposal data. Review the verification result above, then check the server log and repair the ledger before deciding.</p></section>"
        );
    } else {
        html.append(&proposal_html);
    }
    html.append(&ui::session_controls(session));
    Ok(ui::page(
        "Review queue",
        "Your agent proposes. You review the full posting and decide.",
        "/review",
        &html,
    )
    .into_response())
}

fn verification_panel(
    verification: &Verification,
    session: &HumanSession,
    company: Option<&str>,
) -> Markup {
    let mut html = Markup::default();
    write_markup!(
        html,
        "<section aria-labelledby=\"verification\"><h2 id=\"verification\">Ledger verification</h2><p>Last checked: <time>{}</time>. This is a snapshot; re-run after changes made outside this browser.</p>",
        verification
            .at()
            .to_rfc3339_opts(SecondsFormat::Millis, true)
    );
    match verification {
        Verification::Passed { .. } => write_markup!(
            html,
            "<p><strong>Intact: hash chain and all bk verify checks passed.</strong></p>"
        ),
        Verification::Failed { error, .. } => write_markup!(
            html,
            "<p role=\"alert\"><strong>Verification failed. Ledger integrity is not confirmed.</strong> {}</p>",
            error
        ),
    }
    write_markup!(
        html,
        "<form method=\"post\" action=\"/review/verify\"><input type=\"hidden\" name=\"csrf\" value=\"{}\"><input type=\"hidden\" name=\"company\" value=\"{}\"><button type=\"submit\">Re-run verification</button></form></section>",
        session.csrf_token(),
        company.unwrap_or_default()
    );
    html
}

fn summary(review: &Review, proposal: &Proposal, html: &mut Markup) -> Result<(), CliError> {
    write_markup!(
        html,
        "<dl><dt>Company</dt><dd>{}</dd><dt>Amount (debit total)</dt><dd>{}</dd><dt>Proposer</dt><dd>{}</dd><dt>Proposer token ID</dt><dd>{}</dd><dt>Created</dt><dd>{}</dd><dt>Status</dt><dd>{}</dd><dt>Reason / purpose</dt><dd>{}</dd></dl>",
        &proposal.company_slug,
        &money::minor(proposal.debit_total, &proposal.currency),
        &proposal.proposer_uri,
        &proposal.proposer_jti,
        &proposal.created_at,
        proposal.state.code(),
        &proposal.payload.description
    );
    let limits = db::spend_limits::list(
        review.db.conn(),
        &proposal.proposer_uri,
        Some(&proposal.company_slug),
    )?;
    write_markup!(html, "<p>Current spend-limit context: ");
    let currency = Currency::from_code(&proposal.currency)
        .map_err(|error| CliError::Validation(error.to_string()))?;
    if let Some(limit) = limits.iter().find(|limit| limit.currency() == currency) {
        let cap = |value: Option<Cap>| {
            value.map_or_else(
                || "uncapped".into(),
                |value| money::ledger(Money::from_minor(i128::from(value.minor_units()), currency)),
            )
        };
        let caps = limit.caps();
        write_markup!(
            html,
            "max per posting {}, daily cap {}.",
            cap(caps.max_post()),
            cap(caps.daily_limit()),
        );
    } else if limits.is_empty() {
        write_markup!(html, "no caps configured for this agent and company.");
    } else {
        write_markup!(
            html,
            "no cap configured for this currency; direct agent posting would be blocked."
        );
    }
    write_markup!(
        html,
        " These are current caps, not historical usage or the reason the agent proposed. Human approval can override caps; the amount still counts toward the proposer’s spend.</p>"
    );
    Ok(())
}

fn proposal_detail(
    review: &Review,
    proposal: &Proposal,
    session: &HumanSession,
    html: &mut Markup,
) -> Result<(), CliError> {
    write_markup!(
        html,
        "<section><h2>{}</h2><p>Proposal <code>{}</code></p>",
        &proposal.payload.description,
        proposal.id
    );
    summary(review, proposal, html)?;
    let posting = &proposal.payload;
    write_markup!(
        html,
        "<h3>Proposed posting</h3><dl><dt>Date</dt><dd>{}</dd><dt>Currency</dt><dd>{}</dd><dt>Reference</dt><dd>{}</dd></dl><div class=\"table-scroll\"><table><caption>Proposed entries in transaction currency</caption><thead><tr><th scope=\"col\">Account</th><th scope=\"col\">Direction</th><th scope=\"col\">Amount</th><th scope=\"col\">Memo</th><th scope=\"col\">Tax category</th></tr></thead><tbody>",
        &posting.date,
        &posting.currency,
        posting.reference.as_deref().unwrap_or("Not recorded")
    );
    for entry in &posting.entries {
        write_markup!(
            html,
            "<tr><th scope=\"row\">{}</th><td>{}</td><td class=\"amount\">{}</td><td>{}</td><td>{}</td></tr>",
            &entry.account_code,
            &entry.direction,
            &money::minor(entry.amount, &posting.currency),
            entry.memo.as_deref().unwrap_or("Not recorded"),
            entry.tax_category.as_deref().unwrap_or("Not recorded")
        );
    }
    write_markup!(html, "</tbody></table></div>");
    let payload = serde_json::to_string_pretty(&proposal.payload)
        .map_err(|error| CliError::General(error.to_string()))?;
    write_markup!(
        html,
        "<details><summary>Full proposed posting, including metadata and retry policy (raw minor units)</summary><pre>{}</pre></details>",
        &payload
    );
    match &proposal.state {
        ProposalState::Pending => {
            write_markup!(
                html,
                "<p>Approve posts this exact request as the local human. Reject records your decision without posting. Approval cannot be undone from this queue.</p><form method=\"post\" action=\"/review/{}/approve\"><input type=\"hidden\" name=\"csrf\" value=\"{}\"><input type=\"hidden\" name=\"company\" value=\"{}\"><button type=\"submit\">Approve and post {}</button></form><form method=\"post\" action=\"/review/{}/reject\"><input type=\"hidden\" name=\"csrf\" value=\"{}\"><input type=\"hidden\" name=\"company\" value=\"{}\"><label>Rejection reason (optional)<input name=\"reason\" maxlength=\"2000\"></label><button type=\"submit\">Reject proposal</button></form>",
                proposal.id,
                session.csrf_token(),
                &proposal.company_slug,
                &money::minor(proposal.debit_total, &proposal.currency),
                proposal.id,
                session.csrf_token(),
                &proposal.company_slug
            );
        }
        ProposalState::Approved {
            resolution,
            transaction_id,
        } => {
            resolution_detail(proposal, resolution, Some(*transaction_id), html);
        }
        ProposalState::Rejected { resolution, .. }
        | ProposalState::Withdrawn { resolution, .. } => {
            resolution_detail(proposal, resolution, None, html);
        }
    }
    write_markup!(
        html,
        "<p><a href=\"/review?company={}\">Back to pending proposals</a></p></section>",
        encode(&proposal.company_slug)
    );
    Ok(())
}

fn resolution_detail(
    proposal: &Proposal,
    resolution: &Resolution,
    transaction: Option<i64>,
    html: &mut Markup,
) {
    let resolver = match &resolution.by {
        ResolvedBy::Local => "local human",
        ResolvedBy::Agent { uri, .. } => uri,
    };
    write_markup!(
        html,
        "<p>Resolved at {} by {}.</p>",
        &resolution.at,
        resolver
    );
    if let Some(reason) = proposal.state.reason() {
        write_markup!(html, "<p>Decision reason: {}</p>", reason);
    }
    if let Some(id) = transaction {
        write_markup!(
            html,
            "<p><a href=\"/transactions/{}?company={}\">View posted transaction</a></p>",
            id,
            encode(&proposal.company_slug)
        );
    }
}
