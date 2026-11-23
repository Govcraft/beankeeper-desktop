//! Desktop grant presets and their status as reported from a verified credential.

use beankeeper_cli::{
    capability::Capability,
    identity::{VerifiedAgent, WriteMode},
};
use clap::{Args, ValueEnum};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize, Serializer, ser::SerializeStruct};
use serde_with::{As, DisplayFromStr};

/// Least-privilege grants for the owner's MCP client.
#[derive(
    Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq, Serialize, ValueEnum, JsonSchema,
)]
#[serde(rename_all = "kebab-case")]
pub enum Preset {
    /// Read reports and ledger data. The standalone setup default.
    #[default]
    ReadOnly,
    /// Read, post and propose.
    Bookkeeper,
}

impl Preset {
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

/// Access settings accepted by both the local CLI and MCP.
#[derive(Args, Clone, Copy, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AccessOptions {
    /// Choose read-only or bookkeeper access.
    #[arg(long, value_enum)]
    preset: Preset,
    /// Choose direct posting or proposals (bookkeeper only for proposals).
    #[arg(long)]
    #[serde(
        default,
        deserialize_with = "As::<Option<DisplayFromStr>>::deserialize"
    )]
    #[schemars(schema_with = "write_mode_schema")]
    write_mode: Option<WriteMode>,
}

// Upstream owns the parser. This only describes the accepted wire values.
fn write_mode_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
    schemars::json_schema!({ "enum": [WriteMode::Direct, WriteMode::ProposeOnly, null] })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ManagedAccess {
    ReadOnly,
    Bookkeeper(WriteMode),
}

impl Default for ManagedAccess {
    fn default() -> Self {
        Self::Bookkeeper(WriteMode::default())
    }
}

impl From<Preset> for ManagedAccess {
    fn from(preset: Preset) -> Self {
        match preset {
            Preset::ReadOnly => Self::ReadOnly,
            Preset::Bookkeeper => Self::default(),
        }
    }
}

impl TryFrom<AccessOptions> for ManagedAccess {
    type Error = UnrecognizedPreset;

    fn try_from(options: AccessOptions) -> Result<Self, Self::Error> {
        match (options.preset, options.write_mode) {
            (Preset::ReadOnly, Some(WriteMode::ProposeOnly)) => Err(UnrecognizedPreset),
            (Preset::Bookkeeper, Some(mode)) => Ok(Self::Bookkeeper(mode)),
            (preset, _) => Ok(Self::from(preset)),
        }
    }
}

impl ManagedAccess {
    pub(super) const fn selection(self) -> (Preset, WriteMode) {
        match self {
            Self::ReadOnly => (Preset::ReadOnly, WriteMode::Direct),
            Self::Bookkeeper(mode) => (Preset::Bookkeeper, mode),
        }
    }

    pub(super) fn grants(self) -> (&'static [Capability], WriteMode) {
        let (preset, mode) = self.selection();
        (preset.capabilities(), mode)
    }

    pub(super) fn matches(self, agent: &VerifiedAgent) -> bool {
        let (preset, mode) = self.selection();
        agent.write_mode() == mode && Preset::try_from(agent) == Ok(preset)
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
    #[serde(default)]
    migrated: bool,
}

impl From<&VerifiedAgent> for AgentAccess {
    fn from(agent: &VerifiedAgent) -> Self {
        Self {
            preset: Preset::try_from(agent).ok(),
            write_mode: agent.write_mode(),
            migrated: false,
        }
    }
}

impl From<ManagedAccess> for AgentAccess {
    fn from(access: ManagedAccess) -> Self {
        let (preset, write_mode) = access.selection();
        Self {
            preset: Some(preset),
            write_mode,
            migrated: false,
        }
    }
}

impl Serialize for AgentAccess {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut access =
            serializer.serialize_struct("AgentAccess", 3 + usize::from(self.migrated))?;
        access.serialize_field("preset", &self.preset)?;
        access.serialize_field("write_mode", &self.write_mode)?;
        access.serialize_field("note", self.note())?;
        if self.migrated {
            access.serialize_field("migrated", &true)?;
        }
        access.end()
    }
}

impl AgentAccess {
    pub(crate) const fn with_migration_notice(mut self) -> Self {
        self.migrated = true;
        self
    }

    pub(crate) const fn note(&self) -> &'static str {
        if self.migrated {
            return "I can now record transactions for you. Say ‘make me read-only’ to undo.";
        }
        match (self.preset, self.write_mode) {
            (Some(Preset::ReadOnly), _) => "This agent can read the books.",
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
