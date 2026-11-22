//! Tax report computation and rendering over the ledger's typed embedding API.

use beankeeper::types::{Currency, Money};
use beankeeper_cli::{
    commands::report_tax::{
        ScheduleRow, ScheduleSummary, TaxReport, VendorSummary, VendorThreshold, build_schedule_c,
        build_vendors, render_schedule_c_csv, render_vendors_csv,
    },
    db::{Db, companies::CompanySlug},
};
use serde::Serialize;

use crate::{
    links::values::Threshold,
    markup::{Markup, write_markup},
    money,
    views::{ViewError, filters::Filters},
};

use super::{report_dates, report_error, resolve_currency, table_start};

#[derive(Clone, Copy)]
pub(super) enum TaxKind {
    Schedule,
    Vendor,
}

#[derive(Serialize)]
#[serde(untagged)]
pub(super) enum TaxData {
    Schedule(TaxReport<ScheduleSummary>),
    Vendor(TaxReport<VendorSummary>),
}

impl TaxData {
    pub(super) fn read(
        db: &Db,
        company: &CompanySlug,
        filters: &Filters,
        threshold: Option<&Threshold>,
        kind: TaxKind,
    ) -> Result<Self, ViewError> {
        let currency = resolve_currency(db, company, filters)?;
        let period = report_dates(filters)?;
        match kind {
            TaxKind::Schedule => build_schedule_c(db, company.as_str(), period, currency)
                .map(Self::Schedule)
                .map_err(report_error),
            TaxKind::Vendor => {
                let threshold = match threshold {
                    Some(value) => value
                        .money(currency)
                        .map_err(|error| ViewError::Invalid(error.0))?,
                    None => Money::from_minor(
                        600 * 10_i128.pow(u32::from(currency.minor_units())),
                        currency,
                    ),
                };
                let threshold = VendorThreshold::new(threshold).map_err(report_error)?;
                build_vendors(db, company.as_str(), period, threshold)
                    .map(Self::Vendor)
                    .map_err(report_error)
            }
        }
    }

    pub(super) fn currency(&self) -> Currency {
        match self {
            Self::Schedule(report) => report.currency(),
            Self::Vendor(report) => report.currency(),
        }
    }

    pub(super) fn csv(&self) -> Result<String, ViewError> {
        match self {
            Self::Schedule(report) => render_schedule_c_csv(report),
            Self::Vendor(report) => render_vendors_csv(report),
        }
        .map_err(report_error)
    }

    pub(super) fn render(&self, html: &mut Markup) {
        match self {
            Self::Schedule(report) => {
                let period = report.period();
                write_markup!(html, "<p>{} through {}.</p>", period.from(), period.to());
                let summary = report.summary();
                for (label, rows) in [
                    ("Schedule C lines", summary.lines()),
                    ("Unmapped tags", summary.unmapped()),
                    ("Untagged accounts", summary.untagged()),
                ] {
                    schedule_rows(html, label, rows);
                }
            }
            Self::Vendor(report) => {
                let period = report.period();
                write_markup!(
                    html,
                    "<p>{} through {}. Minimum total paid: {}.</p>",
                    period.from(),
                    period.to(),
                    money::ledger(report.summary().threshold()),
                );
                vendor_rows(html, report.summary());
            }
        }
    }
}

fn schedule_rows(html: &mut Markup, caption: &str, rows: &[ScheduleRow]) {
    if rows.is_empty() {
        write_markup!(
            html,
            "<h3>{}</h3><p class=\"empty\">No matching rows.</p>",
            caption
        );
        return;
    }
    table_start(
        html,
        caption,
        &[
            "Line",
            "Title",
            "Tax category",
            "Account",
            "Account type",
            "Debits",
            "Credits",
            "Net",
        ],
    );
    for row in rows {
        write_markup!(
            html,
            "<tr><td>{}</td><th scope=\"row\">{}</th><td>{}</td><td>{}</td><td>{}</td><td class=\"amount\">{}</td><td class=\"amount\">{}</td><td class=\"amount\">{}</td></tr>",
            row.line().unwrap_or("Not recorded"),
            row.title(),
            row.tax_category().unwrap_or("Not recorded"),
            row.account_code().unwrap_or("Not recorded"),
            row.account_type().unwrap_or("Not recorded"),
            money::ledger(row.debit_total()),
            money::ledger(row.credit_total()),
            money::ledger(row.net()),
        );
    }
    write_markup!(html, "</tbody></table></div>");
}

fn vendor_rows(html: &mut Markup, summary: &VendorSummary) {
    if summary.vendors().is_empty() {
        write_markup!(html, "<p class=\"empty\">No matching rows.</p>");
        return;
    }
    table_start(
        html,
        "Vendor payments",
        &[
            "Payee",
            "TIN",
            "Address",
            "Payments",
            "Total",
            "Meets threshold",
        ],
    );
    // A typed vendor summary only contains rows that meet its threshold.
    for row in summary.vendors() {
        write_markup!(
            html,
            "<tr><th scope=\"row\">{}</th><td>{}</td><td>{}</td><td>{}</td><td class=\"amount\">{}</td><td>true</td></tr>",
            &row.payee,
            row.tin.as_deref().unwrap_or("Not recorded"),
            row.payee_address.as_deref().unwrap_or("Not recorded"),
            row.payment_count,
            money::ledger(row.total()),
        );
    }
    write_markup!(html, "</tbody></table></div>");
}
