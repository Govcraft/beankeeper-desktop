//! Read-only HTML views backed exclusively by the upstream ledger query API.

use std::{collections::HashSet, error::Error, fmt, fmt::Write};

use axum::http::StatusCode;
use beankeeper::types::ClearanceStatus;
use beankeeper_cli::{
    cli::Cli,
    commands::{
        report_currency::resolve_report_currency,
        summary::{SummaryArgs, execute_json as execute_summary},
    },
    db::{self, Db, EntryRow, ListAuditParams, ListTransactionParams, TransactionRow},
    error::CliError,
};
use clap::Parser;
use serde::Deserialize;

use crate::{
    ledger_rows::{self, AccountGroup, AuditSubject, EntryValues, RowError},
    links::values::{AccountSelection, Currency, Date, SCAN_LIMIT},
    links::{ACCOUNTS, HOME, LedgerRoute, REPORTS, REVIEW, ReportKind, SUMMARY, TRANSACTIONS},
    markup::{Attribute, Markup, markup, write_markup},
    money,
};

pub(crate) mod filters;
use filters::{Comparison, Filters};

const PAGE_SIZE: usize = 50;

pub(crate) struct ViewPage {
    pub title: &'static str,
    pub description: &'static str,
    pub content: Markup,
    pub nav: &'static str,
}

#[derive(Debug)]
pub(crate) enum ViewError {
    Invalid(&'static str),
    ReportOptions(String),
    Ledger(CliError),
    Row(RowError),
    NotFound,
}

impl ViewError {
    pub(crate) fn status(&self) -> StatusCode {
        match self {
            Self::Invalid(_) | Self::ReportOptions(_) | Self::Ledger(CliError::Validation(_)) => {
                StatusCode::BAD_REQUEST
            }
            Self::NotFound | Self::Ledger(CliError::NotFound(_)) => StatusCode::NOT_FOUND,
            Self::Ledger(_) | Self::Row(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }
}

impl fmt::Display for ViewError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Invalid(message) => f.write_str(message),
            Self::ReportOptions(message) => f.write_str(message.as_str()),
            Self::NotFound | Self::Ledger(CliError::NotFound(_)) => {
                f.write_str("The requested company, account or transaction was not found.")
            }
            Self::Ledger(CliError::Validation(_)) => {
                f.write_str("Choose a valid currency for these books.")
            }
            Self::Ledger(_) | Self::Row(_) => {
                f.write_str("The ledger could not be read. Check the server log and try again.")
            }
        }
    }
}

impl Error for ViewError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Ledger(error) => Some(error),
            Self::Row(error) => Some(error),
            _ => None,
        }
    }
}

impl From<CliError> for ViewError {
    fn from(error: CliError) -> Self {
        Self::Ledger(error)
    }
}

impl From<RowError> for ViewError {
    fn from(error: RowError) -> Self {
        Self::Row(error)
    }
}

pub(crate) fn encode(value: &str) -> String {
    let mut encoded = String::new();
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || b"-._~".contains(&byte) {
            encoded.push(char::from(byte));
        } else {
            let _ = write!(encoded, "%{byte:02X}");
        }
    }
    encoded
}

pub(crate) fn append_url_param(url: &mut String, name: &str, value: Option<impl fmt::Display>) {
    if let Some(value) = value {
        let _ = write!(url, "&{name}={}", encode(&value.to_string()));
    }
}

pub(crate) fn input(
    name: &str,
    label: &str,
    kind: &str,
    value: Option<impl fmt::Display>,
) -> Markup {
    let value = value.map(|value| value.to_string()).unwrap_or_default();
    markup!(
        "<label>{}<input name=\"{}\" type=\"{}\" value=\"{}\"></label>",
        label,
        name,
        kind,
        value
    )
}

pub(crate) fn hidden(html: &mut Markup, name: &str, value: Option<impl fmt::Display>) {
    if let Some(value) = value {
        write_markup!(
            html,
            "<input type=\"hidden\" name=\"{}\" value=\"{}\">",
            name,
            value
        );
    }
}

fn company_form(
    companies: &[db::CompanyRow],
    company: &str,
    query: &Filters,
    path: &str,
) -> Markup {
    let mut html = markup!(
        "<form method=\"get\" action=\"{}\" class=\"filters\"><label>Company<select name=\"company\">",
        path
    );
    for item in companies {
        write_markup!(
            html,
            "<option value=\"{}\"{}>{} ({})</option>",
            &item.slug,
            Attribute::Selected(item.slug == company),
            &item.name,
            &item.slug
        );
    }
    write_markup!(html, "</select></label>");
    hidden(&mut html, "category", query.category.as_deref());
    hidden(&mut html, "payee", query.payee.as_deref());
    if let Some(comparison) = query.comparison {
        hidden(&mut html, "compare_from", comparison.bounds().from());
        hidden(&mut html, "compare_to", comparison.bounds().to());
    }
    if path == "/transactions" {
        html.append(&input(
            "account",
            "Account code",
            "text",
            query.account.as_deref(),
        ));
        html.append(&input(
            "from",
            "From date (inclusive)",
            "date",
            query.dates.from(),
        ));
        html.append(&input(
            "to",
            "To date (inclusive)",
            "date",
            query.dates.to(),
        ));
        html.append(&input(
            "min_amount",
            "Minimum entry amount (minor units, inclusive)",
            "number",
            query.min_amount(),
        ));
        html.append(&input(
            "max_amount",
            "Maximum entry amount (minor units, inclusive)",
            "number",
            query.max_amount(),
        ));
        html.append(&input(
            "search",
            "Payee / description or reference contains",
            "search",
            query.search.as_ref().map(|search| &search.original),
        ));
        write_markup!(
            html,
            "<label>Clearance<select name=\"status\"><option value=\"\">Any clearance</option>"
        );
        for status in [
            ClearanceStatus::Uncleared,
            ClearanceStatus::Cleared,
            ClearanceStatus::Reconciled,
        ] {
            write_markup!(
                html,
                "<option{}>{}</option>",
                Attribute::Selected(
                    query
                        .status
                        .as_ref()
                        .map(crate::links::values::Clearance::status)
                        == Some(status)
                ),
                status
            );
        }
        write_markup!(html, "</select></label>");
    }
    if path == "/" || path == "/transactions" {
        html.append(&input(
            "currency",
            "Currency code (optional)",
            "text",
            query.currency.as_ref(),
        ));
    }
    write_markup!(html, "<button type=\"submit\">Apply</button></form>");
    html
}

pub(crate) fn render(db: &Db, path: &str, query: &Filters) -> Result<ViewPage, ViewError> {
    let route = LedgerRoute::parse(path).map_err(|_| ViewError::NotFound)?;
    if matches!(query.comparison, Some(Comparison::AsOf(_))) {
        match route {
            LedgerRoute::Register => {
                return Err(ViewError::Invalid(
                    "Register comparison requires compare_from and compare_to.",
                ));
            }
            LedgerRoute::Summary => {
                return Err(ViewError::Invalid(
                    "Financial summary comparison requires compare_from and compare_to.",
                ));
            }
            _ => {}
        }
    }
    let (title, description, nav) = match route {
        LedgerRoute::Home => ("Your books", "A snapshot of your recorded finances.", HOME),
        LedgerRoute::Summary => (
            "Financial summary",
            "Recorded balances and changes over the selected periods.",
            HOME,
        ),
        LedgerRoute::Accounts => (
            "Accounts",
            "Browse companies and their complete chart of accounts.",
            ACCOUNTS,
        ),
        LedgerRoute::Register => (
            "Transactions",
            "Filter your register. Entry amounts are shown in their transaction currency.",
            TRANSACTIONS,
        ),
        LedgerRoute::Transaction(_) => (
            "Transaction detail",
            "Entries, metadata and recorded audit history.",
            TRANSACTIONS,
        ),
    };
    let companies = db::list_companies(db.conn())?;
    let selected = query
        .company
        .as_deref()
        .or_else(|| companies.first().map(|company| company.slug.as_str()));
    let Some(company) = selected else {
        let note = markup!(
            "<p class=\"empty\">No companies yet. Ask your agent to initialize your books and create a company.</p>"
        );
        let content = if matches!(route, LedgerRoute::Home | LedgerRoute::Summary) {
            empty_snapshot(&note)
        } else {
            note
        };
        return Ok(ViewPage {
            title,
            description,
            content,
            nav,
        });
    };
    if !companies.iter().any(|item| item.slug == company) {
        return Err(ViewError::NotFound);
    }
    if let Some(account) = query.account.as_deref() {
        db::get_account(db.conn(), company, account)?;
    }
    let mut content = if matches!(route, LedgerRoute::Home | LedgerRoute::Summary) {
        Markup::default()
    } else {
        company_form(&companies, company, query, path)
    };
    match route {
        LedgerRoute::Home => {
            financial_snapshot(db, company, query, SnapshotView::Home, &mut content)?;
        }
        LedgerRoute::Summary => {
            financial_snapshot(db, company, query, SnapshotView::Detail, &mut content)?;
        }
        LedgerRoute::Accounts => accounts(db, company, query, &mut content)?,
        LedgerRoute::Register => render_register(db, company, query, &mut content)?,
        LedgerRoute::Transaction(id) => {
            let id = i64::from(id);
            detail(db, company, query, id, &mut content)?;
        }
    }
    Ok(ViewPage {
        title,
        description,
        content,
        nav,
    })
}

fn render_register(
    db: &Db,
    company: &str,
    query: &Filters,
    content: &mut Markup,
) -> Result<(), ViewError> {
    register(db, company, query, RegisterSection::Primary, content)?;
    if let Some(comparison) = query.comparison_filters() {
        write_markup!(
            content,
            "<section aria-label=\"Comparison period\"><h2>Comparison period: {} through {}</h2>",
            comparison
                .dates
                .from()
                .map(|date| date.to_string())
                .unwrap_or_default(),
            comparison
                .dates
                .to()
                .map(|date| date.to_string())
                .unwrap_or_default()
        );
        register(
            db,
            company,
            &comparison,
            RegisterSection::Comparison,
            content,
        )?;
        write_markup!(content, "</section>");
    }
    Ok(())
}

fn empty_snapshot(note: &Markup) -> Markup {
    let mut home = markup!(
        "<section aria-labelledby=\"snapshot-title\"><h2 id=\"snapshot-title\">Financial snapshot</h2>{}</section>",
        note
    );
    snapshot_links(&mut home, None, None);
    home
}

#[derive(Deserialize)]
struct SummaryResponse {
    data: SummaryHighlights,
}

#[derive(Deserialize)]
struct SummaryHighlights {
    highlights: Vec<String>,
    periods: SummaryPeriods,
}

#[derive(Deserialize)]
struct SummaryPeriods {
    current: SummaryPeriod,
    prior: SummaryPeriod,
}

#[derive(Deserialize)]
struct SummaryPeriod {
    from: Date,
    to: Date,
}

#[derive(Clone, Copy)]
enum SnapshotView {
    Home,
    Detail,
}

impl SnapshotView {
    fn request(self, query: &Filters, currency: beankeeper::types::Currency) -> SummaryArgs {
        let mut args = SummaryArgs {
            currency: Some(currency.code().to_owned()),
            ..SummaryArgs::default()
        };
        if let Self::Detail = self {
            args.from = query.dates.from().map(|date| date.to_string());
            args.to = query.dates.to().map(|date| date.to_string());
            if let Some(comparison) = query.comparison {
                let bounds = comparison.bounds();
                args.compare_from = bounds.from().map(|date| date.to_string());
                args.compare_to = bounds.to().map(|date| date.to_string());
            }
            args.cash_account = selected_accounts(query.cash_account.as_ref());
            args.receivable_account = selected_accounts(query.receivable_account.as_ref());
            args.payable_account = selected_accounts(query.payable_account.as_ref());
        }
        args
    }
}

fn selected_accounts(filter: Option<&AccountSelection>) -> Vec<String> {
    filter.map_or_else(Vec::new, AccountSelection::codes)
}

fn financial_snapshot(
    db: &Db,
    company: &str,
    query: &Filters,
    view: SnapshotView,
    html: &mut Markup,
) -> Result<(), ViewError> {
    write_markup!(
        html,
        "<section aria-labelledby=\"snapshot-title\"><h2 id=\"snapshot-title\">Financial snapshot</h2>"
    );
    let currency = query.currency.as_ref().map_or_else(
        || resolve_report_currency(db.conn(), company, None),
        |currency| Ok(currency.ledger()),
    );
    match currency {
        Ok(currency) => {
            let cli = Cli::try_parse_from(["bk", "summary"])
                .map_err(|_| ViewError::Invalid("Financial summary is unavailable."))?;
            let args = view.request(query, currency);
            let result = execute_summary(&cli, db, company, &args)?;
            let summary: SummaryResponse = serde_json::from_value(result)
                .map_err(|_| ViewError::Invalid("Financial summary is unavailable."))?;
            let headlines = match view {
                SnapshotView::Home => 5,
                SnapshotView::Detail => {
                    let SummaryPeriods { current, prior } = summary.data.periods;
                    write_markup!(
                        html,
                        "<p>{} through {}, compared with {} through {}.</p>",
                        current.from,
                        current.to,
                        prior.from,
                        prior.to,
                    );
                    summary.data.highlights.len()
                }
            };
            write_markup!(html, "<ul>");
            for highlight in summary.data.highlights.iter().take(headlines) {
                write_markup!(html, "<li>{}</li>", highlight);
            }
            write_markup!(html, "</ul>");
        }
        Err(CliError::Validation(_)) if query.currency.is_none() => write_markup!(
            html,
            "<p>These books use more than one currency. Ask your agent for a summary in the currency you want to see.</p>"
        ),
        Err(error) => return Err(error.into()),
    }
    write_markup!(html, "</section>");
    snapshot_links(html, Some(company), query.currency.as_ref());
    Ok(())
}

fn snapshot_links(html: &mut Markup, company: Option<&str>, currency: Option<&Currency>) {
    write_markup!(
        html,
        "<p>You can ask your agent any question about these books.</p><section aria-labelledby=\"detail-title\"><h2 id=\"detail-title\">View details</h2><ul>"
    );
    for (label, path, select_currency) in [
        ("Financial summary", SUMMARY, true),
        ("Accounts", ACCOUNTS, false),
        ("Transactions", TRANSACTIONS, true),
        ("Reports", REPORTS, true),
        ("Reconciliation", ReportKind::Reconciliation.path(), true),
        ("Proposal review", REVIEW, false),
    ] {
        let mut href = path.to_owned();
        if let Some(company) = company {
            href.push_str("?company=");
            href.push_str(&encode(company));
            append_url_param(
                &mut href,
                "currency",
                currency.filter(|_| select_currency).map(Currency::code),
            );
        }
        write_markup!(html, "<li><a href=\"{}\">{}</a></li>", href, label);
    }
    write_markup!(html, "</ul></section>");
}

fn accounts(db: &Db, company: &str, query: &Filters, html: &mut Markup) -> Result<(), ViewError> {
    let rows = db::list_accounts(
        db.conn(),
        &db::ListAccountParams {
            company_slug: company,
            type_filter: None,
            name_filter: None,
        },
    )?;
    if rows.is_empty() {
        write_markup!(
            html,
            "<p class=\"empty\">No accounts yet. Ask your agent to create your chart of accounts.</p>"
        );
        return Ok(());
    }
    write_markup!(
        html,
        "<section><h2>{}</h2><p>{} accounts, grouped by type. All accounts are shown.</p><ul class=\"accounts-tree\">",
        company,
        rows.len()
    );
    for AccountGroup { kind, rows } in ledger_rows::accounts_by_kind(&rows)? {
        write_markup!(
            html,
            "<li><strong>{}</strong><ul>",
            ledger_rows::account_kind_name(kind)
        );
        for account in rows {
            let mut account_filters = query.clone();
            account_filters.account = Some(account.code.clone());
            let link = account_filters.url("/transactions", company, 0);
            write_markup!(
                html,
                "<li id=\"account-{}\"><a href=\"{}\">{} · {}</a>{}</li>",
                &account.code,
                link,
                &account.code,
                &account.name,
                account
                    .default_tax_category
                    .as_ref()
                    .map(|tax| markup!(" <span class=\"note\">Tax category: {}</span>", tax))
                    .unwrap_or_default()
            );
        }
        write_markup!(html, "</ul></li>");
    }
    write_markup!(html, "</ul></section>");
    Ok(())
}

#[derive(Clone, Copy)]
enum RegisterSection {
    Primary,
    Comparison,
}

impl RegisterSection {
    const fn row_prefix(self) -> &'static str {
        match self {
            Self::Primary => "transaction-",
            Self::Comparison => "comparison-transaction-",
        }
    }
}

fn transaction_table(
    db: &Db,
    rows: &[TransactionRow],
    company: &str,
    section: RegisterSection,
    html: &mut Markup,
) -> Result<(), ViewError> {
    if rows.is_empty() {
        write_markup!(html, "<p class=\"empty\">No transactions to show.</p>");
        return Ok(());
    }
    write_markup!(
        html,
        "<div class=\"table-scroll\"><table><caption>Transaction register</caption><thead><tr><th scope=\"col\">Date</th><th scope=\"col\">Payee / description</th><th scope=\"col\">Reference</th><th scope=\"col\">Currency</th><th scope=\"col\">Entries</th></tr></thead><tbody>"
    );
    for row in rows {
        let currency = ledger_rows::currency(&row.currency)?;
        write_markup!(
            html,
            "<tr id=\"{}{}\"><td>{}</td><th scope=\"row\"><a href=\"/transactions/{}?company={}\">{}</a></th><td>{}</td><td>{}</td>",
            section.row_prefix(),
            row.id,
            &row.date,
            row.id,
            encode(company),
            &row.description,
            row.reference.as_deref().unwrap_or("Not recorded"),
            &row.currency
        );
        write_markup!(html, "<td><ul>");
        for entry in db::get_entries_for_transaction(db.conn(), row.id)? {
            let values = EntryValues::read(&entry, currency)?;
            write_markup!(
                html,
                "<li>{}: {} {} ({})</li>",
                &entry.account_code,
                values.direction_label(),
                money::ledger(values.amount),
                values.clearance
            );
        }
        write_markup!(html, "</ul></td></tr>");
    }
    write_markup!(html, "</tbody></table></div>");
    Ok(())
}

fn matches_extra(
    query: &Filters,
    txn: &TransactionRow,
    entries: &[EntryRow],
) -> Result<bool, ViewError> {
    if let Some(search) = &query.search {
        let search = &search.folded;
        if !txn.description.to_lowercase().contains(search)
            && !txn
                .reference
                .as_deref()
                .unwrap_or_default()
                .to_lowercase()
                .contains(search)
        {
            return Ok(false);
        }
    }
    if let Some(payee) = query.payee.as_deref() {
        let metadata = txn
            .metadata
            .as_deref()
            .and_then(|raw| serde_json::from_str::<serde_json::Value>(raw).ok());
        if metadata
            .as_ref()
            .and_then(|value| value.get("payee"))
            .and_then(serde_json::Value::as_str)
            != Some(payee)
        {
            return Ok(false);
        }
    }
    let min = query.min_amount();
    let max = query.max_amount();
    for entry in entries {
        if query
            .category
            .as_deref()
            .is_some_and(|category| entry.tax_category.as_deref() != Some(category))
            || query
                .account
                .as_deref()
                .is_some_and(|account| entry.account_code != account)
            || min.is_some_and(|min| entry.amount < min)
            || max.is_some_and(|max| entry.amount > max)
        {
            continue;
        }
        if let Some(status) = &query.status
            && ledger_rows::clearance(entry)? != status.status()
        {
            continue;
        }
        return Ok(true);
    }
    Ok(false)
}

fn register(
    db: &Db,
    company: &str,
    query: &Filters,
    section: RegisterSection,
    html: &mut Markup,
) -> Result<(), ViewError> {
    let mut params = ListTransactionParams::for_company(company);
    params.account_filter = query.account.as_deref();
    params.tax_category_filter = query.category.as_deref();
    let from = query.dates.from().map(|date| date.to_string());
    let to = query.dates.to().map(|date| date.to_string());
    params.from_date = from.as_deref();
    params.to_date = to.as_deref();
    params.currency_filter = query.currency.as_ref().map(Currency::code);
    // Upstream bounds are exclusive; desktop labels and URLs use inclusive bounds.
    params.amount_gt = query.min_amount().and_then(|min| min.checked_sub(1));
    params.amount_lt = query.max_amount().and_then(|max| max.checked_add(1));
    let total = db::count_transactions(db.conn(), &params)?;
    let start = query.offset.position();
    let mut cursor = start;
    let mut rows = Vec::new();
    while rows.len() < PAGE_SIZE && cursor < total && cursor - start < SCAN_LIMIT {
        params.offset = cursor;
        params.limit = 50;
        let batch = db::list_transactions(db.conn(), &params)?;
        if batch.is_empty() {
            break;
        }
        for txn in batch {
            cursor += 1;
            let entries = db::get_entries_for_transaction(db.conn(), txn.id)?;
            if matches_extra(query, &txn, &entries)? {
                rows.push(txn);
            }
            if rows.len() == PAGE_SIZE || cursor - start == SCAN_LIMIT {
                break;
            }
        }
    }
    write_markup!(
        html,
        "<section><h2>Register</h2><p>Ordered by transaction date, then ID, oldest first. Amount and clearance filters match the same entry. Search is literal and case-insensitive.</p>"
    );
    for (label, value) in [
        ("Tax category", query.category.as_deref()),
        ("Payee (exact)", query.payee.as_deref()),
        ("From date", from.as_deref()),
        ("Through date", to.as_deref()),
    ] {
        if let Some(value) = value {
            write_markup!(html, "<p>{}: {}.</p>", label, value);
        }
    }
    transaction_table(db, &rows, company, section, html)?;
    write_markup!(
        html,
        "<p>{} matching transactions shown. Scanned source rows {} through {} of {} candidates after company, account, date, currency and amount filters.</p>",
        rows.len(),
        if cursor > start { start + 1 } else { start },
        cursor,
        total
    );
    if cursor < total {
        if cursor - start == SCAN_LIMIT {
            write_markup!(
                html,
                "<p class=\"note\">Search paused after 1,000 candidates. More rows remain; continue to search the rest.</p>"
            );
        }
        write_markup!(
            html,
            "<a rel=\"next\" href=\"{}\">Continue register</a> ",
            query.url("/transactions", company, cursor)
        );
    } else {
        write_markup!(html, "<p>End of register for these filters.</p>");
    }
    if start > 0 {
        write_markup!(
            html,
            "<a href=\"{}\">Back to first results</a>",
            query.url("/transactions", company, 0)
        );
    }
    write_markup!(html, "</section>");
    Ok(())
}

fn field(html: &mut Markup, label: &str, value: Option<&str>) {
    write_markup!(
        html,
        "<dt>{}</dt><dd>{}</dd>",
        label,
        value.unwrap_or("Not recorded")
    );
}

fn detail(
    db: &Db,
    company: &str,
    query: &Filters,
    id: i64,
    html: &mut Markup,
) -> Result<(), ViewError> {
    let (txn, entries) = db::get_transaction(db.conn(), company, id)?;
    write_markup!(
        html,
        "<section><h2>Transaction {}: {}</h2><dl>",
        id,
        &txn.description
    );
    for (label, value) in [
        ("Company", Some(company)),
        ("Date", Some(txn.date.as_str())),
        ("Currency", Some(txn.currency.as_str())),
        ("Posted at", Some(txn.posted_at.as_str())),
        ("Reference", txn.reference.as_deref()),
        ("Posted by", txn.posted_by.as_deref()),
        ("Agent identity", txn.agent_uri.as_deref()),
        ("Token ID", txn.token_jti.as_deref()),
    ] {
        field(html, label, value);
    }
    write_markup!(html, "</dl><h3>Metadata</h3><pre>");
    html.push_text(txn.metadata.as_deref().unwrap_or("No metadata recorded."));
    write_markup!(
        html,
        "</pre></section><section><h2>Entries</h2><div class=\"table-scroll\"><table><caption>Entry amounts in transaction currency</caption><thead><tr><th scope=\"col\">Account</th><th scope=\"col\">Direction</th><th scope=\"col\">Amount</th><th scope=\"col\">Clearance</th><th scope=\"col\">Memo</th><th scope=\"col\">Tax category</th></tr></thead><tbody>"
    );
    let currency = ledger_rows::currency(&txn.currency)?;
    for entry in &entries {
        let values = EntryValues::read(entry, currency)?;
        write_markup!(
            html,
            "<tr><th scope=\"row\">{}</th><td>{}</td><td class=\"amount\">{}</td><td>{}</td><td>{}</td><td>{}</td></tr>",
            &entry.account_code,
            values.direction_label(),
            money::ledger(values.amount),
            values.clearance,
            entry.memo.as_deref().unwrap_or("Not recorded"),
            entry.tax_category.as_deref().unwrap_or("Not recorded")
        );
    }
    write_markup!(html, "</tbody></table></div></section>");
    audit(db, company, query, &txn, &entries, html)
}

fn audit(
    db: &Db,
    company: &str,
    query: &Filters,
    txn: &TransactionRow,
    entries: &[EntryRow],
    html: &mut Markup,
) -> Result<(), ViewError> {
    let start = query.offset.position();
    let params = ListAuditParams {
        agent_uri: None,
        company_slug: Some(company),
        entity: None,
        action: None,
        limit: SCAN_LIMIT,
        offset: start,
    };
    let total = db::count_audit(db.conn(), &params)?;
    let rows = db::list_audit(db.conn(), &params)?;
    let count = i64::try_from(rows.len())
        .map_err(|_| ViewError::Invalid("The audit trail is too large to paginate."))?;
    let cursor = start + count;
    let entry_ids = entries.iter().map(|entry| entry.id).collect::<HashSet<_>>();
    let mut shown = 0;
    write_markup!(
        html,
        "<section><h2>Audit trail</h2><p>Recorded transaction changes and changes to its entries, newest first.</p>"
    );
    for row in &rows {
        let Some(subject) = AuditSubject::read(row)? else {
            continue;
        };
        if !subject.matches(txn.id, &entry_ids) {
            continue;
        }
        shown += 1;
        write_markup!(
            html,
            "<article><h3>{}: {} {}</h3><dl>",
            &row.changed_at,
            subject.kind().as_str(),
            &row.action
        );
        for (label, value) in [
            ("Actor", Some(row.actor.as_str())),
            ("Agent identity", row.agent_uri.as_deref()),
            ("Token ID", row.token_jti.as_deref()),
        ] {
            field(html, label, value);
        }
        write_markup!(
            html,
            "</dl><details><summary>Recorded change</summary><h4>Before</h4><pre>{}</pre><h4>After</h4><pre>{}</pre></details></article>",
            row.before.as_deref().unwrap_or("Not recorded"),
            row.after.as_deref().unwrap_or("Not recorded")
        );
    }
    if shown == 0 {
        write_markup!(
            html,
            "<p>No audit records for this transaction in this portion of the trail. Historical writes may have no creation audit.</p>"
        );
    }
    write_markup!(
        html,
        "<p>{} relevant records shown from company audit rows {} through {} of {}.</p>",
        shown,
        if cursor > start { start + 1 } else { start },
        cursor,
        total
    );
    if cursor < total {
        write_markup!(
            html,
            "<p class=\"note\">More audit rows remain. <a rel=\"next\" href=\"{}\">Continue audit trail</a></p>",
            query.url(&format!("/transactions/{}", txn.id), company, cursor)
        );
    }
    if start > 0 {
        write_markup!(
            html,
            "<a href=\"{}\">Back to newest audit rows</a>",
            query.url(&format!("/transactions/{}", txn.id), company, 0)
        );
    }
    write_markup!(html, "</section>");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::filters::ViewQuery;
    use super::*;
    use beankeeper_cli::db::{Actor, ConflictStrategy, PostEntryParams, PostTransactionParams};

    type TestResult<T = ()> = Result<T, Box<dyn Error>>;

    pub(super) fn render_query(
        db: &Db,
        path: &str,
        query: &ViewQuery,
    ) -> Result<ViewPage, ViewError> {
        render(db, path, &query.clone().parse()?)
    }

    fn fixture() -> TestResult<Db> {
        let db = Db::open_in_memory()?;
        db::create_company(db.conn(), "acme", "Acme & Sons", None)?;
        db::create_company(db.conn(), "other", "Other", None)?;
        for company in ["acme", "other"] {
            db::create_account(db.conn(), company, "1000", "Cash <bank>", "asset", None)?;
            db::create_account(db.conn(), company, "4000", "Sales", "revenue", None)?;
        }
        Ok(db)
    }

    fn post(
        db: &Db,
        description: &str,
        reference: &str,
        amount: i64,
        currency: &str,
        actor: &Actor,
    ) -> TestResult<i64> {
        let entries = [
            PostEntryParams {
                account_code: "1000".into(),
                direction: "debit".into(),
                amount,
                memo: Some("<memo>".into()),
                tax_category: None,
            },
            PostEntryParams {
                account_code: "4000".into(),
                direction: "credit".into(),
                amount,
                memo: None,
                tax_category: Some("sales".into()),
            },
        ];
        let result = db::post_transaction(
            db.conn(),
            &PostTransactionParams {
                company_slug: "acme",
                description,
                metadata: Some("{\"note\":\"<script>\"}"),
                currency,
                date: "2024-02-29",
                entries: &entries,
                correlate: None,
                reference: Some(reference),
                on_conflict: ConflictStrategy::Error,
                actor,
            },
        )?;
        match result {
            db::PostResult::Created(id) => Ok(id),
            db::PostResult::Skipped(_) => Err("expected created transaction".into()),
        }
    }

    #[test]
    fn filter_validation_rejects_invalid_calendar_dates_ranges_and_status() {
        for date in [
            "2023-02-29",
            "2024-04-31",
            "0000-01-01",
            "2024-00-10",
            "2024-01-00",
            "2024-1-01",
            "2024-13-01",
            "<script>",
        ] {
            assert!(
                ViewQuery {
                    from: Some(date.into()),
                    ..Default::default()
                }
                .parse()
                .is_err(),
                "{date}"
            );
        }
        for date in ["2024-02-29", "2000-02-29", "2025-12-31"] {
            assert!(
                ViewQuery {
                    from: Some(date.into()),
                    ..Default::default()
                }
                .parse()
                .is_ok()
            );
        }
        for query in [
            ViewQuery {
                from: Some("2024-03-01".into()),
                to: Some("2024-02-01".into()),
                ..Default::default()
            },
            ViewQuery {
                min_amount: Some("-1".into()),
                ..Default::default()
            },
            ViewQuery {
                min_amount: Some("1.50".into()),
                ..Default::default()
            },
            ViewQuery {
                min_amount: Some("20".into()),
                max_amount: Some("10".into()),
                ..Default::default()
            },
            ViewQuery {
                status: Some("pending".into()),
                ..Default::default()
            },
            ViewQuery {
                offset: Some(i64::MAX),
                ..Default::default()
            },
        ] {
            assert!(query.parse().is_err());
        }
    }

    #[test]
    fn links_escape_query_and_preserve_filter_values() -> TestResult {
        let query = ViewQuery {
            company: Some("acme".into()),
            search: Some("O'Reilly & 20% <x>".into()),
            status: Some("cleared".into()),
            min_amount: Some("100".into()),
            ..Default::default()
        };
        let url = query.parse()?.url("/transactions", "acme & son", 50);
        assert!(url.contains("&offset=50"));
        let link = markup!("<a href=\"{}\">Continue</a>", url);
        let url = link.as_str();
        assert!(url.contains("company=acme%20%26%20son"));
        assert!(url.contains("search=O%27Reilly%20%26%2020%25%20%3Cx%3E"));
        assert!(url.contains("&amp;offset=50"));
        assert!(url.contains("&amp;status=cleared"));
        Ok(())
    }

    #[test]
    fn snapshot_accounts_and_detail_use_upstream_data_and_escape_content() -> TestResult {
        let db = fixture()?;
        let id = post(
            &db,
            "Coffee <script>alert(1)</script>",
            "invoice&1",
            12345,
            "USD",
            &Actor::new("owner"),
        )?;
        let query = ViewQuery {
            company: Some("acme".into()),
            ..Default::default()
        };
        let snapshot = render_query(&db, "/", &query)?;
        let home = snapshot.content.as_str();
        assert!(home.contains("Cash on hand in the selected accounts is USD 123.45"));
        assert!(home.contains("Net worth is USD 123.45"));
        assert!(home.contains("href=\"/transactions?company=acme\""));
        assert!(!home.contains("Recent activity"));
        let accounts = render_query(&db, "/accounts", &query)?;
        assert!(accounts.content.as_str().contains("2 accounts"));
        assert!(accounts.content.as_str().contains("Cash &lt;bank&gt;"));
        assert!(
            accounts
                .content
                .as_str()
                .contains("<strong>revenue</strong>")
        );
        let detail = render_query(&db, &format!("/transactions/{id}"), &query)?;
        assert!(detail.content.as_str().contains("Entries"));
        assert!(detail.content.as_str().contains("&lt;memo&gt;"));
        assert!(detail.content.as_str().contains("&lt;script&gt;"));
        assert!(detail.content.as_str().contains("Audit trail"));
        assert!(detail.content.as_str().contains("owner"));
        assert!(detail.content.as_str().contains("Not recorded"));
        assert!(!detail.content.as_str().contains("<script>"));
        let other = ViewQuery {
            company: Some("other".into()),
            ..Default::default()
        };
        assert_eq!(
            render_query(&db, &format!("/transactions/{id}"), &other)
                .err()
                .ok_or("expected not found")?
                .status(),
            StatusCode::NOT_FOUND
        );
        Ok(())
    }

    #[test]
    fn register_combines_literal_reference_date_amount_account_and_clearance_filters() -> TestResult
    {
        let db = fixture()?;
        let id = post(
            &db,
            "Coffee",
            "INVOICE%_ONE",
            250,
            "USD",
            &Actor::new("owner"),
        )?;
        post(
            &db,
            "Other description",
            "different",
            500,
            "USD",
            &Actor::new("owner"),
        )?;
        let (_, entries) = db::get_transaction(db.conn(), "acme", id)?;
        db::set_entry_status(
            db.conn(),
            &Actor::new("owner"),
            "acme",
            id,
            entries[0].id,
            db::EntryStatus::Cleared,
        )?;
        let query = ViewQuery {
            company: Some("acme".into()),
            account: Some("1000".into()),
            from: Some("2024-02-29".into()),
            to: Some("2024-02-29".into()),
            min_amount: Some("250".into()),
            max_amount: Some("250".into()),
            search: Some("invoice%_".into()),
            status: Some("cleared".into()),
            ..Default::default()
        };
        let page = render_query(&db, "/transactions", &query)?;
        assert!(page.content.as_str().contains(">Coffee</a>"));
        assert!(!page.content.as_str().contains(">Other description</a>"));
        assert!(
            page.content
                .as_str()
                .contains("1 matching transactions shown")
        );
        let category_on_other_leg = ViewQuery {
            category: Some("sales".into()),
            ..query.clone()
        };
        assert!(
            render_query(&db, "/transactions", &category_on_other_leg)?
                .content
                .as_str()
                .contains("0 matching transactions shown")
        );
        // The cleared cash entry must not make the uncleared revenue account match.
        let different_account = ViewQuery {
            account: Some("4000".into()),
            ..query
        };
        assert!(
            render_query(&db, "/transactions", &different_account)?
                .content
                .as_str()
                .contains("0 matching transactions shown")
        );
        Ok(())
    }

    #[test]
    fn mixed_currency_register_and_details_use_each_transactions_scale() -> TestResult {
        let db = fixture()?;
        for (code, amount, formatted) in [
            ("USD", 5_562_000, "$55,620.00"),
            ("JPY", 123_456, "¥123,456"),
            ("BHD", 1234, "BHD 1.234"),
            ("MXN", 4_032_470, "MX$40,324.70"),
        ] {
            let id = post(&db, code, code, amount, code, &Actor::new("owner"))?;
            let query = ViewQuery {
                company: Some("acme".into()),
                ..Default::default()
            };
            let register = render_query(&db, "/transactions", &query)?;
            assert!(register.content.as_str().contains(formatted));
            let detail = render_query(&db, &format!("/transactions/{id}"), &query)?;
            assert!(
                detail
                    .content
                    .as_str()
                    .contains(&format!("class=\"amount\">{formatted}</td>"))
            );
        }
        Ok(())
    }

    #[test]
    fn register_pagination_is_bounded_and_uses_continuation_cursor() -> TestResult {
        let db = fixture()?;
        for n in 0..53 {
            post(
                &db,
                &if n == 10 {
                    "Other".into()
                } else {
                    format!("Sale {n}")
                },
                &format!("ref{n}"),
                100,
                "USD",
                &Actor::new("owner"),
            )?;
        }
        let query = ViewQuery {
            company: Some("acme".into()),
            search: Some("Sale".into()),
            ..Default::default()
        };
        let first = render_query(&db, "/transactions", &query)?;
        assert!(
            first
                .content
                .as_str()
                .contains("50 matching transactions shown")
        );
        assert!(first.content.as_str().contains("&amp;offset=51"));
        assert!(first.content.as_str().contains("search=Sale"));
        let second = render_query(
            &db,
            "/transactions",
            &ViewQuery {
                offset: Some(51),
                ..query
            },
        )?;
        assert!(
            second
                .content
                .as_str()
                .contains("2 matching transactions shown")
        );
        assert!(second.content.as_str().contains("End of register"));
        assert!(!second.content.as_str().contains("rel=\"next\""));
        Ok(())
    }

    #[test]
    fn sparse_search_and_audit_scans_disclose_truncation_and_continue() -> TestResult {
        let db = fixture()?;
        let first_id = post(&db, "First", "first", 100, "USD", &Actor::new("owner"))?;
        for n in 1..1_001 {
            post(
                &db,
                "Other",
                &format!("ref{n}"),
                100,
                "USD",
                &Actor::new("owner"),
            )?;
        }
        post(&db, "Needle", "last", 100, "USD", &Actor::new("owner"))?;
        let query = ViewQuery {
            company: Some("acme".into()),
            search: Some("Needle".into()),
            ..Default::default()
        };
        let first = render_query(&db, "/transactions", &query)?;
        assert!(
            first
                .content
                .as_str()
                .contains("0 matching transactions shown")
        );
        assert!(
            first
                .content
                .as_str()
                .contains("Search paused after 1,000 candidates")
        );
        assert!(first.content.as_str().contains("&amp;offset=1000"));
        let continued = render_query(
            &db,
            "/transactions",
            &ViewQuery {
                offset: Some(1_000),
                ..query
            },
        )?;
        assert!(
            continued
                .content
                .as_str()
                .contains("1 matching transactions shown")
        );
        assert!(continued.content.as_str().contains(">Needle</a>"));
        assert!(continued.content.as_str().contains("End of register"));
        let detail_path = format!("/transactions/{first_id}");
        let audit_query = ViewQuery {
            company: Some("acme".into()),
            ..Default::default()
        };
        let newest = render_query(&db, &detail_path, &audit_query)?;
        assert!(newest.content.as_str().contains("Continue audit trail"));
        assert!(
            newest
                .content
                .as_str()
                .contains("No audit records for this transaction in this portion")
        );
        let oldest = render_query(
            &db,
            &detail_path,
            &ViewQuery {
                offset: Some(1_000),
                ..audit_query
            },
        )?;
        assert!(oldest.content.as_str().contains("1 relevant records shown"));
        assert!(oldest.content.as_str().contains("transaction create"));
        assert!(!oldest.content.as_str().contains("Continue audit trail"));
        Ok(())
    }

    #[test]
    fn mixed_currency_snapshot_requires_currency_before_aggregating() -> TestResult {
        let db = fixture()?;
        post(&db, "USD sale", "usd", 100, "USD", &Actor::new("owner"))?;
        post(&db, "MXN sale", "mxn", 200, "MXN", &Actor::new("owner"))?;
        let query = ViewQuery {
            company: Some("acme".into()),
            ..Default::default()
        };
        assert!(
            render_query(&db, "/", &query)?
                .content
                .as_str()
                .contains("These books use more than one currency.")
        );
        let selected = render_query(
            &db,
            "/",
            &ViewQuery {
                currency: Some("USD".into()),
                ..query
            },
        )?;
        assert!(selected.content.as_str().contains("Net worth is USD 1.00"));
        assert!(!selected.content.as_str().contains("Net worth is USD 3.00"));
        Ok(())
    }

    #[test]
    fn empty_and_invalid_company_states_are_explicit() -> TestResult {
        let db = Db::open_in_memory()?;
        assert!(
            render_query(&db, "/", &ViewQuery::default())?
                .content
                .as_str()
                .contains("No companies yet")
        );
        db::create_company(db.conn(), "empty", "Empty", None)?;
        assert!(
            render_query(&db, "/accounts", &ViewQuery::default())?
                .content
                .as_str()
                .contains("No accounts yet")
        );
        assert!(
            render_query(&db, "/transactions", &ViewQuery::default())?
                .content
                .as_str()
                .contains("No transactions to show")
        );
        assert_eq!(
            render_query(
                &db,
                "/",
                &ViewQuery {
                    company: Some("missing".into()),
                    ..Default::default()
                }
            )
            .err()
            .ok_or("expected failure")?
            .status(),
            StatusCode::NOT_FOUND
        );
        Ok(())
    }
}

#[cfg(test)]
mod identity_tests {
    use super::filters::ViewQuery;
    use super::tests::render_query;
    use super::*;
    use beankeeper_cli::identity::{self, IssueRequest};
    use std::time::Duration;

    #[test]
    fn detail_shows_recorded_agent_uri_and_token_identifier() -> Result<(), Box<dyn Error>> {
        let dir = tempfile::tempdir()?;
        let db = Db::open(
            &dir.path().join("books.db"),
            Some(&"test-passphrase".into()),
        )?;
        db::create_company(db.conn(), "acme", "Acme", None)?;
        db::create_account(db.conn(), "acme", "1000", "Cash", "asset", None)?;
        db::create_account(db.conn(), "acme", "4000", "Sales", "revenue", None)?;
        let owner = db::Actor::new("owner");
        identity::init_root(db.conn(), &owner)?;
        let issued = identity::issue(
            db.conn(),
            &owner,
            &IssueRequest {
                name: "bookkeeper",
                capabilities: &["ledger/read".into(), "ledger/post".into()],
                ttl: Duration::from_secs(3600),
                public_key: None,
                generate_key: true,
            },
        )?;
        let token = issued["token"].as_str().ok_or("missing token")?;
        let verified = identity::verify(db.conn(), token)?;
        let uri = verified.uri().to_owned();
        let jti = verified.jti().to_owned();
        let actor = db::Actor::new("assistant").with_identity(verified);
        let entries = [
            db::PostEntryParams {
                account_code: "1000".into(),
                direction: "debit".into(),
                amount: 100,
                memo: None,
                tax_category: None,
            },
            db::PostEntryParams {
                account_code: "4000".into(),
                direction: "credit".into(),
                amount: 100,
                memo: None,
                tax_category: None,
            },
        ];
        let result = db::post_transaction(
            db.conn(),
            &db::PostTransactionParams {
                company_slug: "acme",
                description: "Agent sale",
                metadata: None,
                currency: "USD",
                date: "2024-01-01",
                entries: &entries,
                correlate: None,
                reference: None,
                on_conflict: db::ConflictStrategy::Error,
                actor: &actor,
            },
        )?;
        let db::PostResult::Created(id) = result else {
            return Err("expected created".into());
        };
        let page = render_query(
            &db,
            &format!("/transactions/{id}"),
            &ViewQuery {
                company: Some("acme".into()),
                ..Default::default()
            },
        )?;
        // Both the creation fields and audit record retain the verified identity.
        assert!(
            page.content
                .as_str()
                .matches(Markup::escaped(&uri).as_str())
                .count()
                >= 2
        );
        assert!(
            page.content
                .as_str()
                .matches(Markup::escaped(&jti).as_str())
                .count()
                >= 2
        );
        assert!(!page.content.as_str().contains(token));
        Ok(())
    }
}
