//! Supported user-scope configuration destinations and client discovery.

use std::{
    env,
    path::{Path, PathBuf},
};

use clap::ValueEnum;
use serde::{Deserialize, Serialize};

use super::{AgentError, config::read_config_text};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub enum Agent {
    ClaudeDesktop,
    ClaudeCode,
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

    pub fn extension_installed(&self) -> Result<bool, AgentError> {
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
            return Ok(false);
        };
        let Some(text) = read_config_text(path)? else {
            return Ok(false);
        };
        let registry: Installations =
            serde_json::from_str(&text).map_err(|_| AgentError::Invalid(path.clone()))?;
        Ok(registry.extensions.into_iter().any(|(key, value)| {
            let Ok(extension) = serde_json::from_value::<Extension>(value) else {
                return false;
            };
            let config = extension.manifest.server.mcp_config;
            key == extension.id
                && extension.manifest.name == "beankeeper"
                && matches!(
                    config.command.as_str(),
                    "${__dirname}/server/bk" | "${__dirname}/server/bk.exe"
                )
                && config.args == ["mcp", "extension"]
        }))
    }
}

impl Agent {
    pub(super) const ALL: [Self; 8] = [
        Self::ClaudeDesktop,
        Self::ClaudeCode,
        Self::Codex,
        Self::Cursor,
        Self::Gemini,
        Self::VsCode,
        Self::ChatGpt,
        Self::Grok,
    ];

    pub(super) fn label(self) -> &'static str {
        match self {
            Self::ClaudeDesktop => "Claude Desktop",
            Self::ClaudeCode => "Claude Code",
            Self::Codex => "Codex CLI",
            Self::Cursor => "Cursor",
            Self::Gemini => "Gemini CLI",
            Self::VsCode => "VS Code",
            Self::ChatGpt => "ChatGPT",
            Self::Grok => "Grok",
        }
    }

    pub(super) fn adapter(self, paths: &Locations) -> Option<Adapter> {
        let skill = |base: &Path| base.join("skills/beankeeper-desktop/SKILL.md");
        let (config, format, skill, app_dir, program, extension_registry) = match self {
            Self::ClaudeDesktop => {
                let app_dir = paths.gui.join("Claude");
                (
                    app_dir.join("claude_desktop_config.json"),
                    ConfigFormat::Json,
                    None,
                    app_dir.clone(),
                    Some("claude-desktop"),
                    Some(app_dir.join("extensions-installations.json")),
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
            Self::ChatGpt | Self::Grok => return None,
        };
        Some(Adapter {
            config,
            format,
            skill,
            app_dir,
            program,
            extension_registry,
        })
    }
}

pub(super) struct Locations {
    pub home: PathBuf,
    pub gui: PathBuf,
    pub claude: PathBuf,
    pub codex: PathBuf,
    pub copilot: PathBuf,
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
        let claude = path_from_env("CLAUDE_CONFIG_DIR").unwrap_or_else(|| home.join(".claude"));
        let codex = path_from_env("CODEX_HOME").unwrap_or_else(|| home.join(".codex"));
        let copilot = path_from_env("COPILOT_HOME").unwrap_or_else(|| home.join(".copilot"));
        Some(Self {
            home,
            gui,
            claude,
            codex,
            copilot,
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

    #[test]
    fn claude_detects_the_managed_extension_without_requiring_a_legacy_config()
    -> Result<(), Box<dyn std::error::Error>> {
        let temp = tempfile::tempdir()?;
        let home = temp.path();
        let locations = Locations {
            home: home.into(),
            gui: home.join("profile"),
            claude: home.join(".claude"),
            codex: home.join(".codex"),
            copilot: home.join(".copilot"),
        };
        let adapter = Agent::ClaudeDesktop
            .adapter(&locations)
            .ok_or("missing adapter")?;
        let registry = adapter
            .extension_registry
            .as_ref()
            .ok_or("missing registry path")?;
        std::fs::create_dir_all(&adapter.app_dir)?;
        let mut extension = serde_json::json!({"extensions": {
            "another-extension": {"id":"another-extension", "manifest":{"name":"Sample"}},
            "local-extension": {
            "id":"local-extension", "manifest": {"name": "beankeeper", "server": {
                "mcp_config": {"command":"${__dirname}/server/bk", "args":["mcp","extension"]}
            }}
        }}});
        std::fs::write(registry, serde_json::to_vec(&extension)?)?;
        assert!(adapter.installed());
        assert!(adapter.extension_installed()?);
        assert!(!adapter.config.exists());
        extension["extensions"]["local-extension"]["manifest"]["server"]["mcp_config"]["args"] =
            serde_json::json!(["service", "mcp"]);
        std::fs::write(registry, serde_json::to_vec(&extension)?)?;
        assert!(!adapter.extension_installed()?);
        Ok(())
    }
}
