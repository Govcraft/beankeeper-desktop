//! Linux handoff independent of the system's MIME associations.

use std::{
    env,
    ffi::OsString,
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

use super::ExtensionError;

/// Untrusted process environment, resolved once into a launcher.
pub(super) struct LauncherEnvironment {
    data_home: Option<PathBuf>,
    data_dirs: Vec<PathBuf>,
    path: Vec<PathBuf>,
}

impl Default for LauncherEnvironment {
    fn default() -> Self {
        let data_home = env::var_os("XDG_DATA_HOME")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .filter(|value| value.is_absolute())
            .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share")));
        let data_dirs = env::var_os("XDG_DATA_DIRS")
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| OsString::from("/usr/local/share:/usr/share"));
        Self {
            data_home,
            data_dirs: env::split_paths(&data_dirs).collect(),
            path: env::var_os("PATH")
                .map(|value| env::split_paths(&value).collect())
                .unwrap_or_default(),
        }
    }
}

#[derive(Debug)]
enum LaunchCommand {
    Desktop { gio: PathBuf, entry: PathBuf },
    Executable(PathBuf),
}

/// A resolved launcher with no public constructor or mutable launch policy.
#[derive(Debug)]
pub(super) struct ClaudeLauncher(LaunchCommand);

impl TryFrom<LauncherEnvironment> for ClaudeLauncher {
    type Error = ExtensionError;

    fn try_from(environment: LauncherEnvironment) -> Result<Self, Self::Error> {
        let executable = |name| {
            environment.path.iter().find_map(|directory| {
                let path = directory.join(name);
                fs::metadata(&path)
                    .ok()
                    .filter(|metadata| {
                        metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
                    })
                    .map(|_| path)
            })
        };
        if let Some(gio) = executable("gio") {
            let entry = environment
                .data_home
                .iter()
                .chain(&environment.data_dirs)
                .filter(|directory| directory.is_absolute())
                .map(|directory| directory.join("applications/com.anthropic.Claude.desktop"))
                .find(|entry| entry.is_file());
            if let Some(entry) = entry {
                return Ok(Self(LaunchCommand::Desktop { gio, entry }));
            }
        }
        executable("claude-desktop")
            .map(|path| Self(LaunchCommand::Executable(path)))
            .ok_or(ExtensionError::ClaudeNotFound)
    }
}

impl ClaudeLauncher {
    fn command(&self, bundle: &Path) -> Command {
        match &self.0 {
            LaunchCommand::Desktop { gio, entry } => {
                let mut command = Command::new(gio);
                command.arg("launch").arg(entry).arg(bundle);
                command
            }
            LaunchCommand::Executable(executable) => {
                let mut command = Command::new(executable);
                command.arg(bundle);
                command
            }
        }
    }

    pub(super) fn open(&self, bundle: &Path) -> std::io::Result<()> {
        let mut command = self.command(bundle);
        command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        match self.0 {
            LaunchCommand::Desktop { .. } => {
                if command.status()?.success() {
                    Ok(())
                } else {
                    Err(std::io::Error::other("Claude Desktop handoff failed"))
                }
            }
            LaunchCommand::Executable(_) => {
                let mut child = command.spawn()?;
                std::thread::spawn(move || {
                    let _ = child.wait();
                });
                Ok(())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn executable(directory: &Path, name: &str, body: &str) -> std::io::Result<PathBuf> {
        let path = directory.join(name);
        fs::write(&path, format!("#!/bin/sh\n{body}\n"))?;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755))?;
        Ok(path)
    }

    fn environment(root: &Path) -> LauncherEnvironment {
        LauncherEnvironment {
            data_home: Some(root.join("user")),
            data_dirs: vec![root.join("system")],
            path: vec![root.to_path_buf()],
        }
    }

    fn desktop(root: &Path, directory: &str) -> std::io::Result<PathBuf> {
        let directory = root.join(directory).join("applications");
        fs::create_dir_all(&directory)?;
        let entry = directory.join("com.anthropic.Claude.desktop");
        fs::write(
            &entry,
            "[Desktop Entry]\nType=Application\nExec=fixture %U\n",
        )?;
        Ok(entry)
    }

    #[test]
    fn desktop_handoff_prefers_user_entry_and_preserves_literal_bundle_path()
    -> Result<(), Box<dyn std::error::Error>> {
        let root = tempfile::tempdir()?;
        let entry = desktop(root.path(), "user")?;
        desktop(root.path(), "system")?;
        let gio = executable(root.path(), "gio", "printf '%s\\n' \"$@\"")?;
        executable(root.path(), "claude-desktop", "exit 99")?;
        let launcher = ClaudeLauncher::try_from(environment(root.path()))?;
        let bundle = root.path().join("extension with spaces;$.mcpb");
        let mut command = launcher.command(&bundle);
        assert_eq!(command.get_program(), gio);
        let output = command.output()?;
        assert!(output.status.success());
        launcher.open(&bundle)?;
        assert_eq!(
            String::from_utf8(output.stdout)?,
            format!("launch\n{}\n{}\n", entry.display(), bundle.display())
        );
        Ok(())
    }

    #[test]
    fn fallback_works_without_gio_or_desktop_entry() -> Result<(), Box<dyn std::error::Error>> {
        let root = tempfile::tempdir()?;
        desktop(root.path(), "system")?;
        let program = executable(root.path(), "claude-desktop", "printf '%s\\n' \"$@\"")?;
        let launcher = ClaudeLauncher::try_from(environment(root.path()))?;
        assert_eq!(
            launcher.command(Path::new("verified.mcpb")).get_program(),
            program
        );
        let output = launcher
            .command(Path::new("verified bundle.mcpb"))
            .output()?;
        assert!(output.status.success());
        assert_eq!(String::from_utf8(output.stdout)?, "verified bundle.mcpb\n");
        executable(root.path(), "gio", "exit 0")?;
        fs::remove_dir_all(root.path().join("system"))?;
        let launcher = ClaudeLauncher::try_from(environment(root.path()))?;
        assert_eq!(
            launcher.command(Path::new("verified.mcpb")).get_program(),
            program
        );
        Ok(())
    }

    #[test]
    fn system_entry_is_used_and_failed_handoff_is_reported()
    -> Result<(), Box<dyn std::error::Error>> {
        let root = tempfile::tempdir()?;
        let entry = desktop(root.path(), "system")?;
        executable(root.path(), "gio", "exit 1")?;
        let launcher = ClaudeLauncher::try_from(environment(root.path()))?;
        assert!(
            launcher
                .command(Path::new("bundle.mcpb"))
                .get_args()
                .any(|arg| arg == entry)
        );
        assert!(launcher.open(Path::new("bundle.mcpb")).is_err());
        Ok(())
    }

    #[test]
    fn missing_and_nonexecutable_launchers_have_named_plain_error()
    -> Result<(), Box<dyn std::error::Error>> {
        let root = tempfile::tempdir()?;
        fs::write(root.path().join("claude-desktop"), "not executable")?;
        let error = ClaudeLauncher::try_from(environment(root.path())).unwrap_err();
        assert!(matches!(error, ExtensionError::ClaudeNotFound));
        assert_eq!(error.code(), "AGENT_EXTENSION_CLAUDE_NOT_FOUND");
        assert!(error.to_string().contains("Claude Desktop"));
        Ok(())
    }
}
