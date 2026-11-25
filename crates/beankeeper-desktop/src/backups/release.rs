use std::{fmt, str::FromStr, sync::OnceLock};

use beankeeper_cli::error::CliError;
use semver::Version;
use serde::{Deserialize, Serialize};

use super::git_oid::GitOid;

/// The Desktop version and core Git object ID that produced a backup.
#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(try_from = "String", into = "String")]
pub(super) struct Release {
    desktop: Version,
    core: GitOid,
}

impl Release {
    pub(super) fn current() -> &'static Self {
        static CURRENT: OnceLock<Release> = OnceLock::new();
        const BUILT_RELEASE: &str =
            concat!(env!("CARGO_PKG_VERSION"), "/", env!("BK_CORE_REVISION"));

        CURRENT.get_or_init(|| {
            BUILT_RELEASE
                .parse()
                .unwrap_or_else(|error| panic!("invalid build release {BUILT_RELEASE}: {error}"))
        })
    }
}

impl Default for Release {
    fn default() -> Self {
        Self::current().clone()
    }
}

impl FromStr for Release {
    type Err = CliError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let invalid =
            || CliError::Validation("expected a Desktop version and core Git revision".into());
        let (desktop, revision) = value.split_once('/').ok_or_else(invalid)?;
        Ok(Self {
            desktop: desktop.parse().map_err(|_| invalid())?,
            core: revision.parse().map_err(|_| invalid())?,
        })
    }
}

impl fmt::Display for Release {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.desktop, self.core.as_ref())
    }
}

impl TryFrom<String> for Release {
    type Error = CliError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        value.parse()
    }
}

impl From<Release> for String {
    fn from(release: Release) -> Self {
        release.to_string()
    }
}
