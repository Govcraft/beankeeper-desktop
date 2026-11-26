//! Desktop grant presets and their status as reported from a verified credential.

use std::sync::LazyLock;

use beankeeper_cli::{
    capability::Capability,
    identity::{VerifiedAgent, WriteMode},
};
use clap::{Args, ValueEnum};
use rmcp::ErrorData;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize, Serializer, ser::SerializeStruct};
use serde_with::{As, DisplayFromStr};

use crate::{agent_skill, service::ACCESS_TOOL};

static READ_ONLY_NOTE: LazyLock<String> = LazyLock::new(|| {
    format!(
        "This agent can read the books. Restarting cannot grant posting access. For managed access, \
         the owner can say “let my agent post” in chat. If they ask, use {ACCESS_TOOL} on the local \
         lifecycle or managed ledger connection. Respect the owner's choice to stay read-only."
    )
});

const AGENT_UNDO_GUIDANCE: &str = "Correct your own work in chat with audit_undo by receipt ID or txn_reverse by transaction ID when the tool is listed. Never make offsetting entries by hand. Offer Desktop links only for viewing and handle undo, tagging, renaming, and data entry in chat.";
const PROPOSE_ONLY_NOTE: &str = "This agent submits transactions for sign-off. Undo and reversal also return pending proposals for an authorized approver.";
static DIRECT_NOTE: LazyLock<String> = LazyLock::new(|| {
    format!(
        "This agent records transactions directly. Use changes_since when available to tell the owner what changed. {AGENT_UNDO_GUIDANCE}"
    )
});
static PROPOSE_NOTE: LazyLock<String> =
    LazyLock::new(|| format!("{PROPOSE_ONLY_NOTE} {AGENT_UNDO_GUIDANCE}"));

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

    pub(crate) fn note(&self) -> &'static str {
        if self.migrated {
            return "I can now record transactions for you. Say ‘make me read-only’ to undo.";
        }
        match (self.preset, self.write_mode) {
            (Some(Preset::ReadOnly), _) => &READ_ONLY_NOTE,
            (Some(Preset::Bookkeeper), WriteMode::Direct) => &DIRECT_NOTE,
            (Some(Preset::Bookkeeper), WriteMode::ProposeOnly) => &PROPOSE_NOTE,
            (None, _) => {
                "This credential has a custom grant. Its signed permissions determine which actions it can take."
            }
        }
    }

    pub(crate) fn mcp_instructions() -> Result<&'static str, ErrorData> {
        static INSTRUCTIONS: LazyLock<Result<String, ErrorData>> = LazyLock::new(|| {
            let skill = agent_skill::EmbeddedSkill::get()?;
            Ok(format!(
                "When the owner asks to set up their books or a posting tool is missing, call \
                 setup_status on the ledger connection if listed. For setup, explain the next step \
                 and ask for any missing facts. {demo} For service readiness, use service_status on the \
                 lifecycle connection if available. Explain the verified agent_access. \
                 If preset is read-only, follow this guidance: {read_only} \
                 Call {ACCESS_TOOL} only when the owner asks to change access to those managed \
                 books and the tool is listed. For direct posting, use preset bookkeeper and \
                 write_mode direct. Use preset read-only to turn posting off. If \
                 agent_access.migrated is true, tell the \
                 owner they can now ask you to record transactions and can say “make me read-only” \
                 to undo the access change. \
                 With preset bookkeeper and write_mode direct, use txn_post when asked to record \
                 a transaction. Follow the owner's inform preference after recording changes. By \
                 default, report them in the same conversation after the work is complete. Also \
                 report when asked what changed. When listed, use changes_since on the ledger \
                 connection to explain the changes in plain words, offer undo in chat, and offer \
                 receipt links for viewing. After delivering each page, pass its company and \
                 through to changes_mark_seen if listed. If changes_since is unavailable after a \
                 post, tell the owner what was recorded and give the book and transaction ID. \
                 With preset bookkeeper, \
                 use txn_recategorize when listed to fix a posted category. {AGENT_UNDO_GUIDANCE} If a typed rule blocks \
                 a correction, explain it in plain words and take the next authorized step named \
                 by the error. If neither tool is available, explain that in chat and check for \
                 the CLI equivalent or a newer signed release. If an earlier hand-made offset \
                 and its original should both be undone, explain the pair and undo each by receipt \
                 only when the owner asks you to undo both in chat. \
                 With write_mode propose-only, report the proposal ID as pending. \
                 {PROPOSE_ONLY_NOTE} If preset is null, the grant is custom. \
                 Use the listed tools within its signed write mode. If agent_access itself is null, \
                 use the status and its error code to explain why access is unverified. Never ask \
                 for, read, or share owner and approval credentials, recovery material, or \
                 passphrases. {voice} Include chartable data when available. Offer \
                 ui_get_link for optional Desktop detail. Read update_status when listed to learn \
                 about cached updates without network access. Mention an available release once \
                 per conversation in plain words, offer help, and continue the owner's work. \
                 Never block bookkeeping on an update. Use update_check only when the owner \
                 asks for a fresh check.",
                read_only = READ_ONLY_NOTE.as_str(),
                demo = skill.demo(),
                voice = skill.voice(),
            ))
        });
        INSTRUCTIONS.as_deref().map_err(Clone::clone)
    }
}

#[cfg(test)]
mod tests {
    use rmcp::ErrorData;
    use serde_json::json;

    use super::{AgentAccess, ManagedAccess, Preset, WriteMode};

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
            (None, "propose-only", "custom grant"),
        ] {
            let mut wire = json!({ "preset": preset, "write_mode": write_mode });
            let access: AgentAccess = serde_json::from_value(wire.clone())?;
            assert!(access.note().contains(guidance));

            wire["note"] = json!("This agent can administer the ledger.");
            wire["docs_url"] = json!("https://example.invalid");
            let received: AgentAccess = serde_json::from_value(wire.clone())?;
            assert_eq!(received.note(), access.note());
            assert_eq!(
                serde_json::to_value(received)?,
                json!({ "preset": preset, "write_mode": write_mode, "note": access.note() })
            );
        }
        Ok(())
    }

    #[test]
    fn read_only_note_and_mcp_instructions_give_the_same_chat_guidance() -> Result<(), ErrorData> {
        let access = AgentAccess::from(ManagedAccess::ReadOnly);
        let note = access.note();
        let instructions = AgentAccess::mcp_instructions()?;
        for guidance in [
            "Restarting cannot grant posting access",
            "“let my agent post” in chat",
            "use access_set on the local lifecycle or managed ledger connection",
            "Respect the owner's choice to stay read-only",
        ] {
            assert!(note.contains(guidance), "{note}");
        }
        assert!(instructions.contains(note));
        for guidance in [
            "Call access_set only when the owner asks to change access to those managed books and the tool is listed",
            "say “make me read-only”",
            "If agent_access itself is null",
        ] {
            assert!(instructions.contains(guidance), "{instructions}");
        }
        assert!(!instructions.contains("docs_url"));
        Ok(())
    }

    #[test]
    fn mcp_instructions_keep_blocked_corrections_and_legacy_cleanup_in_chat()
    -> Result<(), ErrorData> {
        let instructions = AgentAccess::mcp_instructions()?;
        for guidance in [
            "Never make offsetting entries by hand",
            "Offer Desktop links only for viewing",
            "take the next authorized step named by the error",
            "undo each by receipt only when the owner asks you to undo both in chat",
        ] {
            assert!(instructions.contains(guidance), "{guidance}");
        }
        Ok(())
    }

    #[test]
    fn mcp_instructions_cover_each_serialized_access_state()
    -> Result<(), Box<dyn std::error::Error>> {
        let instructions = AgentAccess::mcp_instructions().map_err(|error| error.message)?;
        for (preset, write_mode, action) in [
            (
                Some(Preset::ReadOnly),
                WriteMode::Direct,
                "Restarting cannot grant posting access",
            ),
            (
                Some(Preset::Bookkeeper),
                WriteMode::Direct,
                "audit_undo by receipt ID or txn_reverse by transaction ID",
            ),
            (
                Some(Preset::Bookkeeper),
                WriteMode::ProposeOnly,
                "Undo and reversal also return pending proposals",
            ),
            (None, WriteMode::Direct, "the grant is custom"),
            (None, WriteMode::ProposeOnly, "within its signed write mode"),
        ] {
            let access = AgentAccess {
                preset,
                write_mode,
                migrated: false,
            };
            let reported = serde_json::to_value(access)?;
            let name = reported["preset"].as_str().unwrap_or("null");
            let mode = reported["write_mode"].as_str().unwrap_or_default();
            let condition = match (preset, write_mode) {
                (None | Some(Preset::ReadOnly), _) => format!("If preset is {name}"),
                (Some(Preset::Bookkeeper), WriteMode::Direct) => {
                    format!("With preset {name} and write_mode {mode}")
                }
                (Some(Preset::Bookkeeper), WriteMode::ProposeOnly) => {
                    format!("With write_mode {mode}")
                }
            };
            assert!(instructions.contains(&condition), "{reported}");
            assert!(instructions.contains(action), "{reported}");
        }
        Ok(())
    }
}
