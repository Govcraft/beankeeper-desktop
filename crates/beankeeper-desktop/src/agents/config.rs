//! Edit exact Beankeeper registrations, preserving unrelated client configuration.

use std::{
    fs::{self, File},
    io::{self, Read, Write},
    path::{Path, PathBuf},
};

use jsonc_parser::{
    ParseOptions,
    cst::{CstInputValue, CstObject, CstObjectProp, CstRootNode},
};
use serde::Deserialize;
use toml_edit::{DocumentMut, Item, Table, value};

use super::{
    AgentError,
    adapters::{Adapter, ConfigFormat},
};

const MAX_CONFIG_BYTES: u64 = 4 * 1024 * 1024;
const SKILL: &str = include_str!("../../../../SKILL.md");
const SKILL_MARKER: &str = "\n<!-- Managed by Beankeeper Desktop agent registration. -->\n";

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Registration {
    Missing,
    Partial,
    Owned,
    Conflict,
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

struct FileChange {
    original: FileSnapshot,
    revised: Option<String>,
}

impl FileChange {
    fn commit(&self) -> Result<(), AgentError> {
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
    Json { root: CstRootNode, kind: JsonClient },
    Toml(DocumentMut),
}

#[derive(Clone, Copy)]
enum JsonClient {
    Standard,
    Copilot,
    VsCode,
}

impl JsonClient {
    const fn key(self) -> &'static str {
        match self {
            Self::Standard | Self::Copilot => "mcpServers",
            Self::VsCode => "servers",
        }
    }

    fn entry(self, server: Server, command: &str) -> ConfiguredServer {
        let (transport, tools) = match self {
            Self::Standard => (None, None),
            Self::Copilot => (Some(TransportKind::Local), Some(vec!["*".into()])),
            Self::VsCode => (Some(TransportKind::Stdio), None),
        };
        ConfiguredServer {
            command: command.into(),
            args: server.arguments().iter().map(|arg| (*arg).into()).collect(),
            transport,
            tools,
        }
    }
}

#[derive(Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct ConfiguredServer {
    command: String,
    args: Vec<String>,
    #[serde(rename = "type", default, deserialize_with = "present")]
    transport: Option<TransportKind>,
    #[serde(default, deserialize_with = "present")]
    tools: Option<Vec<String>>,
}

fn present<'de, D: serde::Deserializer<'de>, T: Deserialize<'de>>(
    deserializer: D,
) -> Result<Option<T>, D::Error> {
    T::deserialize(deserializer).map(Some)
}

#[derive(Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum TransportKind {
    Local,
    Stdio,
}

impl From<ConfiguredServer> for CstInputValue {
    fn from(spec: ConfiguredServer) -> Self {
        let mut fields = vec![
            ("command".into(), spec.command.into()),
            ("args".into(), spec.args.into()),
        ];
        if let Some(transport) = spec.transport {
            let name = match transport {
                TransportKind::Local => "local",
                TransportKind::Stdio => "stdio",
            };
            fields.push(("type".into(), name.into()));
        }
        if let Some(tools) = spec.tools {
            fields.push(("tools".into(), tools.into()));
        }
        fields.into()
    }
}

impl JsonClient {
    fn options(self) -> ParseOptions {
        let comments = !matches!(self, Self::Standard);
        ParseOptions {
            allow_comments: comments,
            allow_trailing_commas: comments,
            allow_loose_object_property_names: false,
            allow_missing_commas: false,
            allow_single_quoted_strings: false,
            allow_hexadecimal_numbers: false,
            allow_unary_plus_numbers: false,
            allow_bare_decimal_point_numbers: false,
            allow_non_finite_numbers: false,
            allow_extended_string_escapes: false,
        }
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

impl Document {
    fn parse(file: &FileSnapshot, format: &ConfigFormat) -> Result<Self, AgentError> {
        let invalid = || AgentError::Invalid(file.path.clone());
        let kind = match format {
            ConfigFormat::Toml => {
                return file
                    .contents()
                    .parse()
                    .map(Self::Toml)
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

    fn state(
        &self,
        command: &str,
        server: Server,
        path: &Path,
    ) -> Result<Registration, AgentError> {
        let invalid = || AgentError::Invalid(path.into());
        let owned = match self {
            Self::Json { root, kind } => {
                let Some(object) = root.object_value() else {
                    return Ok(Registration::Missing);
                };
                let Some(servers) = property(&object, kind.key(), path)? else {
                    return Ok(Registration::Missing);
                };
                let servers = servers.object_value().ok_or_else(invalid)?;
                property(&servers, server.name(), path)?.map(|entry| {
                    entry.value().is_some_and(|node| {
                        jsonc_parser::parse_to_serde_value::<ConfiguredServer>(
                            &node.to_string(),
                            &kind.options(),
                        )
                        .is_ok_and(|actual| actual == kind.entry(server, command))
                    })
                })
            }
            Self::Toml(toml) => {
                let Some(servers) = toml.get("mcp_servers") else {
                    return Ok(Registration::Missing);
                };
                let servers = servers.as_table_like().ok_or_else(invalid)?;
                servers.get(server.name()).map(|entry| {
                    entry.as_table_like().is_some_and(|fields| {
                        fields.len() == 2
                            && fields.get("command").and_then(Item::as_str) == Some(command)
                            && fields
                                .get("args")
                                .and_then(Item::as_array)
                                .is_some_and(|items| {
                                    items.len() == server.arguments().len()
                                        && items.iter().zip(server.arguments()).all(
                                            |(item, argument)| item.as_str() == Some(*argument),
                                        )
                                })
                    })
                })
            }
        };
        Ok(match owned {
            None => Registration::Missing,
            Some(true) => Registration::Owned,
            Some(false) => Registration::Conflict,
        })
    }

    fn edited(
        mut self,
        command: &str,
        change: Change,
        path: &Path,
    ) -> Result<Option<String>, AgentError> {
        let mut changed = false;
        for server in Server::ALL {
            match (change, self.state(command, server, path)?) {
                (_, Registration::Conflict) => return Err(AgentError::Conflict(path.into())),
                (Change::Connect, Registration::Missing)
                | (Change::Disconnect, Registration::Owned) => {
                    self.edit(command, server, change, path)?;
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
            Self::Toml(toml) => toml.to_string(),
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
                        servers.append(server.name(), kind.entry(server, command).into());
                    }
                    Change::Disconnect => {
                        if let Some(entry) = servers.get(server.name()) {
                            entry.remove();
                        }
                    }
                }
            }
            Self::Toml(toml) => {
                if !toml.contains_key("mcp_servers") {
                    toml["mcp_servers"] = Item::Table(Table::new());
                }
                let servers = toml["mcp_servers"]
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
        let ledger = document.state(binary, Server::Ledger, &self.config)?;
        let lifecycle = document.state(binary, Server::Lifecycle, &self.config)?;
        Ok(match (ledger, lifecycle) {
            (Registration::Conflict, _) | (_, Registration::Conflict) => Registration::Conflict,
            (Registration::Owned, Registration::Owned) => Registration::Owned,
            (Registration::Missing, Registration::Missing) => Registration::Missing,
            _ => Registration::Partial,
        })
    }

    pub(super) fn skill_available(&self) -> Result<bool, AgentError> {
        self.skill.as_ref().map_or(Ok(true), |path| {
            Ok(FileSnapshot::read(path)?
                .before
                .as_deref()
                .is_some_and(|text| text == SKILL || text.ends_with(SKILL_MARKER)))
        })
    }

    pub(super) fn change(&self, binary: &str, change: Change) -> Result<(), AgentError> {
        let snapshot = FileSnapshot::read(&self.config)?;
        let document = Document::parse(&snapshot, &self.format)?;
        let config = document
            .edited(binary, change, &self.config)?
            .map(|updated| FileChange {
                revised: Some(updated),
                original: snapshot,
            });
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
    use crate::agents::adapters::{Agent, Locations};

    fn locations(home: &Path) -> Locations {
        Locations {
            home: home.into(),
            gui: home.join("profile"),
            claude: home.join(".claude"),
            codex: home.join(".codex"),
            copilot: home.join(".copilot"),
            grok: home.join(".grok"),
        }
    }

    fn sample(format: &ConfigFormat) -> &'static str {
        match format {
            ConfigFormat::Toml => {
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
            if let Some(adapter) = agent.adapter(&home) {
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
            .adapter(&legacy)
            .ok_or("missing profile adapter")?;
        assert!(matches!(adapter.format, ConfigFormat::VsCode));
        roundtrip(&adapter, binary)
    }

    #[test]
    fn incompatible_registrations_and_skills_leave_original_bytes()
    -> Result<(), Box<dyn std::error::Error>> {
        let temp = tempfile::tempdir()?;
        let home = locations(temp.path());
        let adapter = Agent::Gemini.adapter(&home).ok_or("missing adapter")?;
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
