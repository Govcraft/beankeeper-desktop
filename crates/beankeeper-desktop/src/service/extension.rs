//! First launch of a user-approved native MCP bundle.

use std::{
    fs, io,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

use super::{Config, Endpoint, Paths, ServiceError, UserSessionEnv, keychain, lock};

pub(crate) fn connection() -> Result<Endpoint, ServiceError> {
    let paths = Paths::discover()?;
    if Config::load(&paths)?.is_none() {
        paths.prepare()?;
        let _lock = lock(&paths.state.join("extension-install.lock"))?;
        if Config::load(&paths)?.is_none() {
            let session = UserSessionEnv::discover()?;
            let executable = install_executable(&std::env::current_exe()?, &paths)?;
            let mut installer = Command::new(executable);
            session.apply(&mut installer);
            #[cfg(windows)]
            {
                use std::os::windows::process::CommandExt;
                const CREATE_NO_WINDOW: u32 = 0x0800_0000;
                installer.creation_flags(CREATE_NO_WINDOW);
            }
            let status = installer
                .args(["service", "install-extension"])
                .env_remove("BEANKEEPER_PASSPHRASE_CMD")
                .env_remove("BK_AGENT_TOKEN")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .status()?;
            if !status.success() {
                return Err(ServiceError::Configuration(
                    "Beankeeper could not set up local hosting for the extension. Check the extension log and unlock your OS keychain before trying again.",
                ));
            }
        }
    }
    super::connection()
}

fn install_executable(source: &Path, paths: &Paths) -> Result<PathBuf, ServiceError> {
    let directory = paths.state.join("claude-extension");
    fs::create_dir_all(&directory)?;
    keychain::restrict_directory(&directory)?;
    let binary = directory.join(if cfg!(windows) { "bk.exe" } else { "bk" });
    let mut pending = tempfile::NamedTempFile::new_in(&directory)?;
    keychain::restrict_file(pending.path())?;
    io::copy(&mut fs::File::open(source)?, pending.as_file_mut())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        pending
            .as_file()
            .set_permissions(fs::Permissions::from_mode(0o700))?;
    }
    pending.as_file().sync_all()?;
    pending.persist(&binary).map_err(|error| error.error)?;
    Ok(binary)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn service_binary_is_independent_of_the_extension_cache()
    -> Result<(), Box<dyn std::error::Error>> {
        let root = tempfile::tempdir()?;
        let paths = Paths {
            home: root.path().join("home"),
            data: root.path().join("data"),
            state: root.path().join("state"),
            logs: root.path().join("logs"),
            definition: root.path().join("service-definition"),
        };
        let source = root.path().join("claude-cache");
        fs::write(&source, b"fixture executable")?;
        let installed = install_executable(&source, &paths)?;
        fs::remove_file(source)?;
        assert!(installed.starts_with(&paths.state));
        assert_eq!(fs::read(&installed)?, b"fixture executable");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(fs::metadata(installed)?.permissions().mode() & 0o777, 0o700);
        }
        Ok(())
    }
}
