//! Cash flow and trend views over currency-bearing ledger results.

use beankeeper::types::Currency;
use beankeeper_cli::{
    commands::{
        report_cash_flow::{CashFlowStatement, Section, compute_cash_flow},
        report_periods::PeriodArg,
        report_trend::{TrendMetric, TrendReport, TrendRequest, compute_trend},
    },
    db::{Db, companies::CompanySlug},
};
use serde::Serialize;

use crate::{
    links::values::AccountSelection,
    markup::{Markup, write_markup},
    money,
    views::{ViewError, filters::Filters},
};

use super::{report_dates, report_error, resolve_currency, table_start};

#[derive(Serialize)]
#[serde(untagged)]
pub(super) enum FlowReport {
    Cash(Box<CashFlowStatement>),
    Trend(TrendReport),
}

impl FlowReport {
    pub(super) fn cash(
        db: &Db,
        company: &CompanySlug,
        filters: &Filters,
    ) -> Result<Self, ViewError> {
        let dates = report_dates(filters)?;
        let currency = resolve_currency(db, company, filters)?;
        let accounts = filters
            .cash_account
            .as_ref()
            .map_or_else(Vec::new, AccountSelection::ledger_codes);
        compute_cash_flow(db, company.as_str(), dates, currency, &accounts)
            .map(Box::new)
            .map(Self::Cash)
            .map_err(report_error)
    }

    pub(super) fn trend(
        db: &Db,
        company: &CompanySlug,
        filters: &Filters,
        metric: TrendMetric,
        period: PeriodArg,
    ) -> Result<Self, ViewError> {
        let dates = report_dates(filters)?;
        let currency = resolve_currency(db, company, filters)?;
        compute_trend(
            db,
            company.as_str(),
            TrendRequest {
                dates,
                currency,
                metric,
                period,
            },
        )
        .map(Self::Trend)
        .map_err(report_error)
    }

    pub(super) fn currency(&self) -> Currency {
        match self {
            Self::Cash(report) => report.currency(),
            Self::Trend(report) => report.currency(),
        }
    }

    pub(super) fn render(&self, html: &mut Markup) {
        match self {
            Self::Cash(report) => cash_html(report, html),
            Self::Trend(report) => trend_html(report, html),
        }
    }
}

fn cash_html(report: &CashFlowStatement, html: &mut Markup) {
    let dates = report.dates();
    write_markup!(html, "<p>{} through {}.</p>", dates.from(), dates.to());
    if !report.cash_accounts().is_empty() {
        write_markup!(html, "<h3>Cash accounts</h3><ul>");
        for account in report.cash_accounts() {
            write_markup!(html, "<li>{} {}</li>", &account.code, &account.name);
        }
        write_markup!(html, "</ul>");
    }
    for (kind, title) in [
        (Section::Operating, "Operating activities"),
        (Section::Investing, "Investing activities"),
        (Section::Financing, "Financing activities"),
    ] {
        let section = report.section(kind);
        table_start(
            html,
            title,
            &["Code", "Account", "Account type", "Cash movement"],
        );
        for row in section.lines() {
            write_markup!(
                html,
                "<tr><td>{}</td><th scope=\"row\">{}</th><td>{}</td><td class=\"amount\">{}</td></tr>",
                row.code(),
                row.name(),
                row.account_type().code(),
                money::ledger(row.amount()),
            );
        }
        write_markup!(
            html,
            "</tbody><tfoot><tr><th scope=\"row\" colspan=\"3\">Total</th><td class=\"amount\">{}</td></tr></tfoot></table></div>",
            money::ledger(section.total()),
        );
    }
    write_markup!(html, "<dl>");
    for (label, amount) in [
        ("Opening cash", report.opening_cash()),
        ("Net change", report.net_change()),
        ("Closing cash", report.closing_cash()),
        ("Reconciliation difference", report.difference()),
    ] {
        write_markup!(
            html,
            "<dt>{}</dt><dd class=\"amount\">{}</dd>",
            label,
            money::ledger(amount)
        );
    }
    let status = if report.reconciled() {
        "Reconciled"
    } else {
        "Difference remains"
    };
    write_markup!(
        html,
        "<dt>Cash reconciliation</dt><dd data-reconciled=\"{}\">{}</dd></dl>",
        report.reconciled(),
        status,
    );
}

fn trend_html(report: &TrendReport, html: &mut Markup) {
    let title = match report.metric() {
        TrendMetric::Spending => "Spending",
        TrendMetric::NetWorth => "Net worth",
    };
    let dates = report.dates();
    write_markup!(
        html,
        "<p>{} by {} from {} through {}.</p>",
        title,
        report.period().as_str(),
        dates.from(),
        dates.to(),
    );
    if report.series().is_empty() {
        write_markup!(html, "<p class=\"empty\">No matching activity.</p>");
        return;
    }
    write_markup!(
        html,
        "<div class=\"table-scroll\" tabindex=\"0\" role=\"region\" aria-label=\"{}\"><table><caption>{}</caption><thead><tr><th scope=\"col\">Code</th><th scope=\"col\">Series</th>",
        title,
        title,
    );
    for column in report.columns() {
        write_markup!(
            html,
            "<th scope=\"col\">{} <small>({} through {})</small></th>",
            &column.label,
            column.from,
            column.to,
        );
    }
    write_markup!(html, "</tr></thead><tbody>");
    for series in report.series() {
        write_markup!(
            html,
            "<tr><td>{}</td><th scope=\"row\">{}</th>",
            series.code().unwrap_or(""),
            series.name(),
        );
        for amount in series.amounts() {
            write_markup!(html, "<td class=\"amount\">{}</td>", money::ledger(*amount));
        }
        write_markup!(html, "</tr>");
    }
    write_markup!(html, "</tbody></table></div>");
}
