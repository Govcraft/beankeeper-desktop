pub mod common;

use std::{io::Write, time::Duration};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use beankeeper_cli::{
    capability::Capability,
    db::{self, Actor, Db, audit::setup::Generation},
    identity::{self, IssueRequest},
};
use common::{Ledger, Server, TestResult, clean_command, rpc, token};
use ed25519_dalek::SigningKey;
use secrecy::{ExposeSecret, SecretString};
use serde_json::{Value, json};

const LEDGER_SETUP_STATUS: &str = "ledger_setup_status";
const LEDGER_SETUP_BEGIN: &str = "ledger_setup_begin";
const LEDGER_SETUP_FINISH: &str = "ledger_setup_finish";
const LEDGER_SETUP_REVOKE: &str = "ledger_setup_revoke";

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
    let tools = response["result"]["tools"].as_array().ok_or("tools")?;
    for name in ["setup_status", LEDGER_SETUP_STATUS] {
        assert_eq!(tools.iter().filter(|tool| tool["name"] == name).count(), 1);
    }
    let status = |name| -> TestResult<Value> {
        let response = rpc(server.request(
            "tools/call",
            json!({"name":name,"arguments":{}}),
            Some(reader),
        )?)?;
        assert_ne!(response["result"]["isError"], true, "{response}");
        Ok(response["result"]["structuredContent"].clone())
    };
    let empty = status("setup_status")?;
    assert_eq!(empty["state"], "installed");
    assert_eq!(empty["next_step"], "list_accounts");
    assert_eq!(empty["books"][0]["accounts"], json!([]));
    assert_eq!(
        status(LEDGER_SETUP_STATUS)?,
        json!({"companies":1,"accounts":0,"transactions":0,"owner_key_initialized":true})
    );
    let db = ledger.db()?;
    db::create_account(db.conn(), "acme", "1000", "Checking", "asset", None)?;
    let active = status("setup_status")?;
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
    let historical = status("setup_status")?;
    assert_eq!(historical["next_step"], "review_existing_activity");
    assert_eq!(
        historical["books"][0]["observed_currencies"],
        json!(["JPY"])
    );
    assert_eq!(historical["books"][0]["imported_history"], "source_unknown");
    assert_eq!(
        status(LEDGER_SETUP_STATUS)?,
        json!({"companies":1,"accounts":2,"transactions":1,"owner_key_initialized":true})
    );

    assert_navigation_cannot_read_setup_status(&db, &server)
}

fn assert_navigation_cannot_read_setup_status(db: &Db, server: &Server) -> TestResult {
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
    for name in ["setup_status", LEDGER_SETUP_STATUS] {
        let denied = rpc(server.request(
            "tools/call",
            json!({"name":name,"arguments":{}}),
            Some(token(&other)?),
        )?)?;
        assert_eq!(denied["result"]["isError"], true, "{denied}");
        assert_eq!(denied["result"]["structuredContent"]["code"], "FORBIDDEN");
    }
    Ok(())
}

#[test]
fn desktop_dispatches_owner_setup_lifecycle_across_cli_and_mcp() -> TestResult {
    let ledger = Ledger::new()?;
    let owner = setup_owner(&ledger)?;
    let owner = owner.expose_secret();
    let public_key = setup_public_key();
    let mut server = Server::start(&ledger, &["serve"], None)?;
    server.initialize(Some(owner))?;

    let input = json!({"name":"fixture_setup","public_key":public_key,"ttl":"5m"});
    let reader = token(&ledger.reader)?;
    let denied = setup_call(&server, reader, LEDGER_SETUP_BEGIN, input.clone())?;
    assert_eq!(denied["result"]["structuredContent"]["code"], "FORBIDDEN");
    let started = setup_call(&server, owner, LEDGER_SETUP_BEGIN, input)?;
    let (generation, scoped) = setup_grant(&started["result"]["structuredContent"])?;
    let scoped = scoped.expose_secret();
    let tools = setup_tool_names(&server, scoped)?;
    assert_eq!(tools, [LEDGER_SETUP_STATUS]);
    let status = setup_call(&server, scoped, LEDGER_SETUP_STATUS, json!({}))?;
    assert_eq!(status["result"]["structuredContent"]["companies"], 1);
    let denied = setup_call(
        &server,
        scoped,
        LEDGER_SETUP_FINISH,
        json!({"generation":generation}),
    )?;
    assert_eq!(denied["result"]["structuredContent"]["code"], "FORBIDDEN");
    verify_scoped_stdio(&server, scoped)?;

    let generation_arg = generation.get().to_string();
    let finished = setup_cli(&ledger, owner, &["finish", &generation_arg])?;
    assert_eq!(finished["state"], "finished");
    let retry = setup_call(
        &server,
        owner,
        LEDGER_SETUP_FINISH,
        json!({"generation":generation}),
    )?;
    assert_eq!(retry["result"]["structuredContent"], finished);
    assert_inactive(&server, scoped)?;

    let started = setup_cli(
        &ledger,
        owner,
        &["begin", "fixture_setup", "--public-key", &public_key],
    )?;
    let (next, scoped) = setup_grant(&started)?;
    assert!(next.get() > generation.get());
    let revoked = setup_call(
        &server,
        owner,
        LEDGER_SETUP_REVOKE,
        json!({"generation":next}),
    )?;
    assert_eq!(revoked["result"]["structuredContent"]["state"], "revoked");
    assert_inactive(&server, scoped.expose_secret())?;
    Ok(())
}

#[test]
fn read_only_desktop_exposes_setup_status_and_blocks_owner_lifecycle() -> TestResult {
    let ledger = Ledger::new()?;
    let owner = setup_owner(&ledger)?;
    let owner = owner.expose_secret();
    let mut server = Server::start(&ledger, &["serve", "--read-only"], None)?;
    server.initialize(Some(owner))?;
    let tools = setup_tool_names(&server, owner)?;
    assert!(tools.iter().any(|tool| tool == LEDGER_SETUP_STATUS));
    for tool in [LEDGER_SETUP_BEGIN, LEDGER_SETUP_FINISH, LEDGER_SETUP_REVOKE] {
        assert!(tools.iter().all(|listed| listed != tool));
    }
    let status = setup_call(&server, owner, LEDGER_SETUP_STATUS, json!({}))?;
    assert_eq!(status["result"]["structuredContent"]["companies"], 1);
    let denied = setup_call(
        &server,
        owner,
        LEDGER_SETUP_BEGIN,
        json!({"name":"fixture_setup","public_key":setup_public_key()}),
    )?;
    assert!(denied.get("error").is_some() || denied["result"]["isError"] == true);
    Ok(())
}

fn setup_owner(ledger: &Ledger) -> TestResult<SecretString> {
    let db = ledger.db()?;
    let capabilities =
        [Capability::Read, Capability::Admin].map(|capability| capability.path().to_owned());
    let owner = identity::issue_credential(
        db.conn(),
        &Actor::new("fixture-owner"),
        &IssueRequest {
            name: "setup_owner",
            capabilities: &capabilities,
            ttl: Duration::from_secs(600),
            public_key: None,
            generate_key: true,
        },
    )?;
    Ok(owner.token().clone())
}

fn setup_public_key() -> String {
    STANDARD.encode(SigningKey::from_bytes(&[7; 32]).verifying_key().to_bytes())
}

fn setup_call(server: &Server, token: &str, name: &str, arguments: Value) -> TestResult<Value> {
    rpc(server.request(
        "tools/call",
        json!({"name":name,"arguments":arguments}),
        Some(token),
    )?)
}

fn setup_cli(ledger: &Ledger, owner: &str, args: &[&str]) -> TestResult<Value> {
    let output = ledger
        .command()
        .env("BK_AGENT_TOKEN", owner)
        .args(["--json", "setup"])
        .args(args)
        .output()?;
    assert!(
        output.status.success(),
        "setup exited with {}",
        output.status
    );
    let mut response: Value = serde_json::from_slice(&output.stdout)?;
    Ok(response["data"].take())
}

fn setup_tool_names(server: &Server, token: &str) -> TestResult<Vec<String>> {
    let response = rpc(server.request("tools/list", json!({}), Some(token))?)?;
    response["result"]["tools"]
        .as_array()
        .ok_or("missing tools")?
        .iter()
        .map(|tool| Ok(tool["name"].as_str().ok_or("missing tool name")?.to_owned()))
        .collect()
}

fn setup_grant(payload: &Value) -> TestResult<(Generation, SecretString)> {
    assert_eq!(payload["session"]["state"], "active");
    let credential = &payload["credential"];
    assert_eq!(
        credential["capabilities"],
        json!([Capability::Setup.path()])
    );
    assert!(credential.get("private_key").is_none());
    let generation = serde_json::from_value(payload["session"]["generation"].clone())?;
    Ok((generation, token(credential)?.into()))
}

fn assert_inactive(server: &Server, scoped: &str) -> TestResult {
    let response = server.request(
        "tools/call",
        json!({"name":LEDGER_SETUP_STATUS,"arguments":{}}),
        Some(scoped),
    )?;
    assert_eq!(response.status(), reqwest::StatusCode::UNAUTHORIZED);
    Ok(())
}

fn verify_scoped_stdio(server: &Server, scoped: &str) -> TestResult {
    let mut token_file = tempfile::NamedTempFile::new()?;
    token_file.write_all(scoped.as_bytes())?;
    token_file.flush()?;
    let input = [
        json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"setup-fixture","version":"1.0"}}}),
        json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
        json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":LEDGER_SETUP_STATUS,"arguments":{}}}),
    ];
    let mut stdin = Vec::new();
    for message in input {
        writeln!(stdin, "{message}")?;
    }
    let mut command = clean_command();
    command
        .args([
            "mcp",
            "bridge",
            "--url",
            &format!("{}/mcp", server.base),
            "--token-file",
        ])
        .arg(token_file.path());
    let output = assert_cmd::Command::from_std(command)
        .write_stdin(stdin)
        .timeout(Duration::from_secs(10))
        .output()?;
    assert!(
        output.status.success(),
        "bridge exited with {}",
        output.status
    );
    let stdout = std::str::from_utf8(&output.stdout)?;
    assert!(!stdout.contains(scoped));
    assert!(!String::from_utf8_lossy(&output.stderr).contains(scoped));
    for line in stdout.lines() {
        let response: Value = serde_json::from_str(line)?;
        if response["id"] == 2 {
            assert_eq!(response["result"]["structuredContent"]["companies"], 1);
            return Ok(());
        }
    }
    Err("bridge returned no setup status".into())
}
