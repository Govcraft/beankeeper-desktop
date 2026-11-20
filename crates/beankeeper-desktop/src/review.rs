//! Browser-only proposal review using the upstream atomic ledger operations.

use std::{
    fmt::Write,
    sync::{Arc, Mutex},
    time::Duration,
};

use axum::{
    Extension, Form, Router,
    extract::{Path, Query, State},
    http::StatusCode,
    middleware,
    response::{IntoResponse, Redirect, Response},
    routing::{get, post},
};
use beankeeper_cli::{
    commands::verify,
    db::{
        self, Actor, Db,
        proposals::{self, Proposal, ProposalId},
    },
    error::CliError,
    identity::{self, IssueRequest},
};
use secrecy::{ExposeSecret, SecretString};
use serde::Deserialize;

use crate::{
    pairing::{self, PairingState, Session},
    ui,
    views::{encode, escape},
};

#[derive(Clone)]
struct ReviewState(Arc<Mutex<Review>>);

struct Review {
    db: Db,
    company: Option<String>,
    // Never sourced from request bearer credentials or exposed to a client.
    human_token: Option<SecretString>,
    verification: Option<Verification>,
}

struct Verification {
    checked_at: String,
    error: Option<String>,
}

impl Review {
    fn actor(&self) -> Result<Actor, CliError> {
        let actor = Actor::new("desktop-local-human");
        match &self.human_token {
            Some(token) => {
                Ok(actor.with_identity(identity::verify(self.db.conn(), token.expose_secret())?))
            }
            None => Ok(actor),
        }
    }

    fn recheck(&mut self) -> Result<(), CliError> {
        let checked_at = self.db.conn().query_row(
            "SELECT strftime('%Y-%m-%dT%H:%M:%fZ', 'now')",
            [],
            |row| row.get(0),
        )?;
        self.verification = Some(Verification {
            checked_at,
            error: verify::execute_json(&self.db)
                .err()
                .map(|error| error.to_string()),
        });
        Ok(())
    }
}

/// The decrypted local trust root delegates approval to this launch only.
/// This credential never becomes an MCP fallback or a browser credential.
pub(crate) fn router(
    db: Db,
    company: Option<String>,
    pairing: PairingState,
) -> Result<Router, CliError> {
    let human_token = if db::policy::get(db.conn())?.enforce {
        let issued = identity::issue(
            db.conn(),
            &Actor::new("desktop-local-human"),
            &IssueRequest {
                // Upstream generates a unique TypeID from this name prefix.
                name: "desktop_human",
                capabilities: &["ledger/read".into(), "ledger/approve".into()],
                ttl: Duration::from_secs(12 * 60 * 60),
                public_key: None,
                generate_key: true,
            },
        )?;
        Some(SecretString::from(
            issued["token"]
                .as_str()
                .ok_or_else(|| {
                    CliError::General("human approval credential could not be created".into())
                })?
                .to_owned(),
        ))
    } else {
        None
    };
    let state = ReviewState(Arc::new(Mutex::new(Review {
        db,
        company,
        human_token,
        verification: None,
    })));
    Ok(Router::new()
        .route("/review", get(queue))
        .route("/review/{id}", get(detail))
        .route("/review/{id}/approve", post(approve))
        .route("/review/{id}/reject", post(reject))
        .route("/review/verify", post(recheck))
        .route_layer(middleware::from_fn_with_state(
            pairing,
            pairing::human_guard,
        ))
        .with_state(state))
}

#[derive(Default, Deserialize)]
struct ReviewQuery {
    company: Option<String>,
}

#[derive(Deserialize)]
struct Decision {
    company: String,
    reason: Option<String>,
}

fn failure(error: CliError) -> Response {
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
            "<p><a href=\"/review\">Return to review queue</a></p>",
        ),
    )
        .into_response()
}

async fn run<F>(state: ReviewState, operation: F) -> Response
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
        Ok(Err(error)) => failure(error),
        Err(_) => failure(CliError::General("review worker failed".into())),
    }
}

async fn queue(
    State(state): State<ReviewState>,
    Extension(session): Extension<Session>,
    Query(query): Query<ReviewQuery>,
) -> Response {
    run(state, move |review| render(review, &query, None, &session)).await
}

async fn detail(
    State(state): State<ReviewState>,
    Extension(session): Extension<Session>,
    Path(id): Path<String>,
    Query(query): Query<ReviewQuery>,
) -> Response {
    run(state, move |review| {
        render(review, &query, Some(&id), &session)
    })
    .await
}

async fn approve(
    State(state): State<ReviewState>,
    Path(id): Path<String>,
    Form(form): Form<Decision>,
) -> Response {
    resolve(state, id, form, true).await
}

async fn reject(
    State(state): State<ReviewState>,
    Path(id): Path<String>,
    Form(form): Form<Decision>,
) -> Response {
    resolve(state, id, form, false).await
}

async fn resolve(state: ReviewState, id: String, form: Decision, approve: bool) -> Response {
    run(state, move |review| {
        let id: ProposalId = id.parse()?;
        let actor = review.actor()?;
        let proposal = proposals::get(review.db.conn(), &form.company, id.as_str())?;
        if actor.agent_uri() == Some(proposal.proposer_uri.as_str()) {
            return Err(CliError::Forbidden(
                "a proposer cannot approve or reject as the local human".into(),
            ));
        }
        // A failed fresh check must not leave an older intact result visible.
        review.verification = None;
        // Full verification and resolution share one upstream savepoint.
        db::audit::atomically(review.db.conn(), || {
            verify::execute_json(&review.db)?;
            if approve {
                db::audit::approve_proposal(review.db.conn(), &actor, &form.company, id.as_str())?;
            } else {
                db::audit::reject_proposal(
                    review.db.conn(),
                    &actor,
                    &form.company,
                    id.as_str(),
                    form.reason
                        .as_deref()
                        .filter(|reason| !reason.trim().is_empty()),
                )?;
            }
            Ok(())
        })?;
        review.verification = None;
        Ok(
            Redirect::to(&format!("/review/{id}?company={}", encode(&form.company)))
                .into_response(),
        )
    })
    .await
}

async fn recheck(State(state): State<ReviewState>, Form(query): Form<ReviewQuery>) -> Response {
    run(state, move |review| {
        review.recheck()?;
        let suffix = query
            .company
            .as_deref()
            .map(|company| format!("?company={}", encode(company)))
            .unwrap_or_default();
        Ok(Redirect::to(&format!("/review{suffix}")).into_response())
    })
    .await
}

fn render(
    review: &mut Review,
    query: &ReviewQuery,
    id: Option<&str>,
    session: &Session,
) -> Result<Response, CliError> {
    if review.verification.is_none() {
        review.recheck()?;
    }
    let actor = review.actor()?;
    let _snapshot = review.db.conn().unchecked_transaction()?;
    let companies = db::list_companies(review.db.conn())?;
    let selected = query
        .company
        .as_deref()
        .filter(|value| !value.is_empty())
        .or(review.company.as_deref())
        .or_else(|| companies.first().map(|company| company.slug.as_str()));
    let mut html = String::new();
    if let Some(verification) = &review.verification {
        let _ = write!(
            html,
            "<section aria-labelledby=\"verification\"><h2 id=\"verification\">Ledger verification</h2><p>Last checked: <time>{}</time>. This is a snapshot; re-run after changes made outside this browser.</p>",
            escape(&verification.checked_at)
        );
        if let Some(error) = &verification.error {
            let _ = write!(
                html,
                "<p role=\"alert\"><strong>Verification failed. Ledger integrity is not confirmed.</strong> {}</p>",
                escape(error)
            );
        } else {
            html.push_str(
                "<p><strong>Intact: hash chain and all bk verify checks passed.</strong></p>",
            );
        }
        let _ = write!(
            html,
            "<form method=\"post\" action=\"/review/verify\"><input type=\"hidden\" name=\"csrf\" value=\"{}\"><input type=\"hidden\" name=\"company\" value=\"{}\"><button type=\"submit\">Re-run verification</button></form></section>",
            session.csrf.expose(),
            escape(selected.unwrap_or_default())
        );
    }
    let panel_length = html.len();
    let result = (|| -> Result<(), CliError> {
        if let Some(company) = selected {
            if !companies.iter().any(|item| item.slug == company) {
                return Err(CliError::NotFound("company not found".into()));
            }
            html.push_str(
                "<form method=\"get\" action=\"/review\"><label>Company<select name=\"company\">",
            );
            for item in &companies {
                let _ = write!(
                    html,
                    "<option value=\"{}\"{}>{} ({})</option>",
                    escape(&item.slug),
                    if item.slug == company {
                        " selected"
                    } else {
                        ""
                    },
                    escape(&item.name),
                    escape(&item.slug)
                );
            }
            html.push_str("</select></label><button type=\"submit\">Show queue</button></form>");
            if let Some(id) = id {
                let id: ProposalId = id
                    .parse()
                    .map_err(|_| CliError::NotFound("proposal not found".into()))?;
                let proposal = proposals::get(review.db.conn(), company, id.as_str())?;
                proposal_detail(review, &proposal, session, &mut html)?;
            } else {
                let pending = proposals::list(review.db.conn(), company, &actor, false)?;
                html.push_str("<section><h2>Pending proposals</h2>");
                if pending.is_empty() {
                    html.push_str("<p>No pending proposals. Ask your agent to propose a posting when work needs your review.</p>");
                }
                for proposal in &pending {
                    let _ = write!(
                        html,
                        "<article><h3><a href=\"/review/{}?company={}\">{}</a></h3>",
                        proposal.id,
                        encode(company),
                        escape(&proposal.payload.description)
                    );
                    summary(review, proposal, &mut html)?;
                    html.push_str("</article>");
                }
                html.push_str("</section>");
            }
        } else {
            html.push_str("<p>No companies yet. Ask your agent to create your books.</p>");
        }
        Ok(())
    })();
    if let Err(error) = result {
        if matches!(error, CliError::NotFound(_) | CliError::Validation(_)) {
            return Err(error);
        }
        eprintln!("Browser proposal data failed: {error}");
        html.truncate(panel_length);
        html.push_str("<section><h2>Proposal data unavailable</h2><p>The ledger could not supply trustworthy proposal data. Review the verification result above, then check the server log and repair the ledger before deciding.</p></section>");
    }
    html.push_str(&ui::session_controls(session));
    Ok(ui::page(
        "Review queue",
        "Your agent proposes. You review the full posting and decide.",
        "/review",
        &html,
    )
    .into_response())
}

fn summary(review: &Review, proposal: &Proposal, html: &mut String) -> Result<(), CliError> {
    let _ = write!(
        html,
        "<dl><dt>Company</dt><dd>{}</dd><dt>Amount (debit total)</dt><dd>{} {} minor units</dd><dt>Proposer</dt><dd>{}</dd><dt>Proposer token ID</dt><dd>{}</dd><dt>Created</dt><dd>{}</dd><dt>Status</dt><dd>{}</dd><dt>Reason / purpose</dt><dd>{}</dd></dl>",
        escape(&proposal.company_slug),
        proposal.debit_total,
        escape(&proposal.currency),
        escape(&proposal.proposer_uri),
        escape(&proposal.proposer_jti),
        escape(&proposal.created_at),
        escape(&proposal.status),
        escape(&proposal.payload.description)
    );
    let limits = db::spend_limits::list(
        review.db.conn(),
        &proposal.proposer_uri,
        Some(&proposal.company_slug),
    )?;
    html.push_str("<p>Current spend-limit context: ");
    if let Some(limit) = limits
        .iter()
        .find(|limit| limit.currency == proposal.currency)
    {
        let cap = |value: Option<i64>| {
            value
                .map(|value| value.to_string())
                .unwrap_or_else(|| "uncapped".into())
        };
        let _ = write!(
            html,
            "max per posting {}, daily cap {} ({} minor units).",
            cap(limit.max_post),
            cap(limit.daily_limit),
            escape(&limit.currency)
        );
    } else if limits.is_empty() {
        html.push_str("no caps configured for this agent and company.");
    } else {
        html.push_str(
            "no cap configured for this currency; direct agent posting would be blocked.",
        );
    }
    html.push_str(" These are current caps, not historical usage or the reason the agent proposed. Human approval can override caps; the amount still counts toward the proposer’s spend.</p>");
    Ok(())
}

fn proposal_detail(
    review: &Review,
    proposal: &Proposal,
    session: &Session,
    html: &mut String,
) -> Result<(), CliError> {
    let _ = write!(
        html,
        "<section><h2>{}</h2><p>Proposal <code>{}</code></p>",
        escape(&proposal.payload.description),
        proposal.id
    );
    summary(review, proposal, html)?;
    let posting = &proposal.payload;
    let _ = write!(
        html,
        "<h3>Proposed posting</h3><dl><dt>Date</dt><dd>{}</dd><dt>Currency</dt><dd>{}</dd><dt>Reference</dt><dd>{}</dd></dl><div class=\"table-scroll\"><table><caption>Proposed entries, amounts in minor units</caption><thead><tr><th scope=\"col\">Account</th><th scope=\"col\">Direction</th><th scope=\"col\">Amount</th><th scope=\"col\">Memo</th><th scope=\"col\">Tax category</th></tr></thead><tbody>",
        escape(&posting.date),
        escape(&posting.currency),
        escape(posting.reference.as_deref().unwrap_or("Not recorded"))
    );
    for entry in &posting.entries {
        let _ = write!(
            html,
            "<tr><th scope=\"row\">{}</th><td>{}</td><td class=\"amount\">{}</td><td>{}</td><td>{}</td></tr>",
            escape(&entry.account_code),
            escape(&entry.direction),
            entry.amount,
            escape(entry.memo.as_deref().unwrap_or("Not recorded")),
            escape(entry.tax_category.as_deref().unwrap_or("Not recorded"))
        );
    }
    html.push_str("</tbody></table></div>");
    let payload = serde_json::to_string_pretty(&proposal.payload)
        .map_err(|error| CliError::General(error.to_string()))?;
    let _ = write!(
        html,
        "<details><summary>Full proposed posting, including metadata and retry policy</summary><pre>{}</pre></details>",
        escape(&payload)
    );
    if proposal.status == "pending" {
        let _ = write!(
            html,
            "<p>Approve posts this exact request as the local human. Reject records your decision without posting. Approval cannot be undone from this queue.</p><form method=\"post\" action=\"/review/{}/approve\"><input type=\"hidden\" name=\"csrf\" value=\"{}\"><input type=\"hidden\" name=\"company\" value=\"{}\"><button type=\"submit\">Approve and post {} {} minor units</button></form><form method=\"post\" action=\"/review/{}/reject\"><input type=\"hidden\" name=\"csrf\" value=\"{}\"><input type=\"hidden\" name=\"company\" value=\"{}\"><label>Rejection reason (optional)<input name=\"reason\" maxlength=\"2000\"></label><button type=\"submit\">Reject proposal</button></form>",
            proposal.id,
            session.csrf.expose(),
            escape(&proposal.company_slug),
            proposal.debit_total,
            escape(&proposal.currency),
            proposal.id,
            session.csrf.expose(),
            escape(&proposal.company_slug)
        );
    } else {
        let _ = write!(
            html,
            "<p>Resolved at {} by {}.</p>",
            escape(proposal.resolved_at.as_deref().unwrap_or("not recorded")),
            escape(proposal.resolved_by_uri.as_deref().unwrap_or("local human"))
        );
        if let Some(reason) = &proposal.reason {
            let _ = write!(html, "<p>Decision reason: {}</p>", escape(reason));
        }
        if let Some(id) = proposal.transaction_id {
            let _ = write!(
                html,
                "<p><a href=\"/transactions/{id}?company={}\">View posted transaction</a></p>",
                encode(&proposal.company_slug)
            );
        }
    }
    let _ = write!(
        html,
        "<p><a href=\"/review?company={}\">Back to pending proposals</a></p></section>",
        encode(&proposal.company_slug)
    );
    Ok(())
}
