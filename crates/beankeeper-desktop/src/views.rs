//! Read-only HTML views backed exclusively by the upstream ledger query API.

use std::{error::Error, fmt, fmt::Write};

use axum::http::StatusCode;
use beankeeper_cli::{
    commands::report_currency::resolve_report_currency,
    db::{self, Db, EntryRow, ListAuditParams, ListTransactionParams, TransactionRow},
    error::CliError,
};
use serde::Deserialize;

use crate::{
    markup::{Attribute, Markup, markup, write_markup},
    money,
};

const PAGE_SIZE: usize = 50;
const SCAN_LIMIT: usize = 1_000;

#[derive(Clone, Default, Deserialize)]
pub(crate) struct ViewQuery {
    pub company: Option<String>,
    pub account: Option<String>,
    pub category: Option<String>,
    pub payee: Option<String>,
    pub compare_from: Option<String>,
    pub compare_to: Option<String>,
    pub from: Option<String>,
    pub to: Option<String>,
    pub min_amount: Option<String>,
    pub max_amount: Option<String>,
    pub search: Option<String>,
    pub status: Option<String>,
    pub offset: Option<usize>,
    pub currency: Option<String>,
}

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
    NotFound,
}

impl ViewError {
    pub(crate) fn status(&self) -> StatusCode {
        match self {
            Self::Invalid(_) | Self::ReportOptions(_) => StatusCode::BAD_REQUEST,
            Self::NotFound | Self::Ledger(CliError::NotFound(_)) => StatusCode::NOT_FOUND,
            Self::Ledger(CliError::Validation(_)) => StatusCode::BAD_REQUEST,
            Self::Ledger(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }
}

impl fmt::Display for ViewError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Invalid(message) => f.write_str(message),
            Self::ReportOptions(message) => f.write_str(message),
            Self::NotFound | Self::Ledger(CliError::NotFound(_)) => {
                f.write_str("The requested company, account or transaction was not found.")
            }
            Self::Ledger(CliError::Validation(_)) => {
                f.write_str("Choose a valid currency for these books.")
            }
            Self::Ledger(_) => {
                f.write_str("The ledger could not be read. Check the server log and try again.")
            }
        }
    }
}

impl Error for ViewError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Ledger(error) => Some(error),
            _ => None,
        }
    }
}

impl From<CliError> for ViewError {
    fn from(error: CliError) -> Self {
        Self::Ledger(error)
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

pub(crate) fn nonempty(value: &Option<String>) -> Option<&str> {
    value.as_deref().filter(|value| !value.is_empty())
}

pub(crate) fn valid_date(value: &str) -> bool {
    let bytes = value.as_bytes();
    if bytes.len() != 10
        || bytes[4] != b'-'
        || bytes[7] != b'-'
        || bytes
            .iter()
            .enumerate()
            .any(|(i, b)| i != 4 && i != 7 && !b.is_ascii_digit())
    {
        return false;
    }
    let (Ok(year), Ok(month), Ok(day)) = (
        value[..4].parse::<u32>(),
        value[5..7].parse::<u32>(),
        value[8..].parse::<u32>(),
    ) else {
        return false;
    };
    let days = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if year.is_multiple_of(400) || (year.is_multiple_of(4) && !year.is_multiple_of(100)) => {
            29
        }
        2 => 28,
        _ => 0,
    };
    year > 0 && day > 0 && day <= days
}

fn amount(value: &Option<String>) -> Result<Option<i64>, ViewError> {
    nonempty(value)
        .map(|value| {
            value
                .parse::<i64>()
                .ok()
                .filter(|amount| *amount >= 0)
                .ok_or(ViewError::Invalid(
                    "Amounts must be nonnegative whole minor units, such as cents.",
                ))
        })
        .transpose()
}

impl ViewQuery {
    pub(crate) fn comparison(&self) -> Option<Self> {
        self.compare_to.as_ref()?;
        Some(Self {
            from: self.compare_from.clone(),
            to: self.compare_to.clone(),
            compare_from: None,
            compare_to: None,
            offset: None,
            ..self.clone()
        })
    }

    pub(crate) fn validate(&self) -> Result<(), ViewError> {
        if [self.compare_from.as_deref(), self.compare_to.as_deref()]
            .into_iter()
            .flatten()
            .any(|date| !valid_date(date))
        {
            return Err(ViewError::Invalid(
                "Comparison dates must be valid YYYY-MM-DD calendar dates.",
            ));
        }
        if [
            nonempty(&self.from),
            nonempty(&self.to),
            nonempty(&self.compare_from),
            nonempty(&self.compare_to),
        ]
        .into_iter()
        .flatten()
        .any(|date| !valid_date(date))
        {
            return Err(ViewError::Invalid(
                "Dates must be valid calendar dates in YYYY-MM-DD format.",
            ));
        }
        if matches!((nonempty(&self.from), nonempty(&self.to)), (Some(from), Some(to)) if from > to)
        {
            return Err(ViewError::Invalid(
                "The start date must not be after the end date.",
            ));
        }
        match (nonempty(&self.compare_from), nonempty(&self.compare_to)) {
            (Some(from), Some(to)) if from <= to => {}
            (None, None | Some(_)) => {}
            _ => {
                return Err(ViewError::Invalid(
                    "Comparison requires compare_from and compare_to in chronological order.",
                ));
            }
        }
        if matches!((amount(&self.min_amount)?, amount(&self.max_amount)?), (Some(min), Some(max)) if min > max)
        {
            return Err(ViewError::Invalid(
                "Minimum amount must not exceed maximum amount.",
            ));
        }
        if nonempty(&self.status)
            .is_some_and(|status| !["uncleared", "cleared", "reconciled"].contains(&status))
        {
            return Err(ViewError::Invalid(
                "Clearance must be uncleared, cleared or reconciled.",
            ));
        }
        if self.offset.unwrap_or(0) > i64::MAX as usize - SCAN_LIMIT {
            return Err(ViewError::Invalid("The pagination offset is too large."));
        }
        Ok(())
    }

    fn url(&self, path: &str, company: &str, offset: usize) -> String {
        let mut url = format!("{path}?company={}", encode(company));
        for (name, value) in [
            ("account", &self.account),
            ("category", &self.category),
            ("payee", &self.payee),
            ("compare_from", &self.compare_from),
            ("compare_to", &self.compare_to),
            ("from", &self.from),
            ("to", &self.to),
            ("min_amount", &self.min_amount),
            ("max_amount", &self.max_amount),
            ("search", &self.search),
            ("status", &self.status),
            ("currency", &self.currency),
        ] {
            if let Some(value) = nonempty(value) {
                let _ = write!(url, "&{name}={}", encode(value));
            }
        }
        if offset > 0 {
            let _ = write!(url, "&offset={offset}");
        }
        url
    }
}

pub(crate) fn input(name: &str, label: &str, kind: &str, value: &Option<String>) -> Markup {
    markup!(
        "<label>{}<input name=\"{}\" type=\"{}\" value=\"{}\"></label>",
        label,
        name,
        kind,
        value.as_deref().unwrap_or_default()
    )
}

fn company_form(
    companies: &[db::CompanyRow],
    company: &str,
    query: &ViewQuery,
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
    for (name, value) in [
        ("category", &query.category),
        ("payee", &query.payee),
        ("compare_from", &query.compare_from),
        ("compare_to", &query.compare_to),
    ] {
        if let Some(value) = value {
            write_markup!(
                html,
                "<input type=\"hidden\" name=\"{}\" value=\"{}\">",
                name,
                value
            );
        }
    }

    if path == "/transactions" {
        html.append(&input("account", "Account code", "text", &query.account));
        html.append(&input("from", "From date (inclusive)", "date", &query.from));
        html.append(&input("to", "To date (inclusive)", "date", &query.to));
        html.append(&input(
            "min_amount",
            "Minimum entry amount (minor units, inclusive)",
            "number",
            &query.min_amount,
        ));
        html.append(&input(
            "max_amount",
            "Maximum entry amount (minor units, inclusive)",
            "number",
            &query.max_amount,
        ));
        html.append(&input(
            "search",
            "Payee / description or reference contains",
            "search",
            &query.search,
        ));
        write_markup!(
            html,
            "<label>Clearance<select name=\"status\"><option value=\"\">Any clearance</option>"
        );
        for status in ["uncleared", "cleared", "reconciled"] {
            write_markup!(
                html,
                "<option{}>{}</option>",
                Attribute::Selected(nonempty(&query.status) == Some(status)),
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
            &query.currency,
        ));
    }
    write_markup!(html, "<button type=\"submit\">Apply</button></form>");
    html
}

pub(crate) fn render(db: &Db, path: &str, query: &ViewQuery) -> Result<ViewPage, ViewError> {
    query.validate()?;
    if path == "/transactions"
        && query.compare_to.is_some()
        && nonempty(&query.compare_from).is_none()
    {
        return Err(ViewError::Invalid(
            "Register comparison requires compare_from and compare_to.",
        ));
    }
    let (title, description, nav) = match path {
        "/" => (
            "Dashboard",
            "Your balances, recent activity and next steps.",
            "/",
        ),
        "/accounts" => (
            "Accounts",
            "Browse companies and their complete chart of accounts.",
            "/accounts",
        ),
        "/transactions" => (
            "Transactions",
            "Filter your register. Entry amounts are shown in their transaction currency.",
            "/transactions",
        ),
        path if path.starts_with("/transactions/") => (
            "Transaction detail",
            "Entries, metadata and recorded audit history.",
            "/transactions",
        ),
        _ => return Err(ViewError::NotFound),
    };
    let companies = db::list_companies(db.conn())?;
    let selected =
        nonempty(&query.company).or_else(|| companies.first().map(|company| company.slug.as_str()));
    let Some(company) = selected else {
        return Ok(ViewPage {
            title,
            description,
            nav,
            content: markup!(
                "<p class=\"empty\">No companies yet. Ask your agent to initialize your books and create a company.</p>"
            ),
        });
    };
    if !companies.iter().any(|item| item.slug == company) {
        return Err(ViewError::NotFound);
    }
    if let Some(account) = nonempty(&query.account) {
        db::get_account(db.conn(), company, account)?;
    }
    let mut content = company_form(&companies, company, query, path);
    match path {
        "/" => dashboard(db, company, query, &mut content)?,
        "/accounts" => accounts(db, company, query, &mut content)?,
        "/transactions" => {
            register(db, company, query, RegisterSection::Primary, &mut content)?;
            if let Some(comparison) = query.comparison() {
                write_markup!(
                    content,
                    "<section aria-label=\"Comparison period\"><h2>Comparison period: {} through {}</h2>",
                    comparison.from.as_deref().unwrap_or_default(),
                    comparison.to.as_deref().unwrap_or_default()
                );
                register(
                    db,
                    company,
                    &comparison,
                    RegisterSection::Comparison,
                    &mut content,
                )?;
                write_markup!(content, "</section>");
            }
        }
        _ => {
            let id = path
                .trim_start_matches("/transactions/")
                .parse::<i64>()
                .ok()
                .filter(|id| *id > 0)
                .ok_or(ViewError::NotFound)?;
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

fn dashboard(
    db: &Db,
    company: &str,
    query: &ViewQuery,
    html: &mut Markup,
) -> Result<(), ViewError> {
    write_markup!(html, "<section><h2>Account balances</h2>");
    match resolve_report_currency(db.conn(), company, nonempty(&query.currency)) {
        Ok(currency) => {
            let code = currency.code();
            write_markup!(
                html,
                "<p>Currency: {}. Amounts are shown in the selected currency. Net is debits minus credits; negative balances are credit balances.</p><div class=\"table-scroll\"><table><caption>Balances for {}</caption><thead><tr><th scope=\"col\">Account</th><th scope=\"col\">Debit</th><th scope=\"col\">Credit</th><th scope=\"col\">Net balance</th></tr></thead><tbody>",
                code,
                company
            );
            let balances =
                db::compute_trial_balance(db.conn(), company, None, None, None, Some(code))?;
            if balances.is_empty() {
                write_markup!(
                    html,
                    "<tr><td colspan=\"4\">No accounts yet. Ask your agent to set up your chart of accounts.</td></tr>"
                );
            }
            for row in balances {
                let account_query = ViewQuery {
                    company: Some(company.into()),
                    account: Some(row.code.clone()),
                    currency: Some(code.into()),
                    ..Default::default()
                };
                write_markup!(
                    html,
                    "<tr><th scope=\"row\"><a href=\"{}\">{} · {}</a></th><td class=\"amount\">{}</td><td class=\"amount\">{}</td><td class=\"amount\">{}</td></tr>",
                    account_query.url("/transactions", company, 0),
                    &row.code,
                    &row.name,
                    money::minor(row.debit_total, code),
                    money::minor(row.credit_total, code),
                    money::minor(
                        i128::from(row.debit_total) - i128::from(row.credit_total),
                        code
                    )
                );
            }
            write_markup!(html, "</tbody></table></div>");
        }
        Err(CliError::Validation(_)) if nonempty(&query.currency).is_none() => write_markup!(
            html,
            "<p class=\"empty\">These books use multiple currencies. Enter a currency code above to view balances without mixing currencies.</p>"
        ),
        Err(error) => return Err(error.into()),
    }
    write_markup!(html, "</section><section><h2>Recent activity</h2>");
    let mut params = ListTransactionParams::for_company(company);
    let total = db::count_transactions(db.conn(), &params)?;
    params.limit = 5;
    params.offset = total.saturating_sub(5).max(0);
    let mut recent = db::list_transactions(db.conn(), &params)?;
    recent.reverse();
    transaction_table(db, &recent, company, RegisterSection::Primary, html)?;
    write_markup!(
        html,
        "<p>{} transactions in these books. <a href=\"{}\">Browse the register</a></p></section><section><h2>Next actions</h2><p><a href=\"/accounts?company={}\">Browse accounts</a> or ask your connected agent to prepare your next bookkeeping task.</p></section>",
        total,
        query.url("/transactions", company, 0),
        encode(company)
    );
    Ok(())
}

fn accounts(db: &Db, company: &str, query: &ViewQuery, html: &mut Markup) -> Result<(), ViewError> {
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
    for kind in ["asset", "liability", "equity", "revenue", "expense"] {
        let group: Vec<_> = rows.iter().filter(|row| row.account_type == kind).collect();
        if group.is_empty() {
            continue;
        }
        write_markup!(html, "<li><strong>{}</strong><ul>", kind);
        for account in group {
            let link = ViewQuery {
                account: Some(account.code.clone()),
                ..query.clone()
            }
            .url("/transactions", company, 0);
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
            write_markup!(
                html,
                "<li>{}: {} {} ({})</li>",
                &entry.account_code,
                &entry.direction,
                &money::minor(entry.amount, &row.currency),
                &entry.status
            );
        }
        write_markup!(html, "</ul></td></tr>");
    }
    write_markup!(html, "</tbody></table></div>");
    Ok(())
}

fn matches_extra(
    query: &ViewQuery,
    txn: &TransactionRow,
    entries: &[EntryRow],
) -> Result<bool, ViewError> {
    if let Some(search) = nonempty(&query.search) {
        let search = search.to_lowercase();
        if !txn.description.to_lowercase().contains(&search)
            && !txn
                .reference
                .as_deref()
                .unwrap_or_default()
                .to_lowercase()
                .contains(&search)
        {
            return Ok(false);
        }
    }
    if let Some(payee) = nonempty(&query.payee) {
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
    let min = amount(&query.min_amount)?;
    let max = amount(&query.max_amount)?;
    Ok(entries.iter().any(|entry| {
        nonempty(&query.category)
            .is_none_or(|category| entry.tax_category.as_deref() == Some(category))
            && nonempty(&query.account).is_none_or(|account| entry.account_code == account)
            && nonempty(&query.status).is_none_or(|status| entry.status == status)
            && min.is_none_or(|min| entry.amount >= min)
            && max.is_none_or(|max| entry.amount <= max)
    }))
}

fn register(
    db: &Db,
    company: &str,
    query: &ViewQuery,
    section: RegisterSection,
    html: &mut Markup,
) -> Result<(), ViewError> {
    let mut params = ListTransactionParams::for_company(company);
    params.account_filter = nonempty(&query.account);
    params.tax_category_filter = nonempty(&query.category);
    params.from_date = nonempty(&query.from);
    params.to_date = nonempty(&query.to);
    let currency = nonempty(&query.currency)
        .map(|code| resolve_report_currency(db.conn(), company, Some(code)))
        .transpose()?;
    params.currency_filter = currency.as_ref().map(|currency| currency.code());
    // Upstream bounds are exclusive; desktop labels and URLs use inclusive bounds.
    params.amount_gt = amount(&query.min_amount)?.and_then(|min| min.checked_sub(1));
    let max = amount(&query.max_amount)?;
    params.amount_lt = max.and_then(|max| max.checked_add(1));
    let total = usize::try_from(db::count_transactions(db.conn(), &params)?)
        .map_err(|_| ViewError::Invalid("The ledger is too large to paginate."))?;
    let start = query.offset.unwrap_or(0);
    let mut cursor = start;
    let mut rows = Vec::new();
    while rows.len() < PAGE_SIZE && cursor < total && cursor - start < SCAN_LIMIT {
        params.offset = i64::try_from(cursor)
            .map_err(|_| ViewError::Invalid("The pagination offset is too large."))?;
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
        ("Tax category", &query.category),
        ("Payee (exact)", &query.payee),
        ("From date", &query.from),
        ("Through date", &query.to),
    ] {
        if let Some(value) = nonempty(value) {
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
    query: &ViewQuery,
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
    for entry in &entries {
        write_markup!(
            html,
            "<tr><th scope=\"row\">{}</th><td>{}</td><td class=\"amount\">{}</td><td>{}</td><td>{}</td><td>{}</td></tr>",
            &entry.account_code,
            &entry.direction,
            &money::minor(entry.amount, &txn.currency),
            &entry.status,
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
    query: &ViewQuery,
    txn: &TransactionRow,
    entries: &[EntryRow],
    html: &mut Markup,
) -> Result<(), ViewError> {
    let start = query.offset.unwrap_or(0);
    let params = ListAuditParams {
        agent_uri: None,
        company_slug: Some(company),
        entity: None,
        action: None,
        limit: SCAN_LIMIT as i64,
        offset: start as i64,
    };
    let total = usize::try_from(db::count_audit(db.conn(), &params)?)
        .map_err(|_| ViewError::Invalid("The audit trail is too large to paginate."))?;
    let rows = db::list_audit(db.conn(), &params)?;
    let cursor = start + rows.len();
    let id = txn.id.to_string();
    let mut shown = 0;
    write_markup!(
        html,
        "<section><h2>Audit trail</h2><p>Recorded transaction changes and changes to its entries, newest first.</p>"
    );
    for row in rows.iter().filter(|row| {
        (row.entity == "transaction" && row.entity_id == id)
            || (row.entity == "entry"
                && entries
                    .iter()
                    .any(|entry| entry.id.to_string() == row.entity_id))
    }) {
        shown += 1;
        write_markup!(
            html,
            "<article><h3>{}: {} {}</h3><dl>",
            &row.changed_at,
            &row.entity,
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
    use super::*;
    use beankeeper_cli::db::{Actor, ConflictStrategy, PostEntryParams, PostTransactionParams};

    type TestResult<T = ()> = Result<T, Box<dyn Error>>;

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
            _ => Err("expected created transaction".into()),
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
                .validate()
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
                .validate()
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
                offset: Some(usize::MAX),
                ..Default::default()
            },
        ] {
            assert!(query.validate().is_err());
        }
    }

    #[test]
    fn links_escape_query_and_preserve_filter_values() {
        let query = ViewQuery {
            company: Some("acme".into()),
            search: Some("O'Reilly & 20% <x>".into()),
            status: Some("cleared".into()),
            min_amount: Some("100".into()),
            ..Default::default()
        };
        let url = query.url("/transactions", "acme & son", 50);
        assert!(url.contains("&offset=50"));
        let link = markup!("<a href=\"{}\">Continue</a>", url);
        let url = link.as_str();
        assert!(url.contains("company=acme%20%26%20son"));
        assert!(url.contains("search=O%27Reilly%20%26%2020%25%20%3Cx%3E"));
        assert!(url.contains("&amp;offset=50"));
        assert!(url.contains("&amp;status=cleared"));
    }

    #[test]
    fn dashboard_accounts_and_detail_use_upstream_data_and_escape_content() -> TestResult {
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
        let dashboard = render(&db, "/", &query)?;
        assert!(dashboard.content.as_str().contains("$123.45"));
        assert!(dashboard.content.as_str().contains("Net balance"));
        assert!(
            dashboard
                .content
                .as_str()
                .contains("class=\"amount\">-$123.45</td>")
        );
        assert!(dashboard.content.as_str().contains("Recent activity"));
        assert!(
            dashboard
                .content
                .as_str()
                .contains(&format!("/transactions/{id}?company=acme"))
        );
        assert!(!dashboard.content.as_str().contains("<script>"));
        let accounts = render(&db, "/accounts", &query)?;
        assert!(accounts.content.as_str().contains("2 accounts"));
        assert!(accounts.content.as_str().contains("Cash &lt;bank&gt;"));
        assert!(
            accounts
                .content
                .as_str()
                .contains("<strong>revenue</strong>")
        );
        let detail = render(&db, &format!("/transactions/{id}"), &query)?;
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
            render(&db, &format!("/transactions/{id}"), &other)
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
        let page = render(&db, "/transactions", &query)?;
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
            render(&db, "/transactions", &category_on_other_leg)?
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
            render(&db, "/transactions", &different_account)?
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
            let register = render(&db, "/transactions", &query)?;
            assert!(register.content.as_str().contains(formatted));
            let detail = render(&db, &format!("/transactions/{id}"), &query)?;
            assert!(
                detail
                    .content
                    .as_str()
                    .contains(&format!("class=\"amount\">{formatted}</td>"))
            );
            let dashboard = render(
                &db,
                "/",
                &ViewQuery {
                    currency: Some(code.into()),
                    ..query
                },
            )?;
            assert!(
                dashboard
                    .content
                    .as_str()
                    .contains(&format!("class=\"amount\">{formatted}</td>"))
            );
            assert!(
                dashboard
                    .content
                    .as_str()
                    .contains(&format!("class=\"amount\">-{formatted}</td>"))
            );
            assert!(
                !dashboard
                    .content
                    .as_str()
                    .contains("Totals and net balances in minor units")
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
        let first = render(&db, "/transactions", &query)?;
        assert!(
            first
                .content
                .as_str()
                .contains("50 matching transactions shown")
        );
        assert!(first.content.as_str().contains("&amp;offset=51"));
        assert!(first.content.as_str().contains("search=Sale"));
        let second = render(
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
        let first = render(&db, "/transactions", &query)?;
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
        let continued = render(
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
        let newest = render(&db, &detail_path, &audit_query)?;
        assert!(newest.content.as_str().contains("Continue audit trail"));
        assert!(
            newest
                .content
                .as_str()
                .contains("No audit records for this transaction in this portion")
        );
        let oldest = render(
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
    fn mixed_currency_dashboard_requires_currency_before_aggregating() -> TestResult {
        let db = fixture()?;
        post(&db, "USD sale", "usd", 100, "USD", &Actor::new("owner"))?;
        post(&db, "MXN sale", "mxn", 200, "MXN", &Actor::new("owner"))?;
        let query = ViewQuery {
            company: Some("acme".into()),
            ..Default::default()
        };
        assert!(
            render(&db, "/", &query)?
                .content
                .as_str()
                .contains("without mixing currencies")
        );
        let selected = render(
            &db,
            "/",
            &ViewQuery {
                currency: Some("USD".into()),
                ..query
            },
        )?;
        assert!(selected.content.as_str().contains("Currency: USD"));
        assert!(
            selected
                .content
                .as_str()
                .contains("class=\"amount\">$1.00</td>")
        );
        assert!(
            !selected
                .content
                .as_str()
                .contains("class=\"amount\">$3.00</td>")
        );
        Ok(())
    }

    #[test]
    fn empty_and_invalid_company_states_are_explicit() -> TestResult {
        let db = Db::open_in_memory()?;
        assert!(
            render(&db, "/", &ViewQuery::default())?
                .content
                .as_str()
                .contains("No companies yet")
        );
        db::create_company(db.conn(), "empty", "Empty", None)?;
        assert!(
            render(&db, "/accounts", &ViewQuery::default())?
                .content
                .as_str()
                .contains("No accounts yet")
        );
        assert!(
            render(&db, "/transactions", &ViewQuery::default())?
                .content
                .as_str()
                .contains("No transactions to show")
        );
        assert_eq!(
            render(
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
        let page = render(
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
