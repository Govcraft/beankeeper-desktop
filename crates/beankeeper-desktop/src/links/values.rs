//! Validated wire values shared by every address entry point.

use std::{fmt, ops::RangeInclusive, str::FromStr};

use beankeeper::types::{
    AccountCode as LedgerAccountCode, Amount, ClearanceStatus, Currency as LedgerCurrency,
};
use chrono::{Datelike, NaiveDate};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::LinkError;

macro_rules! string_wire {
    ($name:ident) => {
        impl TryFrom<String> for $name {
            type Error = LinkError;

            fn try_from(value: String) -> Result<Self, Self::Error> {
                value.parse()
            }
        }

        impl From<$name> for String {
            fn from(value: $name) -> Self {
                value.to_string()
            }
        }
    };
}

/// A real calendar date in the URL's four-digit Gregorian year range.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Deserialize, Serialize, JsonSchema,
)]
#[serde(try_from = "String", into = "String")]
pub(crate) struct Date(NaiveDate);

impl Date {
    pub(crate) fn date(self) -> NaiveDate {
        self.0
    }
}

impl FromStr for Date {
    type Err = LinkError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let error = || LinkError("Dates must be valid calendar dates in YYYY-MM-DD format.");
        let date = NaiveDate::parse_from_str(value, "%Y-%m-%d").map_err(|_| error())?;
        if !(1..=9999).contains(&date.year()) || date.format("%Y-%m-%d").to_string() != value {
            return Err(error());
        }
        Ok(Self(date))
    }
}

impl fmt::Display for Date {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.format("%Y-%m-%d").fmt(f)
    }
}

string_wire!(Date);

/// A register amount, in nonnegative whole minor units within SQLite's range.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Deserialize, Serialize, JsonSchema)]
#[serde(try_from = "String", into = "String")]
pub(crate) struct MinorAmount(i64);

impl MinorAmount {
    pub(crate) const fn units(&self) -> i64 {
        self.0
    }
}

impl FromStr for MinorAmount {
    type Err = LinkError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        value
            .parse::<i64>()
            .ok()
            .filter(|amount| *amount >= 0)
            .map(Self)
            .ok_or(LinkError(
                "Amounts must be nonnegative whole minor units, such as cents.",
            ))
    }
}

impl fmt::Display for MinorAmount {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

string_wire!(MinorAmount);

/// An exact decimal quantity, independent of the currency selected by the ledger.
#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(try_from = "String", into = "String")]
pub(crate) struct DecimalAmount {
    coefficient: Amount,
    scale: u8,
}

impl DecimalAmount {
    /// Check the currency-dependent precision and SQLite bounds at the boundary.
    pub(super) fn validate_currency(&self, currency: &Currency) -> Result<(), LinkError> {
        let scale = currency.0.minor_units();
        if self.scale > scale {
            return Err(LinkError(
                "The amount has too many decimal places for its currency.",
            ));
        }
        let units = self
            .coefficient
            .minor_units()
            .checked_mul(10_i128.pow(u32::from(scale - self.scale)))
            .filter(|units| units.unsigned_abs() <= i64::MAX as u128);
        if units.is_none() {
            return Err(LinkError(
                "The amount exceeds the ledger's supported range.",
            ));
        }
        Ok(())
    }
}

impl FromStr for DecimalAmount {
    type Err = LinkError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let error = || LinkError("Amounts must be exact decimal strings, without exponents.");
        let (negative, unsigned) = value.strip_prefix('-').map_or_else(
            || (false, value.strip_prefix('+').unwrap_or(value)),
            |v| (true, v),
        );
        let (whole, fraction) = unsigned.split_once('.').unwrap_or((unsigned, ""));
        if whole.is_empty()
            || !whole.bytes().all(|b| b.is_ascii_digit())
            || !fraction.bytes().all(|b| b.is_ascii_digit())
        {
            return Err(error());
        }
        let scale = u8::try_from(fraction.len()).map_err(|_| error())?;
        if scale > 38 {
            return Err(error());
        }
        let coefficient = format!("{}{whole}{fraction}", if negative { "-" } else { "" })
            .parse::<i128>()
            .map_err(|_| error())?;
        Ok(Self {
            coefficient: Amount::new(coefficient),
            scale,
        })
    }
}

impl fmt::Display for DecimalAmount {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let coefficient = self.coefficient.minor_units();
        let absolute = coefficient.unsigned_abs();
        let sign = if coefficient < 0 { "-" } else { "" };
        if self.scale == 0 {
            return write!(f, "{sign}{absolute}");
        }
        let divisor = 10_u128.pow(u32::from(self.scale));
        write!(
            f,
            "{sign}{}.{:0width$}",
            absolute / divisor,
            absolute % divisor,
            width = usize::from(self.scale)
        )
    }
}

string_wire!(DecimalAmount);

/// A nonnegative vendor threshold, in exact major units.
#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(try_from = "String", into = "String")]
pub(crate) struct Threshold(DecimalAmount);

impl Threshold {
    pub(super) fn validate_currency(&self, currency: &Currency) -> Result<(), LinkError> {
        self.0.validate_currency(currency)
    }
}

impl FromStr for Threshold {
    type Err = LinkError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let amount: DecimalAmount = value.parse()?;
        if amount.coefficient.is_negative() {
            return Err(LinkError("The vendor threshold must be nonnegative."));
        }
        Ok(Self(amount))
    }
}

impl fmt::Display for Threshold {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

string_wire!(Threshold);

/// Exact account codes selected by an upstream report, accepting the old scalar form.
#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(untagged)]
pub(crate) enum AccountSelection {
    One(AccountCode),
    Multiple(Vec<AccountCode>),
}

impl AccountSelection {
    fn as_slice(&self) -> &[AccountCode] {
        match self {
            Self::One(account) => std::slice::from_ref(account),
            Self::Multiple(accounts) => accounts,
        }
    }

    pub(crate) fn codes(&self) -> Vec<String> {
        self.as_slice().iter().map(ToString::to_string).collect()
    }
}

impl FromStr for AccountSelection {
    type Err = LinkError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if !value.contains(',') {
            return value.trim().parse().map(Self::One);
        }
        value
            .split(',')
            .map(|code| code.trim().parse())
            .collect::<Result<Vec<_>, _>>()
            .map(Self::Multiple)
    }
}

impl fmt::Display for AccountSelection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (index, account) in self.as_slice().iter().enumerate() {
            if index > 0 {
                f.write_str(",")?;
            }
            account.fmt(f)?;
        }
        Ok(())
    }
}

/// A positive upstream SQLite transaction ID, retaining existing link addresses.
#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(try_from = "i64", into = "i64")]
pub(crate) struct TransactionId(i64);

impl TryFrom<i64> for TransactionId {
    type Error = LinkError;

    fn try_from(value: i64) -> Result<Self, Self::Error> {
        if value <= 0 {
            return Err(LinkError("transaction requires a positive transaction_id."));
        }
        Ok(Self(value))
    }
}

impl From<TransactionId> for i64 {
    fn from(value: TransactionId) -> Self {
        value.0
    }
}

impl FromStr for TransactionId {
    type Err = LinkError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        value
            .parse::<i64>()
            .map_err(|_| LinkError("transaction requires a positive transaction_id."))?
            .try_into()
    }
}

impl fmt::Display for TransactionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// The upstream proposal `TypeID` with its prefix checked by upstream parsing.
#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(try_from = "String", into = "String")]
pub(crate) struct ProposalId(beankeeper_cli::db::proposals::ProposalId);

impl FromStr for ProposalId {
    type Err = LinkError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        value
            .parse()
            .map(Self)
            .map_err(|_| LinkError("proposal_id must be a proposal TypeID."))
    }
}

impl fmt::Display for ProposalId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

string_wire!(ProposalId);

macro_rules! ledger_string {
    ($name:ident, $inner:ty, $error:literal) => {
        #[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
        #[serde(try_from = "String", into = "String")]
        pub(crate) struct $name($inner);

        impl FromStr for $name {
            type Err = LinkError;

            fn from_str(value: &str) -> Result<Self, Self::Err> {
                value.parse().map(Self).map_err(|_| LinkError($error))
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.fmt(f)
            }
        }

        string_wire!($name);
    };
}

ledger_string!(Currency, LedgerCurrency, "Choose a valid currency code.");
ledger_string!(
    AccountCode,
    LedgerAccountCode,
    "Choose a valid account code."
);
ledger_string!(
    Clearance,
    ClearanceStatus,
    "Clearance must be uncleared, cleared or reconciled."
);

impl From<LedgerCurrency> for Currency {
    fn from(currency: LedgerCurrency) -> Self {
        Self(currency)
    }
}

impl Currency {
    pub(crate) const fn ledger(&self) -> LedgerCurrency {
        self.0
    }

    pub(crate) fn code(&self) -> &str {
        self.0.code()
    }
}

impl Clearance {
    pub(crate) const fn status(&self) -> ClearanceStatus {
        self.0
    }
}

/// A four-digit report year, serialized as the existing string field.
#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(try_from = "String", into = "String")]
pub(crate) struct Year(i32);

impl Year {
    pub(crate) fn get(&self) -> i32 {
        self.0
    }
}

impl FromStr for Year {
    type Err = LinkError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        value
            .parse::<i32>()
            .ok()
            .filter(|year| (1..=9999).contains(year))
            .map(Self)
            .ok_or(LinkError("The report year must be between 1 and 9999."))
    }
}

impl fmt::Display for Year {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

string_wire!(Year);

/// A report year with an optional inclusive window of complete months.
#[derive(Clone)]
pub(crate) struct BudgetWindow {
    year: i32,
    months: Option<RangeInclusive<i32>>,
}

impl BudgetWindow {
    pub(crate) const ERROR: &'static str = "Budget variance requires a year (1-9999), or a date range covering whole calendar months within one year.";

    pub(crate) fn new(
        from: Option<Date>,
        to: Option<Date>,
        year: Option<&Year>,
    ) -> Result<Option<Self>, LinkError> {
        match (from, to) {
            (None, None) => Ok(year.map(|year| Self {
                year: year.get(),
                months: None,
            })),
            (Some(from), Some(to)) => {
                let from = from.date();
                let to = to.date();
                if from > to
                    || from.day() != 1
                    || from.year() != to.year()
                    || to.succ_opt().is_none_or(|next| next.day() != 1)
                    || year.is_some_and(|year| year.get() != from.year())
                {
                    return Err(LinkError(Self::ERROR));
                }
                // chrono guarantees that calendar months are numbered 1–12.
                let first = i32::try_from(from.month()).map_err(|_| LinkError(Self::ERROR))?;
                let last = i32::try_from(to.month()).map_err(|_| LinkError(Self::ERROR))?;
                Ok(Some(Self {
                    year: from.year(),
                    months: Some(first..=last),
                }))
            }
            _ => Err(LinkError(Self::ERROR)),
        }
    }

    pub(crate) fn year(&self) -> i32 {
        self.year
    }

    pub(crate) fn first_month(&self) -> Option<i32> {
        self.months.as_ref().map(|months| *months.start())
    }

    pub(crate) fn last_month(&self) -> Option<i32> {
        self.months.as_ref().map(|months| *months.end())
    }
}

/// Maximum number of ledger records examined while filling a browser page.
pub(crate) const SCAN_LIMIT: i64 = 1_000;

/// An offset leaving room for one full register scan in SQLite's signed range.
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, JsonSchema)]
#[serde(try_from = "i64", into = "i64")]
pub(crate) struct Offset(i64);

impl Offset {
    pub(crate) const fn position(self) -> i64 {
        self.0
    }
}

impl TryFrom<i64> for Offset {
    type Error = LinkError;

    fn try_from(value: i64) -> Result<Self, Self::Error> {
        if !(0..=i64::MAX - SCAN_LIMIT).contains(&value) {
            return Err(LinkError("The pagination offset is too large."));
        }
        Ok(Self(value))
    }
}

impl From<Offset> for i64 {
    fn from(value: Offset) -> Self {
        value.0
    }
}

impl FromStr for Offset {
    type Err = LinkError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        value
            .parse::<i64>()
            .map_err(|_| LinkError("The pagination offset is too large."))?
            .try_into()
    }
}

impl fmt::Display for Offset {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn dates_and_years_reject_normalization_and_impossible_calendar_values() {
        for value in ["2026-02-29", "2026-1-01", "0000-01-01", "+10000-01-01", ""] {
            assert!(value.parse::<Date>().is_err(), "{value}");
        }
        assert!("2024-02-29".parse::<Date>().is_ok());
        for value in ["0", "10000", "-1", "2026.0"] {
            assert!(value.parse::<Year>().is_err(), "{value}");
        }
    }

    #[test]
    fn amounts_keep_exact_decimal_precision_without_floating_point() -> Result<(), LinkError> {
        for value in ["9007199254740993.01", "-0.0001", "0", "1234.5600"] {
            let amount: DecimalAmount = value.parse()?;
            assert_eq!(amount.to_string(), value);
        }
        for value in ["NaN", "1e3", ".5", "1.2.3", "", "-", " 1"] {
            assert!(value.parse::<DecimalAmount>().is_err(), "{value}");
        }
        assert!("-0.01".parse::<Threshold>().is_err());
        for value in ["-1", "1.01", "9223372036854775808"] {
            assert!(value.parse::<MinorAmount>().is_err(), "{value}");
        }
        assert!("9223372036854775807".parse::<MinorAmount>().is_ok());
        let usd: Currency = "USD".parse()?;
        assert!(
            "1.001"
                .parse::<DecimalAmount>()?
                .validate_currency(&usd)
                .is_err()
        );
        assert!(
            "92233720368547758.07"
                .parse::<DecimalAmount>()?
                .validate_currency(&usd)
                .is_ok()
        );
        assert!(
            "92233720368547758.08"
                .parse::<DecimalAmount>()?
                .validate_currency(&usd)
                .is_err()
        );
        assert!(
            "-92233720368547758.08"
                .parse::<DecimalAmount>()?
                .validate_currency(&usd)
                .is_err()
        );
        Ok(())
    }

    #[test]
    fn record_identifiers_and_offsets_validate_at_deserialization() {
        for value in [json!(0), json!(-1), json!("1")] {
            assert!(serde_json::from_value::<TransactionId>(value).is_err());
        }
        assert!(serde_json::from_value::<TransactionId>(json!(42)).is_ok());
        assert!(
            "agent_01h455vb4pex5vsknk084sn02q"
                .parse::<ProposalId>()
                .is_err()
        );
        assert!(
            "proposal_01h455vb4pex5vsknk084sn02q"
                .parse::<ProposalId>()
                .is_ok()
        );
        assert!(Offset::try_from(i64::MAX - SCAN_LIMIT).is_ok());
        assert!(Offset::try_from(i64::MAX - SCAN_LIMIT + 1).is_err());
        assert!(serde_json::from_value::<Offset>(json!(-1)).is_err());
    }

    #[test]
    fn wire_values_reuse_ledger_validation_and_never_accept_floats() {
        for value in [json!("NaN"), json!(1.5), json!(42)] {
            assert!(serde_json::from_value::<DecimalAmount>(value).is_err());
        }
        assert!("XYZ".parse::<Currency>().is_err());
        assert!("usd".parse::<Currency>().is_err());
        assert!("1000.20".parse::<AccountCode>().is_ok());
        assert!("Dining".parse::<AccountCode>().is_err());
        assert!("cleared".parse::<Clearance>().is_ok());
        assert!("pending".parse::<Clearance>().is_err());
        assert_eq!(
            schemars::schema_for!(DecimalAmount).to_value()["type"],
            "string"
        );
        assert_eq!(
            schemars::schema_for!(TransactionId).to_value()["type"],
            "integer"
        );
    }
}
