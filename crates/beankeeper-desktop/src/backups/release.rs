use std::{fmt, str::FromStr, sync::OnceLock};

use beankeeper_cli::error::CliError;
use semver::Version;
use serde::{Deserialize, Serialize};

/// The Desktop version and core Git object ID that produced a backup.
#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(try_from = "String", into = "String")]
pub(super) struct Release((Version, Option<String>));

impl Release {
    const LEGACY_REVISION: &str = "unknown";

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
        let desktop = desktop.parse().map_err(|_| invalid())?;
        let core_revision = match revision {
            // Earlier builds could write this sentinel. Keep those backups
            // readable and let a known revision trigger a fresh backup.
            Self::LEGACY_REVISION => None,
            oid if matches!(oid.len(), 40 | 64)
                && oid.bytes().all(|byte| byte.is_ascii_hexdigit()) =>
            {
                Some(oid.to_ascii_lowercase())
            }
            _ => return Err(invalid()),
        };
        Ok(Self((desktop, core_revision)))
    }
}

impl fmt::Display for Release {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Self((desktop, core_revision)) = self;
        let core = core_revision.as_deref().unwrap_or(Self::LEGACY_REVISION);
        write!(f, "{desktop}/{core}")
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
