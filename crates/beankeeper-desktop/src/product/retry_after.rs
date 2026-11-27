//! Validated retry delay from the issuer's fixed hourly trial quota.
use reqwest::header::HeaderValue;
use serde::{Deserialize, Serialize};
use std::{fmt, str::FromStr};

/// Seconds until the issuer's hourly quota resets, within 1..=3600.
///
/// ```compile_fail
/// use beankeeper_desktop::product::RetryAfter;
/// let invalid = RetryAfter(0);
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "u64", into = "u64")]
pub struct RetryAfter(u64);

/// The issuer supplied an invalid retry delay.
#[derive(Debug)]
pub struct InvalidRetryAfter;
impl fmt::Display for InvalidRetryAfter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("retry delay must be between 1 and 3600 seconds")
    }
}
impl std::error::Error for InvalidRetryAfter {}
impl TryFrom<u64> for RetryAfter {
    type Error = InvalidRetryAfter;
    fn try_from(value: u64) -> Result<Self, Self::Error> {
        if (1..=3600).contains(&value) {
            Ok(Self(value))
        } else {
            Err(InvalidRetryAfter)
        }
    }
}
impl FromStr for RetryAfter {
    type Err = InvalidRetryAfter;
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if !value.bytes().all(|byte| byte.is_ascii_digit()) {
            return Err(InvalidRetryAfter);
        }
        value
            .parse::<u64>()
            .map_err(|_| InvalidRetryAfter)?
            .try_into()
    }
}
impl TryFrom<&HeaderValue> for RetryAfter {
    type Error = InvalidRetryAfter;
    fn try_from(value: &HeaderValue) -> Result<Self, Self::Error> {
        value.to_str().map_err(|_| InvalidRetryAfter)?.parse()
    }
}
impl From<RetryAfter> for u64 {
    fn from(value: RetryAfter) -> Self {
        value.0
    }
}
impl fmt::Display for RetryAfter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "in {} seconds", self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_hourly_quota_at_each_untrusted_boundary() -> Result<(), Box<dyn std::error::Error>>
    {
        for value in ["1", "60", "3600"] {
            let retry: RetryAfter = value.parse()?;
            assert_eq!(RetryAfter::try_from(&HeaderValue::from_str(value)?)?, retry);
            assert_eq!(serde_json::from_str::<RetryAfter>(value)?, retry);
            assert_eq!(serde_json::to_string(&retry)?, value);
            assert_eq!(retry.to_string(), format!("in {value} seconds"));
        }
        for value in [
            "0",
            "+60",
            "3601",
            "-1",
            "18446744073709551616",
            "later",
            " 60",
            "60 ",
        ] {
            assert!(value.parse::<RetryAfter>().is_err());
            assert!(serde_json::from_str::<RetryAfter>(&format!("\"{value}\"")).is_err());
        }
        for value in ["0", "3601", "18446744073709551616"] {
            assert!(serde_json::from_str::<RetryAfter>(value).is_err());
        }
        Ok(())
    }
}
