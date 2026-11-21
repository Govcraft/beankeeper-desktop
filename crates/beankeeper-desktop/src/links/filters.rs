//! Per-view filter sets. Only deserialization can construct a validated set.

use super::{
    LinkError, REPORTS, REVIEW_PREFIX, ReportKind, TRANSACTION_PREFIX, encode_path, fields,
    values::{
        AccountCode, AccountSelection, BudgetWindow, Clearance, Currency, Date, DecimalAmount,
        MinorAmount, Offset, ProposalId, Threshold, TransactionId, Year,
    },
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize, de::Error};

#[derive(Clone, Debug, Serialize, JsonSchema)]
#[serde(transparent)]
pub(crate) struct Validated<T>(T);

pub(crate) trait Validate {
    fn validate(&self) -> Result<(), LinkError>;
}

impl<'de, T: Deserialize<'de> + Validate> Deserialize<'de> for Validated<T> {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = T::deserialize(deserializer)?;
        value.validate().map_err(D::Error::custom)?;
        Ok(Self(value))
    }
}

// Every default below is the valid, unfiltered view. Detail IDs have no default.
impl<T: Default + Validate> Default for Validated<T> {
    fn default() -> Self {
        Self(T::default())
    }
}

macro_rules! filter_set {
    ($name:ident { $($field:ident: $ty:ty),* $(,)? }) => {
        #[derive(Clone, Debug, Default, Deserialize, Serialize, JsonSchema)]
        #[serde(deny_unknown_fields)]
        pub(crate) struct $name { $( $field: Option<$ty>, )* }
    };
}

filter_set!(Empty {});
filter_set!(BookFilter { company: String });
filter_set!(Dashboard {
    company: String,
    currency: Currency
});
filter_set!(Summary {
    company: String,
    currency: Currency,
    from: Date,
    to: Date,
    compare_from: Date,
    compare_to: Date,
    cash_account: AccountSelection,
    receivable_account: AccountSelection,
    payable_account: AccountSelection,
});
filter_set!(Transactions {
    company: String,
    account: AccountCode,
    category: String,
    payee: String,
    from: Date,
    to: Date,
    compare_from: Date,
    compare_to: Date,
    min_amount: MinorAmount,
    max_amount: MinorAmount,
    search: String,
    status: Clearance,
    offset: Offset,
    currency: Currency,
});
filter_set!(Catalog {
    company: String,
    currency: Currency
});
filter_set!(Period {
    company: String,
    currency: Currency,
    from: Date,
    to: Date,
    compare_from: Date,
    compare_to: Date,
});
filter_set!(Balance {
    company: String,
    currency: Currency,
    to: Date,
    compare_to: Date
});
filter_set!(CashFlowFilters {
    company: String,
    currency: Currency,
    from: Date,
    to: Date,
    compare_from: Date,
    compare_to: Date,
    cash_account: AccountSelection,
});
filter_set!(Trend {
    company: String,
    currency: Currency,
    from: Date,
    to: Date,
    compare_from: Date,
    compare_to: Date,
    period: TrendPeriod,
    metric: TrendMetric,
});
filter_set!(Budget {
    company: String,
    currency: Currency,
    from: Date,
    to: Date,
    compare_from: Date,
    compare_to: Date,
    year: Year,
});
filter_set!(Vendor {
    company: String,
    currency: Currency,
    from: Date,
    to: Date,
    compare_from: Date,
    compare_to: Date,
    threshold: Threshold,
});
filter_set!(Reconciliation {
    company: String,
    currency: Currency,
    account: AccountCode,
    to: Date,
    mode: ReconciliationMode,
    statement: String,
    statement_balance: DecimalAmount,
    link_key: String,
});

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct Transaction {
    company: Option<String>,
    #[serde(rename = "transaction_id")]
    id: TransactionId,
    offset: Option<Offset>,
}
#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct ReviewDetail {
    company: Option<String>,
    proposal_id: ProposalId,
}
impl Validated<Transaction> {
    pub(super) fn route(&self) -> String {
        format!("{TRANSACTION_PREFIX}{}", self.0.id)
    }
}
impl Validated<ReviewDetail> {
    pub(super) fn route(&self) -> String {
        format!("{REVIEW_PREFIX}{}", self.0.proposal_id)
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum TrendPeriod {
    Month,
    Quarter,
    Year,
}
#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum TrendMetric {
    Spending,
    NetWorth,
}
#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ReconciliationMode {
    Bank,
    Settlement,
}

fn chronological(from: Option<Date>, to: Option<Date>) -> Result<(), LinkError> {
    if matches!((from, to), (Some(from), Some(to)) if from.date() > to.date()) {
        return Err(LinkError("The start date must not be after the end date."));
    }
    Ok(())
}
fn comparison(from: Option<Date>, to: Option<Date>) -> Result<(), LinkError> {
    match (from, to) {
        (None, None) => Ok(()),
        (Some(_), Some(_)) => chronological(from, to),
        _ => Err(LinkError(
            "Comparison requires compare_from and compare_to in chronological order.",
        )),
    }
}
macro_rules! no_relations {
    ($($name:ident),*) => { $( impl Validate for $name {
        fn validate(&self) -> Result<(), LinkError> { Ok(()) }
    } )* };
}
no_relations!(
    Empty,
    BookFilter,
    Dashboard,
    Catalog,
    Balance,
    Transaction,
    ReviewDetail
);
macro_rules! period_relations {
    ($($name:ident),*) => { $( impl Validate for $name {
        fn validate(&self) -> Result<(), LinkError> {
            chronological(self.from, self.to)?;
            comparison(self.compare_from, self.compare_to)
        }
    } )* };
}
period_relations!(Period, CashFlowFilters, Trend, Summary);
impl Validate for Transactions {
    fn validate(&self) -> Result<(), LinkError> {
        chronological(self.from, self.to)?;
        comparison(self.compare_from, self.compare_to)?;
        if matches!((&self.min_amount, &self.max_amount), (Some(min), Some(max)) if min > max) {
            return Err(LinkError("Minimum amount must not exceed maximum amount."));
        }
        Ok(())
    }
}
impl Validate for Budget {
    fn validate(&self) -> Result<(), LinkError> {
        BudgetWindow::new(self.from, self.to, self.year.as_ref())?;
        comparison(self.compare_from, self.compare_to)?;
        BudgetWindow::new(self.compare_from, self.compare_to, None)?;
        Ok(())
    }
}
impl Validate for Vendor {
    fn validate(&self) -> Result<(), LinkError> {
        chronological(self.from, self.to)?;
        comparison(self.compare_from, self.compare_to)?;
        if let (Some(amount), Some(currency)) = (&self.threshold, &self.currency) {
            amount.validate_currency(currency)?;
        }
        Ok(())
    }
}
impl Validate for Reconciliation {
    fn validate(&self) -> Result<(), LinkError> {
        if let (Some(amount), Some(currency)) = (&self.statement_balance, &self.currency) {
            amount.validate_currency(currency)?;
        }
        Ok(())
    }
}

/// Compatibility spelling: reports + `report_kind` has the same typed alternatives
/// as direct report view IDs. A catalog has no report-specific filters.
#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(untagged)]
pub(crate) enum ReportSelector {
    Catalog(Validated<Catalog>),
    Selected(ReportAddress),
}
impl Default for ReportSelector {
    fn default() -> Self {
        Self::Catalog(Validated::default())
    }
}
#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(tag = "report_kind", rename_all = "kebab-case")]
pub(crate) enum ReportAddress {
    IncomeStatement(Validated<Period>),
    BalanceSheet(Validated<Balance>),
    CashFlow(Validated<CashFlowFilters>),
    Trend(Validated<Trend>),
    BudgetVariance(Validated<Budget>),
    ScheduleC(Validated<Period>),
    #[serde(rename = "1099")]
    Vendor1099(Validated<Vendor>),
    Reconciliation(Validated<Reconciliation>),
}
impl ReportSelector {
    pub(super) fn path(&self) -> Result<String, &'static str> {
        let (route, fields) = match self {
            Self::Catalog(state) => (REPORTS, fields(state)?),
            Self::Selected(report) => match report {
                ReportAddress::IncomeStatement(state) => {
                    (ReportKind::Income.path(), fields(state)?)
                }
                ReportAddress::BalanceSheet(state) => (ReportKind::Balance.path(), fields(state)?),
                ReportAddress::CashFlow(state) => (ReportKind::Cash.path(), fields(state)?),
                ReportAddress::Trend(state) => (ReportKind::Trend.path(), fields(state)?),
                ReportAddress::BudgetVariance(state) => (ReportKind::Budget.path(), fields(state)?),
                ReportAddress::ScheduleC(state) => (ReportKind::Schedule.path(), fields(state)?),
                ReportAddress::Vendor1099(state) => (ReportKind::Vendor.path(), fields(state)?),
                ReportAddress::Reconciliation(state) => {
                    (ReportKind::Reconciliation.path(), fields(state)?)
                }
            },
        };
        encode_path(route.into(), &fields)
    }
}
