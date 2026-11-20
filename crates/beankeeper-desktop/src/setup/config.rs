//! Merge Desktop's stdio bridge into each supported MCP client's configuration.

use std::{
    error::Error,
    fmt, fs,
    io::{self, Write},
    path::{Path, PathBuf},
};

use serde_json::{Value, json};
use toml_edit::{DocumentMut, Item, Table, value};

/// A config file could not be read, merged, or privately persisted.
#[derive(Debug)]
pub enum ConfigError {
    /// Filesystem failure; the path identifies the affected client config.
    Io(PathBuf, io::Error),
    /// Invalid existing config is left untouched.
    Invalid(PathBuf),
    /// An existing Beankeeper entry belongs to a different setup.
    Conflict(PathBuf),
    /// Client config paths must be valid Unicode.
    NonUnicodePath(PathBuf),
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(path, error) => {
                write!(f, "cannot update MCP config {}: {error}", path.display())
            }
            Self::Invalid(path) => write!(
                f,
                "invalid MCP config {}; repair it before setup",
                path.display()
            ),
            Self::Conflict(path) => write!(
                f,
                "MCP config {} already has a different beankeeper server; remove or rename that entry before setup",
                path.display()
            ),
            Self::NonUnicodePath(path) => {
                write!(f, "MCP config requires a Unicode path: {}", path.display())
            }
        }
    }
}
impl Error for ConfigError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io(_, error) => Some(error),
            _ => None,
        }
    }
}

/// Validated config contents, prepared before setup creates credentials.
pub struct ConfigPlan(Vec<(PathBuf, Vec<u8>)>);

impl ConfigPlan {
    /// Exact files that will be updated.
    pub fn paths(&self) -> impl Iterator<Item = &Path> {
        self.0.iter().map(|(path, _)| path.as_path())
    }

    /// Persist each validated config atomically with owner-only Unix permissions.
    ///
    /// # Errors
    /// Returns the affected path if a directory or file cannot be written.
    pub fn write(self) -> Result<Vec<PathBuf>, ConfigError> {
        let mut paths = Vec::new();
        for (path, contents) in self.0 {
            private_write(&path, &contents)
                .map_err(|error| ConfigError::Io(path.clone(), error))?;
            paths.push(path);
        }
        Ok(paths)
    }
}

/// Validate existing files and prepare entries that refer to a private token file.
///
/// `home` is the user's home directory, making paths explicit and testable without
/// mutating the process environment. Existing unrelated servers are preserved.
///
/// # Errors
/// Refuses malformed config or a different existing `beankeeper` entry.
pub fn prepare_configs(
    home: &Path,
    executable: &Path,
    ledger: &Path,
    token_file: &Path,
    url: &str,
) -> Result<ConfigPlan, ConfigError> {
    let text = |path: &Path| {
        path.to_str()
            .map(str::to_owned)
            .ok_or_else(|| ConfigError::NonUnicodePath(path.to_path_buf()))
    };
    let command = text(executable)?;
    let args = vec![
        "--db".to_owned(),
        text(ledger)?,
        "mcp".into(),
        "bridge".into(),
        "--url".into(),
        url.into(),
        "--token-file".into(),
        text(token_file)?,
    ];
    let entry = json!({"command": command, "args": args});
    let mut files = Vec::new();
    for path in [desktop_path(home), home.join(".claude.json")] {
        let contents = read_optional(&path)?;
        files.push((path.clone(), merge_json(&path, &contents, &entry)?));
    }
    let path = home.join(".codex/config.toml");
    let contents = read_optional(&path)?;
    files.push((
        path.clone(),
        merge_toml(&path, &contents, &command, &args)?.into_bytes(),
    ));
    Ok(ConfigPlan(files))
}

/// Prepare and persist the three client config files.
///
/// # Errors
/// Propagates validation, conflict, and filesystem failures.
pub fn write_configs(
    home: &Path,
    executable: &Path,
    ledger: &Path,
    token_file: &Path,
    url: &str,
) -> Result<Vec<PathBuf>, ConfigError> {
    prepare_configs(home, executable, ledger, token_file, url)?.write()
}

fn desktop_path(home: &Path) -> PathBuf {
    if cfg!(target_os = "macos") {
        home.join("Library/Application Support/Claude/claude_desktop_config.json")
    } else if cfg!(target_os = "windows") {
        home.join("AppData/Roaming/Claude/claude_desktop_config.json")
    } else {
        home.join(".config/Claude/claude_desktop_config.json")
    }
}

fn read_optional(path: &Path) -> Result<String, ConfigError> {
    match fs::read_to_string(path) {
        Ok(contents) => Ok(contents),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(String::new()),
        Err(error) => Err(ConfigError::Io(path.to_path_buf(), error)),
    }
}

fn merge_json(path: &Path, contents: &str, entry: &Value) -> Result<Vec<u8>, ConfigError> {
    let invalid = || ConfigError::Invalid(path.to_path_buf());
    let mut document: Value = if contents.trim().is_empty() {
        json!({})
    } else {
        serde_json::from_str(contents).map_err(|_| invalid())?
    };
    let root = document.as_object_mut().ok_or_else(invalid)?;
    let servers = root
        .entry("mcpServers")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .ok_or_else(invalid)?;
    match servers.get("beankeeper") {
        Some(previous) if previous != entry => {
            return Err(ConfigError::Conflict(path.to_path_buf()));
        }
        _ => {
            servers.insert("beankeeper".into(), entry.clone());
        }
    }
    let mut bytes = serde_json::to_vec_pretty(&document).map_err(|_| invalid())?;
    bytes.push(b'\n');
    Ok(bytes)
}

fn merge_toml(
    path: &Path,
    contents: &str,
    command: &str,
    args: &[String],
) -> Result<String, ConfigError> {
    let invalid = || ConfigError::Invalid(path.to_path_buf());
    let mut document: DocumentMut = contents.parse().map_err(|_| invalid())?;
    if !document.contains_key("mcp_servers") {
        document["mcp_servers"] = Item::Table(Table::new());
    }
    let servers = document["mcp_servers"]
        .as_table_like_mut()
        .ok_or_else(invalid)?;
    if let Some(previous) = servers.get("beankeeper") {
        let same_command = previous.get("command").and_then(Item::as_str) == Some(command);
        let same_args = previous
            .get("args")
            .and_then(Item::as_array)
            .is_some_and(|previous| {
                previous.len() == args.len()
                    && previous
                        .iter()
                        .zip(args)
                        .all(|(left, right)| left.as_str() == Some(right.as_str()))
            });
        let only_expected_fields = previous
            .as_table_like()
            .is_some_and(|table| table.len() == 2);
        if !(same_command && same_args && only_expected_fields) {
            return Err(ConfigError::Conflict(path.to_path_buf()));
        }
    } else {
        let mut entry = Table::new();
        entry["command"] = value(command);
        let mut values = toml_edit::Array::new();
        for argument in args {
            values.push(argument.as_str());
        }
        entry["args"] = value(values);
        servers.insert("beankeeper", Item::Table(entry));
    }
    Ok(document.to_string())
}

fn private_write(path: &Path, contents: &[u8]) -> io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::other("config has no parent directory"))?;
    fs::create_dir_all(parent)?;
    let mut file = tempfile::NamedTempFile::new_in(parent)?;
    super::keychain::restrict_file(file.path())?;
    file.write_all(contents)?;
    file.as_file().sync_all()?;
    file.persist(path).map_err(|error| error.error)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    type TestResult = Result<(), Box<dyn Error>>;

    #[test]
    fn merges_three_clients_preserves_settings_and_keeps_tokens_out_of_configs() -> TestResult {
        let home = tempfile::tempdir()?;
        fs::write(
            home.path().join(".claude.json"),
            r#"{"theme":"dark","mcpServers":{"other":{"command":"other"}}}"#,
        )?;
        fs::create_dir(home.path().join(".codex"))?;
        fs::write(
            home.path().join(".codex/config.toml"),
            "# User settings\nmodel = 'example'\n[mcp_servers.other]\ncommand = 'other'\n",
        )?;
        let write = || {
            write_configs(
                home.path(),
                Path::new("/usr/bin/bk"),
                Path::new("/private/books.db"),
                Path::new("/private/agent-token"),
                "http://127.0.0.1:8750/mcp",
            )
        };
        let paths = write()?;
        assert_eq!(paths.len(), 3);
        let before = fs::read_to_string(&paths[2])?;
        write()?;
        assert_eq!(fs::read_to_string(&paths[2])?, before);
        assert!(before.contains("# User settings"));
        let code: Value = serde_json::from_str(&fs::read_to_string(&paths[1])?)?;
        assert_eq!(code["theme"], "dark");
        assert_eq!(code["mcpServers"]["other"]["command"], "other");
        assert_eq!(code["mcpServers"]["beankeeper"]["args"][3], "bridge");
        assert_eq!(
            code["mcpServers"]["beankeeper"]["args"][7],
            "/private/agent-token"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            for path in paths {
                assert_eq!(fs::metadata(path)?.permissions().mode() & 0o777, 0o600);
            }
        }
        Ok(())
    }

    #[test]
    fn invalid_and_conflicting_configs_are_never_overwritten() -> TestResult {
        let home = tempfile::tempdir()?;
        let path = home.path().join(".claude.json");
        for contents in [
            "{broken",
            "null",
            r#"{"mcpServers":[]}"#,
            r#"{"mcpServers":{"beankeeper":{"command":"someone-else"}}}"#,
        ] {
            fs::write(&path, contents)?;
            assert!(
                write_configs(
                    home.path(),
                    Path::new("/bin/bk"),
                    Path::new("/db"),
                    Path::new("/token"),
                    "http://127.0.0.1:8750/mcp"
                )
                .is_err()
            );
            assert_eq!(fs::read_to_string(&path)?, contents);
            assert!(!desktop_path(home.path()).exists());
        }
        Ok(())
    }

    #[test]
    fn inline_toml_servers_and_user_comments_are_preserved() -> TestResult {
        let contents =
            "# existing client settings\nmcp_servers = { other = { command = 'other' } }\n";
        let merged = merge_toml(
            Path::new("config.toml"),
            contents,
            "/bin/bk",
            &["mcp".into(), "bridge".into()],
        )?;
        let document: DocumentMut = merged.parse()?;
        assert_eq!(
            document["mcp_servers"]["other"]["command"].as_str(),
            Some("other")
        );
        assert_eq!(
            document["mcp_servers"]["beankeeper"]["command"].as_str(),
            Some("/bin/bk")
        );
        assert!(merged.contains("# existing client settings"));
        Ok(())
    }

    #[test]
    fn toml_conflicts_and_non_tables_fail_without_leaking_contents() -> TestResult {
        let path = Path::new("config.toml");
        for contents in [
            "mcp_servers = 1",
            "[mcp_servers.beankeeper]\ncommand='other'\n",
            "token = 'private-secret'\ninvalid",
        ] {
            let error = merge_toml(path, contents, "/bin/bk", &[])
                .err()
                .ok_or("expected error")?;
            assert!(!error.to_string().contains("private-secret"));
        }
        Ok(())
    }
}
