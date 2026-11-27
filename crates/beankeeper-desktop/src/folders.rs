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
    pub(crate) fn new(path: &Path) -> Self {
        Self(path.into())
    }
    pub(crate) fn containing(file: &Path) -> Self {
        Self(file.parent().unwrap_or(Path::new(".")).into())
    }
    pub(crate) fn grant(&self) -> FolderGrant {
        let parent = self
            .0
            .ancestors()
            .find(|path| path.is_dir())
            .unwrap_or(&self.0);
        FolderGrant {
            path: self.clone(),
            parent: Self::new(parent),
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

#[derive(Debug, Serialize)]
pub(crate) struct FolderGrant {
    path: Directory,
    parent: Directory,
}

#[derive(Debug, Serialize)]
pub(crate) struct FolderPlan {
    directories: Vec<FolderGrant>,
    resume: String,
}
impl TryFrom<(Vec<Directory>, String)> for FolderPlan {
    type Error = io::Error;
    fn try_from((directories, resume): (Vec<Directory>, String)) -> Result<Self, Self::Error> {
        let directories: std::collections::BTreeSet<_> = directories.into_iter().collect();
        for directory in &directories {
            if !directory.as_ref().is_absolute()
                || directory
                    .as_ref()
                    .metadata()
                    .is_ok_and(|metadata| !metadata.is_dir())
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!(
                        "folder grant requires an absolute directory, never a file: {}",
                        directory.0.display()
                    ),
                ));
            }
        }
        Ok(Self {
            directories: directories.iter().map(Directory::grant).collect(),
            resume,
        })
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
    pub(crate) fn details(&self, resume: &str) -> serde_json::Value {
        let grant = self.path.grant();
        serde_json::json!({"path":grant.path,"parent":grant.parent,"kind":self.kind,"resume":resume})
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
    fn missing_directory_grants_nearest_existing_folder_without_creating_it() -> io::Result<()> {
        let root = tempfile::tempdir()?;
        let directory = Directory::containing(&root.path().join("missing/skills/SKILL.md"));
        let grant = directory.grant();
        assert_eq!(grant.parent.as_ref(), root.path());
        assert_eq!(grant.path.as_ref(), root.path().join("missing/skills"));
        assert!(!directory.as_ref().exists());
        Ok(())
    }
    #[test]
    fn plan_rejects_file_and_relative_roots() -> io::Result<()> {
        let root = tempfile::tempdir()?;
        let file = root.path().join("config.json");
        std::fs::write(&file, [])?;
        for directory in [Directory::new(&file), Directory::new(Path::new("relative"))] {
            assert!(
                FolderPlan::try_from((vec![directory], "bk service install --json".into()))
                    .is_err()
            );
        }
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
            let denied = WriteDenied::at(Directory::new(root.path()), &io::Error::from(kind))
                .ok_or("missing denial")?;
            let details = denied.details("bk agents connect codex --json");
            assert_eq!(details["kind"], name);
            assert_eq!(details["resume"], "bk agents connect codex --json");
            assert_eq!(details["path"], details["parent"]);
            assert!(denied.to_string().contains("It picks up where it stopped."));
        }
        assert!(
            WriteDenied::at(
                Directory::new(root.path()),
                &io::Error::from(io::ErrorKind::NotFound)
            )
            .is_none()
        );
        Ok(())
    }
}
