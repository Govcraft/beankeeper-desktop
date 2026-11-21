//! Read-only Desktop reports over the ledger's typed computation APIs.

use beankeeper::types::Currency;
use beankeeper_cli::{
    cli::ReconciliationMode,
    commands::{
        report::DateRange, report_currency::resolve_report_currency, report_periods::PeriodArg,
        report_reconciliation::ReconciliationReport, report_trend::TrendMetric,
    },
    db::{self, BudgetVarianceReport, Db},
    error::CliError,
    output::statements::{BalanceSheet, IncomeStatement},
};
use chrono::{Datelike, Local, NaiveDate};
use clap::ValueEnum;
use serde::{Deserialize, Serialize};

use crate::{
    links::{
        LinkError, ReportKind as Kind,
        values::{BudgetWindow, Date, DecimalAmount, Threshold, Year},
    },
    markup::{Attribute, Markup, markup, write_markup},
    views::{
        ViewError, ViewPage, append_url_param, encode,
        filters::{Comparison, DateBounds, Filters, ViewQuery, nonempty, parse_optional},
        hidden, input,
    },
};

mod flow;
mod reconciliation;
mod statements;
mod tax;
use flow::FlowReport;
use tax::{TaxData, TaxKind};

#[derive(Serialize)]
#[serde(untagged)]
enum ReportData {
    Tax(TaxData),
    Income(Box<IncomeStatement>),
    Balance(Box<BalanceSheet>),
    Budget(BudgetVarianceReport),
    Flow(FlowReport),
    Reconciliation(Box<ReconciliationReport>),
}

impl ReportData {
    fn read(db: &Db, company: &str, query: &ReportFilters) -> Result<Self, ViewError> {
        let kind = query
            .kind
            .ok_or(ViewError::Invalid("Choose a report first."))?;
        match kind {
            Kind::Income => statements::read_income(db, company, &query.filters)
                .map(Box::new)
                .map(Self::Income),
            Kind::Balance => statements::read_balance(db, company, &query.filters)
                .map(Box::new)
                .map(Self::Balance),
            Kind::Budget => {
                statements::read_budget(db, company, &query.filters, query.budget).map(Self::Budget)
            }
            Kind::Schedule | Kind::Vendor => {
                let tax_kind = if kind == Kind::Schedule {
                    TaxKind::Schedule
                } else {
                    TaxKind::Vendor
                };
                TaxData::read(
                    db,
                    company,
                    &query.filters,
                    query.threshold.as_ref(),
                    tax_kind,
                )
                .map(Self::Tax)
            }
            Kind::Cash => FlowReport::cash(db, company, &query.filters).map(Self::Flow),
            Kind::Trend => FlowReport::trend(
                db,
                company,
                &query.filters,
                query.metric.unwrap_or(TrendMetric::Spending),
                query.period.unwrap_or(PeriodArg::Month),
            )
            .map(Self::Flow),
            Kind::Reconciliation => reconciliation::read(db, company, query)
                .map(Box::new)
                .map(Self::Reconciliation),
        }
    }

    fn currency(&self) -> Currency {
        match self {
            Self::Income(data) => data.currency(),
            Self::Balance(data) => data.currency(),
            Self::Budget(data) => data.currency(),
            Self::Tax(data) => data.currency(),
            Self::Flow(data) => data.currency(),
            Self::Reconciliation(data) => data.currency,
        }
    }

    fn render(&self, html: &mut Markup) {
        match self {
            Self::Tax(data) => data.render(html),
            Self::Income(data) => statements::render_income(data, html),
            Self::Balance(data) => statements::render_balance(data, html),
            Self::Budget(data) => statements::render_budget(data, html),
            Self::Flow(data) => data.render(html),
            Self::Reconciliation(data) => reconciliation::render(data, html),
        }
    }

    fn csv(&self) -> Result<String, ViewError> {
        match self {
            Self::Tax(data) => data.csv(),
            _ => Err(ViewError::Invalid(Format::ERROR)),
        }
    }
}

fn resolve_currency(db: &Db, company: &str, filters: &Filters) -> Result<Currency, ViewError> {
    filters.currency.as_ref().map_or_else(
        || resolve_report_currency(db.conn(), company, None).map_err(report_error),
        |currency| Ok(currency.ledger()),
    )
}

fn report_end(filters: &Filters) -> NaiveDate {
    filters.dates.to().map_or_else(
        || Local::now().date_naive(),
        crate::links::values::Date::date,
    )
}

fn report_dates(filters: &Filters) -> Result<DateRange, ViewError> {
    let to = report_end(filters);
    let from = filters
        .dates
        .from()
        .map(crate::links::values::Date::date)
        .or_else(|| to.with_ordinal(1))
        .ok_or(ViewError::Invalid("Date is outside the supported range."))?;
    DateRange::new(from, to).map_err(report_error)
}

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
            budget: self.comparison_budget,
            comparison_budget: None,
            ..self.clone()
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
    let data = ReportData::read(db, company, query)?;
    let comparison = query
        .comparison()
        .map(|query| {
            let data = ReportData::read(db, company, &query)?;
            Ok::<_, ViewError>((query, data))
        })
        .transpose()?;

    if format != Format::Html {
        return report_download(kind, path, format, &data, comparison.as_ref());
    }
    // Use upstream's resolved dates in the income form and download link rather
    // than resolving the local clock a second time or leaving defaults hidden.
    let mut query = query.clone();
    if let ReportData::Income(data) = &data {
        query.filters.dates = statements::income_dates(data)?;
    }
    let mut body = form(db, &companies, company, path, Some(kind), &query)?;
    body.append(&html);
    let mut html = body;
    let currency = data.currency();
    write_markup!(html, "<p>Report currency: {}.</p>", currency.code());
    if let ReportData::Income(data) = &data {
        html.append(&statements::income_context(db, company, data)?);
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
    render_results(kind, &data, comparison.as_ref(), &mut html);
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

#[derive(Serialize)]
struct ComparisonDownload<'a> {
    current: &'a ReportData,
    comparison: &'a ReportData,
    compare_from: Option<Date>,
    compare_to: Option<Date>,
}

fn report_download(
    kind: Kind,
    path: &str,
    format: Format,
    data: &ReportData,
    comparison: Option<&(ReportFilters, ReportData)>,
) -> Result<ReportResponse, ViewError> {
    if format == Format::Csv && comparison.is_some() {
        return Err(ViewError::Invalid(
            "CSV covers one period. Remove comparison bounds or download JSON for both periods.",
        ));
    }
    let (body, content_type) = if format == Format::Csv {
        (data.csv()?, "text/csv; charset=utf-8")
    } else {
        let payload = match comparison {
            Some((query, comparison)) => serde_json::to_string(&ComparisonDownload {
                current: data,
                comparison,
                compare_from: query.filters.dates.from(),
                compare_to: query.filters.dates.to(),
            }),
            None => serde_json::to_string(data),
        }
        .map_err(|_| ViewError::Invalid("The report could not be encoded."))?;
        (payload, "application/json")
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
    data: &ReportData,
    comparison: Option<&(ReportFilters, ReportData)>,
    html: &mut Markup,
) {
    write_markup!(
        html,
        "</p><section id=\"report-results\" class=\"report-results\"><h2>Report results</h2>"
    );
    data.render(html);
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
        data.render(html);
        write_markup!(html, "</section>");
    }
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

fn table_start(html: &mut Markup, caption: &str, columns: &[&str]) {
    write_markup!(
        html,
        "<div class=\"table-scroll\" tabindex=\"0\" role=\"region\" aria-label=\"{}\"><table><caption>{}</caption><thead><tr>",
        caption,
        caption,
    );
    for header in columns {
        write_markup!(html, "<th scope=\"col\">{}</th>", header);
    }
    write_markup!(html, "</tr></thead><tbody>");
}

#[cfg(test)]
mod tests {
    use super::*;

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
                    .is_ok_and(|q| q.budget.is_some()),
                valid,
                "{from}..{to}"
            );
        }
        let query = ReportQuery {
            filters: ViewQuery {
                from: Some("2024-02-01".into()),
                to: Some("2024-03-31".into()),
                ..Default::default()
            },
            ..Default::default()
        }
        .parse("/reports/budget-variance")?;
        let months = query.budget.ok_or("budget month window")?.months();
        assert_eq!(months.year(), 2024);
        assert_eq!(months.from(), chrono::Month::February);
        assert_eq!(months.to(), chrono::Month::March);
        Ok(())
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
