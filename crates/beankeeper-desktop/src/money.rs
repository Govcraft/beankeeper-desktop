//! Exact, locale-independent monetary presentation for browser views.

use beankeeper::types::{Currency, Money};

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
        for (currency, value, expected) in [
            (Currency::USD, 5_562_000, "$55,620.00"),
            (Currency::USD, 4_032_470, "$40,324.70"),
            (Currency::USD, -5, "-$0.05"),
            (Currency::USD, 0, "$0.00"),
            (Currency::JPY, 123_456, "¥123,456"),
            (Currency::BHD, -1234, "-BHD 1.234"),
            (Currency::KWD, 1, "KWD 0.001"),
            (Currency::EUR, 123_456, "€1,234.56"),
            (Currency::GBP, 123_456, "£1,234.56"),
            (Currency::CHF, 123_456, "CHF 1,234.56"),
            (Currency::CAD, 123_456, "CA$1,234.56"),
            (Currency::AUD, 123_456, "A$1,234.56"),
            (Currency::MXN, 123_456, "MX$1,234.56"),
        ] {
            assert_eq!(ledger(Money::from_minor(value, currency)), expected);
        }
        assert_eq!(
            ledger(Money::from_minor(i64::MIN.into(), Currency::USD)),
            "-$92,233,720,368,547,758.08"
        );
        assert_eq!(
            ledger(Money::from_minor(i64::MAX.into(), Currency::BHD)),
            "BHD 9,223,372,036,854,775.807"
        );
    }

    #[test]
    fn currencies_without_symbol_mappings_use_code_and_exact_major_units() {
        for (currency, value, expected) in [
            (Currency::CHF, 123_456, "CHF 1,234.56"),
            (Currency::CHF, -5, "-CHF 0.05"),
            (Currency::BHD, -1_234, "-BHD 1.234"),
            (Currency::KWD, 1, "KWD 0.001"),
            (Currency::KWD, 0, "KWD 0.000"),
        ] {
            assert_eq!(ledger(Money::from_minor(value, currency)), expected);
        }
    }

    #[test]
    fn large_amounts_keep_exact_digits() {
        assert_eq!(
            ledger(Money::from_minor(900_719_925_474_099_301, Currency::USD)),
            "$9,007,199,254,740,993.01"
        );
    }
}
