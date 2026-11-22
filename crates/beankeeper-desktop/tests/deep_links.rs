pub mod common;

use std::path::Path;

use beankeeper::types::AccountType;
use beankeeper::types::Currency;
use beankeeper::types::DebitOrCredit;
use beankeeper_cli::identity::{Admin, Granted, Post};
use common::{Ledger, Server, TestResult, clean_command, rpc, token};
use serde_json::{Value, json};

fn link(server: &Server, bearer: &str, view: &str, state: Value) -> TestResult<Value> {
    let mut arguments = json!({"view_id":view});
    arguments["state"] = state;
    let response = rpc(server.request(
        "tools/call",
        json!({"name":"ui_get_link","arguments":arguments}),
        Some(bearer),
    )?)?;
    assert_ne!(response["result"]["isError"], true, "{response}");
    Ok(response["result"]["structuredContent"].clone())
}

#[test]
fn upstream_view_targets_restore_summary_dates_and_multiple_cash_accounts() -> TestResult {
    let ledger = Ledger::new()?;
    let db = ledger.db()?;
    Granted::<Admin>::with_db(&db, ledger.owner(), |grant| {
        beankeeper_cli::db::create_account(
            grant,
            &beankeeper_cli::db::CreateAccountParams {
                company_slug: &"acme".parse()?,
                code: "1000",
                name: "Cash",
                account_type: AccountType::Asset,
                default_tax_category: None,
            },
        )
    })?;
    Granted::<Admin>::with_db(&db, ledger.owner(), |grant| {
        beankeeper_cli::db::create_account(
            grant,
            &beankeeper_cli::db::CreateAccountParams {
                company_slug: &"acme".parse()?,
                code: "1100",
                name: "Clearing reserve",
                account_type: AccountType::Asset,
                default_tax_category: None,
            },
        )
    })?;
    drop(db);
    let reader = token(&ledger.reader)?;
    let mut server = Server::start(&ledger, &["serve", "--read-only"], None)?;
    server.initialize(Some(reader))?;
    server.pair()?;
    let answer = rpc(server.request(
        "tools/call",
        json!({"name":"financial_summary","arguments":{
            "company":"acme","currency":"USD",
            "from":"2026-10-01","to":"2026-10-31",
            "compare_from":"2026-09-01","compare_to":"2026-09-30",
            "cash_account":["1000","1100"]
        }}),
        Some(reader),
    )?)?;
    assert_ne!(answer["result"]["isError"], true, "{answer}");
    let target = &answer["result"]["structuredContent"]["data"]["desktop_view"]["link"];
    let summary = link(
        &server,
        reader,
        target["view_id"]
            .as_str()
            .ok_or("upstream summary view ID")?,
        target["state"].clone(),
    )?;
    let summary_page = server
        .client
        .get(summary["url"].as_str().ok_or("summary URL")?)
        .send()?
        .error_for_status()?
        .text()?;
    assert!(
        summary_page
            .contains("2026-10-01 through 2026-10-31, compared with 2026-09-01 through 2026-09-30")
    );
    assert!(summary_page.contains("Cash on hand in the selected accounts is USD 0.00"));

    let cash_flow = link(
        &server,
        reader,
        "cash-flow",
        json!({
            "company":"acme","currency":"USD","from":"2026-10-01","to":"2026-10-31",
            "cash_account":["1000","1100"]
        }),
    )?;
    let cash_url = cash_flow["url"].as_str().ok_or("cash flow URL")?;
    let cash_json: Value = server
        .client
        .get(format!("{cash_url}&format=json"))
        .send()?
        .error_for_status()?
        .json()?;
    let accounts = cash_json["cash_accounts"]
        .as_array()
        .ok_or("selected cash accounts")?;
    assert!(accounts.iter().any(|account| account["code"] == "1000"));
    assert!(accounts.iter().any(|account| account["code"] == "1100"));
    Ok(())
}

#[test]
fn links_work_without_tabs_in_read_only_mode_and_match_the_cli() -> TestResult {
    let ledger = Ledger::new()?;
    ledger.enable_browser_review()?;
    let db = ledger.db()?;
    Granted::<Admin>::with_db(&db, ledger.owner(), |grant| {
        beankeeper_cli::db::create_account(
            grant,
            &beankeeper_cli::db::CreateAccountParams {
                company_slug: &"acme".parse()?,
                code: "1000",
                name: "Cash",
                account_type: AccountType::Asset,
                default_tax_category: None,
            },
        )
    })?;
    Granted::<Admin>::with_db(&db, ledger.owner(), |grant| {
        beankeeper_cli::db::create_account(
            grant,
            &beankeeper_cli::db::CreateAccountParams {
                company_slug: &"acme".parse()?,
                code: "5000",
                name: "Dining",
                account_type: AccountType::Expense,
                default_tax_category: None,
            },
        )
    })?;
    let actor = beankeeper_cli::db::Actor::new("owner".parse()?).with_identity(
        beankeeper_cli::identity::verify(db.read(), token(&ledger.writer)?)?,
    );
    let entries = [
        beankeeper_cli::db::PostEntryParams {
            account_code: "5000".into(),
            direction: DebitOrCredit::Debit,
            amount: 2500.try_into()?,
            memo: None,
            tax_category: Some("sched-c:24b".into()),
        },
        beankeeper_cli::db::PostEntryParams {
            account_code: "1000".into(),
            direction: DebitOrCredit::Credit,
            amount: 2500.try_into()?,
            memo: None,
            tax_category: None,
        },
    ];
    let beankeeper_cli::db::PostResult::Created(transaction_id) =
        Granted::<Post>::with_db(&db, &actor, |grant| {
            beankeeper_cli::db::post_transaction(
                grant,
                &beankeeper_cli::db::PostTransactionParams {
                    company_slug: &"acme".parse()?,
                    description: "Dining",
                    metadata: Some("{\"payee\":\"Café & Tea / 100%\"}"),
                    currency: Currency::USD,
                    date: "2026-10-20".parse().map_err(|error: chrono::ParseError| {
                        beankeeper_cli::error::CliError::Validation(error.to_string())
                    })?,
                    entries: &entries,
                    correlate: None,
                    reference: None,
                    on_conflict: beankeeper_cli::db::ConflictStrategy::Error,
                    actor: &actor,
                },
            )
        })?
    else {
        return Err("transaction was not created".into());
    };
    drop(db);
    let reader = token(&ledger.reader)?;
    let state = tempfile::tempdir()?;
    let mut server = serve_for_links(&ledger, state.path(), "127.0.0.1:0")?;
    server.initialize(Some(reader))?;
    let tools = rpc(server.request("tools/list", json!({}), Some(reader))?)?;
    assert!(
        tools["result"]["tools"]
            .as_array()
            .ok_or("tools")?
            .iter()
            .any(|tool| tool["name"] == "ui_get_link")
    );
    let unpaired_link = link(&server, reader, "transactions", json!({"company":"acme"}))?;
    assert_eq!(
        server
            .client
            .get(unpaired_link["url"].as_str().ok_or("URL")?)
            .send()?
            .status(),
        401
    );
    server.pair()?;
    assert_view_links_resolve(&server, reader, transaction_id)?;
    assert_link_cli_parity(&server, reader, &ledger.path, state.path())?;
    Ok(())
}

#[test]
fn pairing_restores_the_exact_view_without_granting_access_on_get() -> TestResult {
    let ledger = Ledger::new()?;
    let server = Server::start(&ledger, &["serve"], None)?;
    let destination =
        "/transactions?company=acme&payee=Caf%C3%A9%20%26%20Tea&from=2026-10-01&to=2026-10-31";
    let response = server
        .client
        .get(format!("{}{destination}", server.base))
        .send()?;
    assert_eq!(response.status(), 401);
    let pending = response.headers()["set-cookie"]
        .to_str()?
        .split(';')
        .next()
        .ok_or("pending cookie")?
        .to_owned();
    assert!(
        pending
            .split('=')
            .next()
            .is_some_and(|name| name.ends_with("_return"))
    );
    assert!(!pending.contains("acme"));
    let client = reqwest::blocking::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .default_headers({
            let mut headers = reqwest::header::HeaderMap::new();
            headers.insert(reqwest::header::COOKIE, pending.parse()?);
            headers
        })
        .build()?;
    assert_eq!(
        client
            .get(format!("{}{destination}", server.base))
            .send()?
            .status(),
        401
    );
    let pairing_url = server.pairing_url.as_deref().ok_or("pairing link")?;
    let pairing_token = pairing_url.split_once('=').ok_or("token")?.1;
    assert_eq!(client.get(pairing_url).send()?.status(), 200);
    assert_eq!(client.head(pairing_url).send()?.status(), 200);
    // A rejected POST must not consume either the pairing link or the destination.
    assert_eq!(
        client
            .post(format!("{}/pair", server.base))
            .form(&[("token", pairing_token)])
            .send()?
            .status(),
        401
    );
    let response = client
        .post(format!("{}/pair", server.base))
        .header("Origin", &server.base)
        .form(&[("token", pairing_token)])
        .send()?;
    assert_eq!(response.status(), 303);
    assert_eq!(response.headers()["location"], destination);
    let cookies: Vec<_> = response
        .headers()
        .get_all("set-cookie")
        .iter()
        .map(|value| value.to_str())
        .collect::<Result<_, _>>()?;
    assert!(
        cookies
            .iter()
            .any(|cookie| cookie.contains("_return=;") && cookie.contains("Max-Age=0"))
    );
    let session = cookies
        .iter()
        .find(|cookie| !cookie.contains("_return="))
        .ok_or("session cookie")?
        .split(';')
        .next()
        .ok_or("cookie")?;
    let response = server
        .client
        .get(format!("{}{destination}", server.base))
        .header("Cookie", session)
        .send()?;
    assert_eq!(response.status(), 200);
    let html = response.text()?;
    assert!(html.contains("value=\"Café &amp; Tea\""));
    assert!(html.contains("value=\"2026-10-01\""));
    assert!(html.contains("value=\"2026-10-31\""));
    assert_eq!(
        client
            .post(format!("{}/pair", server.base))
            .header("Origin", &server.base)
            .form(&[("token", pairing_token)])
            .send()?
            .status(),
        410
    );
    assert_redirect_parameter_is_rejected(&server)?;
    Ok(())
}

#[test]
fn cli_links_require_a_running_instance_and_reject_invalid_filters_first() -> TestResult {
    let directory = tempfile::tempdir()?;
    let books = directory.path().join("books.db");
    std::fs::write(&books, b"Link discovery does not open the ledger")?;
    let output = clean_command()
        .current_dir(directory.path())
        .env(
            "BEANKEEPER_DESKTOP_STATE_DIR",
            directory.path().join("state"),
        )
        .arg("--db")
        .arg("books.db")
        .args(["--json", "ui", "link", "--view-id", "settings"])
        .output()?;
    assert!(!output.status.success());
    let result: Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(result["code"], "DESKTOP_BOOKS_NOT_SERVED");
    assert!(result["url"].is_null());
    assert_eq!(std::fs::read_dir(directory.path())?.count(), 1);
    let output = clean_command()
        .args([
            "--format",
            "json",
            "ui",
            "link",
            "--view-id",
            "accounts",
            "--payee",
            "ignored",
        ])
        .output()?;
    assert!(!output.status.success());
    assert_eq!(
        serde_json::from_slice::<Value>(&output.stdout)?["code"],
        "VALIDATION"
    );
    for args in [
        vec!["ui", "link", "--view-id", "accounts", "--payee", "ignored"],
        vec![
            "ui",
            "link",
            "--view-id",
            "settings",
            "--bind",
            "192.0.2.1:8750",
        ],
        vec![
            "ui",
            "link",
            "--view-id",
            "transactions",
            "--from",
            "2026-02-30",
        ],
    ] {
        assert!(!clean_command().args(args).output()?.status.success());
    }
    Ok(())
}

fn serve_for_links(ledger: &Ledger, state: &Path, bind: &str) -> TestResult<Server> {
    let mut command = ledger.command();
    command.env("BEANKEEPER_DESKTOP_STATE_DIR", state);
    command.args(["serve", "--read-only", "--bind", bind]);
    Server::launch(command)
}

fn cli_accounts(books: &Path, state: &Path, bind: Option<&str>) -> TestResult<Value> {
    let mut command = clean_command();
    command.env("BEANKEEPER_DESKTOP_STATE_DIR", state);
    command.arg("--db").arg(books);
    command.args(["--json", "ui", "link", "--view-id", "accounts"]);
    if let Some(bind) = bind {
        command.args(["--bind", bind]);
    }
    let output = command.output()?;
    let result: Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(
        output.status.success(),
        result["url"].is_string(),
        "{result}"
    );
    Ok(result)
}

#[test]
fn cli_and_mcp_choose_the_books_listener_and_reject_a_reused_port() -> TestResult {
    let state = tempfile::tempdir()?;
    let first = Ledger::new()?;
    let second = Ledger::new()?;
    let mut first_server = serve_for_links(&first, state.path(), "127.0.0.1:0")?;
    let mut second_server = serve_for_links(&second, state.path(), "127.0.0.1:0")?;
    let first_bind = first_server
        .base
        .strip_prefix("http://")
        .ok_or("first bind")?
        .to_owned();
    let first_link = cli_accounts(&first.path, state.path(), None)?;
    let second_alias = second
        .path
        .parent()
        .ok_or("ledger directory")?
        .join("./books.db");
    let second_link = cli_accounts(&second_alias, state.path(), None)?;
    assert_eq!(first_link["url"], format!("{}/accounts", first_server.base));
    assert_eq!(
        second_link["url"],
        format!("{}/accounts", second_server.base)
    );
    let second_reader = token(&second.reader)?;
    second_server.initialize(Some(second_reader))?;
    assert_eq!(
        second_link,
        link(&second_server, second_reader, "accounts", json!({}))?
    );
    assert_eq!(
        cli_accounts(&second.path, state.path(), Some(&first_bind))?["code"],
        "DESKTOP_BOOKS_NOT_SERVED"
    );
    let first_cookie = first_server.pair()?;
    let second_cookie = second_server.pair()?;
    let cookies = format!("{first_cookie}; {second_cookie}");
    let browser = reqwest::blocking::Client::builder().no_proxy().build()?;
    for result in [&first_link, &second_link, &first_link] {
        assert_eq!(
            browser
                .get(result["url"].as_str().ok_or("URL")?)
                .header("Cookie", &cookies)
                .send()?
                .status(),
            200
        );
    }
    drop(first_server);
    let replacement = serve_for_links(&second, state.path(), &first_bind)?;
    assert_eq!(
        cli_accounts(&first.path, state.path(), None)?["code"],
        "DESKTOP_BOOKS_NOT_SERVED"
    );
    assert_eq!(
        cli_accounts(&second.path, state.path(), None)?["code"],
        "DESKTOP_MULTIPLE_INSTANCES"
    );
    assert_eq!(
        cli_accounts(&second.path, state.path(), Some(&first_bind))?["url"],
        format!("{}/accounts", replacement.base)
    );
    Ok(())
}

fn assert_link_cli_parity(server: &Server, reader: &str, books: &Path, state: &Path) -> TestResult {
    let bind = server.base.strip_prefix("http://").ok_or("bind")?;
    let output = clean_command()
        .env("BEANKEEPER_DESKTOP_STATE_DIR", state)
        .arg("--db")
        .arg(books)
        .args([
            "--json",
            "ui",
            "link",
            "--view-id",
            "transactions",
            "--bind",
            bind,
            "--company",
            "acme",
            "--account",
            "5000",
            "--category",
            "sched-c:24b",
            "--payee",
            "Café & Tea / 100%",
            "--from",
            "2026-10-01",
            "--to",
            "2026-10-31",
        ])
        .output()?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let cli: Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(
        cli,
        link(
            server,
            reader,
            "transactions",
            json!({"company":"acme","account":"5000","category":"sched-c:24b","payee":"Café & Tea / 100%","from":"2026-10-01","to":"2026-10-31"})
        )?
    );
    for arguments in [
        json!({"view_id":"settings","state":{"company":"ignored"}}),
        json!({"view_id":"transactions","state":{"compare_from":"2026-10-01"}}),
        json!({"view_id":"transactions","state":{"bad_filter":true}}),
    ] {
        let result = rpc(server.request(
            "tools/call",
            json!({"name":"ui_get_link","arguments":arguments}),
            Some(reader),
        )?)?;
        assert_eq!(result["result"]["isError"], true);
        assert_eq!(result["result"]["structuredContent"]["code"], "VALIDATION");
    }
    for (view, state, flags) in [
        (
            "transactions",
            json!({"from":"2026-02-30"}),
            vec!["--from", "2026-02-30"],
        ),
        (
            "transactions",
            json!({"min_amount":"2","max_amount":"1"}),
            vec!["--min-amount", "2", "--max-amount", "1"],
        ),
        (
            "balance-sheet",
            json!({"from":"2026-01-01"}),
            vec!["--from", "2026-01-01"],
        ),
        (
            "reports",
            json!({"report_kind":"trend","threshold":"600"}),
            vec!["--report-kind", "trend", "--threshold", "600"],
        ),
        (
            "review_detail",
            json!({"proposal_id":"not-a-proposal"}),
            vec!["--proposal-id", "not-a-proposal"],
        ),
    ] {
        let mcp = rpc(server.request(
            "tools/call",
            json!({"name":"ui_get_link","arguments":{"view_id":view,"state":state}}),
            Some(reader),
        )?)?;
        let cli = clean_command()
            .args(["--json", "ui", "link", "--view-id", view])
            .args(flags)
            .output()?;
        assert!(!cli.status.success());
        let cli: Value = serde_json::from_slice(&cli.stdout)?;
        assert_eq!(cli["code"], "VALIDATION");
        assert_eq!(cli, mcp["result"]["structuredContent"]);
    }
    Ok(())
}

fn assert_view_links_resolve(
    server: &Server,
    reader: &str,
    transaction_id: beankeeper_cli::db::ids::TransactionId,
) -> TestResult {
    for (view, state, expected_status) in [
        ("dashboard", json!({"company":"acme","currency":"USD"}), 200),
        (
            "financial-summary",
            json!({"company":"acme","currency":"USD","from":"2026-10-01","to":"2026-10-31","compare_from":"2026-09-01","compare_to":"2026-09-30","cash_account":"1000"}),
            200,
        ),
        ("accounts", json!({"company":"acme"}), 200),
        (
            "transactions",
            json!({"company":"acme","from":"2026-10-01","to":"2026-10-31","category":"sched-c:24b","payee":"Café & Tea / 100%"}),
            200,
        ),
        (
            "transaction",
            json!({"company":"acme","transaction_id":transaction_id}),
            200,
        ),
        ("reports", json!({"company":"acme"}), 200),
        (
            "income-statement",
            json!({"company":"acme","from":"2026-10-01","to":"2026-10-31","compare_from":"2026-09-01","compare_to":"2026-09-30"}),
            200,
        ),
        (
            "balance-sheet",
            json!({"company":"acme","to":"2026-10-31"}),
            200,
        ),
        ("cash-flow", json!({"company":"acme"}), 200),
        ("trend", json!({"company":"acme","period":"month"}), 200),
        (
            "budget-variance",
            json!({"company":"acme","year":"2026"}),
            200,
        ),
        ("schedule-c", json!({"company":"acme"}), 200),
        ("1099", json!({"company":"acme","threshold":"0"}), 200),
        ("reconciliation", json!({"company":"acme"}), 200),
        ("review", json!({"company":"acme"}), 200),
        (
            "review_detail",
            json!({"company":"acme","proposal_id":"proposal_01h455vb4pex5vsknk084sn02q"}),
            404,
        ),
        ("settings", json!({}), 200),
    ] {
        let result = link(server, reader, view, state)?;
        let url = result["url"].as_str().ok_or("URL")?;
        assert_eq!(
            url,
            format!("{}{}", server.base, result["path"].as_str().ok_or("path")?)
        );
        assert!(!url.contains("token="));
        assert!(!url.contains("invite="));
        let response = server.client.get(url).send()?;
        assert_eq!(
            response.status(),
            expected_status,
            "{view}: {}",
            response.text()?
        );
    }
    Ok(())
}

fn assert_redirect_parameter_is_rejected(server: &Server) -> TestResult {
    // An arbitrary redirect query cannot influence the destination.
    assert_eq!(
        server
            .client
            .get(format!(
                "{}/transactions?next=https://evil.example",
                server.base
            ))
            .send()?
            .status(),
        401
    );
    Ok(())
}
