//! Stable Desktop report views backed by the upstream CLI report contract.

use std::{collections::BTreeSet, fmt::Write};

use beankeeper_cli::{
    cli::{Cli, ReconciliationMode, ReportCommand},
    commands::{
        report::execute_json,
        report_cash_flow::CashFlowArgs,
        report_currency::resolve_report_currency,
        report_periods::PeriodArg,
        report_tax::{TaxArgs, VendorArgs},
        report_trend::{TrendArgs, TrendMetric},
    },
    db::{self, Db},
    error::CliError,
};
use clap::{Parser, ValueEnum};
use serde::Deserialize;
use serde_json::Value;

use crate::views::{ViewError, ViewPage, ViewQuery, encode, escape, input, nonempty, valid_date};

/// Stable view IDs for agent navigation. Financial/tax IDs use `/reports/ID`;
/// reconciliation uses `/reconciliation`. All filters are query parameters.
pub const VIEW_IDS: &[&str] = &[
    "income-statement",
    "balance-sheet",
    "cash-flow",
    "trend",
    "budget-variance",
    "schedule-c",
    "1099",
    "reconciliation",
];

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Income,
    Balance,
    Cash,
    Trend,
    Budget,
    Schedule,
    Vendor,
    Reconciliation,
}

impl Kind {
    fn parse(path: &str) -> Result<Self, ViewError> {
        match path {
            "/reports/income-statement" => Ok(Self::Income),
            "/reports/balance-sheet" => Ok(Self::Balance),
            "/reports/cash-flow" => Ok(Self::Cash),
            "/reports/trend" => Ok(Self::Trend),
            "/reports/budget-variance" => Ok(Self::Budget),
            "/reports/schedule-c" => Ok(Self::Schedule),
            "/reports/1099" => Ok(Self::Vendor),
            "/reports/reconciliation" | "/reconciliation" => Ok(Self::Reconciliation),
            _ => Err(ViewError::NotFound),
        }
    }

    fn title(self) -> &'static str {
        match self {
            Self::Income => "Income statement",
            Self::Balance => "Balance sheet",
            Self::Cash => "Cash flow",
            Self::Trend => "Trends",
            Self::Budget => "Budget variance",
            Self::Schedule => "Schedule C book summary",
            Self::Vendor => "1099 vendor book summary",
            Self::Reconciliation => "Reconciliation",
        }
    }

    fn tax(self) -> bool {
        matches!(self, Self::Schedule | Self::Vendor)
    }
}

#[derive(Clone, Default, Deserialize)]
pub(crate) struct ReportQuery {
    #[serde(flatten)]
    pub filters: ViewQuery,
    metric: Option<String>,
    period: Option<String>,
    year: Option<String>,
    threshold: Option<String>,
    mode: Option<String>,
    statement: Option<String>,
    statement_balance: Option<String>,
    link_key: Option<String>,
    cash_account: Option<String>,
    format: Option<String>,
}

pub(crate) enum ReportResponse {
    Page(ViewPage),
    Download {
        body: String,
        content_type: &'static str,
        filename: String,
    },
}

fn option<T: ValueEnum>(value: &Option<String>, default: &str) -> Result<T, ViewError> {
    T::from_str(nonempty(value).unwrap_or(default), false).map_err(ViewError::ReportOptions)
}

impl ReportQuery {
    fn command(&self, kind: Kind) -> Result<ReportCommand, ViewError> {
        let q = &self.filters;
        q.validate()?;
        let from = nonempty(&q.from).map(str::to_owned);
        let to = nonempty(&q.to).map(str::to_owned);
        let currency = nonempty(&q.currency).map(str::to_owned);
        if matches!(kind, Kind::Balance | Kind::Reconciliation) && from.is_some() {
            return Err(ViewError::Invalid(
                "This is an as-of report. Use the end date; a start date is not supported.",
            ));
        }
        let tax = || TaxArgs {
            from: from.clone(),
            to: to.clone(),
            currency: currency.clone(),
            output_format: None,
        };
        Ok(match kind {
            Kind::Income => ReportCommand::IncomeStatement {
                from,
                to,
                currency,
                period: None,
                compare: None,
            },
            Kind::Balance => ReportCommand::BalanceSheet { to, currency },
            Kind::Cash => ReportCommand::CashFlow(CashFlowArgs {
                from,
                to,
                currency,
                cash_account: nonempty(&self.cash_account)
                    .map(|s| vec![s.into()])
                    .unwrap_or_default(),
            }),
            Kind::Trend => ReportCommand::Trend(TrendArgs {
                from,
                to,
                currency,
                metric: option::<TrendMetric>(&self.metric, "spending")?,
                period: option::<PeriodArg>(&self.period, "month")?,
            }),
            Kind::Budget => {
                let (year, from, to) = self.budget_window()?;
                ReportCommand::BudgetVariance {
                    year,
                    month: None,
                    from,
                    to,
                    currency,
                    account_type: None,
                    include_unbudgeted: true,
                }
            }
            Kind::Schedule => ReportCommand::ScheduleC(tax()),
            Kind::Vendor => ReportCommand::Vendor1099(VendorArgs {
                period: tax(),
                threshold: nonempty(&self.threshold).unwrap_or("600").into(),
            }),
            Kind::Reconciliation => ReportCommand::Reconciliation {
                account: nonempty(&q.account)
                    .ok_or(ViewError::Invalid("Choose an account to reconcile."))?
                    .into(),
                to,
                currency,
                mode: option::<ReconciliationMode>(&self.mode, "bank")?,
                statement: nonempty(&self.statement).map(str::to_owned),
                statement_balance: nonempty(&self.statement_balance).map(str::to_owned),
                link_key: nonempty(&self.link_key).map(str::to_owned),
                filter_metadata: Vec::new(),
                group_by_metadata: None,
            },
        })
    }

    // The CLI accepts a year and whole months, not arbitrary date intervals.
    fn budget_window(&self) -> Result<(i32, Option<i32>, Option<i32>), ViewError> {
        let fail = || {
            ViewError::Invalid(
                "Budget variance requires a year (1-9999), or a date range covering whole calendar months within one year.",
            )
        };
        let year = nonempty(&self.year)
            .map(|s| s.parse::<i32>().map_err(|_| fail()))
            .transpose()?;
        match (nonempty(&self.filters.from), nonempty(&self.filters.to)) {
            (None, None) => Ok((
                year.filter(|y| (1..=9999).contains(y)).ok_or_else(fail)?,
                None,
                None,
            )),
            (Some(from), Some(to)) => {
                let range_year = from[..4].parse::<i32>().map_err(|_| fail())?;
                let day = to[8..].parse::<u32>().map_err(|_| fail())?;
                let next_day = format!("{}{:02}", &to[..8], day + 1);
                if !from.ends_with("-01")
                    || from[..4] != to[..4]
                    || valid_date(&next_day)
                    || year.is_some_and(|year| year != range_year)
                {
                    return Err(fail());
                }
                Ok((
                    range_year,
                    Some(from[5..7].parse().map_err(|_| fail())?),
                    Some(to[5..7].parse().map_err(|_| fail())?),
                ))
            }
            _ => Err(fail()),
        }
    }

    fn fields(&self) -> [(&'static str, &Option<String>); 13] {
        [
            ("account", &self.filters.account),
            ("from", &self.filters.from),
            ("to", &self.filters.to),
            ("currency", &self.filters.currency),
            ("metric", &self.metric),
            ("period", &self.period),
            ("year", &self.year),
            ("threshold", &self.threshold),
            ("mode", &self.mode),
            ("statement", &self.statement),
            ("statement_balance", &self.statement_balance),
            ("link_key", &self.link_key),
            ("cash_account", &self.cash_account),
        ]
    }

    fn url(&self, path: &str, company: &str, format: &str) -> String {
        let mut url = format!("{path}?company={}", encode(company));
        for (key, value) in self.fields() {
            if let Some(value) = nonempty(value) {
                let _ = write!(url, "&{key}={}", encode(value));
            }
        }
        let _ = write!(url, "&format={format}");
        escape(&url)
    }
}

pub(crate) fn render(
    db: &Db,
    path: &str,
    query: &ReportQuery,
) -> Result<ReportResponse, ViewError> {
    query.filters.validate()?;
    let kind = if path == "/reports" {
        None
    } else {
        Some(Kind::parse(path)?)
    };
    let companies = db::list_companies(db.conn())?;
    let company =
        nonempty(&query.filters.company).or_else(|| companies.first().map(|c| c.slug.as_str()));
    let title = kind.map_or("Reports", Kind::title);
    let nav = if kind == Some(Kind::Reconciliation) {
        "/reconciliation"
    } else {
        "/reports"
    };
    let page = |content| {
        ReportResponse::Page(ViewPage {
            title,
            description: "Read-only reports from your books. Amounts use minor units; formatted major-unit amounts are labeled.",
            content,
            nav,
        })
    };
    let Some(company) = company else {
        return Ok(page(
            "<p class=\"empty\">No companies yet. Ask your agent to initialize your books.</p>"
                .into(),
        ));
    };
    if !companies.iter().any(|c| c.slug == company) {
        return Err(ViewError::NotFound);
    }
    let mut html = form(db, &companies, company, path, kind, query)?;
    let Some(kind) = kind else {
        html.push_str("<section><h2>Choose a report</h2><ul>");
        for id in VIEW_IDS {
            let path = if *id == "reconciliation" {
                "/reconciliation".into()
            } else {
                format!("/reports/{id}")
            };
            let title = Kind::parse(&path)?.title();
            let _ = write!(
                html,
                "<li><a href=\"{path}?company={}\">{title}</a></li>",
                encode(company)
            );
        }
        html.push_str("</ul></section>");
        return Ok(page(html));
    };
    if kind.tax() {
        html.push_str(
            "<p class=\"note\">Bookkeeping summary only. Not tax advice; not a filed form.</p>",
        );
    }
    if kind == Kind::Reconciliation {
        html.push_str("<p>Read-only clearance and settlement state. No clear, reconcile or approval actions are available here. Statement balance is in major units.</p>");
    }
    let format = nonempty(&query.format).unwrap_or("html");
    if !["html", "json", "csv"].contains(&format) || (format == "csv" && !kind.tax()) {
        return Err(ViewError::Invalid(
            "Choose HTML or JSON, or CSV for a tax summary.",
        ));
    }
    if format == "html"
        && ((kind == Kind::Reconciliation && nonempty(&query.filters.account).is_none())
            || (kind == Kind::Budget
                && nonempty(&query.year).is_none()
                && nonempty(&query.filters.from).is_none()))
    {
        html.push_str(
            "<p class=\"empty\">Choose the report filters above, then apply to see results.</p>",
        );
        return Ok(page(html));
    }
    let command = query.command(kind)?;
    // This context supplies JSON metadata only. execute_json uses the supplied
    // read-only Db and never opens the CLI's default database or writes stdout.
    let cli = Cli::try_parse_from(["bk", "--quiet", "report", "income-statement"])
        .map_err(|_| ViewError::Invalid("The report context could not be initialized."))?;
    let data = execute_json(&cli, db, company, &command).map_err(report_error)?;
    if format != "html" {
        let (body, content_type) = if format == "csv" {
            (tax_csv(kind, &data), "text/csv; charset=utf-8")
        } else {
            (data.to_string(), "application/json")
        };
        let id = if kind == Kind::Reconciliation {
            "reconciliation"
        } else {
            path.trim_start_matches("/reports/")
        };
        return Ok(ReportResponse::Download {
            body,
            content_type,
            filename: format!("{id}.{format}"),
        });
    }
    let currency = resolve_report_currency(db.conn(), company, nonempty(&query.filters.currency))
        .map_err(report_error)?;
    let _ = write!(html, "<p>Report currency: {}.</p>", escape(currency.code()));
    let _ = write!(
        html,
        "<p><a download href=\"{}\">Download JSON</a>",
        query.url(path, company, "json")
    );
    if kind.tax() {
        let _ = write!(
            html,
            " · <a download href=\"{}\">Download CSV</a>",
            query.url(path, company, "csv")
        );
    }
    html.push_str("</p><section class=\"report-results\"><h2>Report results</h2>");
    value_html(&data, &mut html);
    html.push_str("</section>");
    Ok(page(html))
}

fn report_error(error: CliError) -> ViewError {
    match error {
        CliError::Validation(message) | CliError::Usage(message) => {
            ViewError::ReportOptions(message)
        }
        error => ViewError::Ledger(error),
    }
}

fn select(
    html: &mut String,
    name: &str,
    label: &str,
    value: &Option<String>,
    default: &str,
    options: &[&str],
) {
    let _ = write!(html, "<label>{label}<select name=\"{name}\">");
    for choice in options {
        let selected = if nonempty(value).unwrap_or(default) == *choice {
            " selected"
        } else {
            ""
        };
        let _ = write!(html, "<option{selected}>{choice}</option>");
    }
    html.push_str("</select></label>");
}

fn form(
    db: &Db,
    companies: &[db::CompanyRow],
    company: &str,
    path: &str,
    kind: Option<Kind>,
    q: &ReportQuery,
) -> Result<String, ViewError> {
    let mut html = format!(
        "<form method=\"get\" action=\"{}\" class=\"filters\"><label>Company<select name=\"company\">",
        escape(path)
    );
    for c in companies {
        let _ = write!(
            html,
            "<option value=\"{}\"{}>{} ({})</option>",
            escape(&c.slug),
            if c.slug == company { " selected" } else { "" },
            escape(&c.name),
            escape(&c.slug)
        );
    }
    html.push_str("</select></label>");
    if let Some(kind) = kind {
        if !matches!(kind, Kind::Balance | Kind::Reconciliation) {
            html.push_str(&input(
                "from",
                "From date (inclusive)",
                "date",
                &q.filters.from,
            ));
        }
        html.push_str(&input(
            "to",
            "Through date (inclusive)",
            "date",
            &q.filters.to,
        ));
        html.push_str(&input(
            "currency",
            "Currency (required for multi-currency books)",
            "text",
            &q.filters.currency,
        ));
        match kind {
            Kind::Trend => {
                select(
                    &mut html,
                    "metric",
                    "Metric",
                    &q.metric,
                    "spending",
                    &["spending", "net-worth"],
                );
                select(
                    &mut html,
                    "period",
                    "Calendar period",
                    &q.period,
                    "month",
                    &["month", "quarter", "year"],
                );
            }
            Kind::Budget => {
                html.push_str(&input(
                    "year",
                    "Budget year (or use a whole-month date range)",
                    "number",
                    &q.year,
                ));
            }
            Kind::Vendor => html.push_str(&input(
                "threshold",
                "Minimum payee total (major units; default 600)",
                "text",
                &q.threshold,
            )),
            Kind::Cash => html.push_str(&input(
                "cash_account",
                "Cash account (optional; inferred by name)",
                "text",
                &q.cash_account,
            )),
            Kind::Reconciliation => {
                html.push_str("<label>Account<select name=\"account\"><option value=\"\">Choose account</option>");
                for account in db::list_accounts(
                    db.conn(),
                    &db::ListAccountParams {
                        company_slug: company,
                        type_filter: None,
                        name_filter: None,
                    },
                )? {
                    let _ = write!(
                        html,
                        "<option value=\"{}\"{}>{} · {}</option>",
                        escape(&account.code),
                        if nonempty(&q.filters.account) == Some(&account.code) {
                            " selected"
                        } else {
                            ""
                        },
                        escape(&account.code),
                        escape(&account.name)
                    );
                }
                html.push_str("</select></label>");
                select(
                    &mut html,
                    "mode",
                    "Mode",
                    &q.mode,
                    "bank",
                    &["bank", "settlement"],
                );
                html.push_str(&input(
                    "statement",
                    "Statement ID (required for settlement)",
                    "text",
                    &q.statement,
                ));
                html.push_str(&input(
                    "statement_balance",
                    "Statement balance (major units)",
                    "text",
                    &q.statement_balance,
                ));
                html.push_str(&input(
                    "link_key",
                    "Settlement link key (default applies_to)",
                    "text",
                    &q.link_key,
                ));
            }
            _ => {}
        }
    }
    html.push_str("<button type=\"submit\">Apply</button></form>");
    if kind == Some(Kind::Budget) {
        html.push_str("<p class=\"note\">Budget variance covers whole calendar months within one year. Use both date bounds, starting on the first day and ending on the last day of a month, or enter a year for the full year. Includes accounts without a budget.</p>");
    }
    Ok(html)
}

fn text(value: &Value) -> String {
    match value {
        Value::Null => String::new(),
        Value::String(s) => s.clone(),
        _ => value.to_string(),
    }
}

fn label(key: &str) -> String {
    match key.strip_suffix("_display") {
        Some(key) => format!("{} (major units)", escape(&key.replace('_', " "))),
        None => escape(&key.replace('_', " ")),
    }
}

// Keep the upstream report structure lossless, including nested settlement
// details, unmatched tax categories and trend columns, without raw JSON UI.
fn value_html(value: &Value, html: &mut String) {
    match value {
        Value::Object(map) => {
            html.push_str("<dl>");
            for (key, value) in map {
                let _ = write!(html, "<dt>{}</dt><dd>", label(key));
                value_html(value, html);
                html.push_str("</dd>");
            }
            html.push_str("</dl>");
        }
        Value::Array(rows) if rows.is_empty() => {
            html.push_str("<p class=\"empty\">No matching rows.</p>")
        }
        Value::Array(rows) => {
            if rows.iter().all(|row| {
                row.as_object().is_some_and(|map| {
                    map.values()
                        .all(|value| !value.is_object() && !value.is_array())
                })
            }) {
                let columns: BTreeSet<_> = rows
                    .iter()
                    .filter_map(Value::as_object)
                    .flat_map(|map| map.keys())
                    .collect();
                html.push_str("<div class=\"table-scroll\" tabindex=\"0\" role=\"region\" aria-label=\"Report table\"><table><caption>Report rows</caption><thead><tr>");
                for key in &columns {
                    let _ = write!(html, "<th scope=\"col\">{}</th>", label(key));
                }
                html.push_str("</tr></thead><tbody>");
                for row in rows {
                    html.push_str("<tr>");
                    for key in &columns {
                        html.push_str("<td>");
                        value_html(&row[*key], html);
                        html.push_str("</td>");
                    }
                    html.push_str("</tr>");
                }
                html.push_str("</tbody></table></div>");
            } else {
                html.push_str("<ol class=\"report-items\">");
                for row in rows {
                    html.push_str("<li>");
                    value_html(row, html);
                    html.push_str("</li>");
                }
                html.push_str("</ol>");
            }
        }
        Value::Number(_) => {
            let _ = write!(html, "<span class=\"number\">{value}</span>");
        }
        Value::Null => html.push_str("Not recorded"),
        _ => html.push_str(&escape(&text(value))),
    }
}

fn csv_row(html: &mut String, cells: impl IntoIterator<Item = String>) {
    for (i, cell) in cells.into_iter().enumerate() {
        if i > 0 {
            html.push(',');
        }
        if cell.contains([',', '"', '\r', '\n']) {
            html.push('"');
            html.push_str(&cell.replace('"', "\"\""));
            html.push('"');
        } else {
            html.push_str(&cell);
        }
    }
    html.push('\n');
}

// CSV uses upstream's tax column order and integer amounts. It is a projection
// of execute_json, with no duplicated tax classification or amount arithmetic.
fn tax_csv(kind: Kind, data: &Value) -> String {
    let (columns, sections): (&[&str], &[&str]) = if kind == Kind::Schedule {
        (
            &[
                "section",
                "line",
                "title",
                "tax_category",
                "account_code",
                "account_type",
                "debit_total",
                "credit_total",
                "net",
                "currency",
                "from",
                "to",
            ],
            &["lines", "unmapped", "untagged"],
        )
    } else {
        (
            &[
                "payee",
                "tin",
                "payee_address",
                "payment_count",
                "total",
                "meets_threshold",
                "threshold",
                "currency",
                "from",
                "to",
            ],
            &["vendors"],
        )
    };
    let mut csv = String::new();
    csv_row(&mut csv, columns.iter().map(|s| (*s).into()));
    for section in sections {
        for row in data[*section].as_array().into_iter().flatten() {
            csv_row(
                &mut csv,
                columns.iter().map(|key| {
                    if *key == "section" {
                        (*section).into()
                    } else {
                        text(row.get(*key).unwrap_or(&data[*key]))
                    }
                }),
            );
        }
    }
    csv
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    #[test]
    fn empty_books_and_read_failures_have_explicit_states() -> TestResult {
        let db = Db::open_in_memory()?;
        for path in ["/reports", "/reports/schedule-c", "/reconciliation"] {
            let ReportResponse::Page(page) = render(&db, path, &ReportQuery::default())? else {
                return Err("expected HTML".into());
            };
            assert!(page.content.contains("No companies yet"));
        }
        db.conn().execute("DROP TABLE companies", [])?;
        let error = render(&db, "/reports", &ReportQuery::default())
            .err()
            .ok_or("expected read failure")?;
        assert_eq!(
            error.status(),
            axum::http::StatusCode::INTERNAL_SERVER_ERROR
        );
        assert!(!error.to_string().contains("companies"));
        Ok(())
    }

    #[test]
    fn budget_date_conversion_respects_leap_years_and_rejects_partial_windows() -> TestResult {
        for (from, to, valid) in [
            ("2024-02-01", "2024-02-29", true),
            ("2023-02-01", "2023-02-28", true),
            ("2024-02-01", "2024-02-28", false),
            ("2023-02-01", "2023-02-29", false),
            ("2024-02-02", "2024-02-29", false),
            ("2024-01-01", "2024-12-31", true),
            ("2024-01-01", "2025-12-31", false),
        ] {
            let q = ReportQuery {
                filters: ViewQuery {
                    from: Some(from.into()),
                    to: Some(to.into()),
                    ..Default::default()
                },
                ..Default::default()
            };
            assert_eq!(q.command(Kind::Budget).is_ok(), valid, "{from}..{to}");
        }
        let command = ReportQuery {
            filters: ViewQuery {
                from: Some("2024-02-01".into()),
                to: Some("2024-03-31".into()),
                ..Default::default()
            },
            ..Default::default()
        }
        .command(Kind::Budget)?;
        assert!(matches!(
            command,
            ReportCommand::BudgetVariance {
                year: 2024,
                from: Some(2),
                to: Some(3),
                include_unbudgeted: true,
                ..
            }
        ));
        Ok(())
    }

    #[test]
    fn nested_report_tables_preserve_optional_columns_and_escape_all_text() {
        let value = json!([{"name":"<script> & \"payee\"", "amount":123}, {"name":"second", "later_field":"<private>"}]);
        let mut html = String::new();
        value_html(&value, &mut html);
        assert!(html.contains("later field"));
        assert!(html.contains("&lt;private&gt;"));
        assert!(html.contains("&lt;script&gt; &amp; &quot;payee&quot;"));
        assert!(!html.contains("<script>"));
        assert!(html.contains("scope=\"col\""));
        assert!(html.contains("tabindex=\"0\""));
    }

    #[test]
    fn tax_links_encode_and_preserve_statement_and_report_filters() {
        let q = ReportQuery {
            filters: ViewQuery {
                company: Some("a&b".into()),
                from: Some("2026-01-01".into()),
                to: Some("2026-03-31".into()),
                currency: Some("USD".into()),
                account: Some("1000".into()),
                ..Default::default()
            },
            statement: Some("S1 & \"special\"".into()),
            statement_balance: Some("-10.25".into()),
            mode: Some("settlement".into()),
            link_key: Some("my_key".into()),
            threshold: Some("0".into()),
            ..Default::default()
        };
        let link = q.url("/reports/1099", "a&b", "csv");
        for expected in [
            "company=a%26b",
            "&amp;from=2026-01-01",
            "&amp;to=2026-03-31",
            "&amp;currency=USD",
            "&amp;threshold=0",
            "&amp;statement=S1%20%26%20%22special%22",
            "&amp;statement_balance=-10.25",
            "&amp;mode=settlement",
            "&amp;link_key=my_key",
            "&amp;format=csv",
        ] {
            assert!(link.contains(expected), "{expected}: {link}");
        }
        assert!(!link.contains("\"special\""));
    }
}
