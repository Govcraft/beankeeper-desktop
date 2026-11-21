//! Stable Desktop report views backed by the upstream CLI report contract.

use std::collections::BTreeSet;

use beankeeper::types::AccountType;
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

use crate::{
    ledger_rows,
    links::{
        LinkError, ReportKind as Kind,
        values::{BudgetWindow, Date, DecimalAmount, Threshold, Year},
    },
    markup::{Attribute, Markup, markup, write_markup},
    money,
    views::{
        ViewError, ViewPage, append_url_param, encode,
        filters::{Comparison, DateBounds, Filters, ViewQuery, nonempty, parse_optional},
        hidden, input,
    },
};

impl Kind {
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

#[derive(Clone, Copy, Default, PartialEq, Eq, ValueEnum)]
enum Format {
    #[default]
    Html,
    Json,
    Csv,
}

impl Format {
    const ERROR: &'static str = "Choose HTML or JSON, or CSV for a tax summary.";

    const fn name(self) -> &'static str {
        match self {
            Self::Html => "html",
            Self::Json => "json",
            Self::Csv => "csv",
        }
    }
}

#[derive(Clone)]
pub(crate) struct ReportFilters {
    filters: Filters,
    kind: Option<Kind>,
    metric: Option<TrendMetric>,
    period: Option<PeriodArg>,
    year: Option<Year>,
    budget: Option<BudgetWindow>,
    comparison_budget: Option<BudgetWindow>,
    threshold: Option<Threshold>,
    mode: Option<ReconciliationMode>,
    statement: Option<String>,
    statement_balance: Option<DecimalAmount>,
    link_key: Option<String>,
    format: Format,
}

fn option<T: ValueEnum>(value: Option<String>) -> Result<Option<T>, ViewError> {
    nonempty(value)
        .map(|value| T::from_str(&value, false).map_err(ViewError::ReportOptions))
        .transpose()
}

fn option_name<T: ValueEnum>(option: &T) -> Option<String> {
    option
        .to_possible_value()
        .map(|value| value.get_name().to_owned())
}

impl ReportQuery {
    pub(crate) fn parse(self, path: &str) -> Result<ReportFilters, ViewError> {
        let kind = if path == "/reports" {
            None
        } else {
            Some(Kind::parse(path).ok_or(ViewError::NotFound)?)
        };
        let filters = self.filters.parse()?;
        if filters.category.is_some() || filters.payee.is_some() {
            return Err(ViewError::Invalid(
                "Category and payee filters require the transactions view.",
            ));
        }
        match (kind, filters.comparison) {
            (None | Some(Kind::Reconciliation), Some(_)) => {
                return Err(ViewError::Invalid(
                    "Comparison date ranges require a period report or transactions.",
                ));
            }
            (Some(Kind::Balance), Some(Comparison::Period(_))) => {
                return Err(ViewError::Invalid(
                    "Balance-sheet comparison uses compare_to only.",
                ));
            }
            (Some(Kind::Balance), Some(Comparison::AsOf(_))) => {}
            (_, Some(Comparison::AsOf(_))) => {
                return Err(ViewError::Invalid(
                    "Period comparison requires compare_from and compare_to.",
                ));
            }
            _ => {}
        }
        if matches!(kind, Some(Kind::Balance | Kind::Reconciliation))
            && filters.dates.from().is_some()
        {
            return Err(ViewError::Invalid(
                "This is an as-of report. Use the end date; a start date is not supported.",
            ));
        }
        let format = option(self.format)
            .map_err(|_| ViewError::Invalid(Format::ERROR))?
            .unwrap_or_default();
        if format == Format::Csv && kind.is_some_and(|kind| !kind.tax()) {
            return Err(ViewError::Invalid(Format::ERROR));
        }
        let year = parse_optional(self.year)?;
        let (budget, comparison_budget) = if kind == Some(Kind::Budget) {
            let new_window = |bounds: DateBounds, year: Option<&Year>| {
                BudgetWindow::new(bounds.from(), bounds.to(), year)
                    .map_err(|LinkError(message)| ViewError::Invalid(message))
            };
            (
                new_window(filters.dates, year.as_ref())?,
                filters
                    .comparison
                    .map(|comparison| new_window(comparison.bounds(), None))
                    .transpose()?
                    .flatten(),
            )
        } else {
            (None, None)
        };
        Ok(ReportFilters {
            kind,
            filters,
            metric: option(self.metric)?,
            period: option(self.period)?,
            year,
            budget,
            comparison_budget,
            threshold: parse_optional(self.threshold)?,
            mode: option(self.mode)?,
            statement: nonempty(self.statement),
            statement_balance: parse_optional(self.statement_balance)?,
            link_key: nonempty(self.link_key),
            format,
        })
    }
}

impl ReportFilters {
    fn comparison(&self) -> Option<Self> {
        Some(Self {
            filters: self.filters.comparison_filters()?,
            year: None,
            budget: self.comparison_budget.clone(),
            comparison_budget: None,
            ..self.clone()
        })
    }

    fn command(&self) -> Result<ReportCommand, ViewError> {
        let kind = self
            .kind
            .ok_or(ViewError::Invalid("Choose a report first."))?;
        let q = &self.filters;
        let from = q.dates.from().map(|date| date.to_string());
        let to = q.dates.to().map(|date| date.to_string());
        let currency = q.currency.as_ref().map(ToString::to_string);
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
                cash_account: self
                    .filters
                    .cash_account
                    .as_ref()
                    .map_or_else(Vec::new, crate::links::values::AccountSelection::codes),
            }),
            Kind::Trend => ReportCommand::Trend(TrendArgs {
                from,
                to,
                currency,
                metric: self.metric.unwrap_or(TrendMetric::Spending),
                period: self.period.unwrap_or(PeriodArg::Month),
            }),
            Kind::Budget => {
                let window = self
                    .budget
                    .as_ref()
                    .ok_or(ViewError::Invalid(BudgetWindow::ERROR))?;
                ReportCommand::BudgetVariance {
                    year: window.year(),
                    month: None,
                    from: window.first_month(),
                    to: window.last_month(),
                    currency,
                    account_type: None,
                    include_unbudgeted: true,
                }
            }
            Kind::Schedule => ReportCommand::ScheduleC(tax()),
            Kind::Vendor => ReportCommand::Vendor1099(VendorArgs {
                period: tax(),
                threshold: self
                    .threshold
                    .as_ref()
                    .map_or_else(|| "600".into(), ToString::to_string),
            }),
            Kind::Reconciliation => ReportCommand::Reconciliation {
                account: q
                    .account
                    .clone()
                    .ok_or(ViewError::Invalid("Choose an account to reconcile."))?,
                to,
                currency,
                mode: self.mode.unwrap_or_default(),
                statement: self.statement.clone(),
                statement_balance: self.statement_balance.as_ref().map(ToString::to_string),
                link_key: self.link_key.clone(),
                filter_metadata: Vec::new(),
                group_by_metadata: None,
            },
        })
    }

    fn url(&self, path: &str, company: &str, format: Format) -> String {
        let mut url = format!("{path}?company={}", encode(company));
        append_url_param(&mut url, "account", self.filters.account.as_deref());
        if let Some(comparison) = self.filters.comparison {
            append_url_param(&mut url, "compare_from", comparison.bounds().from());
            append_url_param(&mut url, "compare_to", comparison.bounds().to());
        }
        append_url_param(&mut url, "from", self.filters.dates.from());
        append_url_param(&mut url, "to", self.filters.dates.to());
        append_url_param(&mut url, "currency", self.filters.currency.as_ref());
        append_url_param(
            &mut url,
            "metric",
            self.metric.as_ref().and_then(option_name),
        );
        append_url_param(
            &mut url,
            "period",
            self.period.as_ref().and_then(option_name),
        );
        append_url_param(&mut url, "year", self.year.as_ref());
        append_url_param(&mut url, "threshold", self.threshold.as_ref());
        append_url_param(&mut url, "mode", self.mode.as_ref().and_then(option_name));
        append_url_param(&mut url, "statement", self.statement.as_deref());
        append_url_param(
            &mut url,
            "statement_balance",
            self.statement_balance.as_ref(),
        );
        append_url_param(&mut url, "link_key", self.link_key.as_deref());
        append_url_param(&mut url, "cash_account", self.filters.cash_account.as_ref());
        append_url_param(&mut url, "format", Some(format.name()));
        url
    }
}

pub(crate) fn render(
    db: &Db,
    path: &str,
    query: &ReportFilters,
) -> Result<ReportResponse, ViewError> {
    let kind = query.kind;
    let companies = db::list_companies(db.conn())?;
    let company = query
        .filters
        .company
        .as_deref()
        .or_else(|| companies.first().map(|c| c.slug.as_str()));
    let title = kind.map_or("Reports", Kind::title);
    let nav = if kind == Some(Kind::Reconciliation) {
        "/reconciliation"
    } else {
        "/reports"
    };
    let page = |content| {
        ReportResponse::Page(ViewPage {
            title,
            description: "Read-only reports from your books. Monetary amounts are shown in report currency.",
            content,
            nav,
        })
    };
    let Some(company) = company else {
        return Ok(page(markup!(
            "<p class=\"empty\">No companies yet. Ask your agent to initialize your books.</p>"
        )));
    };
    if !companies.iter().any(|c| c.slug == company) {
        return Err(ViewError::NotFound);
    }
    let mut html = Markup::default();
    let Some(kind) = kind else {
        html = form(db, &companies, company, path, None, query)?;
        report_catalog(company, &mut html);
        return Ok(page(html));
    };
    append_report_notices(&mut html, kind);
    let format = query.format;
    if format == Format::Html
        && ((kind == Kind::Reconciliation && query.filters.account.is_none())
            || (kind == Kind::Budget && query.budget.is_none()))
    {
        let mut body = form(db, &companies, company, path, Some(kind), query)?;
        body.append(&html);
        html = body;
        write_markup!(
            html,
            "<p class=\"empty\">Choose the report filters above, then apply to see results.</p>"
        );
        return Ok(page(html));
    }
    let ReportData { data, comparison } = read_report(db, company, query)?;

    if format != Format::Html {
        return report_download(kind, path, format, &data, comparison.as_ref());
    }
    // Use upstream's resolved dates in the income form and download link rather
    // than resolving the local clock a second time or leaving defaults hidden.
    let mut query = query.clone();
    if kind == Kind::Income {
        query.filters.dates = DateBounds::new(
            Some(report_date(&data, "from")?),
            Some(report_date(&data, "to")?),
        )?;
    }
    let mut body = form(db, &companies, company, path, Some(kind), &query)?;
    body.append(&html);
    let mut html = body;
    let currency = query.filters.currency.as_ref().map_or_else(
        || resolve_report_currency(db.conn(), company, None).map_err(report_error),
        |currency| Ok(currency.ledger()),
    )?;
    write_markup!(html, "<p>Report currency: {}.</p>", currency.code());
    if kind == Kind::Income {
        html.append(&income_context(
            db,
            company,
            &data,
            currency.code(),
            query.filters.dates,
        )?);
    }
    write_markup!(
        html,
        "<p><a download href=\"{}\">Download JSON</a>",
        query.url(path, company, Format::Json)
    );
    if kind.tax() && comparison.is_none() {
        write_markup!(
            html,
            " · <a download href=\"{}\">Download CSV</a>",
            query.url(path, company, Format::Csv)
        );
    }
    render_results(kind, &data, comparison.as_ref(), currency.code(), &mut html);
    Ok(page(html))
}

fn append_report_notices(html: &mut Markup, kind: Kind) {
    if kind.tax() {
        write_markup!(
            html,
            "<p class=\"note\">Bookkeeping summary only. Not tax advice; not a filed form.</p>"
        );
    }
    if kind == Kind::Reconciliation {
        write_markup!(
            html,
            "<p>Read-only clearance and settlement state. No clear, reconcile or approval actions are available here. Statement balance is in major units.</p>"
        );
    }
}

fn report_catalog(company: &str, html: &mut Markup) {
    write_markup!(html, "<section><h2>Choose a report</h2><ul>");
    for report in Kind::ALL {
        let path = report.path();
        let title = report.title();
        write_markup!(
            html,
            "<li><a href=\"{}?company={}\">{}</a></li>",
            path,
            encode(company),
            title
        );
    }
    write_markup!(html, "</ul></section>");
}

struct ReportData {
    data: Value,
    comparison: Option<(ReportFilters, Value)>,
}

fn read_report(db: &Db, company: &str, query: &ReportFilters) -> Result<ReportData, ViewError> {
    let command = query.command()?;
    // This context supplies JSON metadata only. execute_json uses the supplied
    // read-only Db and never opens the CLI's default database or writes stdout.
    let cli = Cli::try_parse_from(["bk", "--quiet", "report", "income-statement"])
        .map_err(|_| ViewError::Invalid("The report context could not be initialized."))?;
    let data = execute_json(&cli, db, company, &command).map_err(report_error)?;
    let comparison = query
        .comparison()
        .map(|query| {
            let command = query.command()?;
            let data = execute_json(&cli, db, company, &command).map_err(report_error)?;
            Ok::<_, ViewError>((query, data))
        })
        .transpose()?;
    Ok(ReportData { data, comparison })
}

fn report_download(
    kind: Kind,
    path: &str,
    format: Format,
    data: &Value,
    comparison: Option<&(ReportFilters, Value)>,
) -> Result<ReportResponse, ViewError> {
    if format == Format::Csv && comparison.is_some() {
        return Err(ViewError::Invalid(
            "CSV covers one period. Remove comparison bounds or download JSON for both periods.",
        ));
    }
    let (body, content_type) = if format == Format::Csv {
        (tax_csv(kind, data), "text/csv; charset=utf-8")
    } else {
        (comparison.map_or_else(|| data.to_string(), |(q, comparison)| serde_json::json!({"current":data, "comparison":comparison, "compare_from":q.filters.dates.from(), "compare_to":q.filters.dates.to()}).to_string()), "application/json")
    };
    let id = if kind == Kind::Reconciliation {
        "reconciliation"
    } else {
        path.trim_start_matches("/reports/")
    };
    Ok(ReportResponse::Download {
        body,
        content_type,
        filename: format!("{id}.{}", format.name()),
    })
}

fn render_results(
    kind: Kind,
    data: &Value,
    comparison: Option<&(ReportFilters, Value)>,
    currency: &str,
    html: &mut Markup,
) {
    write_markup!(
        html,
        "</p><section id=\"report-results\" class=\"report-results\"><h2>Report results</h2>"
    );
    value_html(data, Some(currency), html);
    write_markup!(html, "</section>");
    if let Some((q, data)) = comparison {
        write_markup!(
            html,
            "<section id=\"comparison-results\" class=\"report-results\"><h2>{}</h2>",
            if kind == Kind::Balance {
                format!(
                    "Comparison as of {}",
                    q.filters
                        .dates
                        .to()
                        .map(|date| date.to_string())
                        .unwrap_or_default()
                )
            } else {
                format!(
                    "Comparison period: {} through {}",
                    q.filters
                        .dates
                        .from()
                        .map(|date| date.to_string())
                        .unwrap_or_default(),
                    q.filters
                        .dates
                        .to()
                        .map(|date| date.to_string())
                        .unwrap_or_default()
                )
            }
        );
        value_html(data, Some(currency), html);
        write_markup!(html, "</section>");
    }
}

fn report_date(data: &Value, key: &str) -> Result<Date, ViewError> {
    data[key]
        .as_str()
        .and_then(|date| date.parse().ok())
        .ok_or(ViewError::Invalid(
            "The income statement did not include a reporting period.",
        ))
}

fn income_context(
    db: &Db,
    company: &str,
    data: &Value,
    currency: &str,
    dates: DateBounds,
) -> Result<Markup, ViewError> {
    let (Some(from), Some(to)) = (dates.from(), dates.to()) else {
        return Err(ViewError::Invalid(
            "The income statement did not include a reporting period.",
        ));
    };
    let mut html = markup!(
        "<p>Reporting period: {} through {} (inclusive).</p>",
        from,
        to
    );
    if data["total_revenue"].as_i64() != Some(0) || data["total_expenses"].as_i64() != Some(0) {
        return Ok(html);
    }
    // Zero net balances can still contain offsetting postings. Check gross
    // activity with upstream's query API before calling the period empty.
    let from = from.to_string();
    let to = to.to_string();
    for account_type in [AccountType::Revenue, AccountType::Expense] {
        let account_type = ledger_rows::account_kind_name(account_type);
        let balances = db::compute_trial_balance(
            db.conn(),
            company,
            Some(account_type.as_ref()),
            Some(&from),
            Some(&to),
            Some(currency),
        )?;
        if balances
            .iter()
            .any(|row| row.debit_total != 0 || row.credit_total != 0)
        {
            return Ok(html);
        }
    }
    write_markup!(
        html,
        "<p class=\"empty\">No revenue or expense activity in this range for {}. Choose different dates above to see another period, or <a href=\"/transactions?company={}&amp;currency={}\">view company transactions across all dates</a>.</p>",
        currency,
        encode(company),
        encode(currency)
    );
    Ok(html)
}

fn report_error(error: CliError) -> ViewError {
    match error {
        CliError::Validation(message) | CliError::Usage(message) => {
            ViewError::ReportOptions(message)
        }
        error => ViewError::Ledger(error),
    }
}

fn select<T: ValueEnum>(html: &mut Markup, name: &str, label: &str, value: &T) {
    write_markup!(html, "<label>{}<select name=\"{}\">", label, name);
    let selected = value.to_possible_value();
    for choice in T::value_variants()
        .iter()
        .filter_map(ValueEnum::to_possible_value)
    {
        let choice = choice.get_name();
        let active = selected
            .as_ref()
            .is_some_and(|value| value.get_name() == choice);
        write_markup!(
            html,
            "<option{}>{}</option>",
            Attribute::Selected(active),
            choice
        );
    }
    write_markup!(html, "</select></label>");
}

fn form(
    db: &Db,
    companies: &[db::CompanyRow],
    company: &str,
    path: &str,
    kind: Option<Kind>,
    q: &ReportFilters,
) -> Result<Markup, ViewError> {
    let mut html = markup!(
        "<form id=\"report-filters\" method=\"get\" action=\"{}\" class=\"filters\"><label>Company<select name=\"company\">",
        path
    );
    for c in companies {
        write_markup!(
            html,
            "<option value=\"{}\"{}>{} ({})</option>",
            &c.slug,
            Attribute::Selected(c.slug == company),
            &c.name,
            &c.slug
        );
    }
    write_markup!(html, "</select></label>");
    if let Some(comparison) = q.filters.comparison {
        hidden(&mut html, "compare_from", comparison.bounds().from());
        hidden(&mut html, "compare_to", comparison.bounds().to());
    }
    if let Some(kind) = kind {
        if !matches!(kind, Kind::Balance | Kind::Reconciliation) {
            html.append(&input(
                "from",
                "From date (inclusive)",
                "date",
                q.filters.dates.from(),
            ));
        }
        html.append(&input(
            "to",
            "Through date (inclusive)",
            "date",
            q.filters.dates.to(),
        ));
        html.append(&input(
            "currency",
            "Currency (required for multi-currency books)",
            "text",
            q.filters.currency.as_ref(),
        ));
        match kind {
            Kind::Trend => {
                select(
                    &mut html,
                    "metric",
                    "Metric",
                    &q.metric.unwrap_or(TrendMetric::Spending),
                );
                select(
                    &mut html,
                    "period",
                    "Calendar period",
                    &q.period.unwrap_or(PeriodArg::Month),
                );
            }
            Kind::Budget => {
                html.append(&input(
                    "year",
                    "Budget year (or use a whole-month date range)",
                    "number",
                    q.year.as_ref(),
                ));
            }
            Kind::Vendor => html.append(&input(
                "threshold",
                "Minimum payee total (major units; default 600)",
                "text",
                q.threshold.as_ref(),
            )),
            Kind::Cash => html.append(&input(
                "cash_account",
                "Cash accounts (optional comma-separated codes; inferred by name)",
                "text",
                q.filters.cash_account.as_ref(),
            )),
            Kind::Reconciliation => reconciliation_fields(db, company, q, &mut html)?,
            _ => {}
        }
    }
    write_markup!(html, "<button type=\"submit\">Apply</button></form>");
    if kind == Some(Kind::Budget) {
        write_markup!(
            html,
            "<p class=\"note\">Budget variance covers whole calendar months within one year. Use both date bounds, starting on the first day and ending on the last day of a month, or enter a year for the full year. Includes accounts without a budget.</p>"
        );
    }
    Ok(html)
}

fn reconciliation_fields(
    db: &Db,
    company: &str,
    q: &ReportFilters,
    html: &mut Markup,
) -> Result<(), ViewError> {
    write_markup!(
        html,
        "<label>Account<select name=\"account\"><option value=\"\">Choose account</option>"
    );
    for account in db::list_accounts(
        db.conn(),
        &db::ListAccountParams {
            company_slug: company,
            type_filter: None,
            name_filter: None,
        },
    )? {
        write_markup!(
            html,
            "<option value=\"{}\"{}>{} · {}</option>",
            &account.code,
            Attribute::Selected(q.filters.account.as_deref() == Some(account.code.as_str())),
            &account.code,
            &account.name
        );
    }
    write_markup!(html, "</select></label>");
    select(html, "mode", "Mode", &q.mode.unwrap_or_default());
    html.append(&input(
        "statement",
        "Statement ID (required for settlement)",
        "text",
        q.statement.as_deref(),
    ));
    html.append(&input(
        "statement_balance",
        "Statement balance (major units)",
        "text",
        q.statement_balance.as_ref(),
    ));
    html.append(&input(
        "link_key",
        "Settlement link key (default applies_to)",
        "text",
        q.link_key.as_deref(),
    ));
    Ok(())
}

fn text(value: &Value) -> String {
    match value {
        Value::Null => String::new(),
        Value::String(s) => s.clone(),
        _ => value.to_string(),
    }
}

fn label(key: &str) -> String {
    key.replace('_', " ")
}

fn currency<'a>(value: &'a Value, inherited: Option<&'a str>) -> Option<&'a str> {
    match value.get("currency") {
        Some(code) => code.as_str(),
        None => inherited,
    }
}

fn visible_field(map: &serde_json::Map<String, Value>, key: &str) -> bool {
    !key.strip_suffix("_display")
        .is_some_and(|base| map.contains_key(base))
}

// Upstream identifies monetary fields with display companions. Counts, IDs,
// percentages and user metadata must never be scaled just because they are numbers.
fn field_html(row: &Value, key: &str, inherited: Option<&str>, html: &mut Markup) {
    let code = currency(row, inherited);
    if let Some(display) = row.get(format!("{key}_display")) {
        display_html(&row[key], display, code, html);
    } else {
        value_html(&row[key], code, html);
    }
}

fn display_html(raw: &Value, display: &Value, code: Option<&str>, html: &mut Markup) {
    if let (Some(value), Some(code)) = (display.as_str(), code)
        && let Some(formatted) = money::decimal(value, code)
    {
        write_markup!(html, "<span class=\"amount\">{}</span>", &formatted);
    } else if let (Some(raw), Some(displays)) = (raw.as_array(), display.as_array())
        && raw.len() == displays.len()
    {
        write_markup!(html, "<ol>");
        for (raw, display) in raw.iter().zip(displays) {
            write_markup!(html, "<li>");
            display_html(raw, display, code, html);
            write_markup!(html, "</li>");
        }
        write_markup!(html, "</ol>");
    } else {
        value_html(raw, code, html);
        if !raw.is_null() {
            write_markup!(html, " (minor units; currency format unavailable)");
        }
    }
}

// Preserve nested report details while presenting each monetary field once,
// using its upstream decimal companion as the primary human-readable value.
fn value_html(value: &Value, inherited: Option<&str>, html: &mut Markup) {
    let code = currency(value, inherited);
    match value {
        Value::Object(map) => {
            write_markup!(html, "<dl>");
            for key in map.keys().filter(|key| visible_field(map, key)) {
                write_markup!(html, "<dt>{}</dt><dd>", label(key));
                field_html(value, key, code, html);
                write_markup!(html, "</dd>");
            }
            write_markup!(html, "</dl>");
        }
        Value::Array(rows) if rows.is_empty() => {
            write_markup!(html, "<p class=\"empty\">No matching rows.</p>");
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
                    .flat_map(|map| map.keys().filter(|key| visible_field(map, key)))
                    .collect();
                write_markup!(
                    html,
                    "<div class=\"table-scroll\" tabindex=\"0\" role=\"region\" aria-label=\"Report table\"><table><caption>Report rows</caption><thead><tr>"
                );
                for key in &columns {
                    write_markup!(html, "<th scope=\"col\">{}</th>", label(key));
                }
                write_markup!(html, "</tr></thead><tbody>");
                for row in rows {
                    write_markup!(html, "<tr>");
                    for key in &columns {
                        write_markup!(html, "<td>");
                        field_html(row, key, code, html);
                        write_markup!(html, "</td>");
                    }
                    write_markup!(html, "</tr>");
                }
                write_markup!(html, "</tbody></table></div>");
            } else {
                write_markup!(html, "<ol class=\"report-items\">");
                for row in rows {
                    write_markup!(html, "<li>");
                    value_html(row, code, html);
                    write_markup!(html, "</li>");
                }
                write_markup!(html, "</ol>");
            }
        }
        Value::Number(_) => {
            write_markup!(html, "<span class=\"number\">{}</span>", value);
        }
        Value::Null => write_markup!(html, "Not recorded"),
        _ => html.push_text(&text(value)),
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
            let ReportResponse::Page(page) =
                render(&db, path, &ReportQuery::default().parse(path)?)?
            else {
                return Err("expected HTML".into());
            };
            assert!(page.content.as_str().contains("No companies yet"));
        }
        db.conn().execute("DROP TABLE companies", [])?;
        let error = render(&db, "/reports", &ReportQuery::default().parse("/reports")?)
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
            ("9999-12-01", "9999-12-31", true),
            ("2024-02-01", "２０２４-０２-２９", false),
            ("2024-02-01", "", false),
        ] {
            let q = ReportQuery {
                filters: ViewQuery {
                    from: Some(from.into()),
                    to: Some(to.into()),
                    ..Default::default()
                },
                ..Default::default()
            };
            assert_eq!(
                q.parse("/reports/budget-variance")
                    .and_then(|q| q.command())
                    .is_ok(),
                valid,
                "{from}..{to}"
            );
        }
        let command = ReportQuery {
            filters: ViewQuery {
                from: Some("2024-02-01".into()),
                to: Some("2024-03-31".into()),
                ..Default::default()
            },
            ..Default::default()
        }
        .parse("/reports/budget-variance")?
        .command()?;
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
        let mut html = Markup::default();
        value_html(&value, None, &mut html);
        let html = html.as_str();
        assert!(html.contains("later field"));
        assert!(html.contains("&lt;private&gt;"));
        assert!(html.contains("&lt;script&gt; &amp; &quot;payee&quot;"));
        assert!(!html.contains("<script>"));
        assert!(html.contains("scope=\"col\""));
        assert!(html.contains("tabindex=\"0\""));
    }

    #[test]
    fn report_money_prefers_displays_and_preserves_counts_percentages_and_nulls() {
        let value = json!({
            "currency": "USD", "total": 999, "total_display": "55620.00",
            "payment_count": 12, "change_percent": 2.5,
            "statement_balance": null, "statement_balance_display": null,
            "accounts": [{"amounts": [4_032_470, -5, 0],
                "amounts_display": ["40324.70", "-0.05", "0.00"]}],
            "metadata": {"amount": 777}
        });
        let original = value.clone();
        let mut html = Markup::default();
        value_html(&value, None, &mut html);
        let html = html.as_str();
        for expected in [
            "$55,620.00",
            "$40,324.70",
            "-$0.05",
            "$0.00",
            ">12<",
            ">2.5<",
            "Not recorded",
            ">777<",
        ] {
            assert!(html.contains(expected), "{expected}: {html}");
        }
        for unwanted in [
            ">999<",
            ">4032470<",
            "total display",
            "amounts display",
            "statement balance display",
        ] {
            assert!(!html.contains(unwanted), "{unwanted}: {html}");
        }
        assert_eq!(value, original);
    }

    #[test]
    fn report_rows_override_currency_and_unknown_formats_are_explicit_and_escaped() {
        let value = json!([
            {"name": "<payee>", "amount": 1234, "amount_display": "12.34"},
            {"currency": "JPY", "amount": 1234, "amount_display": "1234"},
            {"currency": "BHD", "amount": -1234, "amount_display": "-1.234"},
            {"currency": "XYZ<script>", "amount": 1234, "amount_display": null},
            {"amount": 5, "amount_display": "<script>"}
        ]);
        let mut html = Markup::default();
        value_html(&value, Some("USD"), &mut html);
        let html = html.as_str();
        for expected in [
            "$12.34",
            "¥1,234",
            "-BHD 1.234",
            "&lt;payee&gt;",
            "XYZ&lt;script&gt;",
            ">1234</span> (minor units; currency format unavailable)",
        ] {
            assert!(html.contains(expected), "{expected}: {html}");
        }
        assert!(!html.contains("<script>"));
        assert!(!html.contains("amount display"));
        assert_eq!(html.matches("scope=\"col\">amount</th>").count(), 1);
    }

    #[test]
    fn tax_links_encode_and_preserve_statement_and_report_filters() -> TestResult {
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
        let url = q
            .parse("/reports/1099")?
            .url("/reports/1099", "a&b", Format::Csv);
        assert!(url.ends_with("&format=csv"));
        let link = markup!("<a href=\"{}\">Download</a>", url);
        let link = link.as_str();
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
        Ok(())
    }
}
