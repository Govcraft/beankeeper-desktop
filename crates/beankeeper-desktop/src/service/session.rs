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
pub(super) struct UserSessionEnv(
    #[cfg(target_os = "linux")] SessionTransport,
    #[cfg(target_os = "linux")] Option<PathBuf>,
);

#[cfg(target_os = "linux")]
enum SessionTransport {
    Bus(BusAddress),
    PrivateSocket(PathBuf),
}

/// Parse address syntax once, preserving the exact address passed to the manager.
#[cfg(target_os = "linux")]
#[derive(Debug, PartialEq)]
enum BusAddress {
    Path { address: OsString, path: PathBuf },
    Other(OsString),
}

#[cfg(target_os = "linux")]
impl TryFrom<&OsStr> for BusAddress {
    type Error = ServiceError;

    fn try_from(address: &OsStr) -> Result<Self, Self::Error> {
        if address.is_empty() {
            return Err(ServiceError::SessionEnvUnavailable);
        }
        let path = address
            .to_str()
            .and_then(|value| value.strip_prefix("unix:path="))
            .and_then(|value| value.split(',').next())
            .filter(|value| !value.is_empty() && !value.contains(';'));
        Ok(match path {
            Some(path) => Self::Path {
                address: address.into(),
                path: path.into(),
            },
            None => Self::Other(address.into()),
        })
    }
}

#[cfg(target_os = "linux")]
impl AsRef<OsStr> for BusAddress {
    fn as_ref(&self) -> &OsStr {
        match self {
            Self::Path { address, .. } | Self::Other(address) => address,
        }
    }
}

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
        let socket_owner = fs::symlink_metadata(path.join("systemd/private"))
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
            Self::resolve(
                env::var_os(BUS).as_deref(),
                env::var_os(RUNTIME),
                rustix::process::geteuid().as_raw(),
            )
        }
        #[cfg(not(target_os = "linux"))]
        Ok(Self())
    }

    #[cfg(target_os = "linux")]
    pub(super) fn resolve(
        bus: Option<&OsStr>,
        runtime: Option<OsString>,
        uid: u32,
    ) -> Result<Self, ServiceError> {
        let private = Self::try_from(RuntimeEvidence::inspect(runtime, uid));
        if let Some(mut session) = Self::inherit_bus(bus) {
            session.1 = private.ok().and_then(|session| match session.0 {
                SessionTransport::PrivateSocket(path) => Some(path),
                SessionTransport::Bus(_) => None,
            });
            return Ok(session);
        }
        private
    }

    #[cfg(target_os = "linux")]
    fn inherit_bus(address: Option<&OsStr>) -> Option<Self> {
        address
            .and_then(|address| BusAddress::try_from(address).ok())
            .map(|address| Self(SessionTransport::Bus(address), None))
    }

    pub(super) fn fallback(&self) -> Option<Self> {
        #[cfg(target_os = "linux")]
        {
            self.1
                .as_ref()
                .map(|runtime| Self(SessionTransport::PrivateSocket(runtime.clone()), None))
        }
        #[cfg(not(target_os = "linux"))]
        None
    }

    pub(super) fn connection_failure(&self) -> Option<super::ManagerFailure> {
        #[cfg(target_os = "linux")]
        {
            use std::os::unix::net::UnixStream;
            let path = match &self.0 {
                SessionTransport::Bus(BusAddress::Path { path, .. }) => path.clone(),
                // Other D-Bus transports are handled by the manager itself. A Unix
                // socket probe cannot establish whether those transports are reachable.
                SessionTransport::Bus(BusAddress::Other(_)) => return None,
                SessionTransport::PrivateSocket(runtime) => runtime.join("systemd/private"),
            };
            UnixStream::connect(path)
                .err()
                .map(|error| match error.kind() {
                    std::io::ErrorKind::PermissionDenied => super::ManagerFailure::PermissionDenied,
                    _ => super::ManagerFailure::Connection,
                })
        }
        #[cfg(not(target_os = "linux"))]
        None
    }

    pub(super) fn apply(&self, command: &mut Command) {
        #[cfg(target_os = "linux")]
        match &self.0 {
            SessionTransport::Bus(address) => {
                command.env(BUS, address.as_ref()).env_remove(RUNTIME);
            }
            SessionTransport::PrivateSocket(runtime) => {
                command.env(RUNTIME, runtime).env_remove(BUS);
            }
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
        Ok(Self(
            SessionTransport::PrivateSocket(evidence.directory.as_ref().into()),
            None,
        ))
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
                [
                    (OsStr::new(BUS), None),
                    (OsStr::new(RUNTIME), Some(OsStr::new("/run/user/1234")))
                ]
            );
        }
        Ok(())
    }

    #[test]
    fn explicit_bus_and_private_socket_are_mutually_exclusive() -> Result<(), ServiceError> {
        let bus = UserSessionEnv::inherit_bus(Some(OsStr::new("unix:path=/custom/bus")))
            .ok_or(ServiceError::SessionEnvUnavailable)?;
        let runtime = UserSessionEnv::try_from(evidence("/custom/session", true))?;
        let mut command = Command::new("systemctl");
        bus.apply(&mut command);
        assert_eq!(
            command.get_envs().collect::<Vec<_>>(),
            [
                (OsStr::new(BUS), Some(OsStr::new("unix:path=/custom/bus"))),
                (OsStr::new(RUNTIME), None)
            ]
        );
        let mut command = Command::new("systemctl");
        runtime.apply(&mut command);
        assert_eq!(
            command.get_envs().collect::<Vec<_>>(),
            [
                (OsStr::new(BUS), None),
                (OsStr::new(RUNTIME), Some(OsStr::new("/custom/session")))
            ]
        );
        assert!(UserSessionEnv::inherit_bus(None).is_none());
        assert!(UserSessionEnv::inherit_bus(Some(OsStr::new(""))).is_none());
        Ok(())
    }

    #[test]
    fn bus_address_parses_once_and_preserves_other_transports() -> Result<(), ServiceError> {
        let address = OsStr::new("unix:path=/custom/bus,guid=1234");
        let parsed = BusAddress::try_from(address)?;
        assert!(
            matches!(&parsed, BusAddress::Path { path, .. } if path == Path::new("/custom/bus"))
        );
        assert_eq!(parsed.as_ref(), address);
        for address in [
            "unix:abstract=bus",
            "tcp:host=localhost,port=1234",
            "unix:path=",
        ] {
            let parsed = BusAddress::try_from(OsStr::new(address))?;
            assert!(matches!(&parsed, BusAddress::Other(_)));
            assert_eq!(parsed.as_ref(), OsStr::new(address));
            assert!(
                UserSessionEnv(SessionTransport::Bus(parsed), None)
                    .connection_failure()
                    .is_none()
            );
        }
        assert!(BusAddress::try_from(OsStr::new("")).is_err());
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
        std::fs::create_dir(runtime.path().join("systemd"))?;
        let bus = runtime.path().join("systemd/private");
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
