//! Resolve Linux session defaults without changing the process environment.

use std::process::Command;

use super::ServiceError;

#[cfg(target_os = "linux")]
use std::{
    env,
    ffi::OsStr,
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
struct RuntimeEvidence {
    path: PathBuf,
    inherited: bool,
    uid: u32,
    directory_owner: Option<u32>,
    socket_owner: Option<u32>,
}

#[cfg(target_os = "linux")]
impl RuntimeEvidence {
    fn inspect(path: PathBuf, inherited: bool, uid: u32) -> Self {
        let directory_owner = fs::symlink_metadata(&path)
            .ok()
            .filter(fs::Metadata::is_dir)
            .map(|directory| directory.uid());
        let socket_owner = fs::symlink_metadata(path.join("bus"))
            .ok()
            .filter(|socket| socket.file_type().is_socket())
            .map(|socket| socket.uid());
        Self {
            path,
            inherited,
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
            let runtime = env::var_os(RUNTIME);
            let inherited = runtime.is_some();
            let path = runtime.map_or_else(|| Self::default_runtime(uid), PathBuf::from);
            Self::try_from(RuntimeEvidence::inspect(path, inherited, uid))
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

    #[cfg(target_os = "linux")]
    fn default_runtime(uid: u32) -> PathBuf {
        Path::new("/run/user").join(uid.to_string())
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
        if !evidence.path.is_absolute()
            || evidence.directory_owner != Some(evidence.uid)
            || evidence.socket_owner != Some(evidence.uid)
        {
            return Err(ServiceError::SessionEnvUnavailable);
        }
        Ok(Self((!evidence.inherited).then_some(evidence.path)))
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use std::os::unix::{fs::symlink, net::UnixListener};

    use super::*;

    fn evidence(path: impl Into<PathBuf>, inherited: bool) -> RuntimeEvidence {
        RuntimeEvidence {
            path: path.into(),
            inherited,
            uid: 1234,
            directory_owner: Some(1234),
            socket_owner: Some(1234),
        }
    }

    #[test]
    fn absent_runtime_uses_the_current_users_runtime_directory() -> Result<(), ServiceError> {
        let runtime = UserSessionEnv::default_runtime(1234);
        let session = UserSessionEnv::try_from(evidence(runtime, false))?;
        let mut command = Command::new("systemctl");
        session.apply(&mut command);
        assert_eq!(
            command.get_envs().collect::<Vec<_>>(),
            [(OsStr::new(RUNTIME), Some(OsStr::new("/run/user/1234")))]
        );
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
            evidence("", true),
            RuntimeEvidence {
                directory_owner: None,
                ..evidence("/run/user/1234", false)
            },
            RuntimeEvidence {
                directory_owner: Some(4321),
                ..evidence("/run/user/1234", false)
            },
            RuntimeEvidence {
                socket_owner: None,
                ..evidence("/run/user/1234", false)
            },
            RuntimeEvidence {
                socket_owner: Some(4321),
                ..evidence("/run/user/1234", false)
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
                runtime.path().into(),
                false,
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
