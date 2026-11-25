//! Supported user-scope configuration destinations and client discovery.

use std::{
    env, fs,
    path::{Path, PathBuf},
};

use clap::ValueEnum;
use serde::{Deserialize, Serialize};

use super::{AgentError, config::read_config_text};

const CLAUDE_CONFIG: &str = "claude_desktop_config.json";
const CLAUDE_REGISTRY: &str = "extensions-installations.json";
const CLAUDE_PACKAGE_FAMILY: &str = "Claude_pzs8sxrjxfjjc";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub enum Agent {
    ClaudeDesktop,
    ClaudeCode,
    #[value(alias = "chatgpt-codex")]
    Codex,
    Cursor,
    Gemini,
    #[value(name = "vscode")]
    #[serde(rename = "vscode")]
    VsCode,
    #[value(name = "chatgpt", alias = "chat-gpt")]
    #[serde(rename = "chatgpt")]
    ChatGpt,
    Grok,
    GrokBuild,
}

pub(super) enum ConfigFormat {
    Json,
    Copilot,
    VsCode,
    Toml,
}

pub(super) struct Adapter {
    pub config: PathBuf,
    pub format: ConfigFormat,
    pub skill: Option<PathBuf>,
    pub app_dir: PathBuf,
    pub program: Option<&'static str>,
    pub extension_registry: Option<PathBuf>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ExtensionRegistration {
    Enabled,
    Disabled,
    Blocked,
}

impl Adapter {
    pub fn installed(&self) -> bool {
        self.app_dir.is_dir()
            || self.config.is_file()
            || self.program.is_some_and(on_path)
            || self
                .extension_registry
                .as_ref()
                .is_some_and(|file| file.is_file())
    }

    pub fn extension_registration(&self) -> Result<Option<ExtensionRegistration>, AgentError> {
        #[derive(Deserialize)]
        struct Installations {
            extensions: std::collections::BTreeMap<String, serde_json::Value>,
        }
        #[derive(Deserialize)]
        struct Extension {
            id: String,
            manifest: Manifest,
        }
        #[derive(Deserialize)]
        struct Manifest {
            name: String,
            server: Server,
        }
        #[derive(Deserialize)]
        struct Server {
            mcp_config: McpConfig,
        }
        #[derive(Deserialize)]
        struct McpConfig {
            command: String,
            args: Vec<String>,
        }
        let Some(path) = &self.extension_registry else {
            return Ok(None);
        };
        let Some(text) = read_config_text(path)? else {
            return Ok(None);
        };
        let registry: Installations =
            serde_json::from_str(&text).map_err(|_| AgentError::Invalid(path.clone()))?;
        let mut registration = None;
        for (key, value) in registry.extensions {
            let Ok(extension) = serde_json::from_value::<Extension>(value) else {
                continue;
            };
            let config = extension.manifest.server.mcp_config;
            if key != extension.id
                || extension.manifest.name != "beankeeper"
                || !matches!(
                    config.command.as_str(),
                    "${__dirname}/server/bk" | "${__dirname}/server/bk.exe"
                )
                || config.args != ["mcp", "extension"]
            {
                continue;
            }
            let id = ExtensionId::parse(&key).ok_or_else(|| AgentError::Invalid(path.clone()))?;
            let current = extension_settings(&self.app_dir, id);
            if current == ExtensionRegistration::Enabled {
                return Ok(Some(current));
            }
            if registration.is_none() || current == ExtensionRegistration::Blocked {
                registration = Some(current);
            }
        }
        Ok(registration)
    }
}

#[derive(Clone, Copy)]
struct ExtensionId<'a>(&'a str);

impl<'a> ExtensionId<'a> {
    fn parse(id: &'a str) -> Option<Self> {
        // Keep the native ID plus the .json suffix in one path component, and
        // reject Windows device names even when followed by an extension.
        if id.len() > 250
            || matches!(id, "" | "." | "..")
            || !id.bytes().all(|byte| {
                byte.is_ascii_lowercase()
                    || byte.is_ascii_digit()
                    || matches!(byte, b'-' | b'_' | b'.')
            })
        {
            return None;
        }
        let stem = id.split('.').next()?;
        let reserved = matches!(stem, "con" | "prn" | "aux" | "nul")
            || matches!(
                stem.as_bytes(),
                [b'c', b'o', b'm', b'0'..=b'9'] | [b'l', b'p', b't', b'0'..=b'9']
            );
        (!reserved).then_some(Self(id))
    }
}

fn extension_settings(profile: &Path, id: ExtensionId<'_>) -> ExtensionRegistration {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Settings {
        #[serde(default)]
        is_enabled: bool,
        org_blocked_reason: Option<String>,
    }

    let dir = profile.join("Claude Extensions Settings");
    if !fs::symlink_metadata(&dir).is_ok_and(|metadata| metadata.is_dir()) {
        return ExtensionRegistration::Disabled;
    }
    let path = dir.join(format!("{}.json", id.0));
    let settings = read_config_text(&path)
        .ok()
        .flatten()
        .and_then(|text| serde_json::from_str::<Settings>(&text).ok());
    match settings {
        Some(settings)
            if settings
                .org_blocked_reason
                .as_deref()
                .is_some_and(|reason| !reason.is_empty()) =>
        {
            ExtensionRegistration::Blocked
        }
        Some(settings) if settings.is_enabled => ExtensionRegistration::Enabled,
        _ => ExtensionRegistration::Disabled,
    }
}

impl Agent {
    pub(super) const ALL: [Self; 9] = [
        Self::ClaudeDesktop,
        Self::ClaudeCode,
        Self::Codex,
        Self::Cursor,
        Self::Gemini,
        Self::VsCode,
        Self::ChatGpt,
        Self::Grok,
        Self::GrokBuild,
    ];

    pub(super) fn label(self) -> &'static str {
        match self {
            Self::ClaudeDesktop => "Claude Desktop",
            Self::ClaudeCode => "Claude Code",
            Self::Codex => "Codex (including ChatGPT desktop)",
            Self::Cursor => "Cursor",
            Self::Gemini => "Gemini CLI",
            Self::VsCode => "VS Code",
            Self::ChatGpt => "ChatGPT",
            Self::Grok => "Grok (hosted)",
            Self::GrokBuild => "Grok Build",
        }
    }

    pub(super) fn unsupported_message(self) -> &'static str {
        match self {
            Self::ChatGpt => {
                "Regular ChatGPT chat has no Beankeeper connection. In the ChatGPT desktop app, switch to Codex and ask it to install Beankeeper from https://beankeeper.app/install.md. To register an existing install, run bk agents connect chatgpt-codex."
            }
            _ => {
                "No supported local MCP configuration is available. Use an agent that can connect to the local Beankeeper service."
            }
        }
    }

    pub(super) fn adapter(self, paths: &Locations) -> Result<Option<Adapter>, AgentError> {
        let skill = |base: &Path| base.join("skills/beankeeper-desktop/SKILL.md");
        let (config, format, skill, app_dir, program, extension_registry) = match self {
            Self::ClaudeDesktop => {
                let ClaudeProfile::Selected(app_dir) = &paths.claude_desktop else {
                    return Err(AgentError::ClaudeProfileAmbiguous);
                };
                (
                    app_dir.join(CLAUDE_CONFIG),
                    ConfigFormat::Json,
                    None,
                    app_dir.clone(),
                    Some("claude-desktop"),
                    Some(app_dir.join(CLAUDE_REGISTRY)),
                )
            }
            Self::ClaudeCode => (
                paths.home.join(".claude.json"),
                ConfigFormat::Json,
                Some(skill(&paths.claude)),
                paths.claude.clone(),
                Some("claude"),
                None,
            ),
            Self::Codex => (
                paths.codex.join("config.toml"),
                ConfigFormat::Toml,
                Some(skill(&paths.home.join(".agents"))),
                paths.codex.clone(),
                Some("codex"),
                None,
            ),
            Self::Cursor => {
                let base = paths.home.join(".cursor");
                (
                    base.join("mcp.json"),
                    ConfigFormat::Json,
                    Some(skill(&base)),
                    base,
                    Some("cursor"),
                    None,
                )
            }
            Self::Gemini => {
                let base = paths.home.join(".gemini");
                (
                    base.join("settings.json"),
                    ConfigFormat::Json,
                    Some(skill(&base)),
                    base,
                    Some("gemini"),
                    None,
                )
            }
            Self::VsCode => {
                let app = paths.gui.join("Code");
                let native = paths.copilot.join("mcp-config.json");
                let profile = app.join("User/mcp.json");
                let (config, format) = if !native.exists() && profile.is_file() {
                    (profile, ConfigFormat::VsCode)
                } else {
                    (native, ConfigFormat::Copilot)
                };
                (
                    config,
                    format,
                    Some(skill(&paths.copilot)),
                    app,
                    Some("code"),
                    None,
                )
            }
            Self::GrokBuild => (
                paths.grok.join("config.toml"),
                ConfigFormat::Toml,
                Some(skill(&paths.grok)),
                paths.grok.clone(),
                Some("grok"),
                None,
            ),
            Self::ChatGpt | Self::Grok => return Ok(None),
        };
        Ok(Some(Adapter {
            config,
            format,
            skill,
            app_dir,
            program,
            extension_registry,
        }))
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum ClaudeProfile {
    Selected(PathBuf),
    Ambiguous,
}

impl ClaudeProfile {
    fn select(
        gui: &Path,
        windows_local: Option<&Path>,
        explicit: Option<PathBuf>,
        is_file: impl Fn(&Path) -> bool,
    ) -> Self {
        if let Some(path) = explicit {
            return Self::Selected(path);
        }
        let roaming = gui.join("Claude");
        let Some(local) = windows_local else {
            return Self::Selected(roaming);
        };
        let relocated = local
            .join("Claude-Data")
            .join(CLAUDE_PACKAGE_FAMILY.to_ascii_lowercase());
        let redirected = local
            .join("Packages")
            .join(CLAUDE_PACKAGE_FAMILY)
            .join("LocalCache/Roaming/Claude");
        let mut profiles = [relocated, redirected, roaming.clone()]
            .into_iter()
            .filter(|profile| {
                [CLAUDE_REGISTRY, CLAUDE_CONFIG]
                    .into_iter()
                    .any(|name| is_file(&profile.join(name)))
            });
        let Some(first) = profiles.next() else {
            return Self::Selected(roaming);
        };
        if profiles.any(|profile| profile != first) {
            Self::Ambiguous
        } else {
            Self::Selected(first)
        }
    }
}

pub(super) struct Locations {
    pub home: PathBuf,
    pub gui: PathBuf,
    pub claude_desktop: ClaudeProfile,
    pub claude: PathBuf,
    pub codex: PathBuf,
    pub copilot: PathBuf,
    pub grok: PathBuf,
}

impl Locations {
    pub fn discover() -> Option<Self> {
        let home = crate::setup::user_home()?;
        let gui = if cfg!(target_os = "macos") {
            home.join("Library/Application Support")
        } else if cfg!(windows) {
            path_from_env("APPDATA").unwrap_or_else(|| home.join("AppData/Roaming"))
        } else {
            path_from_env("XDG_CONFIG_HOME").unwrap_or_else(|| home.join(".config"))
        };
        let windows_local = cfg!(windows)
            .then(|| path_from_env("LOCALAPPDATA").unwrap_or_else(|| home.join("AppData/Local")));
        let claude_desktop = ClaudeProfile::select(
            &gui,
            windows_local.as_deref(),
            path_from_env("CLAUDE_USER_DATA_DIR"),
            Path::is_file,
        );
        let claude = path_from_env("CLAUDE_CONFIG_DIR").unwrap_or_else(|| home.join(".claude"));
        let codex = path_from_env("CODEX_HOME").unwrap_or_else(|| home.join(".codex"));
        let copilot = path_from_env("COPILOT_HOME").unwrap_or_else(|| home.join(".copilot"));
        let grok = path_from_env("GROK_HOME").unwrap_or_else(|| home.join(".grok"));
        Some(Self {
            home,
            gui,
            claude_desktop,
            claude,
            codex,
            copilot,
            grok,
        })
    }
}

fn path_from_env(name: &str) -> Option<PathBuf> {
    env::var_os(name)
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
}

fn on_path(command: &str) -> bool {
    env::var_os("PATH").is_some_and(|path| {
        env::split_paths(&path).any(|dir| {
            let candidate = dir.join(command);
            candidate.is_file()
                || cfg!(windows)
                    && ["exe", "cmd", "bat"]
                        .into_iter()
                        .any(|ext| candidate.with_extension(ext).is_file())
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::{Operation, State, check_agent};

    const EXTENSION_ID: &str = "local.mcpb.govcraft.beankeeper";
    const LAUNCHER: &str = "${__dirname}/server/bk";

    fn locations(home: &Path, claude_desktop: ClaudeProfile) -> Locations {
        Locations {
            home: home.into(),
            gui: home.join("profile"),
            claude_desktop,
            claude: home.join(".claude"),
            codex: home.join(".codex"),
            copilot: home.join(".copilot"),
            grok: home.join(".grok"),
        }
    }

    fn write_registry(profile: &Path, id: &str, command: &str) -> std::io::Result<()> {
        fs::create_dir_all(profile)?;
        let registry = serde_json::json!({"extensions": {
            "another-extension": {"id":"another-extension", "manifest":{"name":"Sample"}},
            id: {
                "id":id, "manifest": {"name": "beankeeper", "server": {
                    "mcp_config": {"command":command, "args":["mcp","extension"]}
                }}
            }
        }});
        fs::write(profile.join(CLAUDE_REGISTRY), registry.to_string())
    }

    #[test]
    fn windows_uses_one_recognizable_profile_and_reports_ambiguity() {
        let gui = Path::new("roaming");
        let local = Path::new("local");
        let roaming = gui.join("Claude");
        let relocated = local.join("Claude-Data/claude_pzs8sxrjxfjjc");
        let redirected = local.join("Packages/Claude_pzs8sxrjxfjjc/LocalCache/Roaming/Claude");
        let select = |files: &[PathBuf]| {
            ClaudeProfile::select(gui, Some(local), None, |path| {
                files.iter().any(|candidate| candidate == path)
            })
        };
        assert_eq!(select(&[]), ClaudeProfile::Selected(roaming.clone()));
        assert_eq!(
            select(&[roaming.join(CLAUDE_CONFIG)]),
            ClaudeProfile::Selected(roaming.clone())
        );
        assert_eq!(
            select(&[
                relocated.join(CLAUDE_REGISTRY),
                relocated.join(CLAUDE_CONFIG)
            ]),
            ClaudeProfile::Selected(relocated.clone())
        );
        assert_eq!(
            select(&[redirected.join(CLAUDE_REGISTRY)]),
            ClaudeProfile::Selected(redirected.clone())
        );
        assert_eq!(
            select(&[relocated.join(CLAUDE_REGISTRY), roaming.join(CLAUDE_CONFIG)]),
            ClaudeProfile::Ambiguous
        );
        assert_eq!(
            select(&[
                relocated.join(CLAUDE_REGISTRY),
                redirected.join(CLAUDE_REGISTRY)
            ]),
            ClaudeProfile::Ambiguous
        );
    }

    #[test]
    fn explicit_profile_sets_config_and_registry_without_changing_other_agents()
    -> Result<(), Box<dyn std::error::Error>> {
        let temp = tempfile::tempdir()?;
        let explicit = temp.path().join("override");
        for windows_local in [None, Some(temp.path())] {
            let profile =
                ClaudeProfile::select(temp.path(), windows_local, Some(explicit.clone()), |_| true);
            let locations = locations(temp.path(), profile);
            let adapter = Agent::ClaudeDesktop
                .adapter(&locations)?
                .ok_or("missing adapter")?;
            assert_eq!(adapter.config, explicit.join(CLAUDE_CONFIG));
            assert_eq!(
                adapter.extension_registry,
                Some(explicit.join(CLAUDE_REGISTRY))
            );
            let vscode = Agent::VsCode
                .adapter(&locations)?
                .ok_or("missing adapter")?;
            assert_eq!(vscode.app_dir, locations.gui.join("Code"));
        }
        let ambiguous = locations(temp.path(), ClaudeProfile::Ambiguous);
        let status = check_agent(
            Agent::ClaudeDesktop,
            &ambiguous,
            "unused",
            Operation::Connect,
            true,
        );
        assert_eq!(
            status.error.ok_or("missing profile error")?.code,
            "AGENT_CLAUDE_PROFILE_AMBIGUOUS"
        );
        Ok(())
    }

    #[test]
    fn claude_registration_uses_settings_and_preserves_managed_connections()
    -> Result<(), Box<dyn std::error::Error>> {
        let temp = tempfile::tempdir()?;
        let profile = temp.path().join("claude-profile");
        let locations = locations(temp.path(), ClaudeProfile::Selected(profile.clone()));
        let adapter = Agent::ClaudeDesktop
            .adapter(&locations)?
            .ok_or("missing adapter")?;
        write_registry(&profile, EXTENSION_ID, LAUNCHER)?;
        assert!(adapter.installed());
        assert_eq!(
            adapter.extension_registration()?,
            Some(ExtensionRegistration::Disabled)
        );
        let settings = profile.join("Claude Extensions Settings");
        fs::create_dir_all(&settings)?;
        let settings = settings.join(format!("{EXTENSION_ID}.json"));
        for (text, expected) in [
            ("{}", ExtensionRegistration::Disabled),
            ("{", ExtensionRegistration::Disabled),
            (r#"{"isEnabled":false}"#, ExtensionRegistration::Disabled),
            (r#"{"isEnabled":"true"}"#, ExtensionRegistration::Disabled),
            (
                r#"{"isEnabled":true,"orgBlockedReason":"policy"}"#,
                ExtensionRegistration::Blocked,
            ),
            (r#"{"isEnabled":true}"#, ExtensionRegistration::Enabled),
            (
                r#"{"isEnabled":true,"orgBlockedReason":""}"#,
                ExtensionRegistration::Enabled,
            ),
        ] {
            fs::write(&settings, text)?;
            assert_eq!(adapter.extension_registration()?, Some(expected));
        }

        let check =
            |operation| check_agent(Agent::ClaudeDesktop, &locations, "unused", operation, true);
        assert!(matches!(
            check(Operation::Doctor).state,
            State::ExtensionManaged
        ));
        fs::write(&settings, r#"{"isEnabled":false}"#)?;
        let detected = check(Operation::Detect);
        assert!(matches!(detected.state, State::ExtensionDisabled));
        assert!(detected.error.is_none());
        assert_eq!(
            check(Operation::Doctor)
                .error
                .ok_or("missing doctor error")?
                .code,
            "AGENT_EXTENSION_DISABLED"
        );
        assert!(matches!(
            check(Operation::Connect).state,
            State::ExtensionDisabled
        ));
        assert_eq!(
            check(Operation::Disconnect)
                .error
                .ok_or("missing disconnect error")?
                .code,
            "AGENT_EXTENSION_MANAGED"
        );
        assert!(!adapter.config.exists());
        fs::write(
            &settings,
            r#"{"isEnabled":true,"orgBlockedReason":"policy"}"#,
        )?;
        let blocked = check(Operation::Doctor);
        assert!(matches!(blocked.state, State::ExtensionBlocked));
        assert_eq!(
            blocked.error.ok_or("missing doctor error")?.code,
            "AGENT_EXTENSION_BLOCKED"
        );

        write_registry(&profile, EXTENSION_ID, "different-server")?;
        assert_eq!(adapter.extension_registration()?, None);
        Ok(())
    }

    #[test]
    fn matching_registration_rejects_an_unsafe_settings_id()
    -> Result<(), Box<dyn std::error::Error>> {
        let temp = tempfile::tempdir()?;
        let profile = temp.path().join("claude-profile");
        let locations = locations(temp.path(), ClaudeProfile::Selected(profile.clone()));
        let adapter = Agent::ClaudeDesktop
            .adapter(&locations)?
            .ok_or("missing adapter")?;
        for id in [
            "../other-profile",
            r"..\other-profile",
            "c:account",
            "nul.device",
            "com1",
            "..",
        ] {
            write_registry(&profile, id, LAUNCHER)?;
            assert!(matches!(
                adapter.extension_registration(),
                Err(AgentError::Invalid(_))
            ));
        }
        let status = check_agent(
            Agent::ClaudeDesktop,
            &locations,
            "unused",
            Operation::Connect,
            true,
        );
        assert_eq!(
            status.error.ok_or("missing config error")?.code,
            "AGENT_INVALID_CONFIG"
        );
        assert!(!adapter.config.exists());
        Ok(())
    }
}
