//! Browser review exercises the real server against isolated encrypted ledgers.

pub mod common;

use beankeeper::types::AccountType;
use std::time::Duration;

use beankeeper::types::Currency;
use beankeeper_cli::{
    commands::verify,
    db::{
        self, Actor, Db,
        proposals::{self, ProposalState, ResolvedBy},
        spend_limits::{Caps, SpendLimit},
    },
    identity::{self, Admin, Granted, IssueRequest},
};
use common::{Ledger, Server, TestResult, token};
use reqwest::blocking::{Client, RequestBuilder};
use serde_json::Value;

struct ReviewFixture {
    ledger: Ledger,
    proposer: Value,
    approver: Value,
}

impl ReviewFixture {
    fn new() -> TestResult<Self> {
        let ledger = Ledger::new()?;
        ledger.enable_browser_review()?;
        let db = ledger.db()?;
        Granted::<Admin>::with_db(&db, ledger.owner(), |grant| {
            db::create_company(grant, &"other".parse()?, &"Other Books".parse()?, None)
        })?;
        for company in ["acme", "other"] {
            Granted::<Admin>::with_db(&db, ledger.owner(), |grant| {
                db::create_account(
                    grant,
                    &db::CreateAccountParams {
                        company_slug: &company.parse()?,
                        code: "1000",
                        name: "Cash",
                        account_type: AccountType::Asset,
                        default_tax_category: None,
                    },
                )
            })?;
            Granted::<Admin>::with_db(&db, ledger.owner(), |grant| {
                db::create_account(
                    grant,
                    &db::CreateAccountParams {
                        company_slug: &company.parse()?,
                        code: "5000",
                        name: "Supplies",
                        account_type: AccountType::Expense,
                        default_tax_category: None,
                    },
                )
            })?;
        }
        let issue = |name, capabilities: &[String]| {
            Granted::<Admin>::with_db(&db, ledger.owner(), |grant| {
                identity::issue(
                    grant,
                    &IssueRequest {
                        name,
                        capabilities,
                        ttl: Duration::from_secs(3600),
                        public_key: None,
                        generate_key: true,
                    },
                )
            })
        };
        let proposer = issue(
            "review_proposer",
            &["ledger/read".into(), "ledger/propose".into()],
        )?;
        let approver = issue(
            "review_approver",
            &[
                "ledger/read".into(),
                "ledger/approve".into(),
                "ledger/admin".into(),
            ],
        )?;
        let owner = Actor::new("fixture-owner".parse()?)
            .with_identity(identity::verify(db.read(), token(&approver)?)?);
        let agent = proposer["agent_uri"]
            .as_str()
            .ok_or("proposer URI")?
            .parse()?;
        let caps = Caps::from_minor_units(Some(100), Some(200))?;
        let limit = SpendLimit::new(agent, "acme".parse()?, Currency::USD, caps);
        Granted::<Admin>::with_db(&db, &owner, |grant| {
            db::audit::set_spend_limit(grant, &limit)
        })?;
        Ok(Self {
            ledger,
            proposer,
            approver,
        })
    }

    fn propose(&self, reference: &str) -> TestResult<String> {
        let output = self
            .ledger
            .command()
            .args([
                "--json",
                "--company",
                "acme",
                "--agent-token",
                token(&self.proposer)?,
                "txn",
                "propose",
                "-d",
                "Supplies <script>alert(1)</script>",
                "--debit",
                "5000:25.00",
                "--credit",
                "1000:25.00",
                "--currency",
                "USD",
                "--date",
                "2026-11-20",
                "--reference",
                reference,
                "--metadata",
                "{\"source\":\"fixture<&>\"}",
                "--tax",
                "5000=office",
            ])
            .output()?;
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let envelope: Value = serde_json::from_slice(&output.stdout)?;
        envelope["data"]["id"]
            .as_str()
            .map(str::to_owned)
            .ok_or_else(|| "proposal ID".into())
    }
}

fn queue(server: &Server) -> TestResult<String> {
    Ok(server
        .client
        .get(format!("{}/review?company=acme", server.base))
        .send()?
        .error_for_status()?
        .text()?)
}

fn csrf(html: &str) -> TestResult<&str> {
    html.split_once("name=\"csrf\" value=\"")
        .and_then(|(_, value)| value.split_once('"').map(|(token, _)| token))
        .ok_or_else(|| "CSRF field".into())
}

fn decision(server: &Server, id: &str, action: &str, csrf: &str) -> RequestBuilder {
    server
        .client
        .post(format!("{}/review/{id}/{action}", server.base))
        .header("Origin", &server.base)
        .form(&[("csrf", csrf), ("company", "acme")])
}

fn transaction_count(db: &Db) -> TestResult<u64> {
    Ok(beankeeper_cli::commands::setup::status(db)?.transactions)
}

#[test]
fn enforced_browser_review_requires_the_private_admin_sidecar() -> TestResult {
    let fixture = ReviewFixture::new()?;
    let id = fixture.propose("no-browser-authority")?;
    let db = fixture.ledger.db()?;
    let owner_path = fixture.ledger.path.with_extension("owner-token");
    std::fs::remove_file(&owner_path)?;
    let registry = identity::list(db.read())?;

    for invalid_sidecar in [None, Some(token(&fixture.ledger.reader)?)] {
        if let Some(token) = invalid_sidecar {
            fixture.ledger.write_owner_token(token)?;
        }
        // Even a valid Admin MCP fallback cannot provide browser authority.
        let mut server =
            Server::start(&fixture.ledger, &["serve"], Some(token(&fixture.approver)?))?;
        server.initialize(None)?;
        server.pair()?;
        let html = queue(&server)?;
        assert!(html.contains("private owner credential with ledger/admin"));
        assert!(html.contains("Intact: hash chain and all bk verify checks passed."));
        assert!(!html.contains(&id));
        assert_eq!(
            decision(&server, &id, "approve", csrf(&html)?)
                .send()?
                .status(),
            403
        );
        assert_eq!(identity::list(db.read())?, registry);
    }

    assert_eq!(
        proposals::get(db.read(), &"acme".parse()?, &id.parse()?)?.state,
        ProposalState::Pending,
    );
    assert_eq!(transaction_count(&db)?, 0);
    Ok(())
}

#[test]
fn cli_proposals_show_full_posting_and_browser_approval_is_atomic_and_human_attributed()
-> TestResult {
    let fixture = ReviewFixture::new()?;
    // --read-only limits agent MCP tools; the paired human can still decide.
    let mut server = Server::start(&fixture.ledger, &["serve", "--read-only"], None)?;
    server.pair()?;
    // Create after startup, proving the queue reads proposals from other clients.
    let id = fixture.propose("browser-approve")?;
    let html = queue(&server)?;
    for expected in [
        &id,
        "Supplies &lt;script&gt;",
        "$25.00",
        "max per posting $1.00",
        "daily cap $2.00",
        fixture.proposer["agent_uri"]
            .as_str()
            .ok_or("proposer URI")?,
    ] {
        assert!(html.contains(expected), "missing {expected}");
    }
    assert!(!html.contains("<script>"));
    assert!(!html.contains(token(&fixture.proposer)?));
    let detail = server
        .client
        .get(format!("{}/review/{id}?company=acme", server.base))
        .send()?
        .error_for_status()?
        .text()?;
    for expected in [
        "Full proposed posting",
        "Approve and post $25.00",
        "class=\"amount\">$25.00</td>",
        "raw minor units",
        "5000",
        "1000",
        "office",
        "2026-11-20",
        "fixture&lt;&amp;&gt;",
    ] {
        assert!(detail.contains(expected), "missing {expected}");
    }
    let db = fixture.ledger.db()?;
    assert_rejected_insert_is_atomic(&fixture.ledger, &db, &server, &id, csrf(&html)?)?;
    let response = decision(&server, &id, "approve", csrf(&html)?).send()?;
    assert_eq!(response.status(), 303);
    assert_eq!(
        response.headers()["location"],
        format!("/review/{id}?company=acme")
    );
    let proposal = proposals::get(db.read(), &"acme".parse()?, &id.parse()?)?;
    let ProposalState::Approved {
        resolution,
        transaction_id,
    } = &proposal.state
    else {
        return Err("expected an approved proposal with a posting".into());
    };
    let ResolvedBy::Agent { uri, jti } = &resolution.by else {
        return Err("expected human approval attribution".into());
    };
    assert_eq!(
        proposal.proposer_uri,
        fixture.proposer["agent_uri"]
            .as_str()
            .ok_or("proposer URI")?,
    );
    assert_ne!(uri, &proposal.proposer_uri);
    assert_human_posting(&db, *transaction_id, uri, jti, &fixture, &id)?;
    let rows = db::audit::list_audit(
        db.read(),
        &db::audit::ListAuditParams {
            company_slug: Some(&"acme".parse()?),
            action: Some(db::audit::AuditAction::ProposalApprove),
            ..common::audit_query()
        },
    )?;
    let row = rows
        .iter()
        .find(|row| row.entity_id == id)
        .ok_or("proposal audit row")?;
    assert_eq!(row.actor, "desktop-local-human");
    assert_eq!(
        decision(&server, &id, "approve", csrf(&html)?)
            .send()?
            .status(),
        409
    );
    assert_eq!(transaction_count(&db)?, 1);
    assert!(!queue(&server)?.contains(&format!("/review/{id}?")));
    verify::execute_json(&db)?;
    Ok(())
}

fn assert_human_posting(
    db: &Db,
    transaction_id: db::ids::TransactionId,
    uri: &str,
    jti: &str,
    fixture: &ReviewFixture,
    id: &str,
) -> TestResult {
    let (transaction, entries) = db::get_transaction(
        &db::companies::Company::bind(db.read(), "acme".parse()?)?,
        transaction_id,
    )?;
    assert_eq!(
        transaction.posted_by.as_deref(),
        Some("desktop-local-human")
    );
    assert_eq!(transaction.agent_uri.as_deref(), Some(uri));
    assert_eq!(transaction.token_jti.as_deref(), Some(jti));
    assert_ne!(
        transaction.token_jti.as_deref(),
        fixture.proposer["token_jti"].as_str()
    );
    assert_eq!(entries.len(), 2);
    assert!(
        entries
            .iter()
            .all(|entry| entry.amount.minor_units() == 2500)
    );
    let metadata: Value = serde_json::from_str(transaction.metadata.as_deref().ok_or("metadata")?)?;
    assert_eq!(metadata["proposal_id"], id);
    assert_eq!(metadata["source"], "fixture<&>");
    Ok(())
}

#[test]
fn human_rejection_records_reason_without_posting_and_cannot_be_approved_later() -> TestResult {
    let fixture = ReviewFixture::new()?;
    let id = fixture.propose("browser-reject")?;
    let mut server = Server::start(&fixture.ledger, &["serve"], None)?;
    server.pair()?;
    let html = queue(&server)?;
    let response = server
        .client
        .post(format!("{}/review/{id}/reject", server.base))
        .header("Origin", &server.base)
        .form(&[
            ("csrf", csrf(&html)?),
            ("company", "acme"),
            ("reason", "Wrong <vendor> & amount"),
        ])
        .send()?;
    assert_eq!(response.status(), 303);
    let db = fixture.ledger.db()?;
    let proposal = proposals::get(db.read(), &"acme".parse()?, &id.parse()?)?;
    let ProposalState::Rejected { resolution, reason } = &proposal.state else {
        return Err("expected a rejected proposal".into());
    };
    assert_eq!(reason.as_deref(), Some("Wrong <vendor> & amount"));
    let ResolvedBy::Agent { uri, .. } = &resolution.by else {
        return Err("expected human rejection attribution".into());
    };
    assert_ne!(uri, &proposal.proposer_uri);
    assert_eq!(transaction_count(&db)?, 0);
    assert_eq!(
        decision(&server, &id, "approve", csrf(&html)?)
            .send()?
            .status(),
        409
    );
    let detail = server
        .client
        .get(format!("{}/review/{id}?company=acme", server.base))
        .send()?
        .error_for_status()?
        .text()?;
    assert!(detail.contains("Wrong &lt;vendor&gt; &amp; amount"));
    assert!(!detail.contains("Approve and post"));
    verify::execute_json(&db)?;
    Ok(())
}

#[test]
fn approval_bearer_tokens_cannot_replace_or_mix_with_browser_authority() -> TestResult {
    let fixture = ReviewFixture::new()?;
    let id = fixture.propose("browser-credentials")?;
    let mut server = Server::start(&fixture.ledger, &["serve"], Some(token(&fixture.approver)?))?;
    let unpaired = Client::builder().no_proxy().build()?;
    for action in ["approve", "reject"] {
        assert_eq!(
            unpaired
                .post(format!("{}/review/{id}/{action}", server.base))
                .header("Origin", &server.base)
                .bearer_auth(token(&fixture.approver)?)
                .form(&[("csrf", "fake"), ("company", "acme")])
                .send()?
                .status(),
            401
        );
    }
    server.pair()?;
    let html = queue(&server)?;
    for action in ["approve", "reject"] {
        assert_eq!(
            decision(&server, &id, action, csrf(&html)?)
                .bearer_auth(token(&fixture.approver)?)
                .send()?
                .status(),
            401
        );
        for (origin, candidate, expected) in [
            (None, csrf(&html)?, 401),
            (Some(server.base.as_str()), "wrong", 401),
            (Some(server.base.as_str()), "", 401),
            (Some("null"), csrf(&html)?, 403),
            (Some("https://evil.example"), csrf(&html)?, 403),
        ] {
            let mut request = server
                .client
                .post(format!("{}/review/{id}/{action}", server.base))
                .form(&[("csrf", candidate), ("company", "acme")]);
            if let Some(origin) = origin {
                request = request.header("Origin", origin);
            }
            assert_eq!(request.send()?.status(), expected);
        }
    }
    let db = fixture.ledger.db()?;
    assert_eq!(
        proposals::get(db.read(), &"acme".parse()?, &id.parse()?)?.state,
        ProposalState::Pending
    );
    assert_eq!(transaction_count(&db)?, 0);
    Ok(())
}

#[test]
fn proposal_resolution_and_deep_links_are_company_scoped() -> TestResult {
    let fixture = ReviewFixture::new()?;
    let id = fixture.propose("browser-company")?;
    let mut server = Server::start(&fixture.ledger, &["--company", "other", "serve"], None)?;
    server.pair()?;
    let html = queue(&server)?;
    for path in [
        format!("/review/{id}?company=other"),
        "/review/invalid?company=acme".into(),
    ] {
        assert_eq!(
            server
                .client
                .get(format!("{}{path}", server.base))
                .send()?
                .status(),
            404
        );
    }
    for action in ["approve", "reject"] {
        assert_eq!(
            server
                .client
                .post(format!("{}/review/{id}/{action}", server.base))
                .header("Origin", &server.base)
                .form(&[("csrf", csrf(&html)?), ("company", "other")])
                .send()?
                .status(),
            404
        );
        assert!(
            !decision(&server, "invalid", action, csrf(&html)?)
                .send()?
                .status()
                .is_success()
        );
    }
    let default = server
        .client
        .get(format!("{}/review", server.base))
        .send()?
        .error_for_status()?
        .text()?;
    assert!(default.contains("value=\"other\" selected"));
    assert!(!default.contains(&id));
    let db = fixture.ledger.db()?;
    assert_eq!(
        proposals::get(db.read(), &"acme".parse()?, &id.parse()?)?.state,
        ProposalState::Pending
    );
    assert_eq!(transaction_count(&db)?, 0);
    Ok(())
}

#[test]
fn verification_caches_its_last_result_and_explicit_recheck_detects_tampering() -> TestResult {
    let fixture = ReviewFixture::new()?;
    let id = fixture.propose("tampered-ledger")?;
    let mut server = Server::start(&fixture.ledger, &["serve"], None)?;
    server.pair()?;
    let intact = queue(&server)?;
    assert!(intact.contains("Intact: hash chain and all bk verify checks passed."));
    assert!(intact.contains("Last checked:"));
    let db = fixture.ledger.db()?;
    // This isolated encrypted temp ledger deliberately simulates an attacker who
    // bypassed the append-only trigger. Never run this against real books.
    let connection = fixture.ledger.fixture_sql()?;
    let rows = db::audit::list_audit(db.read(), &common::audit_query())?;
    let row = rows.last().ok_or("missing audit row to tamper with")?;
    connection.execute_batch("DROP TRIGGER protect_audit_update;")?;
    connection.execute(
        "UPDATE audit_log SET actor = 'tampered' WHERE id = ?1",
        [row.id],
    )?;
    assert!(verify::execute_json(&db).is_err());
    assert_eq!(
        queue(&server)?,
        intact,
        "ordinary reads show the last verified snapshot"
    );
    // Mutation must run a fresh full verification, even while the panel still
    // shows its cached clean result.
    assert_eq!(
        decision(&server, &id, "approve", csrf(&intact)?)
            .send()?
            .status(),
        500
    );
    assert_eq!(
        proposals::get(db.read(), &"acme".parse()?, &id.parse()?)?.state,
        ProposalState::Pending
    );
    assert_eq!(transaction_count(&db)?, 0);
    let failed_approval = queue(&server)?;
    assert!(failed_approval.contains("Verification failed. Ledger integrity is not confirmed."));
    assert!(!failed_approval.contains("Intact: hash chain"));
    assert_eq!(
        server
            .client
            .post(format!("{}/review/verify", server.base))
            .header("Origin", &server.base)
            .form(&[("csrf", "wrong"), ("company", "acme")])
            .send()?
            .status(),
        401
    );
    assert_eq!(
        server
            .client
            .post(format!("{}/review/verify", server.base))
            .header("Origin", &server.base)
            .form(&[("csrf", csrf(&failed_approval)?), ("company", "acme")])
            .send()?
            .status(),
        303
    );
    let failed = queue(&server)?;
    assert!(failed.contains("Verification failed. Ledger integrity is not confirmed."));
    assert!(failed.contains("hash chain broken"));
    assert!(!failed.contains("Intact: hash chain"));
    Ok(())
}

#[test]
fn verification_failure_stays_visible_when_tampered_limits_prevent_queue_reads() -> TestResult {
    let fixture = ReviewFixture::new()?;
    let id = fixture.propose("tampered-limits")?;
    let mut server = Server::start(&fixture.ledger, &["serve"], None)?;
    server.pair()?;
    let intact = queue(&server)?;
    assert!(intact.contains("Intact: hash chain and all bk verify checks passed."));
    let db = fixture.ledger.db()?;
    // Bypass the audited configuration API only in this temporary fixture,
    // leaving a live limit that disagrees with its immutable audit history.
    let affected = fixture.ledger.fixture_sql()?.execute(
        "UPDATE agent_spend_limits SET max_post = 9999
         WHERE agent_uri = ?1 AND company_slug = 'acme' AND currency = 'USD'",
        [fixture.proposer["agent_uri"]
            .as_str()
            .ok_or("proposer URI")?],
    )?;
    assert_eq!(affected, 1);
    assert!(verify::execute_json(&db).is_err());
    assert_eq!(
        server
            .client
            .post(format!("{}/review/verify", server.base))
            .header("Origin", &server.base)
            .form(&[("csrf", csrf(&intact)?), ("company", "acme")])
            .send()?
            .status(),
        303
    );
    // queue() requires HTTP 200, so a generic error page cannot satisfy this.
    let failed = queue(&server)?;
    assert!(failed.contains("Verification failed. Ledger integrity is not confirmed."));
    assert!(failed.contains("spend limits"));
    assert!(failed.contains("Proposal data unavailable"));
    assert!(failed.contains("Re-run verification"));
    assert!(!failed.contains("Intact: hash chain"));
    assert_eq!(
        proposals::get(db.read(), &"acme".parse()?, &id.parse()?)?.state,
        ProposalState::Pending
    );
    assert_eq!(transaction_count(&db)?, 0);
    Ok(())
}

#[test]
fn revoking_the_server_human_credential_prevents_browser_approval() -> TestResult {
    let fixture = ReviewFixture::new()?;
    let id = fixture.propose("revoked-human")?;
    let mut server = Server::start(&fixture.ledger, &["serve"], None)?;
    server.pair()?;
    let html = queue(&server)?;
    let db = fixture.ledger.db()?;
    let rows = db::audit::list_audit(
        db.read(),
        &db::audit::ListAuditParams {
            actor: Some(&"desktop-owner".parse()?),
            entity: Some(db::audit::AuditEntity::Agent),
            action: Some(db::audit::AuditAction::TokenIssue),
            ..common::audit_query()
        },
    )?;
    let human_jti = &rows
        .first()
        .ok_or("missing human token issuance")?
        .entity_id;
    let owner = Actor::new("fixture-owner".parse()?)
        .with_identity(identity::verify(db.read(), token(&fixture.approver)?)?);
    Granted::<Admin>::with_db(&db, &owner, |grant| identity::revoke(grant, human_jti))?;
    assert_eq!(
        decision(&server, &id, "approve", csrf(&html)?)
            .send()?
            .status(),
        403
    );
    assert_eq!(
        proposals::get(db.read(), &"acme".parse()?, &id.parse()?)?.state,
        ProposalState::Pending
    );
    assert_eq!(transaction_count(&db)?, 0);
    verify::execute_json(&db)?;
    Ok(())
}

#[test]
fn generated_proposal_link_opens_the_exact_pending_review() -> TestResult {
    let fixture = ReviewFixture::new()?;
    let id = fixture.propose("deep-linked-proposal")?;
    let reader = token(&fixture.proposer)?;
    let mut server = Server::start(&fixture.ledger, &["serve"], None)?;
    server.initialize(Some(reader))?;
    let response = common::rpc(server.request("tools/call", serde_json::json!({"name":"ui_get_link","arguments":{"view_id":"review_detail","state":{"company":"acme","proposal_id":id}}}), Some(reader))?)?;
    assert_ne!(response["result"]["isError"], true);
    let url = response["result"]["structuredContent"]["url"]
        .as_str()
        .ok_or("URL")?;
    assert_eq!(url, format!("{}/review/{id}?company=acme", server.base));
    server.pair()?;
    let html = server.client.get(url).send()?.error_for_status()?.text()?;
    assert!(html.contains(&id));
    assert!(html.contains("deep-linked-proposal"));
    Ok(())
}

fn assert_rejected_insert_is_atomic(
    ledger: &Ledger,
    db: &Db,
    server: &Server,
    id: &str,
    csrf_token: &str,
) -> TestResult {
    let before = db::audit::count_audit(db.read(), &common::audit_query())?;
    let fixture_db = ledger.fixture_sql()?;
    fixture_db.execute_batch("CREATE TRIGGER fixture_reject_post BEFORE INSERT ON transactions BEGIN SELECT RAISE(ABORT, 'fixture posting failure'); END;")?;
    assert_eq!(
        decision(server, id, "approve", csrf_token).send()?.status(),
        500
    );
    assert_eq!(transaction_count(db)?, 0);
    assert_eq!(
        proposals::get(db.read(), &"acme".parse()?, &id.parse()?)?.state,
        ProposalState::Pending
    );
    let after = db::audit::count_audit(db.read(), &common::audit_query())?;
    assert_eq!(
        before, after,
        "failed posting must not leave a resolution audit"
    );
    fixture_db.execute_batch("DROP TRIGGER fixture_reject_post;")?;
    Ok(())
}
