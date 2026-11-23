//! Exercise Desktop receipts across CLI, MCP and the paired browser.

pub mod common;

use std::{path::Path, process::Command};

use beankeeper::types::AccountType;
use beankeeper_cli::{
    db::{
        self, audit::receipts::ReceiptPageLimit, companies::CompanySlug,
        verification::VerifiedLedger,
    },
    identity::{Admin, Granted},
};
use common::{Ledger, Server, TestResult, rpc, token};
use serde_json::{Value, json};

fn account(
    ledger: &Ledger,
    db: &db::Db,
    company: &CompanySlug,
    code: &str,
    name: &str,
) -> TestResult {
    Granted::<Admin>::with_db(db, ledger.owner(), |grant| {
        db::create_account(
            grant,
            &db::CreateAccountParams {
                company_slug: company,
                code,
                name,
                account_type: AccountType::Asset,
                default_tax_category: None,
            },
        )
    })?;
    Ok(())
}

fn cli(ledger: &Ledger, state: &Path) -> TestResult<Command> {
    let mut command = ledger.command();
    command
        .env("BEANKEEPER_DESKTOP_STATE_DIR", state)
        .env("BK_AGENT_TOKEN", token(&ledger.reader)?)
        .arg("--json");
    Ok(command)
}

fn cli_data(command: &mut Command) -> TestResult<Value> {
    let output = command.output()?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let envelope: Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(envelope["ok"], true);
    Ok(envelope["data"].clone())
}

fn call(server: &Server, bearer: &str, name: &str, arguments: &Value) -> TestResult<Value> {
    let response = rpc(server.request(
        "tools/call",
        json!({"name":name,"arguments":arguments}),
        Some(bearer),
    )?)?;
    Ok(response["result"]["structuredContent"].clone())
}

#[test]
fn cli_and_mcp_share_a_monotonic_book_marker_and_page_all_receipts() -> TestResult {
    let ledger = Ledger::new()?;
    let db = ledger.db()?;
    let acme = "acme".parse()?;
    let other = "other".parse()?;
    Granted::<Admin>::with_db(&db, ledger.owner(), |grant| {
        db::create_company(grant, &other, &"Other Books".parse()?, None)
    })?;
    let page_limit = u8::from(ReceiptPageLimit::default());
    for code in 0..=page_limit {
        account(&ledger, &db, &acme, &format!("1{code:03}"), "Cash")?;
    }
    let state = tempfile::tempdir()?;
    let mut command = ledger.command();
    command
        .env("BEANKEEPER_DESKTOP_STATE_DIR", state.path())
        .args(["serve", "--bind", "127.0.0.1:0"]);
    let mut server = Server::launch(command)?;
    let reader = token(&ledger.reader)?;
    server.initialize(Some(reader))?;

    let first =
        cli_data(cli(&ledger, state.path())?.args(["--company", "acme", "audit", "since"]))?;
    assert_eq!(first["after"], 0);
    assert_eq!(first["has_more"], true);
    assert_eq!(
        first["receipts"].as_array().ok_or("receipts")?.len(),
        usize::from(page_limit)
    );
    assert_eq!(
        call(&server, reader, "changes_since", &json!({"company":"acme"}))?,
        first
    );
    let invalid = call(
        &server,
        reader,
        "changes_mark_seen",
        &json!({"company":"acme", "through":i64::MAX}),
    )?;
    assert_eq!(invalid["code"], "VALIDATION");
    assert_eq!(
        call(&server, reader, "changes_since", &json!({"company":"acme"}))?,
        first
    );
    let marked = call(
        &server,
        reader,
        "changes_mark_seen",
        &json!({"company":"acme", "through":first["through"]}),
    )?;
    assert_eq!(marked["last_seen"], first["through"]);

    let second =
        cli_data(cli(&ledger, state.path())?.args(["--company", "acme", "audit", "since"]))?;
    assert_eq!(second["after"], first["through"]);
    assert_eq!(second["has_more"], false);
    assert_eq!(second["receipts"].as_array().ok_or("receipts")?.len(), 2);
    let through = second["through"].to_string();
    let marked = cli_data(cli(&ledger, state.path())?.args([
        "--company",
        "acme",
        "audit",
        "mark-seen",
        "--through",
        &through,
    ]))?;
    assert_eq!(marked["last_seen"], second["through"]);
    let retried = call(
        &server,
        reader,
        "changes_mark_seen",
        &json!({"company":"acme", "through":first["through"]}),
    )?;
    assert_eq!(retried, marked);
    let empty = call(&server, reader, "changes_since", &json!({"company":"acme"}))?;
    assert_eq!(empty["receipts"], json!([]));
    let independent = call(
        &server,
        reader,
        "changes_since",
        &json!({"company":"other"}),
    )?;
    assert_eq!(independent["after"], 0);
    assert_eq!(
        independent["receipts"]
            .as_array()
            .ok_or("other receipts")?
            .len(),
        1
    );
    let separate_owner = tempfile::tempdir()?;
    let unacknowledged = cli_data(cli(&ledger, separate_owner.path())?.args([
        "--company",
        "acme",
        "audit",
        "since",
    ]))?;
    assert_eq!(unacknowledged["after"], 0);
    Ok(())
}

fn post_fixture(ledger: &Ledger, writer: &str) -> TestResult {
    let posted = ledger
        .command()
        .args([
            "--agent-token",
            writer,
            "--company",
            "acme",
            "txn",
            "post",
            "-d",
            "Supplies <test>",
            "--debit",
            "2000:3.45",
            "--credit",
            "1000:3.45",
            "--currency",
            "USD",
            "--date",
            "2026-11-22",
            "--reference",
            "statement <line>",
        ])
        .output()?;
    assert!(
        posted.status.success(),
        "{}",
        String::from_utf8_lossy(&posted.stderr)
    );
    Ok(())
}

fn receipt_csrf(server: &Server, url: &str) -> TestResult<String> {
    let html = server.client.get(url).send()?.error_for_status()?.text()?;
    for expected in [
        "Changes since you last looked",
        "Supplies &lt;test&gt;",
        "statement &lt;line&gt;",
        "$3.45",
    ] {
        assert!(html.contains(expected), "missing {expected}");
    }
    let csrf = html
        .split_once("name=\"csrf\" value=\"")
        .and_then(|(_, rest)| rest.split_once('"'))
        .ok_or("CSRF field")?
        .0;
    Ok(csrf.to_owned())
}

#[test]
fn paired_owner_applies_one_audited_undo_per_receipt() -> TestResult {
    let ledger = Ledger::new()?;
    ledger.enable_browser_review()?;
    let db = ledger.db()?;
    let company = "acme".parse()?;
    account(&ledger, &db, &company, "1000", "Cash")?;
    account(&ledger, &db, &company, "2000", "Clearing <test>")?;
    let mut server = Server::start(&ledger, &["serve"], None)?;
    let writer = token(&ledger.writer)?;
    server.initialize(Some(writer))?;
    post_fixture(&ledger, writer)?;
    let page = call(&server, writer, "changes_since", &json!({}))?;
    let receipts = page["receipts"].as_array().ok_or("missing receipts")?;
    let posting = receipts
        .iter()
        .find(|row| row["change"]["summary"]["kind"] == "posting_created")
        .ok_or("post receipt")?;
    assert_eq!(
        posting["change"]["attribution"]["agent_uri"],
        ledger.writer["agent_uri"]
    );
    assert_eq!(posting["source_reference"], "statement <line>");
    let url = posting["undo_url"].as_str().ok_or("undo URL")?;
    assert_eq!(server.client.get(url).send()?.status(), 401);
    let marked = call(
        &server,
        writer,
        "changes_mark_seen",
        &json!({"company":"acme","through": page["through"]}),
    )?;
    assert_eq!(marked["last_seen"], page["through"]);
    server.pair()?;
    let csrf = receipt_csrf(&server, url)?;
    let query = format!(
        "company=acme&after={}&through={}",
        page["after"], page["through"]
    );
    let post_id = posting["id"].as_i64().ok_or("receipt ID")?;
    let undo_url = format!("{}/changes/{post_id}/undo?{query}", server.base);
    let undo = |url: &str, csrf: &str| {
        server
            .client
            .post(url)
            .header("Origin", &server.base)
            .form(&[("csrf", csrf)])
            .send()
    };
    assert_eq!(undo(&undo_url, "incorrect")?.status(), 401);
    assert_eq!(
        beankeeper_cli::commands::setup::status(&db)?.transactions,
        1
    );
    for _ in 0..2 {
        assert!(
            undo(&undo_url, &csrf)?
                .error_for_status()?
                .text()?
                .contains("Recorded reversal transaction")
        );
    }
    assert_eq!(
        beankeeper_cli::commands::setup::status(&db)?.transactions,
        2
    );

    let opened = receipts
        .iter()
        .filter(|row| row["change"]["summary"]["kind"] == "account_created")
        .find(|row| row["change"]["target"]["id"] == "2000")
        .ok_or("account receipt")?;
    let account_id = opened["id"].as_i64().ok_or("account receipt ID")?;
    let archive_url = format!("{}/changes/{account_id}/undo?{query}", server.base);
    assert!(
        undo(&archive_url, &csrf)?
            .error_for_status()?
            .text()?
            .contains("Archived account 2000")
    );
    let changes = call(&server, writer, "changes_since", &json!({}))?;
    let rows = changes["receipts"].as_array().ok_or("undo receipts")?;
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0]["change"]["undo_of"], post_id);
    assert_eq!(rows[0]["posting"]["summary"]["kind"], "posting_created");
    assert_eq!(rows[1]["change"]["undo_of"], account_id);
    assert_eq!(rows[1]["change"]["summary"]["kind"], "account_archived");
    assert!(
        rows.iter()
            .all(|row| row["change"]["actor"] == "desktop-owner" && row["undo_url"].is_null())
    );
    VerifiedLedger::read(&db)?;
    Ok(())
}
