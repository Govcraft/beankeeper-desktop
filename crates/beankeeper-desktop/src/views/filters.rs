//! Parse browser wire parameters into filters that are safe to render and query.

use beankeeper_cli::db::companies::CompanySlug;
use serde::Deserialize;

use crate::links::{
    LinkError,
    values::{AccountSelection, Clearance, Currency, Date, MinorAmount, Offset},
};

use super::{ViewError, append_url_param, encode};

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
    pub offset: Option<i64>,
    pub currency: Option<String>,
    pub cash_account: Option<String>,
    pub receivable_account: Option<String>,
    pub payable_account: Option<String>,
}

#[derive(Clone, Copy, Default)]
pub(crate) struct DateBounds {
    from: Option<Date>,
    to: Option<Date>,
}

impl DateBounds {
    pub(crate) fn new(from: Option<Date>, to: Option<Date>) -> Result<Self, ViewError> {
        if matches!((from, to), (Some(from), Some(to)) if from > to) {
            return Err(ViewError::Invalid(
                "The start date must not be after the end date.",
            ));
        }
        Ok(Self { from, to })
    }

    pub(crate) const fn from(self) -> Option<Date> {
        self.from
    }

    pub(crate) const fn to(self) -> Option<Date> {
        self.to
    }
}

#[derive(Clone, Copy)]
pub(crate) enum Comparison {
    Period(Period),
    AsOf(Date),
}

#[derive(Clone, Copy)]
pub(crate) struct Period {
    from: Date,
    to: Date,
}

impl Comparison {
    fn parse(from: Option<&str>, to: Option<&str>) -> Result<Option<Self>, ViewError> {
        fn date(value: &str) -> Result<Date, ViewError> {
            value.parse().map_err(|_| {
                ViewError::Invalid("Comparison dates must be valid YYYY-MM-DD calendar dates.")
            })
        }
        match (from, to) {
            (None, None) => Ok(None),
            (None, Some(to)) => Ok(Some(Self::AsOf(date(to)?))),
            (Some(from), Some(to)) => {
                let (from, to) = (date(from)?, date(to)?);
                if from > to {
                    return Err(ViewError::Invalid(
                        "Comparison requires compare_from and compare_to in chronological order.",
                    ));
                }
                Ok(Some(Self::Period(Period { from, to })))
            }
            (Some(_), None) => Err(ViewError::Invalid(
                "Comparison requires compare_from and compare_to in chronological order.",
            )),
        }
    }

    pub(crate) fn bounds(self) -> DateBounds {
        match self {
            Self::Period(period) => DateBounds {
                from: Some(period.from),
                to: Some(period.to),
            },
            Self::AsOf(to) => DateBounds {
                from: None,
                to: Some(to),
            },
        }
    }
}

#[derive(Clone, Default)]
struct AmountBounds {
    min: Option<MinorAmount>,
    max: Option<MinorAmount>,
}

impl AmountBounds {
    fn parse(min: Option<String>, max: Option<String>) -> Result<Self, ViewError> {
        let min = parse_optional::<MinorAmount>(min)?;
        let max = parse_optional::<MinorAmount>(max)?;
        if matches!((&min, &max), (Some(min), Some(max)) if min > max) {
            return Err(ViewError::Invalid(
                "Minimum amount must not exceed maximum amount.",
            ));
        }
        Ok(Self { min, max })
    }
}

#[derive(Clone)]
pub(super) struct Search {
    pub(super) original: String,
    pub(super) folded: String,
}

#[derive(Clone, Default)]
pub(crate) struct Filters {
    pub(crate) company: Option<CompanySlug>,
    pub(crate) account: Option<String>,
    pub(crate) category: Option<String>,
    pub(crate) payee: Option<String>,
    pub(crate) dates: DateBounds,
    pub(crate) comparison: Option<Comparison>,
    pub(crate) currency: Option<Currency>,
    pub(crate) cash_account: Option<AccountSelection>,
    pub(crate) receivable_account: Option<AccountSelection>,
    pub(crate) payable_account: Option<AccountSelection>,
    amounts: AmountBounds,
    pub(super) search: Option<Search>,
    pub(super) status: Option<Clearance>,
    pub(super) offset: Offset,
}

pub(crate) fn nonempty(value: Option<String>) -> Option<String> {
    value.filter(|value| !value.is_empty())
}

pub(crate) fn parse_optional<T>(value: Option<String>) -> Result<Option<T>, ViewError>
where
    T: std::str::FromStr<Err = LinkError>,
{
    nonempty(value)
        .map(|value| {
            value
                .parse()
                .map_err(|LinkError(message)| ViewError::Invalid(message))
        })
        .transpose()
}

impl ViewQuery {
    pub(crate) fn parse(self) -> Result<Filters, ViewError> {
        let dates = DateBounds::new(parse_optional(self.from)?, parse_optional(self.to)?)?;
        let currency = nonempty(self.currency)
            .map(|value| value.trim().to_ascii_uppercase().parse())
            .transpose()
            .map_err(|LinkError(message)| ViewError::Invalid(message))?;
        Ok(Filters {
            company: nonempty(self.company)
                .map(|slug| slug.parse().map_err(|_| ViewError::NotFound))
                .transpose()?,
            account: nonempty(self.account),
            category: nonempty(self.category),
            payee: nonempty(self.payee),
            dates,
            comparison: Comparison::parse(
                self.compare_from.as_deref(),
                self.compare_to.as_deref(),
            )?,
            amounts: AmountBounds::parse(self.min_amount, self.max_amount)?,
            search: nonempty(self.search).map(|original| Search {
                folded: original.to_lowercase(),
                original,
            }),
            status: parse_optional(self.status)?,
            offset: self
                .offset
                .map(Offset::try_from)
                .transpose()
                .map_err(|LinkError(message)| ViewError::Invalid(message))?
                .unwrap_or_default(),
            currency,
            cash_account: parse_optional(self.cash_account)?,
            receivable_account: parse_optional(self.receivable_account)?,
            payable_account: parse_optional(self.payable_account)?,
        })
    }
}

impl Filters {
    pub(crate) fn with_configured_company(mut self, company: Option<CompanySlug>) -> Self {
        self.company = self.company.or(company);
        self
    }

    pub(crate) fn comparison_filters(&self) -> Option<Self> {
        Some(Self {
            dates: self.comparison?.bounds(),
            comparison: None,
            offset: Offset::default(),
            ..self.clone()
        })
    }

    pub(super) fn min_amount(&self) -> Option<i64> {
        self.amounts.min.as_ref().map(MinorAmount::units)
    }

    pub(super) fn max_amount(&self) -> Option<i64> {
        self.amounts.max.as_ref().map(MinorAmount::units)
    }

    pub(super) fn url(&self, path: &str, company: &str, offset: i64) -> String {
        let mut url = format!("{path}?company={}", encode(company));
        append_url_param(&mut url, "account", self.account.as_deref());
        append_url_param(&mut url, "category", self.category.as_deref());
        append_url_param(&mut url, "payee", self.payee.as_deref());
        append_url_param(
            &mut url,
            "compare_from",
            self.comparison.and_then(|range| range.bounds().from()),
        );
        append_url_param(
            &mut url,
            "compare_to",
            self.comparison.and_then(|range| range.bounds().to()),
        );
        append_url_param(&mut url, "from", self.dates.from());
        append_url_param(&mut url, "to", self.dates.to());
        append_url_param(&mut url, "min_amount", self.min_amount());
        append_url_param(&mut url, "max_amount", self.max_amount());
        append_url_param(
            &mut url,
            "search",
            self.search.as_ref().map(|search| &search.original),
        );
        append_url_param(&mut url, "status", self.status.as_ref());
        append_url_param(&mut url, "currency", self.currency.as_ref());
        append_url_param(&mut url, "offset", (offset > 0).then_some(offset));
        url
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parsed_query_preserves_browser_defaults_and_proves_usable_bounds() -> Result<(), ViewError> {
        let filters = ViewQuery {
            from: Some(String::new()),
            to: Some("2026-06-30".into()),
            compare_from: Some("2026-05-01".into()),
            compare_to: Some("2026-05-31".into()),
            currency: Some(" usd ".into()),
            min_amount: Some("0".into()),
            max_amount: Some(i64::MAX.to_string()),
            offset: Some(500),
            ..ViewQuery::default()
        }
        .parse()?;
        assert!(filters.dates.from().is_none());
        assert_eq!(
            filters.dates.to().map(|date| date.to_string()).as_deref(),
            Some("2026-06-30")
        );
        assert_eq!(filters.currency.as_ref().map(Currency::code), Some("USD"));
        assert_eq!(filters.min_amount(), Some(0));
        assert_eq!(filters.max_amount(), Some(i64::MAX));
        assert_eq!(filters.offset.position(), 500);

        let comparison = filters
            .comparison_filters()
            .ok_or(ViewError::Invalid("missing comparison"))?;
        assert_eq!(comparison.offset.position(), 0);
        assert_eq!(
            comparison
                .dates
                .from()
                .map(|date| date.to_string())
                .as_deref(),
            Some("2026-05-01")
        );
        assert_eq!(
            comparison
                .dates
                .to()
                .map(|date| date.to_string())
                .as_deref(),
            Some("2026-05-31")
        );
        assert!(comparison.comparison.is_none());
        Ok(())
    }
}
