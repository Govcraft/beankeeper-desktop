//! Desktop grant presets and their status as reported from a verified credential.

use beankeeper_cli::{
    capability::Capability,
    identity::{VerifiedAgent, WriteMode},
};
use clap::ValueEnum;
use serde::{Deserialize, Serialize, Serializer, ser::SerializeStruct};
use serde_with::{As, DisplayFromStr};

/// Least-privilege grants for the owner's MCP client.
#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq, Serialize, ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub enum Preset {
    /// Read reports and ledger data. The standalone setup default.
    #[default]
    ReadOnly,
    /// Read, post and propose.
    Bookkeeper,
}

impl Preset {
    pub(super) const MANAGED: Self = Self::Bookkeeper;

    pub(super) fn capabilities(self) -> &'static [Capability] {
        match self {
            Self::ReadOnly => &[Capability::Read],
            Self::Bookkeeper => &[Capability::Read, Capability::Post, Capability::Propose],
        }
    }

    fn matches(self, agent: &VerifiedAgent) -> bool {
        let (capabilities, mode) = (agent.capabilities(), agent.write_mode());
        let expected = match (self, mode) {
            (Self::Bookkeeper, WriteMode::ProposeOnly) => &[Capability::Read, Capability::Propose],
            (Self::ReadOnly, WriteMode::ProposeOnly) => return false,
            (_, WriteMode::Direct) => self.capabilities(),
        };
        expected
            .iter()
            .all(|capability| capabilities.iter().any(|grant| grant == capability.path()))
            && capabilities.iter().all(|grant| {
                expected.iter().any(|capability| grant == capability.path())
                    || WriteMode::from(std::slice::from_ref(grant)) == WriteMode::ProposeOnly
            })
    }
}

/// A verified grant that does not match either Desktop preset.
#[derive(Debug, PartialEq, Eq)]
pub struct UnrecognizedPreset;

impl TryFrom<&VerifiedAgent> for Preset {
    type Error = UnrecognizedPreset;

    fn try_from(agent: &VerifiedAgent) -> Result<Self, Self::Error> {
        [Preset::ReadOnly, Preset::Bookkeeper]
            .into_iter()
            .find(|preset| preset.matches(agent))
            .ok_or(UnrecognizedPreset)
    }
}

/// The caller's current grants and signed write mode, with no credential material.
#[derive(Debug, Deserialize)]
pub struct AgentAccess {
    preset: Option<Preset>,
    #[serde(deserialize_with = "As::<DisplayFromStr>::deserialize")]
    write_mode: WriteMode,
}

impl From<&VerifiedAgent> for AgentAccess {
    fn from(agent: &VerifiedAgent) -> Self {
        Self {
            preset: Preset::try_from(agent).ok(),
            write_mode: agent.write_mode(),
        }
    }
}

impl Serialize for AgentAccess {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut access = serializer.serialize_struct("AgentAccess", 3)?;
        access.serialize_field("preset", &self.preset)?;
        access.serialize_field("write_mode", &self.write_mode)?;
        access.serialize_field("note", self.note())?;
        access.end()
    }
}

impl AgentAccess {
    pub(crate) const fn note(&self) -> &'static str {
        match (self.preset, self.write_mode) {
            (Some(Preset::ReadOnly), _) => {
                "This agent can read the books. The owner can issue a bookkeeper credential to enable posting."
            }
            (Some(Preset::Bookkeeper), WriteMode::Direct) => {
                "This agent records transactions directly. After a post, tell the owner what was recorded and give them the transaction ID for undo."
            }
            (Some(Preset::Bookkeeper), WriteMode::ProposeOnly) => {
                "This agent submits transactions for sign-off. A separate credential with approval access can post them."
            }
            (None, _) => {
                "This credential has a custom grant. Its signed permissions determine which actions it can take."
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::AgentAccess;

    #[test]
    fn status_note_is_derived_from_the_preset_and_write_mode() -> serde_json::Result<()> {
        for (preset, write_mode, guidance) in [
            (Some("read-only"), "direct", "can read the books"),
            (
                Some("bookkeeper"),
                "direct",
                "records transactions directly",
            ),
            (Some("bookkeeper"), "propose-only", "for sign-off"),
            (None, "direct", "custom grant"),
        ] {
            let mut wire = json!({ "preset": preset, "write_mode": write_mode });
            let access: AgentAccess = serde_json::from_value(wire.clone())?;
            assert!(access.note().contains(guidance));

            wire["note"] = json!("This agent can administer the ledger.");
            let received: AgentAccess = serde_json::from_value(wire.clone())?;
            assert_eq!(received.note(), access.note());
            wire["note"] = json!(access.note());
            assert_eq!(serde_json::to_value(received)?, wire);
        }
        Ok(())
    }
}
