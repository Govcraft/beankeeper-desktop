mod common;

use beankeeper_cli::db::{self, Actor, Db};
use chrono::{Datelike, Local};
use common::{Ledger, Server, TestResult, rpc, token};
use serde_json::{Value, json};

fn fixture() -> TestResult<Ledger> {
    let ledger = Ledger::new()?;
    let db = ledger.db()?;
    db::create_company(db.conn(), "other", "Other company", None)?;
    for company in ["acme", "other"] {
        for (code, name, kind) in [
            ("1000", "Checking <bank>", "asset"),
            ("4000", "Sales", "revenue"),
            ("5000", "Office supplies", "expense"),
        ] {
            db::create_account(db.conn(), company, code, name, kind, None)?;
        }
    }
    for (company, date, currency, debit, credit, amount, category) in [
        (
            "acme",
            "2026-01-10",
            "USD",
            "1000",
            "4000",
            123_456,
            "sched-c:1",
        ),
        (
            "acme",
            "2026-02-10",
            "USD",
            "5000",
            "1000",
            65_000,
            "sched-c:18",
        ),
        (
            "acme",
            "2026-03-10",
            "USD",
            "5000",
            "1000",
            4_321,
            "custom-tax",
        ),
        ("acme", "2026-04-10", "USD", "5000", "1000", 3_210, ""),
        (
            "acme",
            "2026-01-10",
            "MXN",
            "1000",
            "4000",
            987_654,
            "sched-c:1",
        ),
        (
            "acme",
            "2025-01-10",
            "USD",
            "1000",
            "4000",
            111,
            "sched-c:1",
        ),
        (
            "other",
            "2026-01-10",
            "USD",
            "1000",
            "4000",
            222_222,
            "sched-c:1",
        ),
    ] {
        let metadata = json!({"payee":"Vendor, \"quoted\"\n<script>", "tin":"000-00-0000", "payee_address":"A & B", "applies_to":"sale"}).to_string();
        let entries = [
            db::PostEntryParams {
                account_code: debit.into(),
                direction: "debit".into(),
                amount,
                memo: None,
                tax_category: (debit == "5000" && !category.is_empty()).then(|| category.into()),
            },
            db::PostEntryParams {
                account_code: credit.into(),
                direction: "credit".into(),
                amount,
                memo: None,
                tax_category: (credit == "4000").then(|| category.into()),
            },
        ];
        let result = db::post_transaction(
            db.conn(),
            &db::PostTransactionParams {
                company_slug: company,
                date,
                currency,
                description: "Fixture <sale>",
                metadata: Some(&metadata),
                entries: &entries,
                correlate: None,
                reference: (debit == "1000" && date == "2026-01-10").then_some(
                    if currency == "USD" {
                        "sale"
                    } else {
                        "sale-mxn"
                    },
                ),
                on_conflict: db::ConflictStrategy::Error,
                actor: &Actor::new("owner"),
            },
        )?;
        if date == "2026-01-10" && company == "acme" && currency == "USD" {
            let db::PostResult::Created(id) = result else {
                return Err("expected transaction".into());
            };
            let entry = db::get_entries_for_transaction(db.conn(), id)?
                .into_iter()
                .find(|e| e.account_code == "1000")
                .ok_or("cash entry")?;
            for (statement, amount, status, date) in [
                ("S1", 40_000, db::ClearingStatus::Reconciled, "2026-01-20"),
                ("S2", 20_000, db::ClearingStatus::Cleared, "2026-02-20"),
            ] {
                db::clear_entry(
                    db.conn(),
                    &Actor::new("owner"),
                    &db::ClearRequest {
                        company_slug: company,
                        transaction_id: id,
                        entry_id: entry.id,
                        amount: Some(amount),
                        date,
                        statement: Some(statement),
                        status,
                    },
                )?;
            }
        }
    }
    db::set_budget(
        db.conn(),
        &Actor::new("owner"),
        &db::SetBudgetParams {
            company_slug: "acme",
            account_code: "5000",
            currency: "USD",
            year: 2026,
            month: 2,
            amount: 70_000,
            notes: None,
        },
    )?;
    Ok(ledger)
}

fn cli_report(ledger: &Ledger, args: &[&str], format: &str) -> TestResult<String> {
    let output = ledger
        .command()
        .args([
            "--company",
            "acme",
            "--agent-token",
            token(&ledger.reader)?,
            "--quiet",
            "--format",
            format,
            "report",
        ])
        .args(args)
        .output()?;
    assert!(
        output.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(String::from_utf8(output.stdout)?)
}

fn rendered_scalars(value: &Value, html: &str) {
    match value {
        Value::Object(map) => {
            for (key, value) in map {
                if key.ends_with("_display") {
                    // Compare decimal companions after removing presentation-only
                    // symbols and grouping. Raw amount fields are verified by the
                    // JSON/CSV contract checks, rather than expected in HTML.
                    let decimals = html
                        .replace(",", "")
                        .replace(['$', '€', '£', '¥'], "")
                        .replace("CA", "")
                        .replace("MX", "");
                    rendered_scalars(value, &decimals);
                } else if !map.contains_key(&format!("{key}_display")) {
                    rendered_scalars(value, html);
                }
            }
        }
        Value::Array(rows) => {
            for row in rows {
                rendered_scalars(row, html);
            }
        }
        Value::Null => {}
        Value::String(s) => assert!(
            html.contains(
                &s.replace('&', "&amp;")
                    .replace('<', "&lt;")
                    .replace('>', "&gt;")
                    .replace('"', "&quot;")
                    .replace('\'', "&#39;")
            ),
            "missing {s}"
        ),
        other => assert!(html.contains(&other.to_string()), "missing {other}"),
    }
}

fn post_income_activity(
    db: &Db,
    company: &str,
    date: &str,
    currency: &str,
    debit: &str,
    credit: &str,
) -> TestResult {
    db::post_transaction(
        db.conn(),
        &db::PostTransactionParams {
            company_slug: company,
            date,
            currency,
            description: "Income period fixture",
            metadata: None,
            entries: &[
                db::PostEntryParams {
                    account_code: debit.into(),
                    direction: "debit".into(),
                    amount: 10_000,
                    memo: None,
                    tax_category: None,
                },
                db::PostEntryParams {
                    account_code: credit.into(),
                    direction: "credit".into(),
                    amount: 10_000,
                    memo: None,
                    tax_category: None,
                },
            ],
            correlate: None,
            reference: None,
            on_conflict: db::ConflictStrategy::Error,
            actor: &Actor::new("owner"),
        },
    )?;
    Ok(())
}

#[test]
fn income_statement_exposes_defaults_and_empty_ranges_without_changing_cli_results() -> TestResult {
    let ledger = Ledger::new()?;
    let db = ledger.db()?;
    let today = Local::now().date_naive();
    let previous_year = today.year() - 1;
    let historical = format!("{previous_year}-01-10");
    for company in ["acme", "other", "empty"] {
        if company != "acme" {
            db::create_company(db.conn(), company, company, None)?;
        }
        db::create_account(db.conn(), company, "1000", "Checking", "asset", None)?;
        db::create_account(db.conn(), company, "4000", "Sales", "revenue", None)?;
    }
    post_income_activity(&db, "acme", &historical, "USD", "1000", "4000")?;
    // Activity in another company or currency must not hide the empty-range note.
    post_income_activity(&db, "other", &today.to_string(), "USD", "1000", "4000")?;
    post_income_activity(&db, "acme", &today.to_string(), "MXN", "1000", "4000")?;
    let mut server = Server::start(&ledger, &["serve"], None)?;
    server.pair()?;
    let before = fingerprint(&db)?;
    let previous_end = format!("{previous_year}-12-31");
    let previous_start = format!("{previous_year}-01-01");
    for (filters, args, empty) in [
        (String::new(), vec![], true),
        // Empty form submissions use the same defaults as omitted bounds.
        ("&from=&to=".into(), vec![], true),
        (
            format!("&to={previous_end}"),
            vec!["--to", previous_end.as_str()],
            false,
        ),
        (
            format!("&from={previous_start}"),
            vec!["--from", previous_start.as_str()],
            false,
        ),
        (
            format!("&from={previous_start}&to={previous_end}"),
            vec![
                "--from",
                previous_start.as_str(),
                "--to",
                previous_end.as_str(),
            ],
            false,
        ),
        (
            "&from=1900-01-01&to=1900-12-31".into(),
            vec!["--from", "1900-01-01", "--to", "1900-12-31"],
            true,
        ),
    ] {
        let mut cli_args = vec!["income-statement", "--currency", "USD"];
        cli_args.extend(args);
        let expected: Value = serde_json::from_str(&cli_report(&ledger, &cli_args, "json")?)?;
        let expected = &expected["data"];
        let path = format!(
            "{}/reports/income-statement?company=acme&currency=USD{filters}",
            server.base
        );
        let html = server
            .client
            .get(&path)
            .send()?
            .error_for_status()?
            .text()?;
        assert_eq!(
            html.contains("No revenue or expense activity in this range for USD"),
            empty,
            "{path}: {html}"
        );
        for field in ["from", "to"] {
            let date = expected[field].as_str().ok_or("resolved report date")?;
            assert!(html.contains(&format!("name=\"{field}\" type=\"date\" value=\"{date}\"")));
        }
        assert!(html.contains(&format!(
            "Reporting period: {} through {} (inclusive).",
            expected["from"].as_str().ok_or("from")?,
            expected["to"].as_str().ok_or("to")?
        )));
        let actual: Value = server
            .client
            .get(format!("{path}&format=json"))
            .send()?
            .json()?;
        assert_eq!(&actual, expected);
        let download = html
            .split_once("<a download href=\"")
            .and_then(|(_, rest)| rest.split_once('"'))
            .map(|(url, _)| url.replace("&amp;", "&"))
            .ok_or("JSON download link")?;
        assert!(download.contains(&format!(
            "from={}",
            expected["from"].as_str().ok_or("from")?
        )));
        assert!(download.contains(&format!("to={}", expected["to"].as_str().ok_or("to")?)));
        let downloaded: Value = server
            .client
            .get(format!("{}{download}", server.base))
            .send()?
            .json()?;
        assert_eq!(&downloaded, expected);
        if empty {
            assert!(html.contains("href=\"/transactions?company=acme&amp;currency=USD\""));
            assert!(html.contains("Choose different dates above"));
        }
    }
    for (filters, empty) in [
        ("company=other&currency=USD", false),
        ("company=acme&currency=MXN", false),
        ("company=empty", true),
    ] {
        let html = server
            .client
            .get(format!(
                "{}/reports/income-statement?{filters}",
                server.base
            ))
            .send()?
            .error_for_status()?
            .text()?;
        assert_eq!(
            html.contains("No revenue or expense activity"),
            empty,
            "{filters}"
        );
    }
    assert_eq!(before, fingerprint(&db)?);
    Ok(())
}

#[test]
fn zero_net_income_and_offsetting_postings_are_not_reported_as_no_activity() -> TestResult {
    let ledger = fixture()?;
    let db = ledger.db()?;
    db::create_account(db.conn(), "acme", "4001", "Refunds", "revenue", None)?;
    for (date, debit, credit) in [
        ("2024-01-10", "1000", "4000"),
        ("2024-01-10", "4000", "1000"),
        ("2023-01-10", "5000", "1000"),
        ("2023-01-10", "1000", "5000"),
        ("2022-01-10", "1000", "4000"),
        ("2022-01-10", "5000", "1000"),
        ("2021-01-10", "1000", "4000"),
        ("2021-01-10", "4001", "1000"),
    ] {
        post_income_activity(&db, "acme", date, "USD", debit, credit)?;
    }
    let mut server = Server::start(&ledger, &["serve"], None)?;
    server.pair()?;
    let before = fingerprint(&db)?;
    for year in [2021, 2022, 2023, 2024] {
        let path = format!(
            "{}/reports/income-statement?company=acme&currency=USD&to={year}-12-31",
            server.base
        );
        let data: Value = server
            .client
            .get(format!("{path}&format=json"))
            .send()?
            .json()?;
        assert_eq!(data["net_income"], 0);
        if year != 2022 {
            assert_eq!(data["total_revenue"], 0);
            assert_eq!(data["total_expenses"], 0);
        }
        let html = server
            .client
            .get(&path)
            .send()?
            .error_for_status()?
            .text()?;
        assert!(
            !html.contains("No revenue or expense activity"),
            "{path}: {html}"
        );
    }
    assert_eq!(before, fingerprint(&db)?);
    Ok(())
}

#[test]
fn every_report_matches_cli_json_and_url_filters_restore_the_rendered_results() -> TestResult {
    let ledger = fixture()?;
    let mut server = Server::start(&ledger, &["serve"], None)?;
    server.pair()?;
    // Startup audits the review identity; only report reads must be mutation-free.
    let before = fingerprint(&ledger.db()?)?;
    let cases: &[(&str, &[&str])] = &[
        (
            "/reports/income-statement?from=2026-01-01&to=2026-03-31",
            &[
                "income-statement",
                "--from",
                "2026-01-01",
                "--to",
                "2026-03-31",
            ],
        ),
        (
            "/reports/balance-sheet?to=2026-03-31",
            &["balance-sheet", "--to", "2026-03-31"],
        ),
        (
            "/reports/cash-flow?from=2026-01-01&to=2026-03-31&cash_account=1000",
            &[
                "cash-flow",
                "--from",
                "2026-01-01",
                "--to",
                "2026-03-31",
                "--cash-account",
                "1000",
            ],
        ),
        (
            "/reports/trend?from=2026-01-01&to=2026-03-31&metric=spending&period=month",
            &[
                "trend",
                "--metric",
                "spending",
                "--period",
                "month",
                "--from",
                "2026-01-01",
                "--to",
                "2026-03-31",
            ],
        ),
        (
            "/reports/trend?from=2026-01-01&to=2026-03-31&metric=net-worth&period=quarter",
            &[
                "trend",
                "--metric",
                "net-worth",
                "--period",
                "quarter",
                "--from",
                "2026-01-01",
                "--to",
                "2026-03-31",
            ],
        ),
        (
            "/reports/budget-variance?from=2026-02-01&to=2026-03-31",
            &[
                "budget-variance",
                "--year",
                "2026",
                "--from",
                "2",
                "--to",
                "3",
                "--include-unbudgeted",
            ],
        ),
        (
            "/reports/budget-variance?year=2026",
            &["budget-variance", "--year", "2026", "--include-unbudgeted"],
        ),
        (
            "/reports/schedule-c?from=2026-01-01&to=2026-12-31",
            &["schedule-c", "--from", "2026-01-01", "--to", "2026-12-31"],
        ),
        (
            "/reports/1099?from=2026-01-01&to=2026-03-31&threshold=0",
            &[
                "1099",
                "--from",
                "2026-01-01",
                "--to",
                "2026-03-31",
                "--threshold",
                "0",
            ],
        ),
        (
            "/reconciliation?account=1000&to=2026-01-31&statement=S1&statement_balance=400",
            &[
                "reconciliation",
                "--account",
                "1000",
                "--to",
                "2026-01-31",
                "--statement",
                "S1",
                "--statement-balance",
                "400",
            ],
        ),
        (
            "/reports/reconciliation?account=1000&to=2026-03-31&mode=settlement&statement=S1&link_key=applies_to",
            &[
                "reconciliation",
                "--account",
                "1000",
                "--to",
                "2026-03-31",
                "--mode",
                "settlement",
                "--statement",
                "S1",
                "--link-key",
                "applies_to",
            ],
        ),
    ];
    for (path, args) in cases {
        for currency in ["USD", "MXN"] {
            let path = format!("{}{path}&company=acme&currency={currency}", server.base);
            let mut cli_args = args.to_vec();
            cli_args.extend_from_slice(&["--currency", currency]);
            let expected: Value = serde_json::from_str(&cli_report(&ledger, &cli_args, "json")?)?;
            let response = server.client.get(format!("{path}&format=json")).send()?;
            assert_eq!(response.status(), 200, "{path}: {}", response.text()?);
            // Fetch again after inspecting status, exercising reload of saved URLs.
            let response = server.client.get(format!("{path}&format=json")).send()?;
            assert!(
                response.headers()["content-disposition"]
                    .to_str()?
                    .starts_with("attachment;")
            );
            assert_eq!(response.headers()["cache-control"], "no-store");
            let actual: Value = response.json()?;
            assert_eq!(actual, expected["data"], "{path}");
            let html = server.client.get(&path).send()?.text()?;
            assert!(html.contains("<h2>Report results</h2>"), "{path}: {html}");
            assert!(!html.contains("<script>"));
            rendered_scalars(&actual, &html);
            let symbol = if currency == "USD" { "$" } else { "MX$" };
            assert!(
                html.contains(&format!("class=\"amount\">{symbol}")),
                "{path}: {html}"
            );
            assert!(
                !html.contains("(major units)</th>") && !html.contains("(major units)</dt>"),
                "duplicate display columns: {path}"
            );
            assert!(html.contains(&format!(
                "name=\"currency\" type=\"text\" value=\"{currency}\""
            )));
            assert_eq!(html.matches("aria-current=\"page\"").count(), 1);
            assert_eq!(server.client.get(&path).send()?.text()?, html);
            if path.contains("schedule-c") || path.contains("1099") {
                assert!(html.contains("not a filed form"));
                let csv = server.client.get(format!("{path}&format=csv")).send()?;
                assert_eq!(csv.status(), 200);
                assert_eq!(csv.headers()["content-type"], "text/csv; charset=utf-8");
                assert_eq!(
                    csv.text()?,
                    cli_report(&ledger, &cli_args, "csv")?
                        .trim_end_matches('\n')
                        .to_owned()
                        + "\n"
                );
            }
        }
    }
    assert_eq!(before, fingerprint(&ledger.db()?)?);
    Ok(())
}

fn fingerprint(db: &Db) -> TestResult<(i64, i64, i64)> {
    Ok((
        db::get_schema_version(db.conn())?,
        db::count_transactions(db.conn(), &db::ListTransactionParams::for_company("acme"))?,
        db.conn()
            .query_row("SELECT count(*) FROM audit_log", [], |row| row.get(0))?,
    ))
}

#[test]
fn reports_reconciliation_and_review_share_the_workspace_shell() -> TestResult {
    let ledger = Ledger::new()?;
    let mut server = Server::start(&ledger, &["serve"], None)?;
    server.pair()?;
    let destinations = [
        ("/", "Dashboard"),
        ("/accounts", "Accounts"),
        ("/transactions", "Transactions"),
        ("/reports", "Reports"),
        ("/reconciliation", "Reconciliation"),
        ("/review", "Review queue"),
    ];
    for (path, _) in destinations {
        let html = server
            .client
            .get(format!("{}{path}", server.base))
            .send()?
            .error_for_status()?
            .text()?;
        let nav = html
            .split_once("<nav aria-label=\"Workspace\">")
            .and_then(|(_, rest)| rest.split_once("</nav>"))
            .map(|(nav, _)| nav)
            .ok_or("workspace navigation")?;
        assert_eq!(nav.matches("aria-current=\"page\"").count(), 1, "{path}");
        for (destination, label) in destinations {
            let attribute = if destination == path {
                "aria-current=\"page\""
            } else {
                "data-view"
            };
            assert!(
                nav.contains(&format!(
                    "<a href=\"{destination}\" {attribute}>{label}</a>"
                )),
                "{path}: {destination}"
            );
        }
        if path == "/review" {
            assert!(html.contains("Ledger verification"));
            assert!(html.contains("action=\"/review/verify\""));
            assert!(html.contains("No pending proposals"));
        }
    }
    Ok(())
}

#[test]
fn reports_and_tax_downloads_require_human_pairing_and_never_offer_writes() -> TestResult {
    let ledger = fixture()?;
    let mut server = Server::start(&ledger, &["serve"], None)?;
    server.initialize(Some(token(&ledger.writer)?))?;
    assert!(rpc(server.request("tools/list", json!({}), Some(token(&ledger.writer)?))?)?["result"]["tools"].is_array());
    for path in [
        "/reports",
        "/reports/income-statement",
        "/reports/schedule-c?format=csv",
        "/reports/1099?format=json",
        "/reconciliation?account=1000",
    ] {
        for bearer in [None, Some(token(&ledger.reader)?)] {
            let mut request = server.client.get(format!("{}{path}", server.base));
            if let Some(token) = bearer {
                request = request.bearer_auth(token);
            }
            assert_eq!(request.send()?.status(), 401, "{path}");
        }
    }
    server.pair()?;
    let page = server
        .client
        .get(format!("{}/reports", server.base))
        .send()?
        .text()?;
    let csrf = page
        .split_once("name=\"csrf\" value=\"")
        .and_then(|(_, rest)| rest.split_once('"'))
        .map(|(token, _)| token)
        .ok_or("CSRF")?;
    for path in [
        "/reports/schedule-c?format=csv&currency=USD",
        "/reconciliation?account=1000&currency=USD",
    ] {
        assert_eq!(
            server
                .client
                .get(format!("{}{path}", server.base))
                .bearer_auth(token(&ledger.reader)?)
                .send()?
                .status(),
            401
        );
        assert_eq!(
            server
                .client
                .post(format!("{}{path}", server.base))
                .header("Origin", &server.base)
                .header("X-CSRF-Token", csrf)
                .send()?
                .status(),
            405
        );
        assert_eq!(
            server
                .client
                .get(format!("{}{path}", server.base))
                .header("Origin", "https://evil.example")
                .send()?
                .status(),
            403
        );
        assert_eq!(
            server
                .client
                .get(format!("{}{path}", server.base))
                .header("Host", "evil.example")
                .send()?
                .status(),
            403
        );
    }
    let html = server
        .client
        .get(format!("{}/reconciliation", server.base))
        .send()?
        .text()?;
    assert!(html.contains("No clear, reconcile or approval actions"));
    assert!(html.contains("Choose account"));
    Ok(())
}

#[test]
fn reports_show_empty_states_defaults_company_scope_and_actionable_errors() -> TestResult {
    let ledger = fixture()?;
    let mut server = Server::start(&ledger, &["serve"], None)?;
    server.pair()?;
    for (path, status, message) in [
        ("/reports/missing", 404, "not found"),
        ("/reports/income-statement", 400, "currency"),
        ("/reports/income-statement?currency=WRONG", 400, "currency"),
        (
            "/reports/income-statement?currency=USD&from=2026-02-30",
            400,
            "valid calendar",
        ),
        (
            "/reports/income-statement?currency=USD&from=2026-03-01&to=2026-01-01",
            400,
            "start date",
        ),
        ("/reports/balance-sheet?from=2026-01-01", 400, "as-of"),
        (
            "/reports/budget-variance?from=2026-02-02&to=2026-03-31",
            400,
            "whole calendar",
        ),
        (
            "/reports/budget-variance?from=2026-02-01&to=2026-03-30",
            400,
            "whole calendar",
        ),
        (
            "/reports/budget-variance?from=2026-12-01&to=2027-01-31",
            400,
            "one year",
        ),
        ("/reports/budget-variance?year=0", 400, "year"),
        ("/reports/trend?metric=wrong", 400, "wrong"),
        ("/reports/trend?period=week", 400, "week"),
        (
            "/reports/1099?currency=USD&threshold=-1",
            400,
            "nonnegative",
        ),
        (
            "/reconciliation?account=missing&currency=USD",
            404,
            "not found",
        ),
        (
            "/reconciliation?account=1000&mode=settlement&currency=USD",
            400,
            "statement",
        ),
        (
            "/reports/schedule-c?company=missing&format=csv",
            404,
            "not found",
        ),
        ("/reports/income-statement?format=csv", 400, "CSV for a tax"),
        ("/reports/1099?format=exe", 400, "Choose HTML"),
    ] {
        let response = server.client.get(format!("{}{path}", server.base)).send()?;
        assert_eq!(response.status(), status, "{path}: {}", response.text()?);
        assert!(
            server
                .client
                .get(format!("{}{path}", server.base))
                .send()?
                .text()?
                .contains(message),
            "{path}: {message}"
        );
    }
    for path in ["/reports", "/reconciliation", "/reports/budget-variance"] {
        assert_eq!(
            server
                .client
                .get(format!("{}{path}", server.base))
                .send()?
                .status(),
            200
        );
    }
    let response = server
        .client
        .get(format!(
            "{}/reports/schedule-c?company=other&from=1900-01-01&to=1900-12-31",
            server.base
        ))
        .send()?;
    assert_eq!(response.status(), 200);
    assert!(response.text()?.contains("No matching rows"));
    // The configured company remains the default, but a URL explicitly overrides it.
    let mut scoped = Server::start(&ledger, &["--company", "other", "serve"], None)?;
    scoped.pair()?;
    let default: Value = scoped
        .client
        .get(format!(
            "{}/reports/schedule-c?format=json&to=2026-12-31",
            scoped.base
        ))
        .send()?
        .json()?;
    assert_eq!(default["lines"][0]["net"], 222_222);
    let explicit: Value = scoped
        .client
        .get(format!(
            "{}/reports/schedule-c?company=acme&format=json&currency=MXN&to=2026-12-31",
            scoped.base
        ))
        .send()?
        .json()?;
    assert_eq!(explicit["lines"][0]["net"], 987_654);
    Ok(())
}

#[test]
fn deep_links_apply_exact_category_payee_and_both_reporting_periods() -> TestResult {
    let ledger = fixture()?;
    let mut server = Server::start(&ledger, &["serve"], None)?;
    server.pair()?;
    let url = reqwest::Url::parse_with_params(
        &format!("{}/transactions", server.base),
        &[
            ("company", "acme"),
            ("account", "5000"),
            ("category", "sched-c:18"),
            ("payee", "Vendor, \"quoted\"\n<script>"),
            ("currency", "USD"),
            ("from", "2026-02-01"),
            ("to", "2026-02-28"),
            ("compare_from", "2026-03-01"),
            ("compare_to", "2026-03-31"),
        ],
    )?;
    let html = server
        .client
        .get(url.clone())
        .send()?
        .error_for_status()?
        .text()?;
    assert!(html.contains("1 matching transactions shown"));
    assert!(html.contains("Comparison period: 2026-03-01 through 2026-03-31"));
    assert!(html.contains("0 matching transactions shown"));
    assert!(!html.contains("<script>"));
    let wrong_payee = reqwest::Url::parse_with_params(
        &format!("{}/transactions", server.base),
        &[
            ("company", "acme"),
            ("payee", "Vendor"),
            ("currency", "USD"),
        ],
    )?;
    assert!(
        server
            .client
            .get(wrong_payee)
            .send()?
            .text()?
            .contains("0 matching transactions shown")
    );
    for view in [
        "income-statement",
        "cash-flow",
        "trend",
        "budget-variance",
        "schedule-c",
        "1099",
    ] {
        let url = format!(
            "{}/reports/{view}?company=acme&currency=USD&from=2026-02-01&to=2026-02-28&compare_from=2026-03-01&compare_to=2026-03-31",
            server.base
        );
        let html = server.client.get(&url).send()?.error_for_status()?.text()?;
        assert!(html.contains("id=\"comparison-results\""), "{view}");
        assert!(html.contains("Comparison period: 2026-03-01 through 2026-03-31"));
        let data: Value = server
            .client
            .get(format!("{url}&format=json"))
            .send()?
            .error_for_status()?
            .json()?;
        assert!(data["current"].is_object());
        assert!(data["comparison"].is_object());
        assert_eq!(data["compare_from"], "2026-03-01");
        assert_eq!(data["compare_to"], "2026-03-31");
        if view == "income-statement" {
            assert_eq!(data["current"]["total_expenses"], 65_000);
            assert_eq!(data["comparison"]["total_expenses"], 4_321);
        }
    }
    let balance_url = format!(
        "{}/reports/balance-sheet?company=acme&currency=USD&to=2026-03-31&compare_to=2026-01-31",
        server.base
    );
    let html = server
        .client
        .get(&balance_url)
        .send()?
        .error_for_status()?
        .text()?;
    assert!(html.contains("Comparison as of 2026-01-31"));
    let data: Value = server
        .client
        .get(format!("{balance_url}&format=json"))
        .send()?
        .error_for_status()?
        .json()?;
    assert_ne!(data["current"], data["comparison"]);
    for path in [
        "/reports/1099?compare_from=2026-02-01&compare_to=2026-02-28&format=csv",
        "/reports/income-statement?compare_from=2026-02-01",
        "/reports/balance-sheet?compare_from=2026-02-01&compare_to=2026-02-28",
    ] {
        assert_eq!(
            server
                .client
                .get(format!("{}{path}&company=acme&currency=USD", server.base))
                .send()?
                .status(),
            400
        );
    }
    Ok(())
}
