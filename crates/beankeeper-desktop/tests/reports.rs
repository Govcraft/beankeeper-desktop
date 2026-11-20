mod common;

use beankeeper_cli::db::{self, Actor, Db};
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
            for value in map.values() {
                rendered_scalars(value, html);
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

#[test]
fn every_report_matches_cli_json_and_url_filters_restore_the_rendered_results() -> TestResult {
    let ledger = fixture()?;
    let before = fingerprint(&ledger.db()?)?;
    let mut server = Server::start(&ledger, &["serve"], None)?;
    server.pair()?;
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
