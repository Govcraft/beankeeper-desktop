use beankeeper::types::{AccountType, Money};
use beankeeper_cli::{
    commands::report::{compute_balance_sheet, compute_income_statement},
    db::{
        self, BudgetCoverage, BudgetVarianceParams, BudgetVarianceReport, Db,
        companies::{Company, CompanySlug},
    },
    output::statements::{BalanceSheet, IncomeStatement, StatementSection},
};

use crate::{
    links::values::{BudgetWindow, Date},
    markup::{Markup, markup, write_markup},
    money,
    views::{
        ViewError, encode,
        filters::{DateBounds, Filters},
    },
};

use super::{report_dates, report_end, report_error, resolve_currency, table_start};

pub(super) fn read_income(
    db: &Db,
    company: &CompanySlug,
    filters: &Filters,
) -> Result<IncomeStatement, ViewError> {
    let dates = report_dates(filters)?;
    let currency = resolve_currency(db, company, filters)?;
    compute_income_statement(db, company.as_str(), dates, currency).map_err(report_error)
}

pub(super) fn read_balance(
    db: &Db,
    company: &CompanySlug,
    filters: &Filters,
) -> Result<BalanceSheet, ViewError> {
    let as_of = report_end(filters);
    let currency = resolve_currency(db, company, filters)?;
    compute_balance_sheet(db, company.as_str(), as_of, currency).map_err(report_error)
}

pub(super) fn read_budget(
    db: &Db,
    company: &CompanySlug,
    filters: &Filters,
    budget: Option<BudgetWindow>,
) -> Result<BudgetVarianceReport, ViewError> {
    let months = budget
        .ok_or(ViewError::Invalid(BudgetWindow::ERROR))?
        .months();
    let currency = resolve_currency(db, company, filters)?;
    db::compute_budget_variance(
        db,
        &BudgetVarianceParams {
            company_slug: company,
            currency,
            months,
            account_type: None,
            coverage: BudgetCoverage::IncludeUnbudgeted,
        },
    )
    .map_err(report_error)
}

pub(super) fn income_dates(statement: &IncomeStatement) -> Result<DateBounds, ViewError> {
    let dates = statement.dates();
    let date =
        |date: chrono::NaiveDate| Date::try_from(date).map_err(|error| ViewError::Invalid(error.0));
    DateBounds::new(Some(date(dates.from())?), Some(date(dates.to())?))
}

pub(super) fn income_context(
    db: &Db,
    company: &CompanySlug,
    statement: &IncomeStatement,
) -> Result<Markup, ViewError> {
    let dates = statement.dates();
    let mut html = markup!(
        "<p>Reporting period: {} through {} (inclusive).</p>",
        dates.from(),
        dates.to()
    );
    if [statement.revenue(), statement.expenses()]
        .iter()
        .any(|section| section.total().amount().minor_units() != 0)
    {
        return Ok(html);
    }
    // A zero net balance can contain offsetting postings. Check gross activity
    // with the same company, currency and inclusive dates as the statement.
    let book = Company::bind(db.read(), company.clone())?;
    let currency = statement.currency();
    for account_type in [AccountType::Revenue, AccountType::Expense] {
        let balances = db::compute_trial_balance(
            &book,
            Some(account_type),
            Some(dates.from()),
            Some(dates.to()),
            currency,
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
        currency.code(),
        encode(company.as_str()),
        encode(currency.code())
    );
    Ok(html)
}

pub(super) fn render_income(statement: &IncomeStatement, html: &mut Markup) {
    let dates = statement.dates();
    write_markup!(
        html,
        "<dl><dt>From</dt><dd>{}</dd><dt>To</dt><dd>{}</dd></dl>",
        dates.from(),
        dates.to()
    );
    for section in [statement.revenue(), statement.expenses()] {
        render_section(html, section);
    }
    render_totals(
        html,
        [
            ("Total revenue", statement.revenue().total()),
            ("Total expenses", statement.expenses().total()),
            ("Net income", statement.net_income()),
        ],
    );
}

pub(super) fn render_balance(sheet: &BalanceSheet, html: &mut Markup) {
    write_markup!(
        html,
        "<dl><dt>As of</dt><dd>{}</dd><dt>Balanced</dt><dd>{}</dd></dl>",
        sheet.as_of(),
        sheet.balanced()
    );
    for section in [sheet.assets(), sheet.liabilities(), sheet.equity()] {
        render_section(html, section);
    }
    render_totals(
        html,
        [
            ("Total assets", sheet.assets().total()),
            ("Total liabilities", sheet.liabilities().total()),
            ("Total equity", sheet.equity().total()),
            ("Computed earnings", sheet.computed_earnings()),
            (
                "Total liabilities and equity",
                sheet.total_liabilities_and_equity(),
            ),
            ("Difference", sheet.difference()),
        ],
    );
}

fn render_section(html: &mut Markup, section: &StatementSection) {
    write_markup!(
        html,
        "<h3>{}</h3><dl><dt>Key</dt><dd>{}</dd><dt>Normal balance</dt><dd>{}</dd><dt>Total</dt><dd><span class=\"amount\">{}</span></dd></dl>",
        section.title(),
        section.key(),
        section.normal_balance().code(),
        money::ledger(section.total())
    );
    if section.accounts().is_empty() {
        write_markup!(html, "<p class=\"empty\">No matching rows.</p>");
        return;
    }
    table_start(
        html,
        section.title(),
        &["Code", "Account", "Type", "Amount", "Computed"],
    );
    for line in section.accounts() {
        write_markup!(
            html,
            "<tr><td>{}</td><th scope=\"row\">{}</th><td>{}</td><td class=\"amount\">{}</td><td>{}</td></tr>",
            line.code().unwrap_or("Not recorded"),
            line.name(),
            line.account_type().code(),
            money::ledger(line.amount()),
            line.is_computed()
        );
    }
    write_markup!(html, "</tbody></table></div>");
}

pub(super) fn render_budget(report: &BudgetVarianceReport, html: &mut Markup) {
    let months = report.months();
    write_markup!(
        html,
        "<dl><dt>Year</dt><dd>{}</dd><dt>From month</dt><dd>{}</dd><dt>To month</dt><dd>{}</dd></dl>",
        months.year(),
        months.from().number_from_month(),
        months.to().number_from_month()
    );
    render_budget_rows(report, html);
    render_totals(
        html,
        [
            ("Budget", report.total_budget()),
            ("Actual", report.total_actual()),
        ],
    );
}

fn render_budget_rows(report: &BudgetVarianceReport, html: &mut Markup) {
    if report.rows().is_empty() {
        write_markup!(html, "<p class=\"empty\">No matching rows.</p>");
        return;
    }
    table_start(
        html,
        "Budget variance rows",
        &[
            "Code",
            "Name",
            "Account type",
            "Budget",
            "Actual",
            "Variance",
            "Variance percent",
            "Favorable",
        ],
    );
    for row in report.rows() {
        let percent = row
            .variance_percent
            .map_or_else(|| "Not recorded".to_owned(), |percent| percent.to_fixed(1));
        write_markup!(
            html,
            "<tr><td>{}</td><th scope=\"row\">{}</th><td>{}</td><td class=\"amount\">{}</td><td class=\"amount\">{}</td><td class=\"amount\">{}</td><td>{}</td><td>{}</td></tr>",
            &row.code,
            &row.name,
            row.account_type.code(),
            money::ledger(row.budget_amount),
            money::ledger(row.actual_amount),
            money::ledger(row.variance_amount),
            percent,
            row.favorable
        );
    }
    write_markup!(html, "</tbody></table></div>");
}

fn render_totals<'a>(html: &mut Markup, totals: impl IntoIterator<Item = (&'a str, Money)>) {
    write_markup!(html, "<dl>");
    for (label, money) in totals {
        write_markup!(
            html,
            "<dt>{}</dt><dd><span class=\"amount\">{}</span></dd>",
            label,
            money::ledger(money)
        );
    }
    write_markup!(html, "</dl>");
}
