//! Folder grants and recoverable filesystem failures shared by local adapters.

use serde::Serialize;
use std::{
    fmt, io,
    path::{Path, PathBuf},
};

/// A directory, never a file grant. Construction is restricted to directory paths.
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize)]
#[serde(transparent)]
pub(crate) struct Directory(PathBuf);

impl Directory {
    pub(crate) fn grant(&self) -> FolderGrant {
        let parent = self
            .0
            .ancestors()
            .find(|path| path.is_dir())
            .unwrap_or(&self.0);
        FolderGrant {
            path: self.clone(),
            parent: Self(parent.into()),
        }
    }
    pub(crate) fn create(&self) -> io::Result<()> {
        std::fs::create_dir_all(&self.0)
    }
    pub(crate) fn prepare(&self) -> io::Result<()> {
        self.create()?;
        crate::setup::keychain::restrict_directory(&self.0)
    }
}
impl AsRef<Path> for Directory {
    fn as_ref(&self) -> &Path {
        &self.0
    }
}

impl TryFrom<PathBuf> for Directory {
    type Error = io::Error;
    fn try_from(path: PathBuf) -> io::Result<Self> {
        if !path.is_absolute() || path.metadata().is_ok_and(|metadata| !metadata.is_dir()) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "folder grant requires an absolute directory, never a file: {}",
                    path.display()
                ),
            ));
        }
        Ok(Self(path))
    }
}
impl TryFrom<&Path> for Directory {
    type Error = io::Error;
    fn try_from(path: &Path) -> io::Result<Self> {
        Self::try_from(path.to_path_buf())
    }
}

/// A resume command constructed only from typed lifecycle or connection options.
#[derive(Clone, Debug, Serialize)]
#[serde(transparent)]
pub(crate) struct ResumeCommand(Resume);
#[derive(Clone, Debug, Serialize)]
#[serde(untagged)]
enum Resume {
    Cli(String),
    Mcp {
        tool: &'static str,
        arguments: ResumeArguments,
    },
}
#[derive(Clone, Debug, Serialize)]
#[serde(untagged)]
enum ResumeArguments {
    Install(crate::service::InstallOptions),
    Connect(crate::agents::ConnectInput),
    Status {},
}
pub(crate) struct McpResume<T>(pub(crate) T);
impl From<McpResume<crate::service::operation::ServiceOperation>> for ResumeCommand {
    fn from(McpResume(operation): McpResume<crate::service::operation::ServiceOperation>) -> Self {
        match operation {
            crate::service::operation::ServiceOperation::Install(options) => {
                Self::from(McpResume(options))
            }
            _ => Self(Resume::Mcp {
                tool: "service_status",
                arguments: ResumeArguments::Status {},
            }),
        }
    }
}
impl From<McpResume<crate::service::InstallOptions>> for ResumeCommand {
    fn from(McpResume(mut options): McpResume<crate::service::InstallOptions>) -> Self {
        options.plan = false;
        Self(Resume::Mcp {
            tool: "service_install",
            arguments: ResumeArguments::Install(options),
        })
    }
}
impl From<McpResume<&crate::agents::ConnectOptions>> for ResumeCommand {
    fn from(McpResume(options): McpResume<&crate::agents::ConnectOptions>) -> Self {
        let mut arguments = crate::agents::ConnectInput::from(options);
        arguments.plan = false;
        Self(Resume::Mcp {
            tool: "agents_connect",
            arguments: ResumeArguments::Connect(arguments),
        })
    }
}
impl From<(crate::service::InstallOptions, Option<&Path>)> for ResumeCommand {
    fn from((options, books): (crate::service::InstallOptions, Option<&Path>)) -> Self {
        let books = books.map_or_else(String::new, |path| {
            format!(
                " --db '{}'",
                path.to_string_lossy().replace('\'', "'\"'\"'")
            )
        });
        Self(Resume::Cli(format!(
            "bk{books} service install{}{} --json",
            if options.file_store {
                " --file-store"
            } else {
                ""
            },
            if options.no_agents {
                " --no-agents"
            } else {
                ""
            }
        )))
    }
}
impl From<crate::service::InstallOptions> for ResumeCommand {
    fn from(options: crate::service::InstallOptions) -> Self {
        Self::from((options, None))
    }
}
impl From<Option<crate::agents::Agent>> for ResumeCommand {
    fn from(selected: Option<crate::agents::Agent>) -> Self {
        use clap::ValueEnum;
        let selection = selected
            .and_then(|agent| agent.to_possible_value())
            .map_or_else(|| "--all".into(), |value| value.get_name().to_owned());
        Self(Resume::Cli(format!("bk agents connect {selection} --json")))
    }
}
impl From<&crate::agents::ConnectOptions> for ResumeCommand {
    fn from(options: &crate::agents::ConnectOptions) -> Self {
        Self::from(options.selected())
    }
}
impl From<crate::service::operation::ServiceOperation> for ResumeCommand {
    fn from(operation: crate::service::operation::ServiceOperation) -> Self {
        use crate::service::operation::ServiceOperation;
        match operation {
            ServiceOperation::Install(options) => Self::from(options),
            ServiceOperation::Start => Self(Resume::Cli("bk service start --json".into())),
            ServiceOperation::Stop => Self(Resume::Cli("bk service stop --json".into())),
            ServiceOperation::Uninstall => Self(Resume::Cli("bk service uninstall --json".into())),
            ServiceOperation::Open => Self(Resume::Cli("bk service open --json".into())),
            ServiceOperation::InstallExtension => {
                Self(Resume::Cli("bk service install-extension --json".into()))
            }
            ServiceOperation::AccessSet(_) | ServiceOperation::Status => {
                Self(Resume::Cli("bk service status --json".into()))
            }
        }
    }
}
impl fmt::Display for ResumeCommand {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.0 {
            Resume::Cli(command) => f.write_str(command),
            Resume::Mcp { tool, arguments } => {
                write!(f, "{tool} ")?;
                let arguments = serde_json::to_string(arguments).map_err(|_| fmt::Error)?;
                f.write_str(&arguments)
            }
        }
    }
}
#[derive(Clone, Debug, Serialize)]
pub(crate) struct WriteDeniedDetails {
    path: Directory,
    parent: Directory,
    kind: DeniedKind,
    pub(crate) resume: ResumeCommand,
}

#[derive(Debug, Serialize)]
pub(crate) struct FolderGrant {
    path: Directory,
    parent: Directory,
}

#[derive(Debug, Serialize)]
pub(crate) struct FolderPlan {
    directories: Vec<FolderGrant>,
    resume: ResumeCommand,
}
impl From<(Vec<Directory>, ResumeCommand)> for FolderPlan {
    fn from((directories, resume): (Vec<Directory>, ResumeCommand)) -> Self {
        let directories: std::collections::BTreeSet<_> = directories.into_iter().collect();
        Self {
            directories: directories.iter().map(Directory::grant).collect(),
            resume,
        }
    }
}
impl FolderPlan {
    pub(crate) fn set_resume(&mut self, resume: ResumeCommand) {
        self.resume = resume;
    }
}
impl fmt::Display for FolderPlan {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for grant in &self.directories {
            writeln!(
                f,
                "{} (grant folder: {})",
                grant.path.0.display(),
                grant.parent.0.display()
            )?;
        }
        write!(
            f,
            "Grant folders, never individual files. Then run {}. It picks up where it stopped.",
            self.resume
        )
    }
}

#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum DeniedKind {
    ReadOnly,
    PermissionDenied,
}
impl TryFrom<io::ErrorKind> for DeniedKind {
    type Error = ();
    fn try_from(kind: io::ErrorKind) -> Result<Self, ()> {
        match kind {
            io::ErrorKind::ReadOnlyFilesystem => Ok(Self::ReadOnly),
            io::ErrorKind::PermissionDenied => Ok(Self::PermissionDenied),
            _ => Err(()),
        }
    }
}

/// Validated once from an OS error at the affected filesystem boundary.
#[derive(Debug, Serialize)]
pub struct WriteDenied {
    path: Directory,
    kind: DeniedKind,
}
impl WriteDenied {
    pub(crate) fn at(directory: Directory, error: &io::Error) -> Option<Self> {
        DeniedKind::try_from(error.kind()).ok().map(|kind| Self {
            path: directory,
            kind,
        })
    }
    pub(crate) fn details(&self, resume: &ResumeCommand) -> WriteDeniedDetails {
        let grant = self.path.grant();
        WriteDeniedDetails {
            path: grant.path,
            parent: grant.parent,
            kind: self.kind,
            resume: resume.clone(),
        }
    }
}
impl std::error::Error for WriteDenied {}
impl fmt::Display for WriteDenied {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Beankeeper can't write to {}. If you're running in a sandbox, ask for write access to that folder and run the same command again. It picks up where it stopped.",
            self.path.0.display()
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn directory_validates_at_construction() -> io::Result<()> {
        let root = tempfile::tempdir()?;
        let file = root.path().join("config.json");
        std::fs::write(&file, [])?;
        assert!(Directory::try_from(file).is_err());
        assert!(Directory::try_from(Path::new("relative")).is_err());
        let directory = Directory::try_from(root.path().join("missing/skills"))?;
        let grant = directory.grant();
        assert_eq!(grant.parent.as_ref(), root.path());
        assert!(!directory.as_ref().exists());
        Ok(())
    }
    #[test]
    fn resume_commands_preserve_options_and_quoting() {
        let options = crate::service::InstallOptions {
            file_store: true,
            no_agents: true,
            plan: true,
        };
        assert_eq!(
            ResumeCommand::from(options).to_string(),
            "bk service install --file-store --no-agents --json"
        );
        assert_eq!(
            ResumeCommand::from((options, Some(Path::new("/books/owner's.db")))).to_string(),
            "bk --db '/books/owner'\"'\"'s.db' service install --file-store --no-agents --json"
        );
        assert_eq!(
            ResumeCommand::from(None::<crate::agents::Agent>).to_string(),
            "bk agents connect --all --json"
        );
        assert_eq!(
            ResumeCommand::from(Some(crate::agents::Agent::Codex)).to_string(),
            "bk agents connect codex --json"
        );
    }
    #[test]
    fn mcp_resumes_preserve_surface_and_options() -> Result<(), serde_json::Error> {
        let install = crate::service::InstallOptions {
            file_store: true,
            no_agents: true,
            plan: true,
        };
        let resume = serde_json::to_value(ResumeCommand::from(McpResume(install)))?;
        assert_eq!(resume["tool"], "service_install");
        assert_eq!(resume["arguments"]["file_store"], true);
        assert_eq!(resume["arguments"]["no_agents"], true);
        assert_eq!(resume["arguments"]["plan"], false);
        let connect = serde_json::from_value::<crate::agents::ConnectOptions>(
            serde_json::json!({"agent":"codex","plan":true}),
        )?;
        let resume = serde_json::to_value(ResumeCommand::from(McpResume(&connect)))?;
        assert_eq!(resume["tool"], "agents_connect");
        assert_eq!(resume["arguments"]["agent"], "codex");
        assert_eq!(resume["arguments"]["plan"], false);
        let cli = serde_json::to_value(ResumeCommand::from(&connect))?;
        assert_eq!(cli, "bk agents connect codex --json");
        Ok(())
    }
    #[test]
    fn denied_errors_classify_os_kinds_once_and_keep_resume()
    -> Result<(), Box<dyn std::error::Error>> {
        let root = tempfile::tempdir()?;
        for (kind, name) in [
            (io::ErrorKind::ReadOnlyFilesystem, "read_only"),
            (io::ErrorKind::PermissionDenied, "permission_denied"),
        ] {
            let denied = WriteDenied::at(Directory::try_from(root.path())?, &io::Error::from(kind))
                .ok_or("missing denial")?;
            let resume = ResumeCommand::from(Some(crate::agents::Agent::Codex));
            let details = serde_json::to_value(denied.details(&resume))?;
            assert_eq!(details["kind"], name);
            assert_eq!(details["resume"], "bk agents connect codex --json");
            assert_eq!(details["path"], details["parent"]);
            assert!(denied.to_string().contains("It picks up where it stopped."));
        }
        assert!(
            WriteDenied::at(
                Directory::try_from(root.path())?,
                &io::Error::from(io::ErrorKind::NotFound)
            )
            .is_none()
        );
        Ok(())
    }
}
