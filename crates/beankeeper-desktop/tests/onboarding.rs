pub mod common;

use beankeeper::types::AccountType;
use std::{io::Write, time::Duration};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use beankeeper_cli::{
    capability::Capability,
    commands::setup::ProfileRevision,
    db::{self, Actor, Db, audit::setup::Generation},
    identity::{self, Admin, Granted, IssueRequest},
};
use common::{Ledger, Server, TestResult, clean_command, rpc, token};
use ed25519_dalek::SigningKey;
use secrecy::{ExposeSecret, SecretString};
use serde_json::{Value, json};

const LEDGER_SETUP_STATUS: &str = "ledger_setup_status";
const LEDGER_SETUP_BEGIN: &str = "ledger_setup_begin";
const LEDGER_SETUP_FINISH: &str = "ledger_setup_finish";
const LEDGER_SETUP_REVOKE: &str = "ledger_setup_revoke";
const OPENING_PREVIEW: &str = "ledger_setup_opening_preview";
const OPENING_RESERVE: &str = "ledger_setup_opening_reserve";

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
    assert_eq!(status["data"]["companies"], 0);
    assert_eq!(status["data"]["owner_key_initialized"], false);
    assert_eq!(status["data"]["next_step"], "choose_books");
    Ok(())
}

#[test]
fn setup_status_reports_json_errors_under_its_existing_command_name() -> TestResult {
    let directory = tempfile::tempdir()?;
    let output = clean_command()
        .arg("--db")
        .arg(directory.path())
        .args(["setup", "status", "--json"])
        .output()?;
    assert!(!output.status.success());
    assert_eq!(output.stdout.as_slice(), b"");
    let response: Value = serde_json::from_slice(&output.stderr)?;
    assert_eq!(response["ok"], false);
    assert_eq!(response["meta"]["command"], "setup.status");
    assert!(response["meta"]["timestamp"].is_string());
    assert!(response["error"]["code"].is_string());
    assert!(response.get("data").is_none());
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
    let invalid = setup_call(&server, reader, "setup_status", json!({"unknown": true}))?;
    assert_eq!(invalid["result"]["isError"], true, "{invalid}");
    assert_eq!(invalid["result"]["structuredContent"]["code"], "USAGE");
    let status = |name| -> TestResult<Value> {
        let response = rpc(server.request(
            "tools/call",
            json!({"name":name,"arguments":{}}),
            Some(reader),
        )?)?;
        assert_ne!(response["result"]["isError"], true, "{response}");
        Ok(response["result"]["structuredContent"].clone())
    };
    let check_parity = |next_step, accounts, transactions| -> TestResult {
        let upstream = status(LEDGER_SETUP_STATUS)?;
        assert_eq!(
            upstream,
            json!({
                "companies":1,"accounts":accounts,"transactions":transactions,
                "owner_key_initialized":true,
            })
        );
        let desktop = status("setup_status")?;
        for (field, value) in upstream
            .as_object()
            .ok_or("expected upstream status object")?
        {
            assert_eq!(&desktop[field], value, "{field}");
        }
        assert_eq!(desktop["state"], "installed");
        assert_eq!(desktop["agent_access"]["preset"], "read-only");
        assert_eq!(desktop["agent_access"]["write_mode"], "direct");
        assert_eq!(desktop["next_step"], next_step);
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
        assert_eq!(cli["data"], desktop);
        Ok(())
    };
    check_parity("list_accounts", 0, 0)?;

    let db = ledger.db()?;
    create_fixture_account(&db, ledger.owner(), "1000", "Checking", AccountType::Asset)?;
    check_parity("review_opening_balances", 1, 0)?;

    create_fixture_account(
        &db,
        ledger.owner(),
        "5000",
        "Groceries",
        AccountType::Expense,
    )?;
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
    check_parity("review_existing_activity", 2, 1)?;

    assert_navigation_cannot_read_setup_status(&db, ledger.owner(), &server)
}

fn create_fixture_account(
    db: &Db,
    owner: &Actor,
    code: &str,
    name: &str,
    account_type: AccountType,
) -> TestResult {
    Granted::<Admin>::with_db(db, owner, |grant| {
        db::create_account(
            grant,
            &db::CreateAccountParams {
                company_slug: &"acme".parse()?,
                code,
                name,
                account_type,
                default_tax_category: None,
            },
        )
    })?;
    Ok(())
}

fn assert_navigation_cannot_read_setup_status(
    db: &Db,
    owner: &Actor,
    server: &Server,
) -> TestResult {
    let credentials = [Capability::Navigate.path().to_owned()];
    let other = Granted::<Admin>::with_db(db, owner, |grant| {
        identity::issue(
            grant,
            &IssueRequest {
                name: "onboarding_navigation",
                capabilities: &credentials,
                ttl: Duration::from_secs(3600),
                public_key: None,
                generate_key: true,
            },
        )
    })?;
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
    let mut tools = setup_tool_names(&server, scoped)?;
    tools.sort_unstable();
    assert_eq!(
        tools,
        [
            "ledger_setup_account_add",
            "ledger_setup_book_create",
            "ledger_setup_book_initialize",
            "ledger_setup_book_show",
            "ledger_setup_book_update",
            "ledger_setup_chart_apply",
            OPENING_PREVIEW,
            OPENING_RESERVE,
            "ledger_setup_statement_preview",
            LEDGER_SETUP_STATUS,
        ]
    );
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
    let owner = Granted::<Admin>::with_db(&db, ledger.owner(), |grant| {
        identity::issue_credential(
            grant,
            &IssueRequest {
                name: "setup_owner",
                capabilities: &capabilities,
                ttl: Duration::from_secs(600),
                public_key: None,
                generate_key: true,
            },
        )
    })?;
    Ok(owner.token().clone())
}

fn setup_public_key() -> String {
    STANDARD.encode(SigningKey::from_bytes(&[7; 32]).verifying_key().to_bytes())
}

fn setup_call(server: &Server, token: &str, name: &str, arguments: Value) -> TestResult<Value> {
    let mut params = json!({"name":name});
    params["arguments"] = arguments;
    rpc(server.request("tools/call", params, Some(token))?)
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

#[test]
fn desktop_guides_a_book_through_profile_chart_and_named_account() -> TestResult {
    let ledger = Ledger::new()?;
    let owner = setup_owner(&ledger)?;
    let owner = owner.expose_secret();
    let mut server = Server::start(&ledger, &["serve"], None)?;
    server.initialize(Some(owner))?;
    let started = setup_result(
        &server,
        owner,
        LEDGER_SETUP_BEGIN,
        json!({"name":"profile_fixture","public_key":setup_public_key()}),
    )?;
    let (generation, scoped) = setup_grant(&started)?;

    let apply = |tool, arguments| setup_result(&server, scoped.expose_secret(), tool, arguments);
    let target =
        |reference| json!({"company":"studio","generation":generation,"reference":reference});
    let create = json!({
        "target": target("studio-profile"),
        "name": "Studio",
        "settings": {"purpose":"business","base_currency":"CAD","fiscal_year_start":"04-01"},
    });
    let book = apply("ledger_setup_book_create", create.clone())?;
    assert_eq!(book["profile"]["company"], "studio");
    assert_eq!(book["profile"]["base_currency"], "CAD");
    assert_eq!(book["profile"]["fiscal_year_start"], "04-01");
    assert_eq!(apply("ledger_setup_book_create", create.clone())?, book);

    let mut conflict = create;
    conflict["settings"]["base_currency"] = json!("USD");
    let denied = setup_call(
        &server,
        scoped.expose_secret(),
        "ledger_setup_book_create",
        conflict,
    )?;
    assert_eq!(denied["result"]["structuredContent"]["code"], "VALIDATION");

    let update = json!({
        "target": target("studio-profile-correction"),
        "supersedes": book["profile"]["revision"],
        "settings": {"purpose":"business","base_currency":"CAD","fiscal_year_start":"07-01"},
    });
    let revised = apply("ledger_setup_book_update", update.clone())?;
    assert_eq!(revised["supersedes"], book["profile"]["revision"]);
    assert_eq!(revised["fiscal_year_start"], "07-01");
    assert_eq!(apply("ledger_setup_book_update", update)?, revised);

    let chart = json!({"target":target("studio-chart"),"template":"business-v1"});
    let chart_result = apply("ledger_setup_chart_apply", chart.clone())?;
    assert_eq!(
        chart_result["accounts"]
            .as_array()
            .ok_or("expected chart accounts")?
            .len(),
        6
    );
    assert_eq!(apply("ledger_setup_chart_apply", chart)?, chart_result);

    let account = json!({
        "target":target("studio-bank"),"code":"1015","name":"Studio Checking","account_type":"asset",
    });
    let added = apply("ledger_setup_account_add", account.clone())?;
    assert_eq!(added["name"], "Studio Checking");
    assert_eq!(apply("ledger_setup_account_add", account)?, added);

    let studio = setup_result(
        &server,
        token(&ledger.reader)?,
        "ledger_setup_book_show",
        json!({"company":"studio"}),
    )?;
    assert_eq!(studio["profile"]["purpose"], "business");
    assert_eq!(studio["profile"]["base_currency"], "CAD");
    assert_eq!(studio["profile"]["fiscal_year_start"], "07-01");
    assert_eq!(studio["profile"]["revision"], revised["revision"]);
    let accounts = studio["accounts"].as_array().ok_or("missing accounts")?;
    assert_eq!(
        accounts
            .iter()
            .filter(|account| account["name"] == "Studio Checking")
            .count(),
        1
    );
    let finished = setup_result(
        &server,
        owner,
        LEDGER_SETUP_FINISH,
        json!({"generation":generation}),
    )?;
    assert_eq!(finished["state"], "finished");
    assert_inactive(&server, scoped.expose_secret())?;
    db::audit::verify_hash_chain(ledger.db()?.read())?;
    Ok(())
}

#[test]
fn desktop_initializes_an_existing_book_without_replacing_its_account() -> TestResult {
    let ledger = Ledger::new()?;
    // Earlier CLI versions accepted account codes that the current write API rejects.
    ledger.fixture_sql()?.execute(
        "INSERT INTO accounts (company_slug, code, name, type) VALUES (?1, ?2, ?3, ?4)",
        ["acme", "CASH", "Existing Checking", "asset"],
    )?;
    let owner = setup_owner(&ledger)?;
    let started = setup_cli(
        &ledger,
        owner.expose_secret(),
        &[
            "begin",
            "existing_profile_fixture",
            "--public-key",
            &setup_public_key(),
        ],
    )?;
    let (generation, scoped) = setup_grant(&started)?;
    let mut server = Server::start(&ledger, &["serve"], None)?;
    let reader = token(&ledger.reader)?;
    server.initialize(Some(reader))?;
    let show = || {
        setup_result(
            &server,
            reader,
            "ledger_setup_book_show",
            json!({"company":"acme"}),
        )
    };
    let initial = show()?;
    assert!(initial["profile"].is_null());
    let initialize = json!({
        "target":{"company":"acme","generation":generation,"reference":"existing-book-profile"},
        "settings":{"purpose":"personal","base_currency":"MXN","fiscal_year_start":"01-01"},
    });
    let initialize_book = |arguments| {
        setup_result(
            &server,
            scoped.expose_secret(),
            "ledger_setup_book_initialize",
            arguments,
        )
    };
    let profile = initialize_book(initialize.clone())?;
    assert_eq!(initialize_book(initialize)?, profile);
    let configured = show()?;
    assert_eq!(configured["name"], initial["name"]);
    assert_eq!(configured["accounts"], initial["accounts"]);
    assert_eq!(configured["accounts"][0]["code"], "CASH");
    assert_eq!(configured["accounts"][0]["name"], "Existing Checking");
    assert_eq!(configured["profile"]["base_currency"], "MXN");
    assert_eq!(configured["profile"]["fiscal_year_start"], "01-01");
    db::audit::verify_hash_chain(ledger.db()?.read())?;
    Ok(())
}

struct OpeningFixture {
    ledger: Ledger,
    server: Server,
    generation: Generation,
    scoped: SecretString,
    profile_revision: ProfileRevision,
}

impl OpeningFixture {
    fn new() -> TestResult<Self> {
        let ledger = Ledger::new()?;
        let db = ledger.db()?;
        create_fixture_account(&db, ledger.owner(), "1010", "Checking", AccountType::Asset)?;
        create_fixture_account(
            &db,
            ledger.owner(),
            "3000",
            "Opening Balance Equity",
            AccountType::Equity,
        )?;
        let owner = setup_owner(&ledger)?;
        let owner = owner.expose_secret();
        let mut server = Server::start(&ledger, &["serve"], None)?;
        server.initialize(Some(owner))?;
        let started = setup_result(
            &server,
            owner,
            LEDGER_SETUP_BEGIN,
            json!({"name":"opening_fixture","public_key":setup_public_key()}),
        )?;
        let (generation, scoped) = setup_grant(&started)?;
        let mut profile = setup_result(
            &server,
            scoped.expose_secret(),
            "ledger_setup_book_initialize",
            json!({
                "target":{"company":"acme","generation":generation,"reference":"opening-fixture-profile"},
                "settings":{"purpose":"business","base_currency":"BHD","fiscal_year_start":"01-01"},
            }),
        )?;
        Ok(Self {
            ledger,
            server,
            generation,
            scoped,
            profile_revision: serde_json::from_value(profile["profile"]["revision"].take())?,
        })
    }

    fn input(&self) -> Value {
        json!({
            "company":"acme","generation":self.generation,"date":"2026-01-01","currency":"BHD",
            "debits":[{"account":"1010","amount":"12.345"}],
            "credits":[{"account":"3000","amount":"12.345"}],
        })
    }
}

#[test]
fn desktop_previews_opening_balances_without_reserving_or_posting_them() -> TestResult {
    let fixture = OpeningFixture::new()?;
    let server = &fixture.server;
    let ledger = &fixture.ledger;
    let generation = fixture.generation;
    let scoped = fixture.scoped.expose_secret();
    // Compare the same reader before and after external CLI and MCP calls.
    // Any commit, including to internal setup tables, changes data_version.
    let observer = ledger.fixture_sql()?;
    let ledger_state = || -> rusqlite::Result<i64> {
        observer.pragma_query_value(None, "data_version", |row| row.get(0))
    };
    let before = ledger_state()?;
    let input = fixture.input();
    let preview = setup_result(server, scoped, OPENING_PREVIEW, input.clone())?;
    assert_eq!(preview["profile_revision"], json!(fixture.profile_revision));
    assert_eq!(preview["entries"][0]["account_name"], "Checking");
    assert_eq!(
        preview["entries"][1]["account_name"],
        "Opening Balance Equity"
    );
    assert_eq!(
        preview["totals"]["debits"],
        json!({"amount_minor":"12345","amount_display":"12.345"})
    );
    assert_eq!(preview["totals"]["debits"], preview["totals"]["credits"]);
    assert_eq!(
        setup_result(server, scoped, OPENING_PREVIEW, input.clone())?,
        preview
    );

    let generation_arg = generation.get().to_string();
    let cli = setup_cli(
        ledger,
        scoped,
        &[
            "opening-preview",
            "acme",
            "--generation",
            &generation_arg,
            "--date",
            "2026-01-01",
            "--currency",
            "BHD",
            "--debit",
            "1010:12.345",
            "--credit",
            "3000:12.345",
        ],
    )?;
    assert_eq!(cli, preview);

    let reader = token(&ledger.reader)?;
    let denied = setup_call(server, reader, OPENING_PREVIEW, input)?;
    assert_eq!(denied["result"]["structuredContent"]["code"], "FORBIDDEN");
    assert_eq!(ledger_state()?, before);
    Ok(())
}

#[test]
fn desktop_reserves_replays_approves_and_reverses_opening_balances() -> TestResult {
    let fixture = OpeningFixture::new()?;
    let server = &fixture.server;
    let scoped = fixture.scoped.expose_secret();
    let approver = opening_approver(&fixture.ledger)?;
    let approver = approver.expose_secret();
    let input = fixture.input();
    let preview = setup_result(server, scoped, OPENING_PREVIEW, input.clone())?;
    let fingerprint = preview["fingerprint"]
        .as_str()
        .ok_or("missing fingerprint")?;
    let reference = "opening-acme-2026";
    let request = json!({"opening":input, "fingerprint":fingerprint, "reference":reference});
    let reservation = setup_result(server, scoped, OPENING_RESERVE, request.clone())?;
    assert_eq!(reservation["generation"], json!(fixture.generation));
    assert_eq!(reservation["reference"], reference);
    assert_eq!(reservation["preview"], preview);
    let id = reservation["proposal_id"]
        .as_str()
        .ok_or("missing proposal ID")?;
    let pending = setup_result(server, approver, "txn_proposals", json!({"company":"acme"}))?;
    assert_eq!(pending.as_array().map(Vec::len), Some(1));
    assert_eq!(pending[0]["id"], id);
    assert_eq!(pending[0]["status"], "pending");

    let audit = || setup_result(server, approver, "audit_log", json!({"company":"acme"}));
    let after_reservation = audit()?;
    assert!(
        after_reservation
            .as_array()
            .ok_or("missing audit rows")?
            .iter()
            .any(|row| row["entity_id"] == "setup.request.opening-acme-2026")
    );
    let replay = setup_cli(
        &fixture.ledger,
        scoped,
        &[
            "opening-reserve",
            "acme",
            "--generation",
            &fixture.generation.get().to_string(),
            "--date",
            "2026-01-01",
            "--currency",
            "BHD",
            "--debit",
            "1010:12.345",
            "--credit",
            "3000:12.345",
            "--reference",
            reference,
            "--fingerprint",
            fingerprint,
        ],
    )?;
    assert_eq!(replay, reservation);

    let mut changed = input;
    changed["description"] = json!("A different opening balance");
    let changed_preview = setup_result(server, scoped, OPENING_PREVIEW, changed.clone())?;
    assert_ne!(changed_preview["fingerprint"], preview["fingerprint"]);
    let conflicting_request = json!({
        "opening":changed,"fingerprint":changed_preview["fingerprint"],"reference":reference,
    });
    let conflict = setup_call(server, scoped, OPENING_RESERVE, conflicting_request)?;
    assert_eq!(
        conflict["result"]["structuredContent"]["code"],
        "VALIDATION"
    );

    let approve = json!({"company":"acme","id":id});
    let denied = setup_call(server, scoped, "txn_approve", approve.clone())?;
    assert_eq!(denied["result"]["structuredContent"]["code"], "FORBIDDEN");
    assert_eq!(audit()?, after_reservation);
    let status = || setup_result(server, scoped, LEDGER_SETUP_STATUS, json!({}));
    assert_eq!(status()?["transactions"], 0);

    let posted = setup_result(server, approver, "txn_approve", approve)?;
    assert_eq!(posted["status"], "approved");
    assert!(posted["resolved_by_uri"].is_string());
    assert_ne!(posted["resolved_by_uri"], pending[0]["proposer_uri"]);
    let transaction = posted["transaction_id"]
        .as_i64()
        .ok_or("missing approved transaction ID")?;
    assert_eq!(status()?["transactions"], 1);
    let after_approval = audit()?;
    assert_eq!(
        setup_result(server, scoped, OPENING_RESERVE, request)?,
        reservation
    );
    assert_eq!(audit()?, after_approval);
    assert_eq!(status()?["transactions"], 1);

    assert_opening_reversal(server, approver, transaction, reference)?;
    assert_eq!(status()?["transactions"], 2);
    assert_eq!(
        setup_result(server, approver, "verify", json!({}))?["status"],
        "healthy"
    );
    Ok(())
}

fn opening_approver(ledger: &Ledger) -> TestResult<SecretString> {
    let db = ledger.db()?;
    let capabilities = [Capability::Read, Capability::Approve, Capability::Reverse]
        .map(|capability| capability.path().to_owned());
    let credential = Granted::<Admin>::with_db(&db, ledger.owner(), |grant| {
        identity::issue_credential(
            grant,
            &IssueRequest {
                name: "opening_approver",
                capabilities: &capabilities,
                ttl: Duration::from_secs(600),
                public_key: None,
                generate_key: true,
            },
        )
    })?;
    Ok(credential.token().clone())
}

fn assert_opening_reversal(
    server: &Server,
    approver: &str,
    transaction: i64,
    reference: &str,
) -> TestResult {
    let reversal = setup_result(
        server,
        approver,
        "txn_reverse",
        json!({"company":"acme","id":transaction,"date":"2026-01-02","reference":"reverse-opening-2026"}),
    )?;
    assert_eq!(reversal["created"], true);
    let reversal_id = reversal["id"].as_i64().ok_or("missing reversal ID")?;
    let read = |id| {
        setup_result(
            server,
            approver,
            "txn_show",
            json!({"company":"acme","id":id}),
        )
    };
    let original = read(transaction)?;
    let reversed = read(reversal_id)?;
    assert_eq!(original[0]["reference"], reference);
    assert_eq!(original[0]["currency"], "BHD");
    assert_eq!(reversed[0]["currency"], "BHD");
    let metadata = |row: &Value| -> TestResult<Value> {
        Ok(serde_json::from_str(
            row["metadata"].as_str().ok_or("missing metadata")?,
        )?)
    };
    assert_eq!(metadata(&original[0])?["reversed_by"], reversal_id);
    assert_eq!(metadata(&reversed[0])?["reverses"], transaction);
    let original_entries = original[0]["entries"]
        .as_array()
        .ok_or("missing original entries")?;
    let reversed_entries = reversed[0]["entries"]
        .as_array()
        .ok_or("missing reversed entries")?;
    assert_eq!(original_entries.len(), 2);
    assert_eq!(reversed_entries.len(), 2);
    for (original, reversed) in original_entries.iter().zip(reversed_entries) {
        assert_eq!(reversed["account_code"], original["account_code"]);
        assert_eq!(original["amount"], 12345);
        assert_eq!(reversed["amount"], original["amount"]);
        assert_ne!(reversed["direction"], original["direction"]);
    }
    Ok(())
}

fn setup_result(server: &Server, token: &str, name: &str, arguments: Value) -> TestResult<Value> {
    let mut response = setup_call(server, token, name, arguments)?;
    let result = &mut response["result"];
    assert_eq!(result["isError"], false, "{name} failed: {result}");
    if let Some(content) = result.get_mut("structuredContent") {
        return Ok(content.take());
    }
    let text = result["content"][0]["text"]
        .as_str()
        .ok_or("missing tool data")?;
    Ok(serde_json::from_str(text)?)
}

fn verify_scoped_stdio(server: &Server, scoped: &str) -> TestResult {
    let mut token_file = tempfile::NamedTempFile::new()?;
    beankeeper_desktop::setup::keychain::restrict_file(token_file.path())?;
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
