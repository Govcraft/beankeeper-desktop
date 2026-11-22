//! Stable Desktop addresses, validated once at CLI, MCP and saved-URL boundaries.

mod filters;
pub(crate) mod mcp;
pub(crate) mod values;

use std::net::SocketAddr;

use beankeeper_cli::db::companies::CompanySlug;
use filters::{
    Balance, BookFilter, Budget, CashFlowFilters, Dashboard, Empty, Period, Reconciliation,
    ReportSelector, ReviewDetail, Summary, Transaction, Transactions, Trend, Validated, Vendor,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::{DesktopError, instance::ListenerAddress, service::LinkBooks};

/// The report catalog, route lookup, and address encoders share this set.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReportKind {
    Income,
    Balance,
    Cash,
    Trend,
    Budget,
    Schedule,
    Vendor,
    Reconciliation,
}

impl ReportKind {
    pub(crate) const ALL: [Self; 8] = [
        Self::Income,
        Self::Balance,
        Self::Cash,
        Self::Trend,
        Self::Budget,
        Self::Schedule,
        Self::Vendor,
        Self::Reconciliation,
    ];

    pub(crate) const fn path(self) -> &'static str {
        match self {
            Self::Income => "/reports/income-statement",
            Self::Balance => "/reports/balance-sheet",
            Self::Cash => "/reports/cash-flow",
            Self::Trend => "/reports/trend",
            Self::Budget => "/reports/budget-variance",
            Self::Schedule => "/reports/schedule-c",
            Self::Vendor => "/reports/1099",
            Self::Reconciliation => "/reconciliation",
        }
    }

    pub(crate) fn parse(path: &str) -> Option<Self> {
        if path == "/reports/reconciliation" {
            return Some(Self::Reconciliation);
        }
        Self::ALL.into_iter().find(|kind| kind.path() == path)
    }

    fn view(self) -> View {
        match self {
            Self::Income => View::IncomeStatement,
            Self::Balance => View::BalanceSheet,
            Self::Cash => View::CashFlow,
            Self::Trend => View::Trend,
            Self::Budget => View::BudgetVariance,
            Self::Schedule => View::ScheduleC,
            Self::Vendor => View::Vendor1099,
            Self::Reconciliation => View::Reconciliation,
        }
    }
}

// CLI selectors and the MCP schema share one list of view names.
macro_rules! desktop_views {
    ($($variant:ident as $wire:literal => $(#[$attr:meta])* $filters:ty),+ $(,)?) => {
        #[derive(Clone, Copy, Debug, Serialize, clap::ValueEnum)]
        pub(crate) enum View {
            $(#[serde(rename = $wire)] #[value(name = $wire)] $variant,)+
        }

        /// Each variant carries only the filters implemented by its destination.
        #[derive(Debug, Deserialize, Serialize, JsonSchema)]
        #[serde(tag = "view_id", deny_unknown_fields)]
        #[schemars(extend("type" = "object"))]
        pub(crate) enum ViewAddress {
            $(#[serde(rename = $wire)] $variant { $(#[$attr])* state: $filters },)+
        }
    };
}

desktop_views! {
    Dashboard as "dashboard" => #[serde(default)] Validated<Dashboard>,
    FinancialSummary as "financial-summary" => #[serde(default)] Validated<Summary>,
    Accounts as "accounts" => #[serde(default)] Validated<BookFilter>,
    Transactions as "transactions" => #[serde(default)] Validated<Transactions>,
    Transaction as "transaction" => Validated<Transaction>,
    Reports as "reports" => #[serde(default)] ReportSelector,
    IncomeStatement as "income-statement" => #[serde(default)] Validated<Period>,
    BalanceSheet as "balance-sheet" => #[serde(default)] Validated<Balance>,
    CashFlow as "cash-flow" => #[serde(default)] Validated<CashFlowFilters>,
    Trend as "trend" => #[serde(default)] Validated<Trend>,
    BudgetVariance as "budget-variance" => #[serde(default)] Validated<Budget>,
    ScheduleC as "schedule-c" => #[serde(default)] Validated<Period>,
    Vendor1099 as "1099" => #[serde(default)] Validated<Vendor>,
    Reconciliation as "reconciliation" => #[serde(default)] Validated<Reconciliation>,
    Review as "review" => #[serde(default)] Validated<BookFilter>,
    ReviewDetail as "review_detail" => Validated<ReviewDetail>,
    Settings as "settings" => #[serde(default)] Validated<Empty>,
}

pub(crate) const HOME: &str = "/";
pub(crate) const SUMMARY: &str = "/summary";
pub(crate) const ACCOUNTS: &str = "/accounts";
pub(crate) const TRANSACTIONS: &str = "/transactions";
pub(crate) const TRANSACTION_ROUTE: &str = "/transactions/{id}";
pub(crate) const REPORTS: &str = "/reports";
pub(crate) const REPORT_ROUTE: &str = "/reports/{id}";
pub(crate) const REVIEW: &str = "/review";
pub(crate) const REVIEW_ROUTE: &str = "/review/{id}";
pub(crate) const SETTINGS: &str = "/settings";

const TRANSACTION_PREFIX: &str = "/transactions/";
const REVIEW_PREFIX: &str = "/review/";

pub(crate) enum LedgerRoute {
    Home,
    Summary,
    Accounts,
    Register,
    Transaction(values::TransactionId),
}

impl LedgerRoute {
    pub(crate) fn parse(path: &str) -> Result<Self, &'static str> {
        match path {
            HOME => Ok(Self::Home),
            SUMMARY => Ok(Self::Summary),
            ACCOUNTS => Ok(Self::Accounts),
            TRANSACTIONS => Ok(Self::Register),
            path => path
                .strip_prefix(TRANSACTION_PREFIX)
                .ok_or("Unknown ledger view.")?
                .parse()
                .map(Self::Transaction)
                .map_err(|_| "Invalid transaction ID."),
        }
    }
}

fn parse_view_segment(
    path: &str,
) -> Result<(View, serde_json::Map<String, serde_json::Value>), &'static str> {
    let mut fields = serde_json::Map::new();
    let view = match path {
        REPORTS => View::Reports,
        REVIEW => View::Review,
        SETTINGS => View::Settings,
        path => {
            if let Some(id) = path.strip_prefix(REVIEW_PREFIX) {
                let id: values::ProposalId = id.parse().map_err(|_| "Invalid proposal ID.")?;
                fields.insert("proposal_id".into(), serde_json::json!(id));
                View::ReviewDetail
            } else if let Some(report) = ReportKind::parse(path) {
                report.view()
            } else {
                match LedgerRoute::parse(path)? {
                    LedgerRoute::Home => View::Dashboard,
                    LedgerRoute::Summary => View::FinancialSummary,
                    LedgerRoute::Accounts => View::Accounts,
                    LedgerRoute::Register => View::Transactions,
                    LedgerRoute::Transaction(id) => {
                        fields.insert("transaction_id".into(), serde_json::json!(id));
                        View::Transaction
                    }
                }
            }
        }
    };
    Ok((view, fields))
}

/// A route recognized by Desktop, with its browser query preserved as visited.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub(crate) struct LocalViewUrl(String);

impl LocalViewUrl {
    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for LocalViewUrl {
    type Error = &'static str;

    fn try_from(url: String) -> Result<Self, Self::Error> {
        if url.len() > 4096
            || url
                .chars()
                .any(|ch| ch.is_control() || matches!(ch, '#' | '\\'))
        {
            return Err("Expected a local Desktop view address.");
        }
        // Query semantics belong to the GET handler. The browser may report
        // empty optional form fields or the explicitly selected HTML format.
        let path = url.split_once('?').map_or(url.as_str(), |(path, _)| path);
        parse_view_segment(path)?;
        Ok(Self(url))
    }
}

impl ViewAddress {
    pub(crate) fn local_url(&self) -> Result<LocalViewUrl, &'static str> {
        self.path().map(LocalViewUrl)
    }

    pub(crate) fn path(&self) -> Result<String, &'static str> {
        let (route, fields) = match self {
            Self::Dashboard { state } => (HOME.into(), fields(state)?),
            Self::FinancialSummary { state } => (SUMMARY.into(), fields(state)?),
            Self::Accounts { state } => (ACCOUNTS.into(), fields(state)?),
            Self::Transactions { state } => (TRANSACTIONS.into(), fields(state)?),
            Self::Transaction { state } => (state.route(), fields(state)?),
            Self::Reports { state } => return state.path(),
            Self::IncomeStatement { state } => (ReportKind::Income.path().into(), fields(state)?),
            Self::BalanceSheet { state } => (ReportKind::Balance.path().into(), fields(state)?),
            Self::CashFlow { state } => (ReportKind::Cash.path().into(), fields(state)?),
            Self::Trend { state } => (ReportKind::Trend.path().into(), fields(state)?),
            Self::BudgetVariance { state } => (ReportKind::Budget.path().into(), fields(state)?),
            Self::ScheduleC { state } => (ReportKind::Schedule.path().into(), fields(state)?),
            Self::Vendor1099 { state } => (ReportKind::Vendor.path().into(), fields(state)?),
            Self::Reconciliation { state } => {
                (ReportKind::Reconciliation.path().into(), fields(state)?)
            }
            Self::Review { state } => (REVIEW.into(), fields(state)?),
            Self::ReviewDetail { state } => (state.route(), fields(state)?),
            Self::Settings { state } => (SETTINGS.into(), fields(state)?),
        };
        encode_path(route, &fields)
    }
}

impl TryFrom<serde_json::Value> for ViewAddress {
    type Error = LinkError;

    fn try_from(value: serde_json::Value) -> Result<Self, Self::Error> {
        serde_json::from_value(value)
            .map_err(|_| LinkError("Invalid view or filters. Check the tool schema."))
    }
}

#[derive(Serialize)]
struct ViewLink<'a> {
    #[serde(flatten)]
    address: &'a ViewAddress,
    path: LocalViewUrl,
    url: String,
}

impl<'a> TryFrom<(&'a ViewAddress, SocketAddr)> for ViewLink<'a> {
    type Error = LinkError;

    fn try_from((address, bound): (&'a ViewAddress, SocketAddr)) -> Result<Self, Self::Error> {
        let path = address.local_url().map_err(LinkError)?;
        let base = crate::server::local_url(bound);
        let url = format!("{}{}", base.trim_end_matches('/'), path.as_str());
        Ok(Self { address, path, url })
    }
}

impl ViewLink<'_> {
    fn payload(&self) -> Result<serde_json::Value, LinkError> {
        serde_json::to_value(self).map_err(|_| LinkError("Invalid view address."))
    }
}

fn fields(value: &impl Serialize) -> Result<serde_json::Value, &'static str> {
    serde_json::to_value(value).map_err(|_| "Invalid view filters.")
}

/// Serialization only: all semantic checks have already run during deserialization.
fn encode_path(mut route: String, fields: &serde_json::Value) -> Result<String, &'static str> {
    const ORDER: &[&str] = &[
        "company",
        "account",
        "category",
        "payee",
        "compare_from",
        "compare_to",
        "from",
        "to",
        "min_amount",
        "max_amount",
        "search",
        "status",
        "currency",
        "period",
        "metric",
        "year",
        "threshold",
        "cash_account",
        "receivable_account",
        "payable_account",
        "mode",
        "statement",
        "statement_balance",
        "link_key",
        "offset",
    ];
    for key in ORDER {
        let value = &fields[*key];
        if value.is_null() {
            continue;
        }
        let value = if let Some(accounts) = value.as_array() {
            if accounts.is_empty() {
                continue;
            }
            accounts
                .iter()
                .map(|account| account.as_str().ok_or("Invalid account selection."))
                .collect::<Result<Vec<_>, _>>()?
                .join(",")
        } else {
            value
                .as_str()
                .map_or_else(|| value.to_string(), str::to_owned)
        };
        route.push(if route.contains('?') { '&' } else { '?' });
        route.push_str(key);
        route.push('=');
        route.push_str(&crate::views::encode(&value));
    }
    if route.len() > 4096 {
        return Err("View address exceeds 4096 bytes.");
    }
    Ok(route)
}

/// Decode a local view address, rejecting redirects, unknown and duplicate fields.
pub(crate) fn parse_path(path: &str) -> Result<ViewAddress, &'static str> {
    if path.len() > 4096
        || !path.starts_with('/')
        || path.starts_with("//")
        || path
            .chars()
            .any(|c| c.is_control() || matches!(c, '#' | '\\'))
    {
        return Err("Expected a local Desktop view address.");
    }
    let url = reqwest::Url::parse(&format!("http://localhost{path}"))
        .map_err(|_| "Invalid view address.")?;
    // Do not let URL normalization turn an untrusted path into an allowed route.
    let raw_path = path.split('?').next().ok_or("Invalid view path.")?;
    if url.path() != raw_path {
        return Err("Invalid view path.");
    }
    let (view_id, mut fields) = parse_view_segment(raw_path)?;
    for (key, value) in url.query_pairs() {
        if fields.contains_key(key.as_ref())
            || ["transaction_id", "proposal_id", "report_kind"].contains(&key.as_ref())
        {
            return Err("Duplicate or unsupported address field.");
        }
        let value = match key.as_ref() {
            "offset" => serde_json::json!(value.parse::<i64>().map_err(|_| "Invalid offset.")?),
            "cash_account" | "receivable_account" | "payable_account" => {
                let accounts: values::AccountSelection =
                    value.parse().map_err(|_| "Invalid account selection.")?;
                serde_json::json!(accounts)
            }
            _ => serde_json::json!(value),
        };
        fields.insert(key.into_owned(), value);
    }
    let address: ViewAddress =
        serde_json::from_value(serde_json::json!({"view_id":view_id,"state":fields}))
            .map_err(|_| "Unknown or invalid view filters.")?;
    address.path()?;
    Ok(address)
}

/// Invalid view selectors or filters. No ledger or browser state was changed.
#[derive(Debug)]
pub struct LinkError(pub(crate) &'static str);

impl std::fmt::Display for LinkError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}
impl std::error::Error for LinkError {}

// Untrusted CLI spelling, consumed once by ViewAddress deserialization.
#[derive(Clone, Debug, Default, Deserialize, Serialize, JsonSchema, clap::Args)]

pub(crate) struct RawLinkFlags {
    #[serde(skip_serializing_if = "Option::is_none")]
    #[arg(long)]
    company: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[arg(long)]
    /// Exact upstream entry tax-category tag. Spending account categories use account.
    category: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[arg(long)]
    /// Exact metadata.payee string, preserving case and punctuation.
    payee: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[arg(long)]
    /// Inclusive comparison start, paired with `compare_to`.
    compare_from: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[arg(long)]
    /// Inclusive comparison end; as-of balance sheets use this alone.
    compare_to: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[arg(long)]
    account: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[arg(long)]
    from: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[arg(long)]
    to: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[arg(long)]
    min_amount: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[arg(long)]
    max_amount: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[arg(long)]
    search: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[arg(long)]
    status: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[arg(long)]
    offset: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[arg(long)]
    currency: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[arg(long)]
    transaction_id: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[arg(long)]
    proposal_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[arg(long)]
    report_kind: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[arg(long)]
    period: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[arg(long)]
    metric: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[arg(long)]
    year: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[arg(long)]
    threshold: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[arg(long)]
    cash_account: Option<values::AccountSelection>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[arg(long)]
    receivable_account: Option<values::AccountSelection>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[arg(long)]
    payable_account: Option<values::AccountSelection>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[arg(long)]
    mode: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[arg(long)]
    statement: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[arg(long)]
    statement_balance: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[arg(long)]
    link_key: Option<String>,
}

/// Options for generating a link without starting or opening Desktop.
#[derive(clap::Args, Debug)]
pub struct LinkOptions {
    /// Stable view ID, as used by `ui_get_link` and `ui_show_view`.
    #[arg(long, value_enum)]
    view_id: View,
    #[command(flatten)]
    state: RawLinkFlags,
    /// Select a particular listener when more than one instance serves these books.
    #[arg(long)]
    bind: Option<ListenerAddress>,
}

impl TryFrom<(LinkOptions, Option<&CompanySlug>)> for ViewAddress {
    type Error = LinkError;

    fn try_from(
        (mut options, company): (LinkOptions, Option<&CompanySlug>),
    ) -> Result<Self, Self::Error> {
        if options.state.company.is_none() && !matches!(options.view_id, View::Settings) {
            options.state.company = company.map(ToString::to_string);
        }
        Self::try_from(serde_json::json!({
            "view_id":options.view_id,"state":options.state
        }))
    }
}

/// Print a credential-free Desktop link using the same contract as MCP.
///
/// # Errors
/// Returns VALIDATION for malformed selectors or a named instance lookup error.
pub fn run(
    options: LinkOptions,
    ledger: &beankeeper_cli::cli::Cli,
    books: LinkBooks<'_>,
) -> Result<(), DesktopError> {
    let json = ledger.is_json();
    let result = (|| {
        let bind = options.bind;
        let address = ViewAddress::try_from((options, ledger.company.as_ref()))
            .map_err(DesktopError::Link)?;
        let bind = crate::service::link_address(books, bind)
            .map_err(|error| DesktopError::Service(Box::new(error)))?;
        let link = ViewLink::try_from((&address, bind.into())).map_err(DesktopError::Link)?;
        if json {
            link.payload()
                .map(|payload| payload.to_string())
                .map_err(DesktopError::Link)
        } else {
            Ok(link.url)
        }
    })();
    let result = match result {
        Ok(result) => result,
        Err(error) => {
            if json {
                let (code, message) = match &error {
                    DesktopError::Service(error) => (error.code(), error.to_string()),
                    DesktopError::Link(error) => ("VALIDATION", error.to_string()),
                    _ => return Err(error),
                };
                println!("{}", serde_json::json!({"code":code,"message":message}));
            }
            return Err(error);
        }
    };
    println!("{result}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn every_view_round_trips_with_all_applicable_filters() -> Result<(), Box<dyn std::error::Error>>
    {
        for (view, state) in [
            ("dashboard", json!({"company":"a & b", "currency":"USD"})),
            (
                "financial-summary",
                json!({"company":"acme","currency":"USD","from":"2026-10-01","to":"2026-10-31","compare_from":"2026-09-01","compare_to":"2026-09-30","cash_account":["1000","1100"],"receivable_account":"1210","payable_account":"2010"}),
            ),
            ("accounts", json!({"company":"acme"})),
            (
                "transactions",
                json!({"company":"acme","account":"5000","category":"sched-c:24b","payee":"Café & Tea / 100%","from":"2026-10-01","to":"2026-10-31","compare_from":"2026-09-01","compare_to":"2026-09-30","search":"<coffee>","status":"cleared","min_amount":"1","max_amount":"100000","offset":50,"currency":"USD"}),
            ),
            (
                "transaction",
                json!({"company":"acme","transaction_id":42,"offset":50}),
            ),
            ("reports", json!({"company":"acme","currency":"USD"})),
            (
                "income-statement",
                json!({"company":"acme","from":"2026-10-01","to":"2026-10-31","compare_from":"2026-09-01","compare_to":"2026-09-30"}),
            ),
            (
                "balance-sheet",
                json!({"to":"2026-10-31","compare_to":"2026-09-30"}),
            ),
            (
                "cash-flow",
                json!({"cash_account":"1000","from":"2026-10-01","to":"2026-10-31"}),
            ),
            (
                "cash-flow",
                json!({"cash_account":["1000","1100"],"from":"2026-10-01","to":"2026-10-31"}),
            ),
            (
                "trend",
                json!({"metric":"spending","period":"quarter","from":"2026-01-01","to":"2026-10-31"}),
            ),
            ("budget-variance", json!({"year":"2026"})),
            ("schedule-c", json!({"from":"2026-01-01","to":"2026-10-31"})),
            (
                "1099",
                json!({"threshold":"0","from":"2026-01-01","to":"2026-10-31"}),
            ),
            (
                "reconciliation",
                json!({"account":"1000","to":"2026-10-31","mode":"bank","statement":"S & 1","statement_balance":"1234.56","link_key":"applies_to"}),
            ),
            ("review", json!({"company":"acme"})),
            (
                "review_detail",
                json!({"company":"acme","proposal_id":"proposal_01h455vb4pex5vsknk084sn02q"}),
            ),
            ("settings", json!({})),
        ] {
            let address: ViewAddress =
                serde_json::from_value(json!({"view_id":view,"state":state}))?;
            let path = address.path()?;
            let decoded = parse_path(&path)?;
            assert_eq!(
                serde_json::to_value(&decoded)?,
                serde_json::to_value(&address)?
            );
        }
        let alias: ViewAddress = serde_json::from_value(
            json!({"view_id":"reports","state":{"report_kind":"trend","period":"month"}}),
        )?;
        assert_eq!(
            parse_path(&alias.path()?)?.path()?,
            "/reports/trend?period=month"
        );
        Ok(())
    }

    #[test]
    fn malformed_or_mismatched_filters_fail_before_an_address_exists() {
        for input in [
            json!({"view_id":"accounts","state":{"from":"2026-01-01"}}),
            json!({"view_id":"settings","state":{"company":null}}),
            json!({"view_id":"transaction","state":{}}),
            json!({"view_id":"transactions","state":{"transaction_id":42}}),
            json!({"view_id":"review_detail","state":{"proposal_id":"agent_01h455vb4pex5vsknk084sn02q"}}),
            json!({"view_id":"transactions","state":{"from":"2026-10-31","to":"2026-10-01"}}),
            json!({"view_id":"transactions","state":{"min_amount":"20","max_amount":"10"}}),
            json!({"view_id":"transactions","state":{"min_amount":"1.00"}}),
            json!({"view_id":"transactions","state":{"offset":i64::MAX}}),
            json!({"view_id":"balance-sheet","state":{"compare_from":"2026-01-01","compare_to":"2026-01-31"}}),
            json!({"view_id":"reports","state":{"report_kind":"trend","threshold":"600"}}),
            json!({"view_id":"reports","state":{"report_kind":"balance-sheet","from":"2026-01-01"}}),
            json!({"view_id":"reports","state":{"period":"month"}}),
            json!({"view_id":"reports","state":{"report_kind":"not-a-report"}}),
            json!({"view_id":"trend","state":{"metric":"not-a-metric"}}),
            json!({"view_id":"1099","state":{"threshold":"-1"}}),
            json!({"view_id":"1099","state":{"threshold":"1.001","currency":"USD"}}),
            json!({"view_id":"reconciliation","state":{"statement_balance":"NaN"}}),
            json!({"view_id":"reconciliation","state":{"statement_balance":"1.01","currency":"JPY"}}),
            json!({"view_id":"budget-variance","state":{"year":"2025","from":"2026-01-01","to":"2026-01-31"}}),
            json!({"view_id":"budget-variance","state":{"compare_from":"2026-01-02","compare_to":"2026-01-31"}}),
        ] {
            assert!(
                serde_json::from_value::<ViewAddress>(input.clone()).is_err(),
                "{input}"
            );
        }
    }

    #[test]
    fn schema_exposes_each_views_supported_filters_and_required_ids()
    -> Result<(), Box<dyn std::error::Error>> {
        let schema = schemars::schema_for!(ViewAddress).to_value();
        assert_eq!(schema["type"], "object");
        let variants = schema["oneOf"].as_array().ok_or("view alternatives")?;
        assert_eq!(variants.len(), 17);
        for (view, expected, required) in [
            ("settings", vec![], None),
            ("accounts", vec!["company"], None),
            (
                "transaction",
                vec!["company", "transaction_id", "offset"],
                Some("transaction_id"),
            ),
            (
                "balance-sheet",
                vec!["company", "currency", "to", "compare_to"],
                None,
            ),
            (
                "review_detail",
                vec!["company", "proposal_id"],
                Some("proposal_id"),
            ),
        ] {
            let variant = variants
                .iter()
                .find(|variant| variant["properties"]["view_id"]["const"] == view)
                .ok_or("missing view alternative")?;
            let mut state = &variant["properties"]["state"];
            while let Some(reference) = state["$ref"].as_str() {
                state = schema
                    .pointer(reference.strip_prefix('#').ok_or("local reference")?)
                    .ok_or("filter definition")?;
            }
            let empty = serde_json::Map::new();
            let properties = state["properties"].as_object().unwrap_or(&empty);
            assert_eq!(properties.len(), expected.len(), "{view}");
            for field in expected {
                assert!(properties.contains_key(field), "{view}: {field}");
            }
            assert_eq!(state["additionalProperties"], false, "{view}");
            if let Some(required) = required {
                assert!(
                    state["required"]
                        .as_array()
                        .ok_or("required fields")?
                        .contains(&json!(required))
                );
            }
        }
        Ok(())
    }

    #[test]
    fn unsafe_paths_and_unsupported_filters_cannot_become_destinations() {
        for path in [
            "https://evil.example/",
            "//evil.example/",
            "/\\evil.example/",
            "/transactions/../settings",
            "/%2f%2fevil.example",
            "/transactions#x",
            "/transactions\r\nLocation: https://evil.example",
            "/pair?token=secret",
            "/api/session/logout",
            "/reports/no-such-report",
            "/transactions/0",
            "/review/not-a-proposal",
            "/accounts?account=1000",
            "/settings?company=acme",
            "/transactions?company=a&company=b",
            "/transactions?company=a&%63ompany=b",
            "/transactions?unknown=x",
            "/transactions?from=2026-02-30",
            "/transactions?offset=-1",
            "/transactions?compare_from=2026-10-01",
            "/transactions?compare_to=2026-10-31",
            "/reports/balance-sheet?compare_to=",
            "/transactions?compare_from=2026-10-31&compare_to=2026-10-01",
            "/reports/balance-sheet?compare_from=2026-09-01&compare_to=2026-09-30",
            "/reports/income-statement?category=food",
            "/reports?from=2026-01-01",
        ] {
            assert!(parse_path(path).is_err(), "{path}");
        }
    }
}
