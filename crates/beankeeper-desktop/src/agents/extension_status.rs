//! Read-only Claude installation status. The registry is never changed here.

use super::{Agent, AgentError, Locations, config::read_config_text};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, fmt, path::Path};

const ID: &str = "local.mcpb.govcraft.beankeeper";

/// A comparison constructed only from parsed semantic versions.
#[derive(Debug, Serialize)]
#[serde(transparent)]
pub(crate) struct ExtensionStatus(State);

#[derive(Debug, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
enum State {
    Current {
        installed: semver::Version,
        running: semver::Version,
    },
    Behind {
        installed: semver::Version,
        running: semver::Version,
    },
    NotInstalled,
}

impl From<(Option<semver::Version>, semver::Version)> for ExtensionStatus {
    fn from((installed, running): (Option<semver::Version>, semver::Version)) -> Self {
        Self(match installed {
            None => State::NotInstalled,
            Some(installed) if installed < running => State::Behind { installed, running },
            Some(installed) => State::Current { installed, running },
        })
    }
}

impl fmt::Display for ExtensionStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.0 {
            State::Behind { installed, running } => write!(f,
                "Beankeeper's Claude extension is out of date ({installed}; Desktop {running}). Ask your agent to upgrade the extension. Click Install in Claude, then restart Claude Desktop or toggle the extension to load it."),
            State::Current { installed, .. } => write!(f, "Beankeeper's Claude extension is current ({installed})."),
            State::NotInstalled => f.write_str("Beankeeper's Claude extension is not installed. Your other connections can still use the books."),
        }
    }
}

#[derive(Deserialize)]
struct Installations {
    extensions: BTreeMap<String, serde_json::Value>,
}

#[derive(Deserialize)]
struct Installation {
    version: semver::Version,
}

fn read(path: &Path, running: semver::Version) -> Result<ExtensionStatus, AgentError> {
    let installed = read_config_text(path)?
        .map(|text| {
            let registry: Installations =
                serde_json::from_str(&text).map_err(|_| AgentError::Invalid(path.into()))?;
            registry
                .extensions
                .get(ID)
                .map(|value| {
                    Installation::deserialize(value)
                        .map(|entry| entry.version)
                        .map_err(|_| AgentError::Invalid(path.into()))
                })
                .transpose()
        })
        .transpose()?
        .flatten();
    Ok((installed, running).into())
}

fn discover(running: semver::Version) -> Result<ExtensionStatus, AgentError> {
    let locations = Locations::discover().ok_or(AgentError::Home)?;
    let adapter = Agent::ClaudeDesktop
        .adapter(&locations)?
        .ok_or(AgentError::Home)?;
    let path = adapter.extension_registry.ok_or(AgentError::Home)?;
    read(&path, running)
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Unavailable {
    Unavailable,
}

/// Diagnostic failure is information, never authority to block the books.
#[derive(Serialize)]
#[serde(untagged)]
pub(crate) enum Observation {
    Known(ExtensionStatus),
    Unavailable {
        state: Unavailable,
        code: &'static str,
        message: String,
    },
}

impl Observation {
    pub(crate) fn discover() -> Self {
        Self::from_service(crate::service::status())
    }

    fn from_service(status: Result<crate::service::Status, crate::service::ServiceError>) -> Self {
        match status {
            Err(crate::service::ServiceError::Version) => Self::Unavailable {
                state: Unavailable::Unavailable,
                code: crate::service::ServiceError::Version.code(),
                message: "Could not read the service version. Your books are still available."
                    .into(),
            },
            status => {
                let running = status
                    .ok()
                    .and_then(|status| status.version)
                    .map_or_else(|| env!("CARGO_PKG_VERSION").parse(), Ok)
                    .map_err(|_| AgentError::Version);
                Self::from(running.and_then(discover))
            }
        }
    }

    pub(crate) fn at_version(version: semver::Version) -> Self {
        Self::from(discover(version))
    }
}

impl From<Result<ExtensionStatus, AgentError>> for Observation {
    fn from(result: Result<ExtensionStatus, AgentError>) -> Self {
        match result {
            Ok(status) => Self::Known(status),
            Err(error) => Self::Unavailable {
                state: Unavailable::Unavailable,
                code: error.code(),
                message:
                    "Could not read the Claude extension version. Your books are still available."
                        .into(),
            },
        }
    }
}

impl fmt::Display for Observation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Known(status) => status.fmt(f),
            Self::Unavailable { message, .. } => f.write_str(message),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn invalid_service_version_has_a_distinct_diagnostic() -> Result<(), serde_json::Error> {
        let observation = Observation::from_service(Err(crate::service::ServiceError::Version));
        let result = serde_json::to_value(observation)?;
        assert_eq!(result["code"], "SERVICE_VERSION_INVALID");
        Ok(())
    }

    #[test]
    fn registry_states_are_semantic_read_only_and_ignore_other_extensions()
    -> Result<(), Box<dyn std::error::Error>> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("extensions-installations.json");
        let running: semver::Version = "0.1.0-rc.33".parse()?;
        assert!(matches!(
            read(&path, running.clone())?.0,
            State::NotInstalled
        ));
        for (version, behind) in [
            ("0.1.0-rc.9", true),
            ("0.1.0-rc.33", false),
            ("0.1.0-rc.34", false),
        ] {
            let bytes = serde_json::to_vec(&json!({"extensions": {
                ID: {"version": version}, "other": {"version": "not a version"}
            }}))?;
            std::fs::write(&path, &bytes)?;
            let status = read(&path, running.clone())?;
            assert_eq!(matches!(status.0, State::Behind { .. }), behind);
            assert_eq!(std::fs::read(&path)?, bytes);
            if behind {
                assert!(status.to_string().contains("restart Claude Desktop"));
            }
        }
        std::fs::write(&path, r#"{"extensions":{}}"#)?;
        assert!(matches!(
            read(&path, running.clone())?.0,
            State::NotInstalled
        ));
        for invalid in [
            r#"{"extensions":{"local.mcpb.govcraft.beankeeper":{"version":"bad"}}}"#,
            "invalid json",
        ] {
            std::fs::write(&path, invalid)?;
            assert!(read(&path, running.clone()).is_err());
            assert_eq!(std::fs::read_to_string(&path)?, invalid);
        }
        Ok(())
    }
}
