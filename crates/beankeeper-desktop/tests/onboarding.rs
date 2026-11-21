pub mod common;

use std::time::Duration;

use beankeeper_cli::{
    capability::Capability,
    db::{self, Actor, Db},
    identity::{self, IssueRequest},
};
use common::{Ledger, Server, TestResult, clean_command, rpc, token};
use serde_json::{Value, json};

#[test]
fn setup_status_distinguishes_a_missing_ledger_from_empty_books() -> TestResult {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("books.db");
    let output = clean_command()
        .arg("--db")
        .arg(&path)
        .args(["setup", "status", "--json"])
        .output()?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let status: Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(status["meta"]["command"], "setup.status");
    assert_eq!(status["data"]["state"], "missing_ledger");
    assert_eq!(status["data"]["next_step"], "install_ledger");
    assert!(!path.exists());

    let passphrase = "empty-encrypted-fixture";
    let passphrase_file = directory.path().join("passphrase");
    std::fs::write(&passphrase_file, passphrase)?;
    drop(Db::open(&path, Some(&passphrase.into()))?);
    let output = clean_command()
        .arg("--db")
        .arg(&path)
        .arg("--passphrase-file")
        .arg(&passphrase_file)
        .args(["setup", "status", "--json"])
        .output()?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let status: Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(status["data"]["state"], "installed");
    assert_eq!(status["data"]["books"], json!([]));
    assert_eq!(status["data"]["next_step"], "choose_books");
    Ok(())
}

#[test]
fn read_only_mcp_reports_live_setup_status_like_the_cli() -> TestResult {
    let ledger = Ledger::new()?;
    let reader = token(&ledger.reader)?;
    let mut server = Server::start(&ledger, &["serve", "--read-only"], None)?;
    server.initialize(Some(reader))?;
    let response = rpc(server.request("tools/list", json!({}), Some(reader))?)?;
    assert!(
        response["result"]["tools"]
            .as_array()
            .ok_or("tools")?
            .iter()
            .any(|tool| tool["name"] == "setup_status")
    );
    let status = || -> TestResult<Value> {
        let response = rpc(server.request(
            "tools/call",
            json!({"name":"setup_status","arguments":{}}),
            Some(reader),
        )?)?;
        assert_ne!(response["result"]["isError"], true, "{response}");
        Ok(response["result"]["structuredContent"].clone())
    };
    let empty = status()?;
    assert_eq!(empty["state"], "installed");
    assert_eq!(empty["next_step"], "list_accounts");
    assert_eq!(empty["books"][0]["accounts"], json!([]));
    let db = ledger.db()?;
    db::create_account(db.conn(), "acme", "1000", "Checking", "asset", None)?;
    let active = status()?;
    assert_eq!(active["next_step"], "review_opening_balances");
    assert_eq!(
        active["books"][0]["accounts"],
        json!([{"code":"1000","name":"Checking"}])
    );
    assert_eq!(active["books"][0]["base_currency"], "unavailable");
    assert_eq!(active["books"][0]["fiscal_year_start"], "unavailable");
    assert_eq!(active["books"][0]["imported_history"], "no_transactions");
    let output = ledger
        .command()
        .args(["--agent-token", reader, "setup", "status", "--json"])
        .output()?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let cli: Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(cli["data"], active);

    db::create_account(db.conn(), "acme", "5000", "Groceries", "expense", None)?;
    let posting = ledger
        .command()
        .args([
            "--agent-token",
            token(&ledger.writer)?,
            "--company",
            "acme",
            "txn",
            "post",
            "-d",
            "Groceries",
            "--debit",
            "5000:100",
            "--credit",
            "1000:100",
            "--currency",
            "JPY",
            "--date",
            "2026-01-01",
        ])
        .output()?;
    assert!(
        posting.status.success(),
        "{}",
        String::from_utf8_lossy(&posting.stderr)
    );
    let historical = status()?;
    assert_eq!(historical["next_step"], "review_existing_activity");
    assert_eq!(
        historical["books"][0]["observed_currencies"],
        json!(["JPY"])
    );
    assert_eq!(historical["books"][0]["imported_history"], "source_unknown");

    assert_status_requires_ledger_read(&db, &server)
}

fn assert_status_requires_ledger_read(db: &Db, server: &Server) -> TestResult {
    let credentials = [Capability::Navigate.path().to_owned()];
    let other = identity::issue(
        db.conn(),
        &Actor::new("fixture-owner"),
        &IssueRequest {
            name: "onboarding_navigation",
            capabilities: &credentials,
            ttl: Duration::from_secs(3600),
            public_key: None,
            generate_key: true,
        },
    )?;
    let denied = rpc(server.request(
        "tools/call",
        json!({"name":"setup_status","arguments":{}}),
        Some(token(&other)?),
    )?)?;
    assert_eq!(denied["result"]["isError"], true, "{denied}");
    assert_eq!(denied["result"]["structuredContent"]["code"], "FORBIDDEN");
    Ok(())
}
