//! Exercise Desktop receipts across CLI, MCP and the paired browser.

pub mod common;

use std::{path::Path, process::Command, time::Duration};

use beankeeper::types::AccountType;
use beankeeper_cli::{
    db::{
        self,
        audit::receipts::ReceiptPageLimit,
        companies::{Company, CompanySlug},
        ids::TransactionId,
        verification::VerifiedLedger,
    },
    identity::{self, Admin, Granted, IssueRequest, Reconcile},
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

fn assert_invalid_cursor_error(ledger: &Ledger, state: &Path, mcp_error: &Value) -> TestResult {
    let output = cli(ledger, state)?
        .args([
            "--company",
            "acme",
            "audit",
            "mark-seen",
            "--through",
            &i64::MAX.to_string(),
        ])
        .output()?;
    assert_eq!(output.status.code(), Some(3));
    assert_eq!(output.stdout.as_slice(), b"");
    let response: Value = serde_json::from_slice(&output.stderr)?;
    assert_eq!(response["ok"], false);
    assert_eq!(response["meta"]["command"], "audit.mark_seen");
    assert_eq!(response["error"], *mcp_error);
    assert!(response.get("data").is_none());
    Ok(())
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
    assert_invalid_cursor_error(&ledger, state.path(), &invalid)?;
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
    Ok(undo_form_token(&html)?.to_owned())
}

fn undo_form_token(html: &str) -> TestResult<&str> {
    html.split_once("name=\"csrf\" value=\"")
        .and_then(|(_, rest)| rest.split_once('"'))
        .map(|(csrf, _)| csrf)
        .ok_or_else(|| "missing Undo form CSRF field".into())
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

fn issue_direct_bookkeeper(ledger: &Ledger, db: &db::Db) -> TestResult<Value> {
    Ok(Granted::<Admin>::with_db(db, ledger.owner(), |grant| {
        identity::issue(
            grant,
            &IssueRequest {
                name: "desktop_bookkeeper",
                capabilities: &[
                    "ledger/read".into(),
                    "ledger/post".into(),
                    "ledger/propose".into(),
                ],
                ttl: Duration::from_secs(3600),
                public_key: None,
                generate_key: true,
            },
        )
    })?)
}

#[test]
fn direct_bookkeeper_recategorizes_and_undoes_in_chat() -> TestResult {
    let ledger = Ledger::new()?;
    let db = ledger.db()?;
    let company = "acme".parse()?;
    account(&ledger, &db, &company, "1000", "Bank")?;
    account(&ledger, &db, &company, "3150", "Statement fallback")?;
    Granted::<Admin>::with_db(&db, ledger.owner(), |grant| {
        db::create_account(
            grant,
            &db::CreateAccountParams {
                company_slug: &company,
                code: "5100",
                name: "Office supplies",
                account_type: AccountType::Expense,
                default_tax_category: None,
            },
        )
    })?;
    let bookkeeper = issue_direct_bookkeeper(&ledger, &db)?;
    let bearer = token(&bookkeeper)?;
    let mut server = Server::start(&ledger, &["serve"], None)?;
    server.initialize(Some(bearer))?;
    let tool = |name: &str, arguments| call(&server, bearer, name, &arguments);
    let original = tool(
        "txn_post",
        json!({
            "company":"acme","reference":"BANK-42","date":"2026-01-02",
            "description":"Statement purchase","currency":"USD",
            "debits":[{"account":"3150","amount":"3.45"}],
            "credits":[{"account":"1000","amount":"3.45"}]
        }),
    )?;
    let read_original = || tool("txn_show", json!({"company":"acme","id":original["id"]}));
    let before = read_original()?;
    let mut request = json!({
        "company":"acme","reference":"BANK-42","from_account":"3150","to_account":"5100",
        "posting_reference":"category-fix"
    });
    let moved = tool("txn_recategorize", request.clone())?;
    assert_eq!(moved["original_id"], original["id"]);
    assert_eq!(moved["from_account"], "3150");
    assert_eq!(moved["to_account"], "5100");
    assert_eq!(moved["amount"], 345);
    assert_eq!(moved["currency"], "USD");
    assert!(moved["desktop_path"].is_string());
    assert_eq!(read_original()?, before);

    let retry = cli_data(ledger.command().env("BK_AGENT_TOKEN", bearer).args([
        "--company",
        "acme",
        "--json",
        "txn",
        "recategorize",
        "--reference",
        "BANK-42",
        "--from-account",
        "3150",
        "--to-account",
        "5100",
        "--posting-reference",
        "category-fix",
        "--on-conflict",
        "skip",
    ]))?;
    assert_eq!(retry["id"], moved["id"]);
    assert!(moved["receipt_id"].is_i64());
    assert_eq!(retry["receipt_id"], moved["receipt_id"]);
    assert_eq!(retry["skipped"], true);
    let undo = tool(
        "audit_undo",
        json!({"company":"acme","receipt":moved["receipt_id"]}),
    )?;
    assert_eq!(undo["undo_of"], moved["receipt_id"]);
    assert_eq!(undo["transaction"], moved["id"]);
    assert_eq!(undo["kind"], "reversed");
    assert_eq!(read_original()?, before);

    reconcile_bank_entry(
        &ledger,
        &db,
        &company,
        serde_json::from_value(original["id"].clone())?,
    )?;
    request["posting_reference"] = json!("blocked-category-fix");
    let blocked = tool("txn_recategorize", request)?;
    assert_eq!(blocked["code"], "RECATEGORIZE_BLOCKED");
    assert!(
        blocked["message"]
            .as_str()
            .ok_or("missing error message")?
            .contains("reconciled")
    );
    assert_eq!(
        beankeeper_cli::commands::setup::status(&db)?.transactions,
        3
    );
    VerifiedLedger::read(&db)?;
    Ok(())
}

fn reconcile_bank_entry(
    ledger: &Ledger,
    db: &db::Db,
    company: &CompanySlug,
    transaction_id: TransactionId,
) -> TestResult {
    let book = Company::bind(db.read(), company.clone())?;
    let entry = db::get_entries_for_transaction(&book, transaction_id)?
        .into_iter()
        .find(|entry| entry.account_code == "1000")
        .ok_or("missing bank entry")?;
    let date = "2026-01-03".parse()?;
    Granted::<Reconcile>::with_db(db, ledger.owner(), |grant| {
        db::clear_entry(
            grant,
            &db::ClearRequest {
                company_slug: company,
                transaction_id,
                entry_id: entry.id,
                amount: None,
                date,
                statement: None,
                status: db::ClearingStatus::Reconciled,
            },
        )
    })?;
    Ok(())
}

fn set_up_books(receipt: impl Fn(&str, Value) -> TestResult<Value>) -> TestResult<[Value; 5]> {
    let settings =
        json!({"purpose":"personal", "base_currency":"USD", "fiscal_year_start":"01-01"});
    let profile = receipt(
        "ledger_setup_book_initialize",
        json!({
            "target":{"company":"acme", "reference":"initialize"}, "settings":settings
        }),
    )?;
    let book = receipt(
        "ledger_setup_book_create",
        json!({
            "target":{"company":"home", "reference":"create"}, "name":"Home", "settings":settings
        }),
    )?;
    let chart = receipt(
        "ledger_setup_chart_apply",
        json!({
            "target":{"company":"home", "reference":"chart"}, "template":"personal-v1"
        }),
    )?;
    let account = receipt(
        "ledger_setup_account_add",
        json!({
            "target":{"company":"home", "reference":"checking"}, "code":"1234",
            "name":"Checking", "account_type":"asset"
        }),
    )?;
    let opening = receipt(
        "ledger_setup_opening_post",
        json!({
            "company":"home", "reference":"opening", "date":"2026-01-01", "currency":"USD",
            "debits":[{"account":"1234", "amount":"100.00"}],
            "credits":[{"account":"3000", "amount":"100.00"}]
        }),
    )?;
    Ok([profile, book, chart, account, opening])
}

fn read_setup_receipt(server: &Server, write: &Value) -> TestResult<String> {
    let path = write["desktop_path"]
        .as_str()
        .ok_or_else(|| format!("missing receipt path: {write}"))?;
    let id = write["receipt_id"].as_i64().ok_or("receipt ID")?;
    let html = server
        .client
        .get(format!("{}{path}", server.base))
        .send()?
        .error_for_status()?
        .text()?;
    assert!(
        html.contains(&format!("aria-label=\"Undo receipt {id}\"")),
        "{path}"
    );
    Ok(html)
}

#[test]
fn direct_bookkeeper_sets_up_books_and_owner_undoes_an_import_in_one_step() -> TestResult {
    let ledger = Ledger::new()?;
    ledger.enable_browser_review()?;
    let db = ledger.db()?;
    let bookkeeper = issue_direct_bookkeeper(&ledger, &db)?;
    let bearer = token(&bookkeeper)?;
    let mut server = Server::start(&ledger, &["serve"], None)?;
    server.initialize(Some(bearer))?;
    let receipt = |name: &str, arguments| call(&server, bearer, name, &arguments);
    let status = receipt("setup_status", json!({}))?;
    assert_eq!(status["agent_access"]["preset"], "bookkeeper");
    assert_eq!(status["agent_access"]["write_mode"], "direct");
    let setup = set_up_books(receipt)?;
    let import = |reference, rows: &str| {
        json!({
            "company":"home", "reference":reference, "bank":"1234", "suspense":"5000",
            "currency":"USD", "format":"csv", "csv":{"reference_col":"reference"},
            "content":format!("date,description,amount,reference\n{rows}")
        })
    };
    let prior_row = "2026-01-02,First deposit,12,bank-first\n";
    let prior = receipt(
        "ledger_setup_statement_import",
        import("first-import", prior_row),
    )?;
    let batch = import(
        "next-import",
        &format!(
            "{prior_row}2026-01-03,Second deposit,3,bank-second\n2026-01-04,Third deposit,7,bank-third\n"
        ),
    );
    let imported = receipt("ledger_setup_statement_import", batch.clone())?;
    assert_eq!(receipt("ledger_setup_statement_import", batch)?, imported);
    assert_eq!(imported["created"].as_array().map(Vec::len), Some(2));
    assert_eq!(imported["preexisting"], prior["created"]);

    let page = receipt("changes_since", json!({"company":"home"}))?;
    let rows = page["receipts"].as_array().ok_or("missing receipts")?;
    let import_path = imported["desktop_path"]
        .as_str()
        .ok_or("import receipt path")?;
    let import_id = imported["receipt_id"].as_i64().ok_or("import receipt ID")?;
    let import_url = format!("{}{import_path}", server.base);
    assert!(
        rows.iter()
            .any(|row| { row["id"] == import_id && row["undo_url"].as_str().is_some() })
    );
    assert_eq!(server.client.get(&import_url).send()?.status(), 401);
    server.pair()?;
    for write in setup.iter().chain([&prior]) {
        read_setup_receipt(&server, write)?;
    }

    let (_, query) = import_path.split_once('?').ok_or("import receipt query")?;
    let (query, _) = query.split_once('#').ok_or("import receipt anchor")?;
    let undo_path = format!("/changes/{import_id}/undo?{query}");
    let html = read_setup_receipt(&server, &imported)?;
    assert!(html.contains(&format!("action=\"{}\"", undo_path.replace('&', "&amp;"))));
    let csrf = undo_form_token(&html)?;
    let undo_url = format!("{}{undo_path}", server.base);
    for _ in 0..2 {
        let feedback = server
            .client
            .post(&undo_url)
            .header("Origin", &server.base)
            .form(&[("csrf", csrf)])
            .send()?
            .error_for_status()?
            .text()?;
        assert!(feedback.contains("Undid that change"));
    }
    let reversals = db::audit::list_audit(
        db.read(),
        &db::audit::ListAuditParams {
            company_slug: Some(&"home".parse()?),
            action: Some(db::audit::AuditAction::Reverse),
            ..common::audit_query()
        },
    )?;
    assert_eq!(reversals.len(), 2);
    let created = imported["created"].as_array().ok_or("imported postings")?;
    for posting in created {
        let id = posting["transaction_id"].to_string();
        assert!(reversals.iter().any(|event| event.entity_id == id));
    }
    VerifiedLedger::read(&db)?;
    Ok(())
}
