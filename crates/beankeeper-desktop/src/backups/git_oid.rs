use std::{error::Error, fmt, str::FromStr};

/// A full SHA-1 or SHA-256 Git object ID in canonical lowercase form.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct GitOid(String);

#[derive(Debug)]
pub(super) struct InvalidGitOid;

impl fmt::Display for InvalidGitOid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("expected a full Git object ID")
    }
}

impl Error for InvalidGitOid {}

impl FromStr for GitOid {
    type Err = InvalidGitOid;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if matches!(value.len(), 40 | 64) && value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            Ok(Self(value.to_ascii_lowercase()))
        } else {
            Err(InvalidGitOid)
        }
    }
}

impl AsRef<str> for GitOid {
    fn as_ref(&self) -> &str {
        &self.0
    }
}
