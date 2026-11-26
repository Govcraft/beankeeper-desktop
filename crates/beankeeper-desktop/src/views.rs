//! Read-only HTML views backed exclusively by the upstream ledger query API.

use std::{collections::HashSet, error::Error, fmt, fmt::Write};

use axum::http::StatusCode;
use beankeeper::types::{ClearanceStatus, Money};
use beankeeper_cli::{
    commands::{
        report_currency::resolve_report_currency,
        summary::{SummaryAccounts, SummaryDates, SummaryRequest},
    },
    db::{
        self, Db, EntryRow, ListAuditParams, ListTransactionParams, TransactionRow,
        audit::{AuditPageLimit, AuditPageOffset},
        companies::{Company, CompanySlug},
        ids::TransactionId,
        transactions::TransactionValueFilter,
    },
    error::CliError,
};

use crate::{
    ledger_rows::{self, AccountGroup, AuditSubject, EntryValues, RowError},
    links::values::{AccountSelection, Currency, Date, SCAN_LIMIT},
    links::{ACCOUNTS, HOME, LedgerRoute, REPORTS, REVIEW, ReportKind, SUMMARY, TRANSACTIONS},
    markup::{Attribute, Markup, markup, write_markup},
    money,
    pairing::HumanSession,
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
            Attribute::Selected(item.slug.as_str() == company),
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
        write_markup!(
            html,
            "<label class=\"checkbox-filter\"><input type=\"checkbox\" name=\"include_reversed\" value=\"true\"{}> Show reversed/cancelled pairs</label>",
            Attribute::Checked(query.include_reversed.into())
        );
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

pub(crate) fn render(
    db: &Db,
    session: &HumanSession,
    path: &str,
    query: &Filters,
) -> Result<ViewPage, ViewError> {
    let route = LedgerRoute::parse(path).map_err(|_| ViewError::NotFound)?;
    let snapshot = match route {
        LedgerRoute::Summary => SnapshotView::Detail(query.try_into()?),
        LedgerRoute::Register if matches!(query.comparison, Some(Comparison::AsOf(_))) => {
            return Err(ViewError::Invalid(
                "Register comparison requires compare_from and compare_to.",
            ));
        }
        _ => SnapshotView::Home,
    };
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
    let companies = db::list_companies(db.read())?;
    let selected = query
        .company
        .as_ref()
        .or_else(|| companies.first().map(|company| &company.slug));
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
    let book = Company::bind(db.read(), company.clone())?;
    if let Some(account) = query.account.as_deref() {
        db::get_account(&book, account)?;
    }
    let mut content = if matches!(route, LedgerRoute::Home | LedgerRoute::Summary) {
        Markup::default()
    } else {
        company_form(&companies, company.as_str(), query, path)
    };
    match route {
        LedgerRoute::Home | LedgerRoute::Summary => {
            financial_snapshot(db, session, company, query, snapshot, &mut content)?;
        }
        LedgerRoute::Accounts => accounts(&book, query, &mut content)?,
        LedgerRoute::Register => render_register(db, &book, query, &mut content)?,
        LedgerRoute::Transaction(id) => {
            detail(db, &book, query, id.into(), &mut content)?;
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
    book: &Company<'_>,
    query: &Filters,
    content: &mut Markup,
) -> Result<(), ViewError> {
    register(db, book, query, RegisterSection::Primary, content)?;
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
        register(db, book, &comparison, RegisterSection::Comparison, content)?;
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

#[derive(Clone, Copy)]
enum SnapshotView {
    Home,
    Detail(SummaryDates),
}

impl SnapshotView {
    fn request(
        self,
        company: &CompanySlug,
        query: &Filters,
        currency: beankeeper::types::Currency,
    ) -> Result<SummaryRequest, CliError> {
        let (dates, accounts) = match self {
            Self::Home => (SummaryDates::default(), SummaryAccounts::default()),
            Self::Detail(dates) => {
                let accounts = [
                    query.cash_account.as_ref(),
                    query.receivable_account.as_ref(),
                    query.payable_account.as_ref(),
                ]
                .map(|selection| selection.map_or_else(Vec::new, AccountSelection::ledger_codes));
                (dates, accounts.into())
            }
        };
        (company.clone(), dates, Some(currency), accounts).try_into()
    }
}

fn financial_snapshot(
    db: &Db,
    session: &HumanSession,
    company: &CompanySlug,
    query: &Filters,
    view: SnapshotView,
    html: &mut Markup,
) -> Result<(), ViewError> {
    write_markup!(
        html,
        "<section aria-labelledby=\"snapshot-title\"><h2 id=\"snapshot-title\">Financial snapshot</h2>"
    );
    let currency = query.currency.as_ref().map_or_else(
        || resolve_report_currency(db.read(), company.as_str(), None),
        |currency| Ok(currency.ledger()),
    );
    match currency {
        Ok(currency) => {
            let request = view.request(company, query, currency)?;
            let summary = session.summary_reader(db)?.summary(request, |read| {
                eprintln!(
                    "{} owner ledger read operation={} company={} actor={}",
                    chrono::Utc::now().to_rfc3339(),
                    read.operation().name(),
                    read.company().as_str(),
                    read.actor(),
                );
            })?;
            let highlights = summary.highlights();
            let headlines = match view {
                SnapshotView::Home => 5,
                SnapshotView::Detail(_) => {
                    let [(from, to), (compare_from, compare_to)] = summary.period_windows();
                    write_markup!(
                        html,
                        "<p>{} through {}, compared with {} through {}.</p>",
                        from,
                        to,
                        compare_from,
                        compare_to,
                    );
                    highlights.len()
                }
            };
            write_markup!(html, "<ul>");
            for highlight in highlights.iter().take(headlines) {
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
    snapshot_links(html, Some(company.as_str()), query.currency.as_ref());
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

fn accounts(book: &Company<'_>, query: &Filters, html: &mut Markup) -> Result<(), ViewError> {
    let company = book.slug().as_str();
    let rows = db::list_accounts(
        book,
        &db::ListAccountParams {
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
    for AccountGroup { kind, rows } in ledger_rows::accounts_by_kind(&rows) {
        write_markup!(html, "<li><strong>{}</strong><ul>", kind.code());
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
    rows: &[(TransactionRow, Vec<EntryRow>)],
    company: &str,
    section: RegisterSection,
    html: &mut Markup,
) {
    if rows.is_empty() {
        write_markup!(html, "<p class=\"empty\">No transactions to show.</p>");
        return;
    }
    write_markup!(
        html,
        "<div class=\"table-scroll\"><table><caption>Transaction register</caption><thead><tr><th scope=\"col\">Date</th><th scope=\"col\">Payee / description</th><th scope=\"col\">Reference</th><th scope=\"col\">Currency</th><th scope=\"col\">Entries</th></tr></thead><tbody>"
    );
    for (row, entries) in rows {
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
            row.currency.code()
        );
        write_markup!(html, "<td><ul>");
        for entry in entries {
            let values = EntryValues::read(entry, row.currency);
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
}

fn matches_extra(query: &Filters, txn: &TransactionRow, entries: &[EntryRow]) -> bool {
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
            return false;
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
            return false;
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
            || min.is_some_and(|min| entry.amount.minor_units() < min)
            || max.is_some_and(|max| entry.amount.minor_units() > max)
        {
            continue;
        }
        if let Some(status) = &query.status
            && ledger_rows::clearance(entry.status) != status.status()
        {
            continue;
        }
        return true;
    }
    false
}

fn register_values(
    db: &Db,
    company: &str,
    query: &Filters,
) -> Result<Option<TransactionValueFilter>, ViewError> {
    let (min, max) = (query.min_amount(), query.max_amount());
    let currency = match (query.currency.as_ref(), min, max) {
        (Some(currency), _, _) => currency.ledger(),
        (None, None, None) => return Ok(None),
        (None, _, _) => resolve_report_currency(db.read(), company, None).map_err(|error| {
            if matches!(error, CliError::Validation(_)) {
                ViewError::Invalid("Choose a currency to filter amounts in these books.")
            } else {
                error.into()
            }
        })?,
    };
    let money = |units| Money::from_minor(i128::from(units), currency);
    // Upstream bounds are exclusive; register query values are inclusive.
    let lower = min.and_then(|units| units.checked_sub(1)).map(money);
    let upper = max.and_then(|units| units.checked_add(1)).map(money);
    Ok(Some(
        TransactionValueFilter::from_bounds(lower, upper, None)?
            .unwrap_or_else(|| TransactionValueFilter::by_currency(currency)),
    ))
}

fn register(
    db: &Db,
    book: &Company<'_>,
    query: &Filters,
    section: RegisterSection,
    html: &mut Markup,
) -> Result<(), ViewError> {
    let company = book.slug().as_str();
    let from = query.dates.from();
    let to = query.dates.to();
    let mut params = ListTransactionParams {
        account_filter: query.account.as_deref(),
        tax_category_filter: query.category.as_deref(),
        from_date: from.map(Date::date),
        to_date: to.map(Date::date),
        value_filter: register_values(db, company, query)?,
        include_reversed: query.include_reversed.into(),
        ..ListTransactionParams::default()
    };
    let total = db::count_transactions(book, &params)?;
    let start = query.offset.position();
    let mut cursor = start;
    let mut rows = Vec::new();
    while rows.len() < PAGE_SIZE && cursor < total && cursor - start < SCAN_LIMIT {
        params.offset = cursor;
        params.limit = 50;
        let batch = db::list_transactions(book, &params)?;
        if batch.is_empty() {
            break;
        }
        for txn in batch {
            cursor += 1;
            let entries = db::get_entries_for_transaction(book, txn.id)?;
            if matches_extra(query, &txn, &entries) {
                rows.push((txn, entries));
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
        ("From date", from.map(|date| date.to_string()).as_deref()),
        ("Through date", to.map(|date| date.to_string()).as_deref()),
    ] {
        if let Some(value) = value {
            write_markup!(html, "<p>{}: {}.</p>", label, value);
        }
    }
    transaction_table(&rows, company, section, html);
    write_markup!(
        html,
        "<p>{} matching transactions shown. Scanned source rows {} through {} of {} candidates after company, account, date, currency, amount and reversal filters.</p>",
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
    book: &Company<'_>,
    query: &Filters,
    id: TransactionId,
    html: &mut Markup,
) -> Result<(), ViewError> {
    let (txn, entries) = db::get_transaction(book, id)?;
    write_markup!(
        html,
        "<section><h2>Transaction {}: {}</h2><dl>",
        id,
        &txn.description
    );
    let date = txn.date.to_string();
    for (label, value) in [
        ("Company", Some(book.slug().as_str())),
        ("Date", Some(date.as_str())),
        ("Currency", Some(txn.currency.code())),
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
    for entry in &entries {
        let values = EntryValues::read(entry, txn.currency);
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
    audit(db, book.slug(), query, &txn, &entries, html)
}

fn audit(
    db: &Db,
    company: &CompanySlug,
    query: &Filters,
    txn: &TransactionRow,
    entries: &[EntryRow],
    html: &mut Markup,
) -> Result<(), ViewError> {
    let start = query.offset.position();
    let (total, rows) = db.read_snapshot(|reader| {
        let mut params = ListAuditParams {
            actor: None,
            agent_uri: None,
            company_slug: Some(company),
            entity: None,
            action: None,
            limit: AuditPageLimit::default(),
            offset: AuditPageOffset::default(),
        };
        let total = db::count_audit(reader, &params)?;
        let end = (start + SCAN_LIMIT).min(total);
        let mut cursor = start;
        let mut rows = Vec::new();
        // Each upstream query has its own page limit. The browser scans up to
        // SCAN_LIMIT rows in one snapshot before offering a continuation.
        while cursor < end {
            params.limit = (end - cursor)
                .min(i64::from(AuditPageLimit::MAX))
                .try_into()?;
            params.offset = cursor.try_into()?;
            rows.extend(db::list_audit(reader, &params)?);
            cursor += i64::from(params.limit.get());
        }
        Ok((total, rows))
    })?;
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
            row.entity.as_str(),
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
            query.url(
                &format!("/transactions/{}", txn.id),
                company.as_str(),
                cursor
            )
        );
    }
    if start > 0 {
        write_markup!(
            html,
            "<a href=\"{}\">Back to newest audit rows</a>",
            query.url(&format!("/transactions/{}", txn.id), company.as_str(), 0)
        );
    }
    write_markup!(html, "</section>");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::filters::ViewQuery;
    use super::*;
    use beankeeper::types::{AccountType, DebitOrCredit};
    use beankeeper_cli::{
        db::{
            Actor, ConflictStrategy, PostEntryParams, PostTransactionParams,
            audit::{ReverseTransactionParams, reverse_transaction},
        },
        identity::{Admin, Granted, Post, Reconcile, Reverse},
    };

    type TestResult<T = ()> = Result<T, Box<dyn Error>>;

    pub(super) fn render_query(
        db: &Db,
        path: &str,
        query: &ViewQuery,
    ) -> Result<ViewPage, ViewError> {
        let session = crate::pairing::authenticated_browser()
            .map_err(|error| CliError::General(error.to_string()))?;
        render(db, &session, path, &query.clone().parse()?)
    }

    fn fixture() -> TestResult<Db> {
        let db = Db::open_in_memory()?;
        let owner = Actor::new("owner".parse()?);
        Granted::<Admin>::with_db(&db, &owner, |grant| {
            db::create_company(grant, &"acme".parse()?, &"Acme & Sons".parse()?, None)
        })?;
        Granted::<Admin>::with_db(&db, &owner, |grant| {
            db::create_company(grant, &"other".parse()?, &"Other".parse()?, None)
        })?;
        for company in ["acme", "other"] {
            Granted::<Admin>::with_db(&db, &owner, |grant| {
                db::create_account(
                    grant,
                    &db::CreateAccountParams {
                        company_slug: &company.parse()?,
                        code: "1000",
                        name: "Cash <bank>",
                        account_type: AccountType::Asset,
                        default_tax_category: None,
                    },
                )
            })?;
            Granted::<Admin>::with_db(&db, &owner, |grant| {
                db::create_account(
                    grant,
                    &db::CreateAccountParams {
                        company_slug: &company.parse()?,
                        code: "4000",
                        name: "Sales",
                        account_type: AccountType::Revenue,
                        default_tax_category: None,
                    },
                )
            })?;
        }
        Ok(db)
    }

    pub(super) fn post(
        db: &Db,
        description: &str,
        reference: &str,
        amount: i64,
        currency: &str,
        actor: &Actor,
    ) -> TestResult<TransactionId> {
        let entries = [
            PostEntryParams {
                account_code: "1000".into(),
                direction: DebitOrCredit::Debit,
                amount: amount.try_into()?,
                memo: Some("<memo>".into()),
                tax_category: None,
            },
            PostEntryParams {
                account_code: "4000".into(),
                direction: DebitOrCredit::Credit,
                amount: amount.try_into()?,
                memo: None,
                tax_category: Some("sales".into()),
            },
        ];
        let currency = currency.parse()?;
        let result = Granted::<Post>::with_db(db, actor, |grant| {
            db::post_transaction(
                grant,
                &PostTransactionParams {
                    company_slug: &"acme".parse()?,
                    description,
                    metadata: Some("{\"note\":\"<script>\"}"),
                    currency,
                    date: "2024-02-29".parse().map_err(|error: chrono::ParseError| {
                        beankeeper_cli::error::CliError::Validation(error.to_string())
                    })?,
                    entries: &entries,
                    correlate: None,
                    reference: Some(reference),
                    on_conflict: ConflictStrategy::Error,
                    actor,
                },
            )
        })?;
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
            &Actor::new("owner".parse()?),
        )?;
        let query = ViewQuery {
            company: Some("acme".into()),
            ..Default::default()
        };
        let snapshot = render_query(&db, "/", &query)?;
        let home = snapshot.content.as_str();
        assert!(home.contains("No cash accounts are classified"));
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
            &Actor::new("owner".parse()?),
        )?;
        post(
            &db,
            "Other description",
            "different",
            500,
            "USD",
            &Actor::new("owner".parse()?),
        )?;
        let (_, entries) = db::get_transaction(&Company::bind(db.read(), "acme".parse()?)?, id)?;
        Granted::<Reconcile>::with_db(&db, &Actor::new("owner".parse()?), |grant| {
            db::set_entry_status(
                grant,
                &"acme".parse()?,
                id,
                entries[0].id,
                db::EntryStatus::Cleared,
            )
        })?;
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
            let id = post(&db, code, code, amount, code, &Actor::new("owner".parse()?))?;
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
    fn register_hides_both_sides_of_an_audited_reversal_until_requested() -> TestResult {
        let db = fixture()?;
        let owner = Actor::new("owner".parse()?);
        let original = post(&db, "Cancelled sale", "cancelled", 100, "USD", &owner)?;
        let active = post(&db, "Active sale", "active", 100, "USD", &owner)?;
        let company = "acme".parse()?;
        let date = "2024-03-01".parse()?;
        let db::PostResult::Created(reversal) =
            Granted::<Reverse>::with_db(&db, &owner, |grant| {
                reverse_transaction(
                    grant,
                    &ReverseTransactionParams {
                        company_slug: &company,
                        id: original,
                        date,
                        description: Some("Cancel sale"),
                        reference: None,
                    },
                )
            })?
        else {
            return Err("reversal was not created".into());
        };
        let query = ViewQuery {
            account: Some("1000".into()),
            from: Some("2024-02-29".into()),
            to: Some("2024-02-29".into()),
            compare_from: Some("2024-03-01".into()),
            compare_to: Some("2024-03-01".into()),
            ..Default::default()
        };
        let checkbox = "name=\"include_reversed\" value=\"true\"";
        for include_reversed in [None, Some(false), Some(true)] {
            let page = render_query(
                &db,
                "/transactions",
                &ViewQuery {
                    include_reversed,
                    ..query.clone()
                },
            )?;
            let html = page.content.as_str();
            let show = include_reversed == Some(true);
            assert!(html.contains("Show reversed/cancelled pairs"));
            assert_eq!(html.contains(&format!("{checkbox} checked")), show);
            assert!(html.contains(&format!("id=\"transaction-{active}\"")));
            assert_eq!(
                html.contains(&format!("id=\"transaction-{original}\"")),
                show
            );
            assert_eq!(
                html.contains(&format!("id=\"comparison-transaction-{reversal}\"")),
                show
            );
            let primary_count = if show { 2 } else { 1 };
            let comparison_count = usize::from(show);
            assert!(html.contains(&format!("of {primary_count} candidates")));
            assert!(html.contains(&format!("of {comparison_count} candidates")));
        }
        for (id, description) in [(original, "Cancelled sale"), (reversal, "Cancel sale")] {
            let detail = render_query(&db, &format!("/transactions/{id}"), &ViewQuery::default())?;
            assert!(detail.content.as_str().contains(description));
            assert!(detail.content.as_str().contains("Audit trail"));
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
                &Actor::new("owner".parse()?),
            )?;
        }
        let query = ViewQuery {
            company: Some("acme".into()),
            search: Some("Sale".into()),
            include_reversed: Some(true),
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
        assert!(
            first
                .content
                .as_str()
                .contains("&amp;include_reversed=true&amp;offset=51")
        );
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
        assert!(
            second
                .content
                .as_str()
                .contains("search=Sale&amp;include_reversed=true\">Back to first results")
        );
        Ok(())
    }

    #[test]
    fn sparse_search_and_audit_scans_disclose_truncation_and_continue() -> TestResult {
        let db = fixture()?;
        let first_id = post(
            &db,
            "First",
            "first",
            100,
            "USD",
            &Actor::new("owner".parse()?),
        )?;
        for n in 1..1_001 {
            post(
                &db,
                "Other",
                &format!("ref{n}"),
                100,
                "USD",
                &Actor::new("owner".parse()?),
            )?;
        }
        post(
            &db,
            "Needle",
            "last",
            100,
            "USD",
            &Actor::new("owner".parse()?),
        )?;
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
        assert!(newest.content.as_str().contains("&amp;offset=1000"));
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
        post(
            &db,
            "USD sale",
            "usd",
            100,
            "USD",
            &Actor::new("owner".parse()?),
        )?;
        post(
            &db,
            "MXN sale",
            "mxn",
            200,
            "MXN",
            &Actor::new("owner".parse()?),
        )?;
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
        Granted::<Admin>::with_db(&db, &Actor::new("owner".parse()?), |grant| {
            db::create_company(grant, &"empty".parse()?, &"Empty".parse()?, None)
        })?;
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
    use super::tests::{post, render_query};
    use super::*;
    use beankeeper::types::AccountType;
    use beankeeper_cli::identity::{self, Admin, Granted, IssueRequest};
    use std::time::Duration;

    #[test]
    fn detail_shows_recorded_agent_uri_and_token_identifier() -> Result<(), Box<dyn Error>> {
        let dir = tempfile::tempdir()?;
        let db = Db::open(
            &dir.path().join("books.db"),
            Some(&"test-passphrase".into()),
        )?;
        let owner = db::Actor::new("owner".parse()?);
        Granted::<Admin>::with_db(&db, &owner, |grant| {
            db::create_company(grant, &"acme".parse()?, &"Acme".parse()?, None)
        })?;
        for (code, name, account_type) in [
            ("1000", "Cash", AccountType::Asset),
            ("4000", "Sales", AccountType::Revenue),
        ] {
            Granted::<Admin>::with_db(&db, &owner, |grant| {
                db::create_account(
                    grant,
                    &db::CreateAccountParams {
                        company_slug: &"acme".parse()?,
                        code,
                        name,
                        account_type,
                        default_tax_category: None,
                    },
                )
            })?;
        }
        Granted::<Admin>::with_db(&db, &owner, identity::init_root)?;
        let issued = Granted::<Admin>::with_db(&db, &owner, |grant| {
            identity::issue(
                grant,
                &IssueRequest {
                    name: "bookkeeper",
                    capabilities: &["ledger/read".into(), "ledger/post".into()],
                    ttl: Duration::from_secs(3600),
                    public_key: None,
                    generate_key: true,
                },
            )
        })?;
        let token = issued["token"].as_str().ok_or("missing token")?;
        let verified = identity::verify(db.read(), token)?;
        let uri = verified.uri().to_owned();
        let jti = verified.jti().to_owned();
        let actor = db::Actor::new("assistant".parse()?).with_identity(verified);
        let id = post(&db, "Agent sale", "identity-fixture", 100, "USD", &actor)?;
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
