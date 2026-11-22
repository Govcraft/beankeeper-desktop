//! Desktop grant presets and their status as reported from a verified credential.

use beankeeper_cli::{
    capability::Capability,
    identity::{VerifiedAgent, WriteMode},
};
use clap::ValueEnum;
use serde::{Deserialize, Deserializer, Serialize};

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
#[derive(Debug, Deserialize, Serialize)]
pub struct AgentAccess {
    preset: Option<Preset>,
    #[serde(deserialize_with = "deserialize_write_mode")]
    write_mode: WriteMode,
    note: String,
}

impl From<&VerifiedAgent> for AgentAccess {
    fn from(agent: &VerifiedAgent) -> Self {
        let preset = Preset::try_from(agent).ok();
        let write_mode = agent.write_mode();
        let note = match (preset, write_mode) {
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
        };
        Self {
            preset,
            write_mode,
            note: note.into(),
        }
    }
}

impl AgentAccess {
    pub(crate) fn note(&self) -> &str {
        &self.note
    }
}

fn deserialize_write_mode<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<WriteMode, D::Error> {
    String::deserialize(deserializer)?
        .parse()
        .map_err(serde::de::Error::custom)
}
