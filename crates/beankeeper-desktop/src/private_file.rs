//! Bound credential reads to the handle whose privacy was verified.

use std::{
    fs::{File, Metadata},
    io::{self, Read},
    path::Path,
};

use secrecy::{ExposeSecret, SecretSlice, SecretString, zeroize::Zeroizing};

/// Maximum on-disk size of a Desktop bearer credential, including its newline.
pub(crate) const BEARER_FILE_LIMIT: usize = 64 * 1024;

/// A regular file opened without following a link, with owner-only access.
pub(crate) struct PrivateFile(File);

impl PrivateFile {
    /// Check the opened file's access controls before making any read available.
    pub(crate) fn open(path: &Path) -> io::Result<Self> {
        let file = open_without_following(path)?;
        let metadata = file.metadata()?;
        if !metadata.is_file() {
            return Err(insecure_file());
        }
        validate_permissions(&file, &metadata)?;
        Ok(Self(file))
    }

    /// Read UTF-8 credential text without exposing it through Debug.
    pub(crate) fn read_secret(self, limit: usize) -> io::Result<SecretString> {
        let bytes = self.read_bytes(limit)?;
        std::str::from_utf8(bytes.expose_secret())
            .map(SecretString::from)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "credential file is not UTF-8"))
    }

    /// Read from the verified handle, imposing a limit even if the file grows.
    pub(crate) fn read_bytes(self, limit: usize) -> io::Result<SecretSlice<u8>> {
        let bounded = u64::try_from(limit)
            .ok()
            .and_then(|limit| limit.checked_add(1))
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, "invalid file size limit")
            })?;
        let mut bytes = Zeroizing::new(Vec::new());
        self.0.take(bounded).read_to_end(&mut bytes)?;
        if bytes.len() > limit {
            return Err(io::Error::new(
                io::ErrorKind::FileTooLarge,
                "credential file exceeds size limit",
            ));
        }
        Ok(std::mem::take(&mut *bytes).into())
    }
}

fn insecure_file() -> io::Error {
    io::Error::new(
        io::ErrorKind::PermissionDenied,
        "credential file is not a private regular file owned by the current user",
    )
}

#[cfg(unix)]
fn open_without_following(path: &Path) -> io::Result<File> {
    use rustix::fs::{Mode, OFlags, open};

    // Nonblocking avoids waiting on a FIFO before the regular-file check.
    let flags =
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::NOCTTY | OFlags::CLOEXEC;
    open(path, flags, Mode::empty())
        .map(File::from)
        .map_err(|error| {
            if error == rustix::io::Errno::LOOP {
                insecure_file()
            } else {
                error.into()
            }
        })
}

#[cfg(unix)]
fn validate_permissions(_file: &File, metadata: &Metadata) -> io::Result<()> {
    use std::os::unix::fs::MetadataExt;

    if metadata.mode() & 0o077 != 0 || metadata.uid() != rustix::process::geteuid().as_raw() {
        return Err(insecure_file());
    }
    Ok(())
}

#[cfg(windows)]
fn open_without_following(path: &Path) -> io::Result<File> {
    use std::os::windows::fs::OpenOptionsExt;

    const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)
}

#[cfg(windows)]
fn validate_permissions(file: &File, metadata: &Metadata) -> io::Result<()> {
    use std::os::windows::fs::MetadataExt;
    use windows_permissions::{
        LocalBox, Sid,
        constants::{AceType, SeObjectType, SecurityInformation},
        utilities, wrappers,
    };

    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
    if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(insecure_file());
    }
    let user = utilities::current_process_sid()?;
    let system: LocalBox<Sid> = "SY".parse()?;
    let descriptor = wrappers::GetSecurityInfo(
        file,
        SeObjectType::SE_FILE_OBJECT,
        SecurityInformation::Owner | SecurityInformation::Dacl,
    )?;
    if !descriptor
        .owner()
        .is_some_and(|owner| owner == user.as_ref() || owner == system.as_ref())
    {
        return Err(insecure_file());
    }
    let sddl = wrappers::ConvertSecurityDescriptorToStringSecurityDescriptor(
        &descriptor,
        SecurityInformation::Dacl,
    )?;
    if !sddl.to_string_lossy().starts_with("D:P") {
        return Err(insecure_file());
    }
    let dacl = descriptor.dacl().ok_or_else(insecure_file)?;
    for index in 0..dacl.len() {
        let ace = dacl.get_ace(index).ok_or_else(insecure_file)?;
        if ace.ace_type() != AceType::ACCESS_ALLOWED_ACE_TYPE
            || !ace
                .sid()
                .is_some_and(|sid| sid == user.as_ref() || sid == system.as_ref())
        {
            return Err(insecure_file());
        }
    }
    Ok(())
}

#[cfg(not(any(unix, windows)))]
fn open_without_following(_path: &Path) -> io::Result<File> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "private credential files are unsupported on this platform",
    ))
}

#[cfg(not(any(unix, windows)))]
fn validate_permissions(_file: &File, _metadata: &Metadata) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "private credential files are unsupported on this platform",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn secret_read_honors_exact_byte_limit_and_requires_utf8() -> io::Result<()> {
        let mut file = tempfile::NamedTempFile::new()?;
        crate::setup::keychain::restrict_file(file.path())?;
        file.write_all("£".as_bytes())?;
        let secret = PrivateFile::open(file.path())?.read_secret(2)?;
        assert_eq!(secret.expose_secret(), "£");

        let err = PrivateFile::open(file.path())?.read_secret(1).err();
        assert_eq!(
            err.map(|error| error.kind()),
            Some(io::ErrorKind::FileTooLarge)
        );

        file.write_all(&[0xff])?;
        let err = PrivateFile::open(file.path())?.read_secret(3).err();
        assert_eq!(
            err.map(|error| error.kind()),
            Some(io::ErrorKind::InvalidData)
        );
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn private_reader_rejects_symlinks() -> io::Result<()> {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir()?;
        let file = tempfile::NamedTempFile::new_in(directory.path())?;
        let link = directory.path().join("credential-link");
        symlink(file.path(), &link)?;
        assert_eq!(
            PrivateFile::open(&link).err().map(|error| error.kind()),
            Some(io::ErrorKind::PermissionDenied)
        );

        Ok(())
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn private_reader_rejects_fifos_without_waiting_on_a_writer() -> io::Result<()> {
        let directory = tempfile::tempdir()?;
        let fifo = directory.path().join("credential-fifo");
        rustix::fs::mkfifoat(
            rustix::fs::CWD,
            &fifo,
            rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR,
        )?;
        assert_eq!(
            PrivateFile::open(&fifo).err().map(|error| error.kind()),
            Some(io::ErrorKind::PermissionDenied)
        );
        Ok(())
    }
}
