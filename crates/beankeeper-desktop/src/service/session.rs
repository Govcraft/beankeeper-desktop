//! Resolve Linux session defaults without changing the process environment.

use std::process::Command;

use super::ServiceError;

#[cfg(target_os = "linux")]
use std::{
    env,
    ffi::{OsStr, OsString},
    fs,
    os::unix::fs::{FileTypeExt, MetadataExt},
    path::{Path, PathBuf},
};

#[cfg(target_os = "linux")]
const RUNTIME: &str = "XDG_RUNTIME_DIR";
#[cfg(target_os = "linux")]
const BUS: &str = "DBUS_SESSION_BUS_ADDRESS";

/// The runtime directory to add to a child, if the caller did not supply one.
pub(super) struct UserSessionEnv(#[cfg(target_os = "linux")] Option<PathBuf>);

#[cfg(target_os = "linux")]
enum RuntimeDirectory {
    Inherited(PathBuf),
    Fallback(PathBuf),
}

#[cfg(target_os = "linux")]
impl From<(Option<OsString>, u32)> for RuntimeDirectory {
    fn from((runtime, uid): (Option<OsString>, u32)) -> Self {
        match runtime.filter(|path| !path.is_empty()) {
            Some(path) => Self::Inherited(path.into()),
            None => Self::Fallback(Path::new("/run/user").join(uid.to_string())),
        }
    }
}

#[cfg(target_os = "linux")]
impl AsRef<Path> for RuntimeDirectory {
    fn as_ref(&self) -> &Path {
        match self {
            Self::Inherited(path) | Self::Fallback(path) => path,
        }
    }
}

#[cfg(target_os = "linux")]
struct RuntimeEvidence {
    directory: RuntimeDirectory,
    uid: u32,
    directory_owner: Option<u32>,
    socket_owner: Option<u32>,
}

#[cfg(target_os = "linux")]
impl RuntimeEvidence {
    fn inspect(runtime: Option<OsString>, uid: u32) -> Self {
        let directory = RuntimeDirectory::from((runtime, uid));
        let path = directory.as_ref();
        let directory_owner = fs::symlink_metadata(path)
            .ok()
            .filter(fs::Metadata::is_dir)
            .map(|directory| directory.uid());
        let socket_owner = fs::symlink_metadata(path.join("bus"))
            .ok()
            .filter(|socket| socket.file_type().is_socket())
            .map(|socket| socket.uid());
        Self {
            directory,
            uid,
            directory_owner,
            socket_owner,
        }
    }
}

impl UserSessionEnv {
    pub(super) fn discover() -> Result<Self, ServiceError> {
        #[cfg(target_os = "linux")]
        {
            if let Some(session) = Self::inherit_bus(env::var_os(BUS).as_deref()) {
                return Ok(session);
            }
            let uid = rustix::process::geteuid().as_raw();
            Self::try_from(RuntimeEvidence::inspect(env::var_os(RUNTIME), uid))
        }
        #[cfg(not(target_os = "linux"))]
        Ok(Self())
    }

    #[cfg(target_os = "linux")]
    fn inherit_bus(address: Option<&OsStr>) -> Option<Self> {
        address
            .filter(|address| !address.is_empty())
            .map(|_| Self(None))
    }

    pub(super) fn apply(&self, command: &mut Command) {
        #[cfg(target_os = "linux")]
        if let Some(runtime) = &self.0 {
            // systemd resolves the bus socket from XDG_RUNTIME_DIR.
            command.env(RUNTIME, runtime);
        }
        #[cfg(not(target_os = "linux"))]
        let _ = command;
    }
}

#[cfg(target_os = "linux")]
impl TryFrom<RuntimeEvidence> for UserSessionEnv {
    type Error = ServiceError;

    fn try_from(evidence: RuntimeEvidence) -> Result<Self, Self::Error> {
        if !evidence.directory.as_ref().is_absolute()
            || evidence.directory_owner != Some(evidence.uid)
            || evidence.socket_owner != Some(evidence.uid)
        {
            return Err(ServiceError::SessionEnvUnavailable);
        }
        Ok(match evidence.directory {
            RuntimeDirectory::Inherited(_) => Self(None),
            RuntimeDirectory::Fallback(path) => Self(Some(path)),
        })
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use std::os::unix::{fs::symlink, net::UnixListener};

    use super::*;

    fn evidence(path: impl Into<OsString>, inherited: bool) -> RuntimeEvidence {
        let uid = 1234;
        RuntimeEvidence {
            directory: RuntimeDirectory::from((inherited.then(|| path.into()), uid)),
            uid,
            directory_owner: Some(uid),
            socket_owner: Some(uid),
        }
    }

    #[test]
    fn absent_or_empty_runtime_uses_the_current_users_runtime_directory() -> Result<(), ServiceError>
    {
        for evidence in [evidence("", false), evidence("", true)] {
            let session = UserSessionEnv::try_from(evidence)?;
            let mut command = Command::new("systemctl");
            session.apply(&mut command);
            assert_eq!(
                command.get_envs().collect::<Vec<_>>(),
                [(OsStr::new(RUNTIME), Some(OsStr::new("/run/user/1234")))]
            );
        }
        Ok(())
    }

    #[test]
    fn existing_session_variables_are_left_inherited() -> Result<(), ServiceError> {
        let bus = UserSessionEnv::inherit_bus(Some(OsStr::new("unix:path=/custom/bus")))
            .ok_or(ServiceError::SessionEnvUnavailable)?;
        let runtime = UserSessionEnv::try_from(evidence("/custom/session", true))?;
        for session in [bus, runtime] {
            let mut command = Command::new("systemctl");
            session.apply(&mut command);
            assert_eq!(command.get_envs().len(), 0);
        }
        assert!(UserSessionEnv::inherit_bus(None).is_none());
        assert!(UserSessionEnv::inherit_bus(Some(OsStr::new(""))).is_none());
        Ok(())
    }

    #[test]
    fn fallback_requires_an_absolute_directory_and_socket_owned_by_the_user() {
        for invalid in [
            evidence("relative/runtime", true),
            RuntimeEvidence {
                directory_owner: None,
                ..evidence("", false)
            },
            RuntimeEvidence {
                directory_owner: Some(4321),
                ..evidence("", false)
            },
            RuntimeEvidence {
                socket_owner: None,
                ..evidence("", false)
            },
            RuntimeEvidence {
                socket_owner: Some(4321),
                ..evidence("", false)
            },
        ] {
            assert!(matches!(
                UserSessionEnv::try_from(invalid),
                Err(ServiceError::SessionEnvUnavailable)
            ));
        }
    }

    #[test]
    fn runtime_evidence_rejects_regular_files_and_symlinks_as_buses() -> std::io::Result<()> {
        let runtime = tempfile::tempdir()?;
        let bus = runtime.path().join("bus");
        let evidence = || {
            RuntimeEvidence::inspect(
                Some(runtime.path().as_os_str().to_owned()),
                rustix::process::geteuid().as_raw(),
            )
        };
        fs::write(&bus, [])?;
        assert!(UserSessionEnv::try_from(evidence()).is_err());
        fs::remove_file(&bus)?;
        let actual_socket = runtime.path().join("other");
        let _socket = UnixListener::bind(&actual_socket)?;
        symlink(&actual_socket, &bus)?;
        assert!(UserSessionEnv::try_from(evidence()).is_err());
        Ok(())
    }
}
