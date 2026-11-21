//! Bank and settlement views backed by the typed ledger reconciliation API.

use beankeeper::types::Money;
use beankeeper_cli::{
    cli::ReconciliationMode,
    commands::report_reconciliation::{
        Charge, ReconciliationReport, ReconciliationRequest, Settlement, UnclearedEntry,
        compute_reconciliation, compute_settlement_reconciliation,
    },
    db::Db,
};
use serde_json::Value;

use crate::{
    markup::{Markup, write_markup},
    money,
    views::ViewError,
};

use super::{ReportFilters, report_error, resolve_currency};

pub(super) fn read(
    db: &Db,
    company: &str,
    query: &ReportFilters,
) -> Result<ReconciliationReport, ViewError> {
    let account = query
        .filters
        .account
        .as_deref()
        .ok_or(ViewError::Invalid("Choose an account to reconcile."))?;
    let currency = resolve_currency(db, company, &query.filters)?;
    let mut request = ReconciliationRequest::for_existing_account(account, currency);
    request.as_of = query
        .filters
        .dates
        .to()
        .map(crate::links::values::Date::date);
    request.statement = query.statement.as_deref();
    if let Some(balance) = &query.statement_balance {
        request = request
            .with_statement_balance(
                balance
                    .money(currency)
                    .map_err(|error| ViewError::Invalid(error.0))?,
            )
            .map_err(report_error)?;
    }
    match (query.mode.unwrap_or_default(), query.link_key.as_deref()) {
        (ReconciliationMode::Bank, None) => compute_reconciliation(db, company, &request),
        (ReconciliationMode::Bank, Some(_)) => {
            return Err(ViewError::Invalid("The link key requires settlement mode."));
        }
        (ReconciliationMode::Settlement, key) => {
            compute_settlement_reconciliation(db, company, &request, key)
        }
    }
    .map_err(report_error)
}

pub(super) fn render(report: &ReconciliationReport, html: &mut Markup) {
    write_markup!(
        html,
        "<h3>{} {}</h3><p>Account type {}. Normal balance {}.</p>",
        &report.code,
        &report.name,
        report.account_type.code(),
        report.normal_balance.code(),
    );
    if let Some(date) = report.as_of {
        write_markup!(html, "<p>As of {}.</p>", date);
    }
    if let Some(statement) = &report.statement {
        write_markup!(html, "<p>Statement {}.</p>", statement);
    }
    write_markup!(html, "<dl>");
    amounts(
        html,
        [
            ("Book balance", report.book_balance),
            ("Cleared balance", report.cleared_balance),
            (
                "Cleared across all statements",
                report.total_cleared_balance,
            ),
            ("Reconciled balance", report.reconciled_balance),
            ("Outstanding balance", report.uncleared_total),
        ],
    );
    if let (Some(statement), Some(difference), Some(reconciled)) = (
        report.statement_balance,
        report.difference(),
        report.is_reconciled(),
    ) {
        amounts(
            html,
            [("Statement balance", statement), ("Difference", difference)],
        );
        write_markup!(
            html,
            "<dt>Matches statement</dt><dd data-reconciled=\"{}\">{}</dd>",
            reconciled,
            yes_no(reconciled),
        );
    }
    write_markup!(html, "</dl><h3>Outstanding entries</h3>");
    if report.uncleared.is_empty() {
        write_markup!(html, "<p class=\"empty\">No outstanding entries.</p>");
    } else {
        write_markup!(
            html,
            "<p>{} outstanding, {} partially cleared.</p>",
            report.uncleared.len(),
            report.partially_cleared_count(),
        );
        for entry in &report.uncleared {
            outstanding(html, entry);
        }
    }
    if let Some(settlement) = &report.settlement {
        write_markup!(
            html,
            "<h3>Settlement details</h3><p>Settlement link key {}.</p>",
            &settlement.link_key
        );
        for (label, rows) in [
            ("Charges", &settlement.charges),
            ("Negative charges", &settlement.negative_charges),
        ] {
            write_markup!(html, "<h4>{}</h4>", label);
            for row in rows {
                charge(html, row);
            }
        }
        for (label, rows) in [
            ("Applied settlements", &settlement.applied_settlements),
            ("Unmatched settlements", &settlement.unmatched_settlements),
        ] {
            write_markup!(html, "<h4>{}</h4>", label);
            for row in rows {
                settlement_item(html, row);
            }
        }
    }
}

fn amounts<const N: usize>(html: &mut Markup, values: [(&str, Money); N]) {
    for (label, amount) in values {
        write_markup!(
            html,
            "<dt>{}</dt><dd class=\"amount\">{}</dd>",
            label,
            money::ledger(amount)
        );
    }
}

fn yes_no(value: bool) -> &'static str {
    if value { "Yes" } else { "No" }
}

fn item_metadata(html: &mut Markup, metadata: Option<&Value>) {
    if let Some(metadata) = metadata {
        write_markup!(html, "<p><strong>Transaction metadata</strong></p>");
        metadata_html(metadata, html);
    }
    write_markup!(html, "</details>");
}

// Transaction metadata is user data. Keys do not change how amounts are rendered.
fn metadata_html(value: &Value, html: &mut Markup) {
    match value {
        Value::Object(fields) => {
            write_markup!(html, "<dl>");
            for (key, value) in fields {
                write_markup!(html, "<dt>{}</dt><dd>", key);
                metadata_html(value, html);
                write_markup!(html, "</dd>");
            }
            write_markup!(html, "</dl>");
        }
        Value::Array(items) => {
            write_markup!(html, "<ol>");
            for item in items {
                write_markup!(html, "<li>");
                metadata_html(item, html);
                write_markup!(html, "</li>");
            }
            write_markup!(html, "</ol>");
        }
        Value::String(text) => html.push_text(text),
        Value::Null => write_markup!(html, "Not recorded"),
        Value::Bool(_) | Value::Number(_) => write_markup!(html, "{}", value),
    }
}

fn outstanding(html: &mut Markup, entry: &UnclearedEntry) {
    write_markup!(
        html,
        "<details><summary>{} {}</summary><dl><dt>Entry</dt><dd>{}</dd><dt>Transaction</dt><dd>{}</dd><dt>Reference</dt><dd>{}</dd><dt>Memo</dt><dd>{}</dd><dt>Direction</dt><dd>{}</dd><dt>Partially cleared</dt><dd data-partially-cleared=\"{}\">{}</dd>",
        entry.date,
        &entry.description,
        entry.entry_id,
        entry.transaction_id,
        entry.reference.as_deref().unwrap_or("Not recorded"),
        entry.memo.as_deref().unwrap_or("Not recorded"),
        entry.direction.code(),
        entry.partially_cleared,
        yes_no(entry.partially_cleared),
    );
    amounts(
        html,
        [
            ("Entry amount", entry.amount),
            ("Signed entry amount", entry.signed_amount),
            ("Cleared", entry.cleared_amount),
            ("Reconciled", entry.reconciled_amount),
            ("Remaining", entry.remaining_amount),
            ("Signed remaining", entry.signed_remaining),
        ],
    );
    write_markup!(html, "</dl>");
    if !entry.clearings.is_empty() {
        write_markup!(html, "<p><strong>Clearings</strong></p><ul>");
        for row in &entry.clearings {
            write_markup!(
                html,
                "<li>Clearing {}: {} on {}. {}.",
                row.id,
                money::ledger(row.amount),
                row.cleared_date,
                row.state.status().as_str(),
            );
            if let Some(date) = row.state.reconciled_on() {
                write_markup!(html, " Reconciled {}.", date);
            }
            if let Some(statement) = &row.statement_ref {
                write_markup!(html, " Statement {}.", statement);
            }
            write_markup!(html, "</li>");
        }
        write_markup!(html, "</ul>");
    }
    item_metadata(html, entry.metadata.as_ref());
}

fn charge(html: &mut Markup, row: &Charge) {
    write_markup!(
        html,
        "<details><summary>{} {}</summary><dl><dt>Entry</dt><dd>{}</dd><dt>Transaction</dt><dd>{}</dd><dt>Reference</dt><dd>{}</dd>",
        row.date,
        &row.description,
        row.entry_id,
        row.transaction_id,
        row.reference.as_deref().unwrap_or("Not recorded"),
    );
    if let Some(age) = row.age_days {
        write_markup!(html, "<dt>Age (days)</dt><dd>{}</dd>", age);
    }
    amounts(
        html,
        [
            ("Expected", row.expected),
            ("Applied to this statement", row.applied_this_statement),
            ("Applied total", row.applied_total),
            (
                "Reconciled on this statement",
                row.reconciled_this_statement,
            ),
            ("Remaining", row.remaining),
            ("Short payment", row.short_pay),
            ("Overpayment", row.overpay),
        ],
    );
    write_markup!(html, "</dl>");
    item_metadata(html, row.metadata.as_ref());
}

fn settlement_item(html: &mut Markup, row: &Settlement) {
    write_markup!(
        html,
        "<details><summary>{} {}</summary><dl><dt>Entry</dt><dd>{}</dd><dt>Transaction</dt><dd>{}</dd><dt>Reference</dt><dd>{}</dd>",
        row.date,
        &row.description,
        row.entry_id,
        row.transaction_id,
        row.reference.as_deref().unwrap_or("Not recorded"),
    );
    amounts(
        html,
        [("Settlement", row.amount), ("Unallocated", row.excess)],
    );
    write_markup!(html, "</dl>");
    if !row.charge_transaction_ids.is_empty() {
        write_markup!(html, "<p><strong>Charge transactions</strong></p><ul>");
        for id in &row.charge_transaction_ids {
            write_markup!(html, "<li>{}</li>", id);
        }
        write_markup!(html, "</ul>");
    }
    if !row.reasons.is_empty() {
        write_markup!(html, "<p><strong>Settlement notes</strong></p><ul>");
        for reason in &row.reasons {
            write_markup!(html, "<li>{}</li>", reason.code());
        }
        write_markup!(html, "</ul>");
    }
    item_metadata(html, row.metadata.as_ref());
}
