//! Resolve Linux session defaults without changing the process environment.

use std::process::Command;

use super::ServiceError;

#[cfg(target_os = "linux")]
use std::{
    env, fs,
    os::unix::fs::{FileTypeExt, MetadataExt},
    path::{Path, PathBuf},
};

#[cfg(target_os = "linux")]
const RUNTIME: &str = "XDG_RUNTIME_DIR";
#[cfg(target_os = "linux")]
const BUS: &str = "DBUS_SESSION_BUS_ADDRESS";

pub(super) struct Session {
    #[cfg(target_os = "linux")]
    bus: Bus,
}

#[cfg(target_os = "linux")]
enum Bus {
    Inherited,
    // Only constructed when the directory and its actual bus socket belong to us.
    UserRuntime(PathBuf),
}

impl Session {
    pub(super) fn discover() -> Result<Self, ServiceError> {
        #[cfg(target_os = "linux")]
        {
            if env::var_os(BUS).is_some_and(|address| !address.is_empty()) {
                return Ok(Self {
                    bus: Bus::Inherited,
                });
            }
            let uid = rustix::process::geteuid().as_raw();
            let runtime = env::var_os(RUNTIME)
                .filter(|path| !path.is_empty())
                .map_or_else(
                    || Path::new("/run/user").join(uid.to_string()),
                    PathBuf::from,
                );
            Ok(Self {
                bus: Bus::from_runtime(runtime, uid)?,
            })
        }
        #[cfg(not(target_os = "linux"))]
        Ok(Self {})
    }

    pub(super) fn apply(&self, command: &mut Command) {
        #[cfg(target_os = "linux")]
        if let Bus::UserRuntime(runtime) = &self.bus {
            command.env(RUNTIME, runtime).env_remove(BUS);
        }
        #[cfg(not(target_os = "linux"))]
        let _ = command;
    }
}

#[cfg(target_os = "linux")]
impl Bus {
    fn from_runtime(runtime: PathBuf, uid: u32) -> Result<Self, ServiceError> {
        let dir = fs::symlink_metadata(&runtime);
        let socket = fs::symlink_metadata(runtime.join("bus"));
        if runtime.is_absolute()
            && dir.is_ok_and(|entry| entry.is_dir() && entry.uid() == uid)
            && socket.is_ok_and(|entry| entry.file_type().is_socket() && entry.uid() == uid)
        {
            Ok(Self::UserRuntime(runtime))
        } else {
            Err(ServiceError::SessionEnvUnavailable)
        }
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use std::os::unix::{fs::symlink, net::UnixListener};

    use super::*;

    #[test]
    fn fallback_requires_a_user_owned_directory_and_socket() -> std::io::Result<()> {
        let temp = tempfile::tempdir()?;
        let path = temp.path().join("bus");
        let uid = rustix::process::geteuid().as_raw();
        fs::write(&path, [])?;
        assert!(Bus::from_runtime(temp.path().into(), uid).is_err());
        fs::remove_file(&path)?;
        let _socket = UnixListener::bind(&path)?;
        assert!(Bus::from_runtime(temp.path().into(), uid.wrapping_add(1)).is_err());
        let session = Session {
            bus: Bus::from_runtime(temp.path().into(), uid).map_err(std::io::Error::other)?,
        };
        let mut command = Command::new("systemctl");
        session.apply(&mut command);
        assert!(
            command
                .get_envs()
                .any(|(key, value)| { key == RUNTIME && value == Some(temp.path().as_os_str()) })
        );
        let moved = temp.path().join("other");
        fs::rename(&path, &moved)?;
        symlink(moved, path)?;
        assert!(Bus::from_runtime(temp.path().into(), uid).is_err());
        Ok(())
    }
}
