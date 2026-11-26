//! Browser-only proposal review using the upstream atomic ledger operations.

mod credential;

use std::{
    path::{Path as FsPath, PathBuf},
    sync::{Arc, Mutex},
};

use axum::{
    Form, Router,
    extract::{Path, Query, State},
    http::StatusCode,
    middleware,
    response::{IntoResponse, Redirect, Response},
    routing::get,
};
use beankeeper::types::{Amount, Money};
use beankeeper_cli::{
    db::{
        self, Actor, Db,
        companies::CompanySlug,
        ids::TransactionId,
        proposals::{self, Proposal, ProposalId, ProposalState, Resolution, ResolvedBy},
        spend_limits::Cap,
        verification::VerifiedLedger,
    },
    error::CliError,
    identity::{self, Admin, Approve, Granted, IssueRequest, ResolveProposal},
};
use chrono::{DateTime, SecondsFormat, Utc};
use credential::HumanCredentialRequest;
use secrecy::{ExposeSecret, SecretString};
use serde::Deserialize;

use crate::{
    markup::{Attribute, Markup, markup, write_markup},
    money,
    pairing::{self, HumanSession, HumanWrite, PairingState},
    private_file::{BEARER_FILE_LIMIT, PrivateFile},
    setup::OwnerCredential,
    ui,
    views::encode,
};

#[derive(Clone)]
struct ReviewState(Arc<Mutex<Review>>);

struct Review {
    db: Db,
    company: Option<CompanySlug>,
    // Never sourced from request bearer credentials or exposed to a client.
    authority: Option<ApprovalAuthority>,
    owner_file: PathBuf,
    verification: Option<Verification>,
}

enum ApprovalAuthority {
    PolicyOff,
    Credential(SecretString),
    Unavailable,
}

impl ApprovalAuthority {
    fn owner(db: &Db, owner_file: &FsPath) -> Result<Option<Actor>, CliError> {
        let Ok(owner_token) =
            PrivateFile::open(owner_file).and_then(|file| file.read_secret(BEARER_FILE_LIMIT))
        else {
            return Ok(None);
        };
        match identity::verify(db.read(), owner_token.expose_secret().trim())
            .and_then(OwnerCredential::try_from)
        {
            Ok(owner) => Ok(Some(Actor::try_from(owner)?)),
            Err(CliError::Unauthorized(_) | CliError::Forbidden(_)) => Ok(None),
            Err(error) => Err(error),
        }
    }

    fn for_launch(db: &Db, owner_file: &FsPath) -> Result<Self, CliError> {
        if !db::policy::get(db.read())?.enforce {
            return Ok(Self::PolicyOff);
        }
        let Some(owner) = Self::owner(db, owner_file)? else {
            return Ok(Self::Unavailable);
        };
        match Granted::<Admin>::with_db(db, &owner, |grant| {
            let request = HumanCredentialRequest::default();
            identity::issue_credential(grant, &IssueRequest::from(&request))
        }) {
            Ok(issued) => Ok(Self::Credential(issued.token().clone())),
            Err(CliError::Unauthorized(_) | CliError::Forbidden(_)) => Ok(Self::Unavailable),
            Err(error) => Err(error),
        }
    }

    fn actor(&self, db: &Db) -> Result<Actor, CliError> {
        let actor = Actor::new("desktop-local-human".parse()?);
        match self {
            Self::Credential(token) => {
                Ok(actor.with_identity(identity::verify(db.read(), token.expose_secret())?))
            }
            Self::PolicyOff => Ok(actor),
            Self::Unavailable => Err(CliError::Forbidden(
                "browser review requires a private owner credential with ledger/admin".into(),
            )),
        }
    }
}

enum Verification {
    Passed { at: DateTime<Utc> },
    Failed { at: DateTime<Utc>, error: CliError },
}

impl Verification {
    fn check(db: &Db) -> Self {
        let at = Utc::now();
        match VerifiedLedger::read(db) {
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

struct VerifiedProposal<'grant, C> {
    grant: Granted<'grant, C>,
    proposal: Proposal,
    _ledger: VerifiedLedger,
}

impl<'grant, C> VerifiedProposal<'grant, C> {
    fn read(
        grant: Granted<'grant, C>,
        db: &Db,
        company: &CompanySlug,
        id: &ProposalId,
        actor: &Actor,
    ) -> Result<Self, CliError> {
        let proposal = proposals::get(db.read(), company, id)?;
        if actor.agent_uri() == Some(proposal.proposer_uri.as_str()) {
            return Err(CliError::Forbidden(
                "a proposer cannot approve or reject as the local human".into(),
            ));
        }
        let ledger = VerifiedLedger::read(db)?;
        Ok(Self {
            grant,
            proposal,
            _ledger: ledger,
        })
    }
}

impl VerifiedProposal<'_, Approve> {
    fn approve(self) -> Result<Proposal, CliError> {
        db::audit::approve_proposal(self.grant, &self.proposal.company_slug, &self.proposal.id)
    }
}

impl VerifiedProposal<'_, ResolveProposal> {
    fn reject(self, reason: Option<&str>) -> Result<Proposal, CliError> {
        db::audit::reject_proposal(
            self.grant,
            &self.proposal.company_slug,
            &self.proposal.id,
            reason,
        )
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
        if self.authority.is_none() {
            self.authority = Some(ApprovalAuthority::for_launch(&self.db, &self.owner_file)?);
        }
        let actor = self
            .authority
            .as_ref()
            .unwrap_or(&ApprovalAuthority::Unavailable)
            .actor(&self.db)?;
        // A failed fresh check must not leave an older intact result visible.
        self.verification = None;
        // Each grant reserves the ledger writer before verification or resolution.
        match decision {
            Decision::Approve => Granted::<Approve>::with_db(&self.db, &actor, |grant| {
                VerifiedProposal::read(grant, &self.db, company, id, &actor)?.approve()
            })?,
            Decision::Reject { reason } => {
                Granted::<ResolveProposal>::with_db(&self.db, &actor, |grant| {
                    VerifiedProposal::read(grant, &self.db, company, id, &actor)?
                        .reject(reason.as_ref().map(NonEmptyReason::as_str))
                })?
            }
        };
        Ok(Redirect::to(&format!(
            "/review/{id}?company={}",
            encode(company.as_str())
        ))
        .into_response())
    }
}

/// The local owner delegates approval to this launch only.
/// This credential never becomes an MCP fallback or a browser credential.
pub(crate) fn router(
    db: Db,
    company: Option<CompanySlug>,
    pairing: PairingState,
    owner_file: &FsPath,
) -> Result<Router, CliError> {
    let authority = match ApprovalAuthority::for_launch(&db, owner_file) {
        Ok(authority) => Some(authority),
        Err(CliError::WriteDenied { .. }) => None,
        Err(error) => return Err(error),
    };
    let state = ReviewState(Arc::new(Mutex::new(Review {
        db,
        company,
        authority,
        owner_file: owner_file.into(),
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
    if let CliError::WriteDenied { reason, .. } = error {
        return (
            StatusCode::FORBIDDEN,
            ui::page(
                "Review unavailable",
                "",
                "/review",
                &markup!("<p>{}</p>", reason.as_ref()),
            ),
        )
            .into_response();
    }
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
            "Browser approval requires a private owner credential with ledger/admin. Check the credential, then restart Desktop and pair again."
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
    let id = id
        .map(|id| {
            id.parse::<ProposalId>()
                .map_err(|_| CliError::NotFound("proposal not found".into()))
        })
        .transpose()?;
    let verification = review
        .verification
        .get_or_insert_with(|| Verification::check(&review.db));
    review.db.read_snapshot(|reader| {
        let companies = db::list_companies(reader)?;
        let selected = query
            .company
            .as_ref()
            .or(review.company.as_ref())
            .or_else(|| companies.first().map(|company| &company.slug));
        let mut html = verification_panel(verification, session, selected);
        if let Some(company) = selected {
            let actor = match &review.authority {
                Some(authority) => authority.actor(&review.db),
                None => ApprovalAuthority::owner(&review.db, &review.owner_file)?.ok_or_else(|| {
                    CliError::Forbidden("browser review requires a private owner credential with ledger/admin".into())
                }),
            };
            let result = actor.and_then(|actor| render_proposals(
                &review.db,
                &actor,
                &companies,
                company,
                id.as_ref(),
                session,
            ));
            match result {
                Ok(proposals) => html.append(&proposals),
                Err(error @ (CliError::NotFound(_) | CliError::Validation(_))) => return Err(error),
                Err(CliError::Unauthorized(_) | CliError::Forbidden(_)) => {
                    write_markup!(html, "<p>Browser approval requires a private owner credential with ledger/admin. Check the credential, then restart Desktop and pair again.</p>");
                }
                Err(error) => {
                    eprintln!("Browser proposal data failed: {error}");
                    write_markup!(
                        html,
                        "<section><h2>Proposal data unavailable</h2><p>The ledger could not supply trustworthy proposal data. Review the verification result above, then check the server log and repair the ledger before deciding.</p></section>"
                    );
                }
            }
        } else {
            write_markup!(html, "<p>No companies yet. Ask your agent to create your books.</p>");
        }
        html.append(&ui::session_controls(session));
        Ok(ui::page(
            "Review queue",
            "Your agent proposes. You review the full posting and decide.",
            "/review",
            &html,
        )
        .into_response())
    })
}

fn render_proposals(
    db: &Db,
    actor: &Actor,
    companies: &[db::CompanyRow],
    company: &CompanySlug,
    id: Option<&ProposalId>,
    session: &HumanSession,
) -> Result<Markup, CliError> {
    if !companies.iter().any(|item| &item.slug == company) {
        return Err(CliError::NotFound("company not found".into()));
    }
    let reader = db.read();
    let mut html =
        markup!("<form method=\"get\" action=\"/review\"><label>Company<select name=\"company\">");
    for item in companies {
        write_markup!(
            html,
            "<option value=\"{}\"{}>{} ({})</option>",
            &item.slug,
            Attribute::Selected(&item.slug == company),
            &item.name,
            &item.slug
        );
    }
    write_markup!(
        html,
        "</select></label><button type=\"submit\">Show queue</button></form>"
    );
    if let Some(id) = id {
        let proposal = proposals::get(reader, company, id)?;
        proposal_detail(db, &proposal, session, &mut html)?;
        return Ok(html);
    }

    let pending = proposals::list(reader, company, actor, false)?;
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
            encode(company.as_str()),
            &proposal.payload.description
        );
        summary(db, proposal, &mut html)?;
        write_markup!(html, "</article>");
    }
    write_markup!(html, "</section>");
    Ok(html)
}

fn verification_panel(
    verification: &Verification,
    session: &HumanSession,
    company: Option<&CompanySlug>,
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
        "<form method=\"post\" action=\"/review/verify\"><input type=\"hidden\" name=\"csrf\" value=\"{}\">",
        session.csrf_token(),
    );
    if let Some(company) = company {
        write_markup!(
            html,
            "<input type=\"hidden\" name=\"company\" value=\"{}\">",
            company
        );
    }
    write_markup!(
        html,
        "<button type=\"submit\">Re-run verification</button></form></section>"
    );
    html
}

fn summary(db: &Db, proposal: &Proposal, html: &mut Markup) -> Result<(), CliError> {
    write_markup!(
        html,
        "<dl><dt>Company</dt><dd>{}</dd><dt>Amount (debit total)</dt><dd>{}</dd><dt>Proposer</dt><dd>{}</dd><dt>Proposer token ID</dt><dd>{}</dd><dt>Created</dt><dd>{}</dd><dt>Status</dt><dd>{}</dd><dt>Reason / purpose</dt><dd>{}</dd></dl>",
        &proposal.company_slug,
        &money::ledger(Money::from_minor(
            proposal.debit_total.into(),
            proposal.currency
        )),
        &proposal.proposer_uri,
        &proposal.proposer_jti,
        &proposal.created_at,
        proposal.state.code(),
        &proposal.payload.description
    );
    let limits = db::spend_limits::list(
        db.read(),
        &proposal.proposer_uri,
        Some(&proposal.company_slug),
    )?;
    write_markup!(html, "<p>Current spend-limit context: ");
    let currency = proposal.currency;
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
    db: &Db,
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
    summary(db, proposal, html)?;
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
            &money::ledger(Money::new(Amount::from(entry.amount), posting.currency)),
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
                &money::ledger(Money::from_minor(
                    proposal.debit_total.into(),
                    proposal.currency
                )),
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
        encode(proposal.company_slug.as_str())
    );
    Ok(())
}

fn resolution_detail(
    proposal: &Proposal,
    resolution: &Resolution,
    transaction: Option<TransactionId>,
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
            encode(proposal.company_slug.as_str())
        );
    }
}
