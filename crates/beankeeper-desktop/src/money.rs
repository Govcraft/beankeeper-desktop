//! Exact, locale-independent monetary presentation for browser views.

use beankeeper::types::{Amount, Currency};

/// Format ledger minor units without passing through floating point.
pub(crate) fn minor(value: impl Into<i128>, code: &str) -> String {
    let value = value.into();
    Currency::from_code(code)
        .ok()
        .and_then(|currency| {
            decimal(
                &Amount::new(value).format_decimal(currency.minor_units()),
                code,
            )
        })
        .unwrap_or_else(|| format!("{value} {code} minor units (unknown currency)"))
}

/// Decorate an upstream major-unit display string, preserving its exact digits.
pub(crate) fn decimal(value: &str, code: &str) -> Option<String> {
    let currency = Currency::from_code(code).ok()?;
    let (sign, value) = value.strip_prefix('-').map_or(("", value), |v| ("-", v));
    let (whole, fraction) = value.split_once('.').unwrap_or((value, ""));
    if whole.is_empty()
        || !whole.bytes().all(|b| b.is_ascii_digit())
        || !fraction.bytes().all(|b| b.is_ascii_digit())
        || fraction.len() != usize::from(currency.minor_units())
    {
        return None;
    }
    let symbol = match code {
        "USD" => "$",
        "EUR" => "€",
        "GBP" => "£",
        "JPY" => "¥",
        "CAD" => "CA$",
        "AUD" => "A$",
        "MXN" => "MX$",
        _ => "",
    };
    // Valid currencies without a familiar symbol retain their ISO code and scale.
    let mut formatted = if symbol.is_empty() {
        format!("{sign}{code} ")
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
    Some(formatted)
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
        for (code, value, display, expected) in [
            ("CHF", 123_456, "1234.56", "CHF 1,234.56"),
            ("CHF", -5, "-0.05", "-CHF 0.05"),
            ("BHD", -1_234, "-1.234", "-BHD 1.234"),
            ("KWD", 1, "0.001", "KWD 0.001"),
            ("KWD", 0, "0.000", "KWD 0.000"),
        ] {
            assert!(Currency::from_code(code).is_ok());
            assert_eq!(minor(value, code), expected);
            assert_eq!(decimal(display, code).as_deref(), Some(expected));
        }
    }

    #[test]
    fn invalid_currencies_and_display_strings_never_guess_a_scale() {
        assert_eq!(minor(123, "XYZ"), "123 XYZ minor units (unknown currency)");
        for value in ["1.2", "1.234", "1e2", "<script>", "", "NaN", "1,234.00"] {
            assert!(decimal(value, "USD").is_none(), "{value}");
        }
        assert!(decimal("1.00", "XYZ").is_none());
        assert_eq!(
            decimal("9007199254740993.01", "USD").as_deref(),
            Some("$9,007,199,254,740,993.01")
        );
    }
}
