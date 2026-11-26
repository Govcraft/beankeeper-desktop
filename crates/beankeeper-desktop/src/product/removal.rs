//! Local license removal has no service client and never opens the books.
use super::ProductError;
use crate::license::{self, LicenseId};
use serde::Serialize;
use std::path::Path;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum BooksAccess {
    Writable,
    ReadOnly,
    NotSelected,
}

impl From<crate::entitlement::LedgerLicenseStatus> for BooksAccess {
    fn from(status: crate::entitlement::LedgerLicenseStatus) -> Self {
        if status.writable() {
            Self::Writable
        } else {
            Self::ReadOnly
        }
    }
}

enum RemovalOutcome {
    Removed(BooksAccess),
    NotInstalled,
}

impl std::fmt::Display for RemovalOutcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let access = match self {
            Self::NotInstalled => {
                return f.write_str(
                    "That license is not installed on this computer, so nothing changed.",
                );
            }
            Self::Removed(access) => access,
        };
        f.write_str("The license was removed from this computer without a refund; ")?;
        f.write_str(match access {
            BooksAccess::Writable => "your books are still writable and unchanged",
            BooksAccess::ReadOnly => "your books are now read-only and unchanged",
            BooksAccess::NotSelected => "books without another active license become read-only",
        })?;
        f.write_str(", and ask me to activate an eligible license to restore access.")
    }
}

pub(crate) struct RemovalResponse {
    license_id: LicenseId,
    outcome: RemovalOutcome,
}

impl Serialize for RemovalResponse {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut response = serializer.serialize_struct("RemovalResponse", 5)?;
        let (code, access) = match self.outcome {
            RemovalOutcome::Removed(access) => ("removed", access),
            RemovalOutcome::NotInstalled => ("not_installed", BooksAccess::NotSelected),
        };
        response.serialize_field("code", code)?;
        response.serialize_field("license_id", &self.license_id)?;
        response.serialize_field("message", &self.message())?;
        response.serialize_field("books_access", &access)?;
        response.serialize_field("desktop_path", "/settings")?;
        response.end()
    }
}

impl RemovalResponse {
    pub(super) fn message(&self) -> String {
        self.outcome.to_string()
    }
}

pub(super) fn remove(
    id: LicenseId,
    target: &super::BooksSelection<'_>,
) -> Result<RemovalResponse, ProductError> {
    let books = target.resolve(&super::LicenseRequest::Status)?;
    let removed = remove_at(&super::state_dir()?, &id)?;
    let outcome = if removed {
        let access = books.map_or(BooksAccess::NotSelected, |books| {
            crate::entitlement::LedgerAccess::from(books)
                .status()
                .into()
        });
        RemovalOutcome::Removed(access)
    } else {
        RemovalOutcome::NotInstalled
    };
    Ok(RemovalResponse {
        license_id: id,
        outcome,
    })
}

pub(crate) fn remove_at(state: &Path, id: &LicenseId) -> Result<bool, ProductError> {
    let mut paths = vec![license::activation_path(state, id)];
    // A legacy activation has no ID in its filename. Authenticate it before
    // deciding whether it belongs to this removal; never remove another license.
    let legacy = state.join("license.json");
    if legacy.try_exists()? {
        let keys = super::trusted_keys()?;
        if license::load(&legacy, &keys)?.is_some_and(|license| license.id() == id) {
            paths.push(legacy);
        }
    }
    let mut removed = false;
    for path in paths {
        removed |= crate::private_file::remove_file(&path)?;
    }
    removed |= crate::entitlement::Bindings(state).remove(id)?;
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn each_access_state_renders_and_serializes_its_sentence() -> Result<(), serde_json::Error> {
        for (access, sentence, value) in [
            (
                BooksAccess::Writable,
                "your books are still writable and unchanged",
                "writable",
            ),
            (
                BooksAccess::ReadOnly,
                "your books are now read-only and unchanged",
                "read_only",
            ),
            (
                BooksAccess::NotSelected,
                "books without another active license become read-only",
                "not_selected",
            ),
        ] {
            let outcome = RemovalOutcome::Removed(access);
            assert_eq!(
                outcome.to_string(),
                format!(
                    "The license was removed from this computer without a refund; {sentence}, and ask me to activate an eligible license to restore access."
                )
            );
            assert_eq!(serde_json::to_value(access)?, value);
            let response = RemovalResponse {
                license_id: "lic_01h455vb4pex5vsknk084sn02q"
                    .parse()
                    .expect("valid fixture"),
                outcome,
            };
            let serialized = serde_json::to_value(&response)?;
            assert_eq!(serialized["message"], response.message());
            assert_eq!(serialized["books_access"], value);
            assert_eq!(serialized["code"], "removed");
        }
        assert_eq!(
            RemovalOutcome::NotInstalled.to_string(),
            "That license is not installed on this computer, so nothing changed."
        );
        Ok(())
    }

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
        assert!(remove_at(state.path(), &id)?);
        assert!(!activation.exists());
        assert!(!binding.exists());
        assert_eq!(fs::read_to_string(other_path)?, "other license");
        for name in ["books.db", "recovery-kit"] {
            assert_eq!(fs::read_to_string(state.path().join(name))?, "untouched");
        }
        assert!(!remove_at(state.path(), &id)?);
        Ok(())
    }
}
