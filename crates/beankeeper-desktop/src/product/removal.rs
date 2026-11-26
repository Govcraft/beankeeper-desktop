//! Local license removal has no service client and never opens the books.
use super::ProductError;
use crate::license::{self, LicenseId};
use serde::Serialize;
use std::{fs, io, path::Path};

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
enum RemovalCode {
    Removed,
    NotInstalled,
}

#[derive(Serialize)]
pub(crate) struct RemovalResponse {
    code: RemovalCode,
    license_id: LicenseId,
    message: &'static str,
    writable: Option<bool>,
    desktop_path: &'static str,
}

impl RemovalResponse {
    pub(super) fn message(&self) -> &str {
        self.message
    }
}

pub(super) fn remove(
    id: LicenseId,
    target: &super::BooksSelection<'_>,
) -> Result<RemovalResponse, ProductError> {
    let books = target.resolve(&super::LicenseRequest::Status)?;
    let mut response = remove_at(&super::state_dir()?, id)?;
    response.writable = books.map(|books| {
        crate::entitlement::LedgerAccess::from(books)
            .status()
            .writable()
    });
    if matches!(response.code, RemovalCode::Removed) {
        response.message = match response.writable {
            Some(true) => {
                "The license was removed from this computer without a refund; your books are still writable and unchanged, and ask me to activate an eligible license to restore access."
            }
            Some(false) => {
                "The license was removed from this computer without a refund; your books are now read-only and unchanged, and ask me to activate an eligible license to restore access."
            }
            None => {
                "The license was removed from this computer without a refund; books without another active license become read-only, and ask me to activate an eligible license to restore access."
            }
        };
    }
    Ok(response)
}

pub(crate) fn remove_at(state: &Path, id: LicenseId) -> Result<RemovalResponse, ProductError> {
    let mut paths = vec![license::activation_path(state, &id)];
    // A legacy activation has no ID in its filename. Authenticate it before
    // deciding whether it belongs to this removal; never remove another license.
    let legacy = state.join("license.json");
    if legacy.try_exists()? {
        let keys = super::trusted_keys()?;
        if license::load(&legacy, &keys)?.is_some_and(|license| license.id() == &id) {
            paths.push(legacy);
        }
    }
    let mut removed = false;
    for path in paths {
        removed |= remove_file(&path)?;
    }
    removed |= crate::entitlement::Bindings(state).remove(&id)?;
    let (code, message) = if removed {
        (
            RemovalCode::Removed,
            "The license was removed from this computer without a refund; books without another active license are now read-only, and your books were never changed; ask me to activate an eligible license to restore access.",
        )
    } else {
        (
            RemovalCode::NotInstalled,
            "That license is not installed on this computer, so nothing changed.",
        )
    };
    Ok(RemovalResponse {
        code,
        license_id: id,
        message,
        writable: None,
        desktop_path: "/settings",
    })
}

fn remove_file(path: &Path) -> io::Result<bool> {
    match fs::remove_file(path) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn removal_preserves_other_license_books_and_recovery_and_is_repeatable()
    -> Result<(), Box<dyn std::error::Error>> {
        let state = tempfile::tempdir()?;
        let id: LicenseId = "lic_01h455vb4pex5vsknk084sn02q".parse()?;
        let other: LicenseId = "lic_01h455vb4pex5vsknk084sn02r".parse()?;
        fs::create_dir(state.path().join("licenses"))?;
        fs::create_dir(state.path().join("license-bindings"))?;
        let activation = license::activation_path(state.path(), &id);
        let binding = state.path().join("license-bindings").join(id.to_string());
        fs::write(&activation, "revoked or complimentary license")?;
        fs::write(&binding, "binding")?;
        let other_path = license::activation_path(state.path(), &other);
        fs::write(&other_path, "other license")?;
        for name in ["books.db", "recovery-kit"] {
            fs::write(state.path().join(name), "untouched")?;
        }
        assert!(matches!(
            remove_at(state.path(), id.clone())?.code,
            RemovalCode::Removed
        ));
        assert!(!activation.exists());
        assert!(!binding.exists());
        assert_eq!(fs::read_to_string(other_path)?, "other license");
        for name in ["books.db", "recovery-kit"] {
            assert_eq!(fs::read_to_string(state.path().join(name))?, "untouched");
        }
        assert!(matches!(
            remove_at(state.path(), id)?.code,
            RemovalCode::NotInstalled
        ));
        Ok(())
    }
}
