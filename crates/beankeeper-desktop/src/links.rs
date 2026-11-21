//! Stable Desktop addresses without browser navigation or ledger access.

use crate::{reports::ReportQuery, views::ViewQuery};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Deserialize, Serialize, JsonSchema, clap::ValueEnum)]
#[serde(rename_all = "snake_case")]
pub(crate) enum View {
    Dashboard,
    Accounts,
    Transactions,
    Transaction,
    Reports,
    #[serde(rename = "income-statement")]
    IncomeStatement,
    #[serde(rename = "balance-sheet")]
    BalanceSheet,
    #[serde(rename = "cash-flow")]
    CashFlow,
    Trend,
    #[serde(rename = "budget-variance")]
    BudgetVariance,
    #[serde(rename = "schedule-c")]
    ScheduleC,
    #[serde(rename = "1099")]
    #[value(name = "1099")]
    Vendor1099,
    Reconciliation,
    Review,
    #[value(name = "review_detail")]
    ReviewDetail,
    Settings,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, JsonSchema, clap::Args)]
#[serde(deny_unknown_fields)]
pub(crate) struct AddressState {
    #[arg(long)]
    pub(crate) company: Option<String>,
    #[arg(long)]
    /// Exact upstream entry tax-category tag. Spending account categories use account.
    pub(crate) category: Option<String>,
    #[arg(long)]
    /// Exact metadata.payee string, preserving case and punctuation.
    pub(crate) payee: Option<String>,
    #[arg(long)]
    /// Inclusive comparison start, paired with compare_to.
    pub(crate) compare_from: Option<String>,
    #[arg(long)]
    /// Inclusive comparison end; as-of balance sheets use this alone.
    pub(crate) compare_to: Option<String>,
    #[arg(long)]
    pub(crate) account: Option<String>,
    #[arg(long)]
    pub(crate) from: Option<String>,
    #[arg(long)]
    pub(crate) to: Option<String>,
    #[arg(long)]
    pub(crate) min_amount: Option<String>,
    #[arg(long)]
    pub(crate) max_amount: Option<String>,
    #[arg(long)]
    pub(crate) search: Option<String>,
    #[arg(long)]
    pub(crate) status: Option<String>,
    #[arg(long)]
    pub(crate) offset: Option<usize>,
    #[arg(long)]
    pub(crate) currency: Option<String>,
    #[arg(long)]
    pub(crate) transaction_id: Option<i64>,
    #[arg(long)]
    pub(crate) proposal_id: Option<String>,
    #[arg(long)]
    pub(crate) report_kind: Option<String>,
    #[arg(long)]
    pub(crate) period: Option<String>,
    #[arg(long)]
    pub(crate) metric: Option<String>,
    #[arg(long)]
    pub(crate) year: Option<String>,
    #[arg(long)]
    pub(crate) threshold: Option<String>,
    #[arg(long)]
    pub(crate) cash_account: Option<String>,
    #[arg(long)]
    pub(crate) mode: Option<String>,
    #[arg(long)]
    pub(crate) statement: Option<String>,
    #[arg(long)]
    pub(crate) statement_balance: Option<String>,
    #[arg(long)]
    pub(crate) link_key: Option<String>,
}

/// Stable, credential-free view selector shared by CLI, MCP and navigation.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct ViewAddress {
    pub(crate) view_id: View,
    #[serde(default)]
    pub(crate) state: AddressState,
}

impl ViewAddress {
    pub(crate) fn path(&self) -> Result<String, &'static str> {
        let state = &self.state;
        let allowed: &[&str] = match self.view_id {
            View::Dashboard => &["company", "currency"],
            View::Accounts => &["company"],
            View::Settings => &[],
            View::Transaction => &["company", "transaction_id", "offset"],
            View::Review => &["company"],
            View::ReviewDetail => &["company", "proposal_id"],
            _ => &[],
        };
        if matches!(
            self.view_id,
            View::Dashboard
                | View::Accounts
                | View::Settings
                | View::Transaction
                | View::Review
                | View::ReviewDetail
        ) {
            let fields = serde_json::to_value(state).map_err(|_| "Invalid filters.")?;
            if fields.as_object().is_some_and(|fields| {
                fields
                    .iter()
                    .any(|(name, value)| !value.is_null() && !allowed.contains(&name.as_str()))
            }) {
                return Err("Unsupported filters for this view.");
            }
        }
        if state.transaction_id.is_some() && !matches!(self.view_id, View::Transaction)
            || state.proposal_id.is_some() && !matches!(self.view_id, View::ReviewDetail)
        {
            return Err("Record IDs require their matching detail view.");
        }
        let report_id = match self.view_id {
            View::Reports => state.report_kind.as_deref(),
            View::IncomeStatement => Some("income-statement"),
            View::BalanceSheet => Some("balance-sheet"),
            View::CashFlow => Some("cash-flow"),
            View::Trend => Some("trend"),
            View::BudgetVariance => Some("budget-variance"),
            View::ScheduleC => Some("schedule-c"),
            View::Vendor1099 => Some("1099"),
            View::Reconciliation => Some("reconciliation"),
            _ => None,
        };
        let is_report = report_id.is_some() || matches!(self.view_id, View::Reports);
        if state.report_kind.is_some() && !matches!(self.view_id, View::Reports) {
            return Err(
                "report_kind requires the reports view; direct report IDs need no report_kind.",
            );
        }
        let report_fields = [
            ("period", &state.period, "trend"),
            ("metric", &state.metric, "trend"),
            ("year", &state.year, "budget-variance"),
            ("threshold", &state.threshold, "1099"),
            ("cash_account", &state.cash_account, "cash-flow"),
            ("mode", &state.mode, "reconciliation"),
            ("statement", &state.statement, "reconciliation"),
            (
                "statement_balance",
                &state.statement_balance,
                "reconciliation",
            ),
            ("link_key", &state.link_key, "reconciliation"),
        ];
        if report_fields
            .iter()
            .any(|(_, value, required)| value.is_some() && report_id != Some(*required))
        {
            return Err(
                "Report filters require their matching report view. Use from/to for register dates.",
            );
        }
        if is_report {
            if report_id.is_some_and(|id| !crate::reports::VIEW_IDS.contains(&id)) {
                return Err("Unknown report_kind. Use a stable report ID from the tool catalog.");
            }
            if state.category.is_some()
                || state.payee.is_some()
                || state.offset.is_some()
                || state.min_amount.is_some()
                || state.max_amount.is_some()
                || state.search.is_some()
                || state.status.is_some()
                || (state.account.is_some() && report_id != Some("reconciliation"))
                || (report_id.is_none() && (state.from.is_some() || state.to.is_some()))
            {
                return Err("Unsupported report filters. Use the report's URL-restorable filters.");
            }
        }
        let path = match self.view_id {
            View::Dashboard => "/".into(),
            View::Settings => "/settings".into(),
            View::Accounts => "/accounts".into(),
            View::Transactions => "/transactions".into(),
            View::Transaction => format!(
                "/transactions/{}",
                state
                    .transaction_id
                    .filter(|id| *id > 0)
                    .ok_or("transaction requires a positive transaction_id.")?
            ),
            View::Review => "/review".into(),
            View::ReviewDetail => {
                let id = state
                    .proposal_id
                    .as_deref()
                    .ok_or("review_detail requires proposal_id.")?
                    .parse::<beankeeper_cli::db::proposals::ProposalId>()
                    .map_err(|_| "proposal_id must be a proposal TypeID.")?;
                format!("/review/{id}")
            }
            _ if is_report => match report_id {
                Some("reconciliation") => "/reconciliation".into(),
                Some(id) => format!("/reports/{id}"),
                None => "/reports".into(),
            },
            _ => return Err("Unknown view."),
        };
        if matches!(self.view_id, View::Review | View::ReviewDetail)
            && (state.offset.is_some()
                || [
                    state.account.as_ref(),
                    state.from.as_ref(),
                    state.to.as_ref(),
                    state.min_amount.as_ref(),
                    state.max_amount.as_ref(),
                    state.search.as_ref(),
                    state.status.as_ref(),
                    state.currency.as_ref(),
                ]
                .iter()
                .any(|v| v.is_some()))
        {
            return Err(
                "Review views support company only; register filters require transactions.",
            );
        }
        if matches!(self.view_id, View::Transactions)
            && state.compare_to.is_some()
            && state.compare_from.as_deref().is_none_or(str::is_empty)
        {
            return Err("Register comparison requires compare_from and compare_to.");
        }
        let query = ViewQuery {
            company: state.company.clone(),
            account: state.account.clone(),
            category: state.category.clone(),
            payee: state.payee.clone(),
            compare_from: state.compare_from.clone(),
            compare_to: state.compare_to.clone(),
            from: state.from.clone(),
            to: state.to.clone(),
            min_amount: state.min_amount.clone(),
            max_amount: state.max_amount.clone(),
            search: state.search.clone(),
            status: state.status.clone(),
            offset: state.offset,
            currency: state.currency.clone(),
        };
        query
            .validate()
            .map_err(|_| "Invalid date, amount, clearance or pagination filters.")?;
        if is_report {
            let query: ReportQuery = serde_json::from_value(
                serde_json::to_value(state).map_err(|_| "Invalid report filters.")?,
            )
            .map_err(|_| "Invalid report filters.")?;
            query.validate_navigation(&path).map_err(
                |_| "Invalid report date range, period, metric, year or reconciliation mode.",
            )?;
        }
        let mut url = path;
        for (name, value) in [
            ("company", &state.company),
            ("account", &state.account),
            ("category", &state.category),
            ("payee", &state.payee),
            ("compare_from", &state.compare_from),
            ("compare_to", &state.compare_to),
            ("from", &state.from),
            ("to", &state.to),
            ("min_amount", &state.min_amount),
            ("max_amount", &state.max_amount),
            ("search", &state.search),
            ("status", &state.status),
            ("currency", &state.currency),
        ]
        .into_iter()
        .chain(report_fields.iter().map(|(name, value, _)| (*name, *value)))
        {
            if let Some(value) = value {
                url.push(if url.contains('?') { '&' } else { '?' });
                url.push_str(name);
                url.push('=');
                url.push_str(&crate::views::encode(value));
            }
        }
        if let Some(offset) = state.offset {
            url.push(if url.contains('?') { '&' } else { '?' });
            url.push_str(&format!("offset={offset}"));
        }
        if url.len() > 4096 {
            return Err("View address exceeds 4096 bytes.");
        }
        Ok(url)
    }
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
    let mut fields = serde_json::Map::new();
    let view_id = match raw_path {
        "/" => View::Dashboard,
        "/accounts" => View::Accounts,
        "/transactions" => View::Transactions,
        "/reports" => View::Reports,
        "/reconciliation" => View::Reconciliation,
        "/review" => View::Review,
        "/settings" => View::Settings,
        _ => {
            if let Some(id) = raw_path.strip_prefix("/transactions/") {
                fields.insert(
                    "transaction_id".into(),
                    serde_json::json!(id.parse::<i64>().map_err(|_| "Invalid transaction ID.")?),
                );
                View::Transaction
            } else if let Some(id) = raw_path.strip_prefix("/review/") {
                fields.insert("proposal_id".into(), serde_json::json!(id));
                View::ReviewDetail
            } else if let Some(id) = raw_path.strip_prefix("/reports/") {
                if !crate::reports::VIEW_IDS.contains(&id) {
                    return Err("Unknown report view.");
                }
                serde_json::from_value(serde_json::json!(id)).map_err(|_| "Unknown report view.")?
            } else {
                return Err("Unknown Desktop view.");
            }
        }
    };
    for (key, value) in url.query_pairs() {
        if fields.contains_key(key.as_ref())
            || ["transaction_id", "proposal_id", "report_kind"].contains(&key.as_ref())
        {
            return Err("Duplicate or unsupported address field.");
        }
        let value = if key == "offset" {
            serde_json::json!(value.parse::<usize>().map_err(|_| "Invalid offset.")?)
        } else {
            serde_json::json!(value)
        };
        fields.insert(key.into_owned(), value);
    }
    let address = ViewAddress {
        view_id,
        state: serde_json::from_value(fields.into())
            .map_err(|_| "Unknown or invalid view filters.")?,
    };
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

/// Options for generating a link without starting or opening Desktop.
#[derive(clap::Args, Debug)]
pub struct LinkOptions {
    /// Stable view ID, as used by ui_get_link and ui_show_view.
    #[arg(long, value_enum)]
    view_id: View,
    #[command(flatten)]
    state: AddressState,
    /// Address of Desktop; link generation does not probe the listener.
    #[arg(long, default_value = "127.0.0.1:8750", value_parser = crate::cli::loopback_address)]
    bind: std::net::SocketAddr,
}

impl ViewAddress {
    pub(crate) fn result(&self, base: &str) -> Result<serde_json::Value, LinkError> {
        let path = self.path().map_err(LinkError)?;
        Ok(
            serde_json::json!({"url":format!("{}{path}", base.trim_end_matches('/')), "path":path, "view_id":self.view_id, "state":self.state}),
        )
    }
}

/// Print a credential-free Desktop link using the same contract as MCP.
///
/// # Errors
/// Returns a named validation error if the view cannot apply the requested filters.
pub fn run(mut options: LinkOptions, company: Option<String>, json: bool) -> Result<(), LinkError> {
    if options.state.company.is_none() && !matches!(options.view_id, View::Settings) {
        options.state.company = company;
    }
    let address = ViewAddress {
        view_id: options.view_id,
        state: options.state,
    };
    let result = match address.result(&crate::server::local_url(options.bind)) {
        Ok(result) => result,
        Err(error) => {
            if json {
                println!(
                    "{}",
                    serde_json::json!({"code":"VALIDATION","message":error.to_string()})
                );
            }
            return Err(error);
        }
    };
    if json {
        println!("{result}");
    } else if let Some(url) = result["url"].as_str() {
        println!("{url}");
    }
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
            assert_eq!(serde_json::to_value(decoded.view_id)?, json!(view));
            assert_eq!(
                serde_json::to_value(decoded.state)?,
                serde_json::to_value(address.state)?
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
