mod common;

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
fn links_work_without_tabs_in_read_only_mode_and_match_the_cli() -> TestResult {
    let ledger = Ledger::new()?;
    let db = ledger.db()?;
    beankeeper_cli::db::create_account(db.conn(), "acme", "1000", "Cash", "asset", None)?;
    beankeeper_cli::db::create_account(db.conn(), "acme", "5000", "Dining", "expense", None)?;
    let actor = beankeeper_cli::db::Actor::new("owner").with_identity(
        beankeeper_cli::identity::verify(db.conn(), token(&ledger.writer)?)?,
    );
    let entries = [
        beankeeper_cli::db::PostEntryParams {
            account_code: "5000".into(),
            direction: "debit".into(),
            amount: 2500,
            memo: None,
            tax_category: Some("sched-c:24b".into()),
        },
        beankeeper_cli::db::PostEntryParams {
            account_code: "1000".into(),
            direction: "credit".into(),
            amount: 2500,
            memo: None,
            tax_category: None,
        },
    ];
    let beankeeper_cli::db::PostResult::Created(transaction_id) =
        beankeeper_cli::db::post_transaction(
            db.conn(),
            &beankeeper_cli::db::PostTransactionParams {
                company_slug: "acme",
                description: "Dining",
                metadata: Some("{\"payee\":\"Café & Tea / 100%\"}"),
                currency: "USD",
                date: "2026-10-20",
                entries: &entries,
                correlate: None,
                reference: None,
                on_conflict: beankeeper_cli::db::ConflictStrategy::Error,
                actor: &actor,
            },
        )?
    else {
        return Err("transaction was not created".into());
    };
    drop(db);
    let reader = token(&ledger.reader)?;
    let mut server = Server::start(&ledger, &["serve", "--read-only"], None)?;
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
    assert_link_cli_parity(&server, reader)?;
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
fn cli_links_need_no_server_or_ledger_and_reject_invalid_filters() -> TestResult {
    let directory = tempfile::tempdir()?;
    let output = clean_command()
        .current_dir(directory.path())
        .args([
            "ui",
            "link",
            "--view-id",
            "settings",
            "--bind",
            "[::1]:54321",
        ])
        .output()?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8(output.stdout)?.trim(),
        "http://[::1]:54321/settings"
    );
    assert_eq!(std::fs::read_dir(directory.path())?.count(), 0);
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

fn assert_link_cli_parity(server: &Server, reader: &str) -> TestResult {
    let bind = server.base.strip_prefix("http://").ok_or("bind")?;
    let output = clean_command()
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

fn assert_view_links_resolve(server: &Server, reader: &str, transaction_id: i64) -> TestResult {
    for (view, state, expected_status) in [
        ("dashboard", json!({"company":"acme","currency":"USD"}), 200),
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
