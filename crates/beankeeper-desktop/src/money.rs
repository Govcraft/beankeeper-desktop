//! Exact, locale-independent monetary presentation for browser views.

use beankeeper::types::{Amount, Currency, Money};

/// Format ledger minor units without passing through floating point.
pub(crate) fn minor(value: impl Into<i128>, code: &str) -> String {
    let value = value.into();
    Currency::from_code(code).map_or_else(
        |_| format!("{value} {code} minor units (unknown currency)"),
        |currency| ledger(Money::new(Amount::new(value), currency)),
    )
}

/// Format already validated ledger money without parsing its currency or amount.
pub(crate) fn ledger(money: Money) -> String {
    let currency = money.currency();
    let value = money.amount().format_decimal(currency.minor_units());
    let (sign, whole, fraction) = decimal_parts(&value);
    decorate(sign, whole, fraction, currency)
}

fn decimal_parts(value: &str) -> (&str, &str, &str) {
    let (sign, value) = value.strip_prefix('-').map_or(("", value), |v| ("-", v));
    let (whole, fraction) = value.split_once('.').unwrap_or((value, ""));
    (sign, whole, fraction)
}

fn decorate(sign: &str, whole: &str, fraction: &str, currency: Currency) -> String {
    let symbol = match currency {
        Currency::USD => "$",
        Currency::EUR => "€",
        Currency::GBP => "£",
        Currency::JPY => "¥",
        Currency::CAD => "CA$",
        Currency::AUD => "A$",
        Currency::MXN => "MX$",
        _ => "",
    };
    // Valid currencies without a familiar symbol retain their ISO code and scale.
    let mut formatted = if symbol.is_empty() {
        format!("{sign}{} ", currency.code())
    } else {
        format!("{sign}{symbol}")
    };
    for (index, digit) in whole.chars().enumerate() {
        if index > 0 && (whole.len() - index).is_multiple_of(3) {
            formatted.push(',');
        }
        formatted.push(digit);
    }
    if !fraction.is_empty() {
        formatted.push('.');
        formatted.push_str(fraction);
    }
    formatted
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn currencies_use_exact_scale_sign_and_unambiguous_symbols() {
        for (code, value, expected) in [
            ("USD", 5_562_000, "$55,620.00"),
            ("USD", 4_032_470, "$40,324.70"),
            ("USD", -5, "-$0.05"),
            ("USD", 0, "$0.00"),
            ("JPY", 123_456, "¥123,456"),
            ("BHD", -1234, "-BHD 1.234"),
            ("KWD", 1, "KWD 0.001"),
            ("EUR", 123_456, "€1,234.56"),
            ("GBP", 123_456, "£1,234.56"),
            ("CHF", 123_456, "CHF 1,234.56"),
            ("CAD", 123_456, "CA$1,234.56"),
            ("AUD", 123_456, "A$1,234.56"),
            ("MXN", 123_456, "MX$1,234.56"),
        ] {
            assert_eq!(minor(value, code), expected);
        }
        assert_eq!(minor(i64::MIN, "USD"), "-$92,233,720,368,547,758.08");
        assert_eq!(minor(i64::MAX, "BHD"), "BHD 9,223,372,036,854,775.807");
    }

    #[test]
    fn currencies_without_symbol_mappings_use_code_and_exact_major_units() {
        for (code, value, expected) in [
            ("CHF", 123_456, "CHF 1,234.56"),
            ("CHF", -5, "-CHF 0.05"),
            ("BHD", -1_234, "-BHD 1.234"),
            ("KWD", 1, "KWD 0.001"),
            ("KWD", 0, "KWD 0.000"),
        ] {
            assert_eq!(minor(value, code), expected);
        }
    }

    #[test]
    fn unknown_currency_keeps_minor_units_and_large_amounts_keep_exact_digits() {
        assert_eq!(minor(123, "XYZ"), "123 XYZ minor units (unknown currency)");
        assert_eq!(
            minor(900_719_925_474_099_301_i64, "USD"),
            "$9,007,199,254,740,993.01"
        );
    }
}
