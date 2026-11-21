//! Browser review exercises the real server against isolated encrypted ledgers.

pub mod common;

use std::time::Duration;

use beankeeper_cli::{
    commands::verify,
    db::{self, Actor, Db, proposals},
    identity::{self, IssueRequest},
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
        let db = ledger.db()?;
        db::create_company(db.conn(), "other", "Other Books", None)?;
        for company in ["acme", "other"] {
            db::create_account(db.conn(), company, "1000", "Cash", "asset", None)?;
            db::create_account(db.conn(), company, "5000", "Supplies", "expense", None)?;
        }
        let issue = |name, capabilities: &[String]| {
            identity::issue(
                db.conn(),
                &Actor::new("fixture-owner"),
                &IssueRequest {
                    name,
                    capabilities,
                    ttl: Duration::from_secs(3600),
                    public_key: None,
                    generate_key: true,
                },
            )
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
        let owner = Actor::new("fixture-owner")
            .with_identity(identity::verify(db.conn(), token(&approver)?)?);
        db::audit::set_spend_limit(
            db.conn(),
            &owner,
            &db::spend_limits::SpendLimit {
                agent_uri: proposer["agent_uri"].as_str().ok_or("proposer URI")?.into(),
                company_slug: "acme".into(),
                currency: "USD".into(),
                max_post: Some(100),
                daily_limit: Some(200),
            },
        )?;
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

fn transaction_count(db: &Db) -> TestResult<i64> {
    Ok(db
        .conn()
        .query_row("SELECT COUNT(*) FROM transactions", [], |row| row.get(0))?)
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
    let before: i64 = db
        .conn()
        .query_row("SELECT COUNT(*) FROM audit_log", [], |row| row.get(0))?;
    db.conn().execute_batch("CREATE TRIGGER fixture_reject_post BEFORE INSERT ON transactions BEGIN SELECT RAISE(ABORT, 'fixture posting failure'); END;")?;
    assert_eq!(
        decision(&server, &id, "approve", csrf(&html)?)
            .send()?
            .status(),
        500
    );
    assert_eq!(transaction_count(&db)?, 0);
    assert_eq!(proposals::get(db.conn(), "acme", &id)?.status, "pending");
    let after: i64 = db
        .conn()
        .query_row("SELECT COUNT(*) FROM audit_log", [], |row| row.get(0))?;
    assert_eq!(
        before, after,
        "failed posting must not leave a resolution audit"
    );
    db.conn()
        .execute_batch("DROP TRIGGER fixture_reject_post;")?;
    let response = decision(&server, &id, "approve", csrf(&html)?).send()?;
    assert_eq!(response.status(), 303);
    assert_eq!(
        response.headers()["location"],
        format!("/review/{id}?company=acme")
    );
    let proposal = proposals::get(db.conn(), "acme", &id)?;
    assert_eq!(proposal.status, "approved");
    assert_eq!(
        proposal.proposer_uri,
        fixture.proposer["agent_uri"]
            .as_str()
            .ok_or("proposer URI")?
    );
    assert_ne!(
        proposal.resolved_by_uri.as_deref(),
        Some(proposal.proposer_uri.as_str())
    );
    let (transaction, entries) = db::get_transaction(
        db.conn(),
        "acme",
        proposal.transaction_id.ok_or("posted ID")?,
    )?;
    assert_eq!(
        transaction.posted_by.as_deref(),
        Some("desktop-local-human")
    );
    assert_eq!(transaction.agent_uri, proposal.resolved_by_uri);
    assert_eq!(transaction.token_jti, proposal.resolved_by_jti);
    assert_ne!(
        transaction.token_jti.as_deref(),
        fixture.proposer["token_jti"].as_str()
    );
    assert_eq!(entries.len(), 2);
    assert!(entries.iter().all(|entry| entry.amount == 2500));
    let metadata: Value = serde_json::from_str(transaction.metadata.as_deref().ok_or("metadata")?)?;
    assert_eq!(metadata["proposal_id"], id);
    assert_eq!(metadata["source"], "fixture<&>");
    let actor: String = db.conn().query_row(
        "SELECT actor FROM audit_log WHERE action = 'proposal_approve' AND entity_id = ?1",
        [&id],
        |row| row.get(0),
    )?;
    assert_eq!(actor, "desktop-local-human");
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
    let proposal = proposals::get(db.conn(), "acme", &id)?;
    assert_eq!(proposal.status, "rejected");
    assert_eq!(proposal.reason.as_deref(), Some("Wrong <vendor> & amount"));
    assert!(proposal.transaction_id.is_none());
    assert_ne!(
        proposal.resolved_by_uri.as_deref(),
        Some(proposal.proposer_uri.as_str())
    );
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
    assert_eq!(proposals::get(db.conn(), "acme", &id)?.status, "pending");
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
    assert_eq!(proposals::get(db.conn(), "acme", &id)?.status, "pending");
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
    db.conn().execute_batch("DROP TRIGGER protect_audit_update; UPDATE audit_log SET actor = 'tampered' WHERE id = (SELECT MIN(id) FROM audit_log);")?;
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
    assert_eq!(proposals::get(db.conn(), "acme", &id)?.status, "pending");
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
    let affected = db.conn().execute(
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
    assert_eq!(proposals::get(db.conn(), "acme", &id)?.status, "pending");
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
    let human_jti: String = db.conn().query_row(
        "SELECT t.jti FROM agent_tokens t JOIN audit_log a ON a.entity_id = t.jti
         WHERE a.action = 'token_issue' AND a.actor = 'desktop-local-human'",
        [],
        |row| row.get(0),
    )?;
    let owner = Actor::new("fixture-owner")
        .with_identity(identity::verify(db.conn(), token(&fixture.approver)?)?);
    identity::revoke(db.conn(), &owner, &human_jti)?;
    assert_eq!(
        decision(&server, &id, "approve", csrf(&html)?)
            .send()?
            .status(),
        403
    );
    assert_eq!(proposals::get(db.conn(), "acme", &id)?.status, "pending");
    assert_eq!(transaction_count(&db)?, 0);
    verify::execute_json(&db)?;
    Ok(())
}
