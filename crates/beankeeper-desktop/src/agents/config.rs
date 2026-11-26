//! Edit exact Beankeeper registrations, preserving unrelated client configuration.

use std::{
    fs::{self, File},
    io::{self, Read, Write},
    path::{Path, PathBuf},
};

use jsonc_parser::cst::{CstObject, CstObjectProp, CstRootNode};
use toml_edit::{DocumentMut, Item, Table, TableLike, Value, value};

use crate::agent_skill::SKILL;

use super::{
    AgentError,
    adapters::{Adapter, ConfigFormat, TomlClient},
};

mod env_overrides;
mod json;

pub(super) use json::ServerConflict;
use json::{ConflictField, JsonClient, ManagedServer, ParsedServer};

const MAX_CONFIG_BYTES: u64 = 4 * 1024 * 1024;
const SKILL_MARKER: &str = "\n<!-- Managed by Beankeeper Desktop agent registration. -->\n";

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Registration {
    Missing,
    Partial,
    Owned,
}

enum OwnedEntry {
    Canonical,
    NeedsRewrite,
    Disabled,
}

#[derive(Default)]
enum CodexServerActivation {
    #[default]
    Enabled,
    Disabled,
}

impl TryFrom<Option<&Item>> for CodexServerActivation {
    type Error = ServerConflict;

    fn try_from(field: Option<&Item>) -> Result<Self, Self::Error> {
        match field {
            None => Ok(Self::default()),
            Some(field) => match field.as_bool() {
                Some(true) => Ok(Self::Enabled),
                Some(false) => Ok(Self::Disabled),
                None => Err(ConflictField::Unknown("enabled").into()),
            },
        }
    }
}

impl From<CodexServerActivation> for OwnedEntry {
    fn from(activation: CodexServerActivation) -> Self {
        match activation {
            CodexServerActivation::Enabled => Self::Canonical,
            CodexServerActivation::Disabled => Self::Disabled,
        }
    }
}

#[derive(Clone, Copy)]
pub(super) enum Change {
    Connect,
    Disconnect,
}

#[derive(Clone, Copy)]
enum Server {
    Ledger,
    Lifecycle,
}

impl Server {
    const ALL: [Self; 2] = [Self::Ledger, Self::Lifecycle];

    const fn name(self) -> &'static str {
        match self {
            Self::Ledger => "beankeeper",
            Self::Lifecycle => "beankeeper-service",
        }
    }

    const fn arguments(self) -> &'static [&'static str] {
        match self {
            Self::Ledger => &["mcp", "bridge", "--service"],
            Self::Lifecycle => &["service", "mcp"],
        }
    }
}

struct FileSnapshot {
    path: PathBuf,
    before: Option<String>,
}

impl FileSnapshot {
    fn read(path: &Path) -> Result<Self, AgentError> {
        let error_at = |error| AgentError::Io(path.into(), error);
        let before = match fs::symlink_metadata(path) {
            Ok(metadata) if metadata.is_file() && metadata.len() <= MAX_CONFIG_BYTES => {
                let file = File::open(path).map_err(error_at)?;
                let mut contents = String::new();
                file.take(MAX_CONFIG_BYTES + 1)
                    .read_to_string(&mut contents)
                    .map_err(error_at)?;
                if u64::try_from(contents.len()).map_or(true, |len| len > MAX_CONFIG_BYTES) {
                    return Err(AgentError::Invalid(path.into()));
                }
                Some(contents)
            }
            Ok(_) => return Err(AgentError::Invalid(path.into())),
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(error) => return Err(error_at(error)),
        };
        Ok(Self {
            path: path.into(),
            before,
        })
    }

    fn contents(&self) -> &str {
        self.before.as_deref().unwrap_or_default()
    }
}

pub(super) fn read_config_text(path: &Path) -> Result<Option<String>, AgentError> {
    Ok(FileSnapshot::read(path)?.before)
}

pub(super) struct FileChange {
    original: FileSnapshot,
    revised: Option<String>,
}

impl FileChange {
    pub(super) fn commit(&self) -> Result<(), AgentError> {
        let path = self.original.path.as_path();
        if FileSnapshot::read(path)?.before != self.original.before {
            return Err(AgentError::Conflict(path.into()));
        }
        if let Some(original) = &self.original.before {
            let suffix = path
                .file_name()
                .ok_or_else(|| AgentError::Invalid(path.into()))?;
            let mut filename = suffix.to_os_string();
            filename.push(".beankeeper-backup");
            write_private(
                &path.with_file_name(filename),
                original.as_bytes(),
                WriteMode::Backup,
            )
            .map_err(|error| AgentError::Io(path.into(), error))?;
        }
        self.replace(self.revised.as_deref())
    }

    fn restore(&self) -> Result<(), AgentError> {
        let path = &self.original.path;
        if FileSnapshot::read(path)?.before != self.revised {
            return Err(AgentError::Conflict(path.clone()));
        }
        self.replace(self.original.before.as_deref())
    }

    fn replace(&self, contents: Option<&str>) -> Result<(), AgentError> {
        let path = &self.original.path;
        match contents {
            Some(contents) => write_private(path, contents.as_bytes(), WriteMode::Replace),
            None => fs::remove_file(path),
        }
        .map_err(|error| AgentError::Io(path.clone(), error))
    }
}

#[derive(Clone, Copy)]
enum WriteMode {
    Backup,
    Replace,
}

fn write_private(path: &Path, content: &[u8], mode: WriteMode) -> io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::other("config path has no parent"))?;
    fs::create_dir_all(parent)?;
    let mut temp = tempfile::NamedTempFile::new_in(parent)?;
    crate::setup::keychain::restrict_file(temp.path())?;
    temp.write_all(content)?;
    temp.as_file().sync_all()?;
    match mode {
        WriteMode::Replace => temp.persist(path).map(drop).map_err(|error| error.error),
        WriteMode::Backup => match temp.persist_noclobber(path) {
            Ok(_) => Ok(()),
            Err(error) if error.error.kind() == io::ErrorKind::AlreadyExists => Ok(()),
            Err(error) => Err(error.error),
        },
    }
}

enum Document {
    Json {
        root: CstRootNode,
        kind: JsonClient,
    },
    Toml {
        document: DocumentMut,
        client: TomlClient,
    },
}

#[derive(Clone, Copy)]
pub(super) struct KnownExecutables<'a> {
    pub(super) current: &'a str,
    pub(super) managed_service: Option<&'a str>,
}

impl<'a> From<&'a str> for KnownExecutables<'a> {
    fn from(current: &'a str) -> Self {
        Self {
            current,
            managed_service: None,
        }
    }
}

impl KnownExecutables<'_> {
    fn matches(self, command: &str) -> bool {
        command == self.current || self.managed_service == Some(command)
    }
}

fn property(
    object: &CstObject,
    name: &str,
    path: &Path,
) -> Result<Option<CstObjectProp>, AgentError> {
    let mut matches = object.properties().into_iter().filter(|prop| {
        prop.name()
            .and_then(|name| name.decoded_value().ok())
            .is_some_and(|key| key == name)
    });
    let first = matches.next();
    if matches.next().is_some() {
        return Err(AgentError::Invalid(path.into()));
    }
    Ok(first)
}

fn owned_toml_entry(
    fields: &dyn TableLike,
    client: TomlClient,
    executables: KnownExecutables<'_>,
    server: Server,
    path: &Path,
) -> Result<OwnedEntry, AgentError> {
    let codex = matches!(client, TomlClient::Codex);
    let allowed = fields
        .iter()
        .all(|(key, _)| matches!(key, "command" | "args") || codex && key == "enabled");
    let command_matches = fields
        .get("command")
        .and_then(Item::as_str)
        .is_some_and(|command| executables.matches(command));
    let args_match = fields
        .get("args")
        .and_then(Item::as_array)
        .is_some_and(|items| {
            items
                .iter()
                .map(Value::as_str)
                .eq(server.arguments().iter().copied().map(Some))
        });
    if !(allowed && command_matches && args_match) {
        return Err(AgentError::Conflict(path.into()));
    }
    CodexServerActivation::try_from(fields.get("enabled"))
        .map(OwnedEntry::from)
        .map_err(|reason| AgentError::RegistrationConflict {
            path: path.into(),
            server: server.name(),
            reason,
        })
}

impl Document {
    fn parse(file: &FileSnapshot, format: &ConfigFormat) -> Result<Self, AgentError> {
        let invalid = || AgentError::Invalid(file.path.clone());
        let kind = match format {
            ConfigFormat::Toml(client) => {
                return file
                    .contents()
                    .parse()
                    .map(|document| Self::Toml {
                        document,
                        client: *client,
                    })
                    .map_err(|_| invalid());
            }
            ConfigFormat::Json => JsonClient::Standard,
            ConfigFormat::Copilot => JsonClient::Copilot,
            ConfigFormat::VsCode => JsonClient::VsCode,
        };
        let root = CstRootNode::parse(file.contents(), &kind.options()).map_err(|_| invalid())?;
        if root.value().is_some() && root.object_value().is_none() {
            return Err(invalid());
        }
        Ok(Self::Json { root, kind })
    }

    fn owned_entry(
        &self,
        executables: KnownExecutables<'_>,
        server: Server,
        path: &Path,
    ) -> Result<Option<OwnedEntry>, AgentError> {
        let invalid = || AgentError::Invalid(path.into());
        match self {
            Self::Json { root, kind } => {
                let Some(object) = root.object_value() else {
                    return Ok(None);
                };
                let Some(servers) = property(&object, kind.key(), path)? else {
                    return Ok(None);
                };
                let servers = servers.object_value().ok_or_else(invalid)?;
                let Some(entry) = property(&servers, server.name(), path)? else {
                    return Ok(None);
                };
                let expected = ManagedServer::from((*kind, server, executables.current));
                ParsedServer::try_from((*kind, &entry))
                    .and_then(|parsed| parsed.check(&expected, executables))
                    .map(Some)
                    .map_err(|reason| AgentError::RegistrationConflict {
                        path: path.into(),
                        server: server.name(),
                        reason,
                    })
            }
            Self::Toml { document, client } => {
                let Some(servers) = document.get("mcp_servers") else {
                    return Ok(None);
                };
                let servers = servers.as_table_like().ok_or_else(invalid)?;
                let Some(entry) = servers.get(server.name()) else {
                    return Ok(None);
                };
                let fields = entry
                    .as_table_like()
                    .ok_or_else(|| AgentError::Conflict(path.into()))?;
                owned_toml_entry(fields, *client, executables, server, path).map(Some)
            }
        }
    }

    fn edited(
        mut self,
        executables: KnownExecutables<'_>,
        change: Change,
        path: &Path,
    ) -> Result<Option<String>, AgentError> {
        let mut changed = false;
        for server in Server::ALL {
            match (change, self.owned_entry(executables, server, path)?) {
                (Change::Connect, Some(OwnedEntry::Disabled)) => {
                    return Err(AgentError::ServerDisabled(server.name()));
                }
                (Change::Connect, None | Some(OwnedEntry::NeedsRewrite))
                | (Change::Disconnect, Some(_)) => {
                    self.edit(executables.current, server, change, path)?;
                    changed = true;
                }
                _ => {}
            }
        }
        Ok(changed.then(|| match self {
            Self::Json { root, .. } => {
                let mut text = root.to_string();
                if !text.ends_with('\n') {
                    text.push('\n');
                }
                text
            }
            Self::Toml { document, .. } => document.to_string(),
        }))
    }

    fn edit(
        &mut self,
        command: &str,
        server: Server,
        change: Change,
        path: &Path,
    ) -> Result<(), AgentError> {
        let invalid = || AgentError::Invalid(path.into());
        match self {
            Self::Json { root, kind } => {
                let servers = root.object_value_or_set().object_value_or_set(kind.key());
                match change {
                    Change::Connect => {
                        let canonical = ManagedServer::from((*kind, server, command)).into();
                        if let Some(entry) = servers.get(server.name()) {
                            entry.set_value(canonical);
                        } else {
                            servers.append(server.name(), canonical);
                        }
                    }
                    Change::Disconnect => {
                        if let Some(entry) = servers.get(server.name()) {
                            entry.remove();
                        }
                    }
                }
            }
            Self::Toml { document, .. } => {
                if !document.contains_key("mcp_servers") {
                    document["mcp_servers"] = Item::Table(Table::new());
                }
                let servers = document["mcp_servers"]
                    .as_table_like_mut()
                    .ok_or_else(invalid)?;
                match change {
                    Change::Connect => {
                        let mut entry = Table::new();
                        entry["command"] = value(command);
                        entry["args"] = value(
                            server
                                .arguments()
                                .iter()
                                .copied()
                                .collect::<toml_edit::Array>(),
                        );
                        servers.insert(server.name(), Item::Table(entry));
                    }
                    Change::Disconnect => {
                        servers.remove(server.name());
                    }
                }
            }
        }
        Ok(())
    }
}

impl Adapter {
    pub(super) fn registration(&self, binary: &str) -> Result<Registration, AgentError> {
        let snapshot = FileSnapshot::read(&self.config)?;
        let document = Document::parse(&snapshot, &self.format)?;
        let ledger = document.owned_entry(binary.into(), Server::Ledger, &self.config)?;
        let lifecycle = document.owned_entry(binary.into(), Server::Lifecycle, &self.config)?;
        match (ledger, lifecycle) {
            (Some(OwnedEntry::Disabled), _) => {
                Err(AgentError::ServerDisabled(Server::Ledger.name()))
            }
            (_, Some(OwnedEntry::Disabled)) => {
                Err(AgentError::ServerDisabled(Server::Lifecycle.name()))
            }
            (Some(_), Some(_)) => Ok(Registration::Owned),
            (None, None) => Ok(Registration::Missing),
            _ => Ok(Registration::Partial),
        }
    }

    pub(super) fn skill_available(&self) -> Result<bool, AgentError> {
        self.skill.as_ref().map_or(Ok(true), |path| {
            Ok(FileSnapshot::read(path)?
                .before
                .as_deref()
                .is_some_and(|text| text == SKILL || text.ends_with(SKILL_MARKER)))
        })
    }

    pub(super) fn stage_config(
        &self,
        executables: KnownExecutables<'_>,
        change: Change,
    ) -> Result<Option<FileChange>, AgentError> {
        let snapshot = FileSnapshot::read(&self.config)?;
        let document = Document::parse(&snapshot, &self.format)?;
        Ok(document
            .edited(executables, change, &self.config)?
            .map(|updated| FileChange {
                revised: Some(updated),
                original: snapshot,
            }))
    }

    pub(super) fn change(&self, binary: &str, change: Change) -> Result<(), AgentError> {
        let config = self.stage_config(binary.into(), change)?;
        let skill = self
            .skill
            .as_ref()
            .map(|path| skill_change(path, change))
            .transpose()?
            .flatten();
        let (first, second) = match change {
            Change::Connect => (skill.as_ref(), config.as_ref()),
            Change::Disconnect => (config.as_ref(), skill.as_ref()),
        };
        if let Some(first) = first {
            first.commit()?;
        }
        if let Some(second) = second
            && let Err(error) = second.commit()
        {
            if let Some(first) = first {
                first.restore()?;
            }
            return Err(error);
        }
        Ok(())
    }
}

fn skill_change(path: &Path, change: Change) -> Result<Option<FileChange>, AgentError> {
    let snapshot = FileSnapshot::read(path)?;
    let managed = snapshot
        .before
        .as_deref()
        .is_some_and(|text| text.ends_with(SKILL_MARKER));
    let desired = format!("{SKILL}{SKILL_MARKER}");
    let revised = match (change, snapshot.before.as_deref()) {
        (Change::Connect, None) => Some(desired),
        (Change::Connect, Some(text)) if text == SKILL || text == desired => return Ok(None),
        (Change::Connect, Some(_)) if managed => Some(desired),
        (Change::Connect, Some(_)) => return Err(AgentError::Conflict(path.into())),
        (Change::Disconnect, Some(_)) if managed => None,
        (Change::Disconnect, _) => return Ok(None),
    };
    Ok(Some(FileChange {
        original: snapshot,
        revised,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::adapters::{Agent, ClaudeProfile, Locations};

    fn locations(home: &Path) -> Locations {
        Locations {
            home: home.into(),
            gui: home.join("profile"),
            claude_desktop: ClaudeProfile::Selected(home.join("profile/Claude")),
            claude: home.join(".claude"),
            codex: home.join(".codex"),
            copilot: home.join(".copilot"),
            grok: home.join(".grok"),
        }
    }

    fn sample(format: &ConfigFormat) -> &'static str {
        match format {
            ConfigFormat::Toml(_) => {
                "# Personal settings\nmodel = \"personal\"\n[mcp_servers.example]\ncommand = \"example\"\n"
            }
            ConfigFormat::Json => {
                "{\n  \"personal\": true,\n  \"mcpServers\": { \"example\": {\"command\":\"example\"} }\n}\n"
            }
            ConfigFormat::Copilot => {
                "{\n  // Personal settings\n  \"mcpServers\": { \"example\": {\"command\":\"example\"}, },\n}\n"
            }
            ConfigFormat::VsCode => {
                "{\n  // Personal settings\n  \"servers\": { \"example\": {\"command\":\"example\"}, },\n}\n"
            }
        }
    }

    fn fixture(adapter: &Adapter, text: &str) -> io::Result<()> {
        let parent = adapter
            .config
            .parent()
            .ok_or_else(|| io::Error::other("missing config directory"))?;
        fs::create_dir_all(parent)?;
        fs::write(&adapter.config, text)
    }

    fn roundtrip(adapter: &Adapter, binary: &str) -> Result<(), Box<dyn std::error::Error>> {
        let original = sample(&adapter.format);
        fixture(adapter, original)?;
        assert!(matches!(
            adapter.registration(binary)?,
            Registration::Missing
        ));
        adapter.change(binary, Change::Connect)?;
        assert!(matches!(adapter.registration(binary)?, Registration::Owned));
        let installed = fs::read_to_string(&adapter.config)?;
        assert!(installed.contains("example"));
        assert!(installed.contains("beankeeper-service"));
        if original.contains("Personal settings") {
            assert!(installed.contains("Personal settings"));
        }
        let mut backup = adapter
            .config
            .file_name()
            .ok_or("missing name")?
            .to_os_string();
        backup.push(".beankeeper-backup");
        let backup = adapter.config.with_file_name(backup);
        assert_eq!(fs::read_to_string(&backup)?, original);
        if let Some(skill) = &adapter.skill {
            assert!(fs::read_to_string(skill)?.ends_with(SKILL_MARKER));
            assert!(adapter.skill_available()?);
        }
        adapter.change(binary, Change::Connect)?;
        assert_eq!(fs::read_to_string(&adapter.config)?, installed);
        assert_eq!(fs::read_to_string(&backup)?, original);
        adapter.change(binary, Change::Disconnect)?;
        assert!(matches!(
            adapter.registration(binary)?,
            Registration::Missing
        ));
        let removed = fs::read_to_string(&adapter.config)?;
        assert!(removed.contains("example"));
        assert!(!removed.contains("beankeeper-service"));
        if let Some(skill) = &adapter.skill {
            assert!(!skill.exists());
        }
        adapter.change(binary, Change::Disconnect)?;
        assert_eq!(fs::read_to_string(&adapter.config)?, removed);
        Ok(())
    }

    #[test]
    fn every_adapter_merges_backs_up_and_removes_only_managed_entries()
    -> Result<(), Box<dyn std::error::Error>> {
        let temp = tempfile::tempdir()?;
        let binary = temp.path().join("bin/bk");
        let binary = binary.to_str().ok_or("invalid binary path")?;
        let home = locations(temp.path());
        for agent in Agent::ALL {
            if let Some(adapter) = agent.adapter(&home)? {
                roundtrip(&adapter, binary)?;
            } else {
                assert!(matches!(agent, Agent::ChatGpt | Agent::Grok));
            }
        }
        let legacy_temp = tempfile::tempdir()?;
        let legacy = locations(legacy_temp.path());
        let profile_dir = legacy.gui.join("Code/User");
        fs::create_dir_all(&profile_dir)?;
        fs::write(profile_dir.join("mcp.json"), sample(&ConfigFormat::VsCode))?;
        let adapter = Agent::VsCode
            .adapter(&legacy)?
            .ok_or("missing profile adapter")?;
        assert!(matches!(adapter.format, ConfigFormat::VsCode));
        roundtrip(&adapter, binary)
    }

    #[test]
    fn claude_code_owns_only_equivalent_stdio_entries() -> Result<(), Box<dyn std::error::Error>> {
        let temp = tempfile::tempdir()?;
        let adapter = Agent::ClaudeCode
            .adapter(&locations(temp.path()))?
            .ok_or("missing adapter")?;
        let binary = "/bin/bk";
        let ledger = r#""command":"/bin/bk","args":["mcp","bridge","--service"]"#;
        let lifecycle = r#""command":"/bin/bk","args":["service","mcp"]"#;
        for fields in [
            "",
            r#","type":"stdio""#,
            r#","env":{}"#,
            r#","type":"stdio","env":{}"#,
        ] {
            let original = format!(
                r#"{{"mcpServers":{{"beankeeper":{{{ledger}{fields}}},"beankeeper-service":{{{lifecycle}{fields}}}}}}}"#
            );
            fixture(&adapter, &original)?;
            assert!(matches!(adapter.registration(binary)?, Registration::Owned));
            assert_eq!(fs::read_to_string(&adapter.config)?, original);
            adapter.change(binary, Change::Disconnect)?;
            assert!(matches!(
                adapter.registration(binary)?,
                Registration::Missing
            ));
        }

        for (entry, field) in [
            (
                format!(r#"{{{ledger},"env":{{"BEANKEEPER_DB":"custom"}}}}"#),
                "`env`",
            ),
            (format!(r#"{{{ledger},"env":null}}"#), "`env`"),
            (format!(r#"{{{ledger},"env":[]}}"#), "`env`"),
            (format!(r#"{{{ledger},"env":{{}},"env":{{}}}}"#), "`env`"),
            (format!(r#"{{{ledger},"type":"local"}}"#), "`type`"),
            (format!(r#"{{{ledger},"type":null}}"#), "`type`"),
            (format!(r#"{{{ledger},"cwd":"/work"}}"#), "\"cwd\""),
            (
                r#"{"command":"other","args":["mcp","bridge","--service"]}"#.into(),
                "`command`",
            ),
            (
                r#"{"command":"/bin/bk","args":["mcp","serve"]}"#.into(),
                "`args`",
            ),
        ] {
            let original = format!(
                r#"{{"mcpServers":{{"beankeeper-service":{{{lifecycle}}},"beankeeper":{entry}}}}}"#
            );
            fixture(&adapter, &original)?;
            for result in [
                adapter.registration(binary).map(drop),
                adapter.change(binary, Change::Connect),
                adapter.change(binary, Change::Disconnect),
            ] {
                let Err(error) = result else {
                    panic!("expected a registration conflict for {entry}");
                };
                assert_eq!(error.code(), "AGENT_CONFIG_CONFLICT");
                assert!(error.to_string().contains(field), "{error}");
            }
            assert_eq!(fs::read_to_string(&adapter.config)?, original);
            assert!(!adapter.skill_available()?);
        }
        Ok(())
    }

    #[test]
    fn copilot_and_vscode_still_require_explicit_type_and_no_env() -> Result<(), AgentError> {
        for (client, tools, transport) in [
            (JsonClient::Copilot, r#","tools":["*"]"#, "local"),
            (JsonClient::VsCode, "", "stdio"),
        ] {
            for (fields, expected) in [
                (String::new(), "`type`"),
                (format!(r#","type":"{transport}","env":{{}}"#), "`env`"),
            ] {
                let snapshot = FileSnapshot {
                    path: "mcp.json".into(),
                    before: Some(format!(
                        r#"{{"{}":{{"beankeeper":{{"command":"/bin/bk","args":["mcp","bridge","--service"]{tools}{fields}}}}}}}"#,
                        client.key()
                    )),
                };
                let format = match client {
                    JsonClient::Copilot => ConfigFormat::Copilot,
                    JsonClient::VsCode => ConfigFormat::VsCode,
                    JsonClient::Standard => ConfigFormat::Json,
                };
                let document = Document::parse(&snapshot, &format)?;
                let result = document.owned_entry("/bin/bk".into(), Server::Ledger, &snapshot.path);
                assert!(matches!(result, Err(error) if error.to_string().contains(expected)));
            }
        }
        Ok(())
    }

    #[test]
    fn incompatible_registrations_and_skills_leave_original_bytes()
    -> Result<(), Box<dyn std::error::Error>> {
        let temp = tempfile::tempdir()?;
        let home = locations(temp.path());
        let adapter = Agent::Gemini.adapter(&home)?.ok_or("missing adapter")?;
        let binary = temp.path().join("bin/bk");
        let binary = binary.to_str().ok_or("invalid binary path")?;
        for original in [
            r#"{"mcpServers":{"beankeeper":{"command":"different","args":[]}}}"#,
            r#"{"mcpServers":{"beankeeper":{},"beankeeper":{}}}"#,
            r#"{"mcpServers":false}"#,
        ] {
            fixture(&adapter, original)?;
            assert!(adapter.change(binary, Change::Connect).is_err());
            assert!(adapter.change(binary, Change::Disconnect).is_err());
            assert_eq!(fs::read_to_string(&adapter.config)?, original);
        }
        let original = sample(&adapter.format);
        fixture(&adapter, original)?;
        let skill = adapter.skill.as_ref().ok_or("missing skill path")?;
        fs::create_dir_all(skill.parent().ok_or("missing skill directory")?)?;
        fs::write(skill, "Owner authored instructions")?;
        assert!(adapter.change(binary, Change::Connect).is_err());
        assert_eq!(fs::read_to_string(&adapter.config)?, original);
        assert_eq!(fs::read_to_string(skill)?, "Owner authored instructions");
        Ok(())
    }
}
