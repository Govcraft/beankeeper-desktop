//! Shared license status and activation output, with only public claims rendered.

use std::{fmt, process::ExitCode, sync::Arc};

use serde::Serialize;

use super::{Activations, BooksSelection, ProductError};
use crate::{
    entitlement::LedgerLicenseStatus,
    license::{LicenseError, LicenseId, MajorVersion, UnrevokedLicense},
};

pub(super) struct LicenseReport {
    pub(super) inventory: Result<LicenseInventory, ProductError>,
    pub(super) access: Option<LedgerLicenseStatus>,
}

impl LicenseReport {
    pub(super) fn inventory(self) -> Result<LicenseInventory, ProductError> {
        self.inventory
    }

    pub(super) fn write_cli(
        self,
        json: bool,
        target: &BooksSelection<'_>,
    ) -> Result<ExitCode, ProductError> {
        if json {
            let settings_url = target.settings_url();
            let response = self.response(settings_url.as_deref());
            println!("{}", serde_json::json!(response));
            return Ok(if response.is_error() {
                ExitCode::FAILURE
            } else {
                ExitCode::SUCCESS
            });
        }
        if let Some(access) = self.access {
            println!("{access}");
        }
        println!("{}", self.inventory?);
        Ok(ExitCode::SUCCESS)
    }

    pub(super) fn response<'a>(&'a self, settings_url: Option<&'a str>) -> LicenseResponse<'a> {
        let mut response = match &self.inventory {
            Ok(inventory) => LicenseResponse::from(inventory),
            Err(error) => LicenseResponse::from(error),
        };
        if let Some(access) = &self.access {
            let summary = access.to_string();
            response.message = format!("{summary}\n{}", response.message);
            response.writable = Some(access.writable());
            response.summary = Some(summary);
        }
        response.settings_url = settings_url;
        response
    }
}

#[derive(Serialize)]
pub(super) struct LicenseResponse<'a> {
    #[serde(flatten)]
    outcome: Outcome<'a>,
    message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    writable: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    summary: Option<String>,
    desktop_path: &'static str,
    settings_url: Option<&'a str>,
}

#[derive(Serialize)]
#[serde(untagged)]
enum Outcome<'a> {
    Success {
        status: Status,
        licenses: Vec<Coverage<'a>>,
    },
    Failure {
        code: LicenseErrorCode,
    },
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
enum Status {
    NotActivated,
    Active,
    Revoked,
}

#[derive(Serialize)]
struct Coverage<'a> {
    license_id: &'a LicenseId,
    eligible_major: MajorVersion,
    storefront: crate::license::Storefront,
    standing: Standing,
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
enum Standing {
    Valid,
    Revoked,
}

impl<'a> Coverage<'a> {
    fn revoked(license: &'a crate::license::VerifiedLicense) -> Self {
        Self {
            license_id: license.id(),
            eligible_major: license.eligible_major(),
            storefront: license.storefront(),
            standing: Standing::Revoked,
        }
    }
}

impl<'a> From<&'a UnrevokedLicense> for Coverage<'a> {
    fn from(license: &'a UnrevokedLicense) -> Self {
        Self {
            license_id: license.id(),
            eligible_major: license.eligible_major(),
            storefront: license.storefront(),
            standing: Standing::Valid,
        }
    }
}

impl<'a> LicenseResponse<'a> {
    fn new(outcome: Outcome<'a>, message: String) -> Self {
        Self {
            outcome,
            message,
            writable: None,
            summary: None,
            desktop_path: "/settings",
            settings_url: None,
        }
    }

    pub(super) fn is_error(&self) -> bool {
        matches!(self.outcome, Outcome::Failure { .. })
    }

    pub(super) fn message(&self) -> &str {
        &self.message
    }
}

impl<'a> From<&'a LicenseInventory> for LicenseResponse<'a> {
    fn from(inventory: &'a LicenseInventory) -> Self {
        let (status, licenses) = match inventory {
            LicenseInventory::NotActivated => (Status::NotActivated, Vec::new()),
            LicenseInventory::Revoked(licenses) => (
                Status::Revoked,
                licenses.iter().map(Coverage::revoked).collect(),
            ),
            LicenseInventory::Active {
                first,
                additional,
                revoked,
                ..
            } => (
                Status::Active,
                std::iter::once(first.as_ref())
                    .chain(additional)
                    .map(Coverage::from)
                    .chain(revoked.iter().map(Coverage::revoked))
                    .collect(),
            ),
        };
        Self::new(Outcome::Success { status, licenses }, inventory.to_string())
    }
}

impl From<&ProductError> for LicenseResponse<'_> {
    fn from(error: &ProductError) -> Self {
        let code = LicenseErrorCode::from(error);
        let message = code.message(error);
        Self::new(Outcome::Failure { code }, message)
    }
}

pub(super) enum LicenseInventory {
    NotActivated,
    Revoked(Vec<crate::license::VerifiedLicense>),
    Active {
        first: Box<UnrevokedLicense>,
        additional: Vec<UnrevokedLicense>,
        warning: Option<Arc<LicenseError>>,
        revoked: Vec<crate::license::VerifiedLicense>,
    },
}

impl From<Activations> for LicenseInventory {
    fn from(activations: Activations) -> Self {
        let mut licenses = activations.licenses.into_iter();
        match licenses.next() {
            Some(first) => Self::Active {
                first: Box::new(first),
                additional: licenses.collect(),
                warning: activations.first_failure,
                revoked: activations.revoked,
            },
            None if activations.revoked.is_empty() => Self::NotActivated,
            None => Self::Revoked(activations.revoked),
        }
    }
}

impl fmt::Display for LicenseInventory {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotActivated => {
                f.write_str("License status: not activated. No Desktop license is active.")
            }
            Self::Revoked(licenses) => {
                f.write_str("License status: revoked. No Desktop license is active.")?;
                write_revoked(f, licenses)
            }
            Self::Active {
                first,
                additional,
                warning,
                revoked,
            } => {
                f.write_str("License status: active and valid.")?;
                for license in std::iter::once(first.as_ref()).chain(additional) {
                    write!(
                        f,
                        "\nLicense {} covers all {}.x releases forever.",
                        license.id(),
                        license.eligible_major()
                    )?;
                }
                if let Some(error) = warning {
                    write!(
                        f,
                        "\nOne or more license files could not be verified ({error})."
                    )?;
                }
                write_revoked(f, revoked)
            }
        }
    }
}

fn write_revoked(
    f: &mut fmt::Formatter<'_>,
    licenses: &[crate::license::VerifiedLicense],
) -> fmt::Result {
    for license in licenses {
        write!(
            f,
            "\nLicense {} was revoked and does not allow writes.",
            license.id()
        )?;
    }
    Ok(())
}

#[derive(Clone, Copy, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub(super) enum LicenseErrorCode {
    LicenseArguments,
    LicenseInvalidClaimCode,
    LicenseRedemptionFailed,
    LicenseRecoveryFailed,
    LicenseBooksRequired,
    LicenseBoundElsewhere,
    LicenseRevoked,
    LicenseInvalid,
    LicenseUnverified,
    LicenseConfiguration,
    LicenseWrongMajor,
    LicenseUnavailable,
    #[serde(rename = "LICENSE_WORKER_FAILED")]
    WorkerFailed,
}

impl From<&ProductError> for LicenseErrorCode {
    fn from(error: &ProductError) -> Self {
        match error {
            ProductError::ActivationInput => Self::LicenseArguments,
            ProductError::InvalidClaimCode => Self::LicenseInvalidClaimCode,
            ProductError::Redemption => Self::LicenseRedemptionFailed,
            ProductError::Recovery => Self::LicenseRecoveryFailed,
            ProductError::BooksRequired => Self::LicenseBooksRequired,
            ProductError::LicenseBoundElsewhere => Self::LicenseBoundElsewhere,
            ProductError::LicenseRevoked(_) => Self::LicenseRevoked,
            ProductError::InvalidLicenseInput => Self::LicenseInvalid,
            ProductError::License(error) => Self::from(error.as_ref()),
            ProductError::ServiceUrlMissing
            | ProductError::ServiceUrlInvalid
            | ProductError::ServiceUrlInsecure
            | ProductError::EmbeddedPublicKeysInvalid
            | ProductError::EmbeddedPublicKeysNotObject
            | ProductError::EmbeddedTrustEncoding
            | ProductError::StateDirectoryMissing
            | ProductError::TrustFileTooLarge
            | ProductError::UpdateChecksDisabled
            | ProductError::BooksConfiguration => Self::LicenseConfiguration,
            ProductError::Update(_) | ProductError::Io(_) => Self::LicenseUnavailable,
            ProductError::LicenseWorkerFailed => Self::WorkerFailed,
        }
    }
}

impl From<&LicenseError> for LicenseErrorCode {
    fn from(error: &LicenseError) -> Self {
        match error {
            LicenseError::Signature | LicenseError::UnknownKey => Self::LicenseUnverified,
            LicenseError::InvalidToken
            | LicenseError::InvalidFooter
            | LicenseError::InvalidClaims
            | LicenseError::TooLarge => Self::LicenseInvalid,
            LicenseError::Unconfigured
            | LicenseError::TrustConfiguration
            | LicenseError::InvalidRevocations => Self::LicenseConfiguration,
            LicenseError::WrongMajor(_) => Self::LicenseWrongMajor,
            LicenseError::Revoked(_) => Self::LicenseRevoked,
            LicenseError::Io(_) => Self::LicenseUnavailable,
        }
    }
}

impl LicenseErrorCode {
    fn message(self, error: &ProductError) -> String {
        match self {
            Self::LicenseUnverified => "Desktop could not verify that license. Use the original license from your purchase or grant.".into(),
            Self::LicenseInvalid => "Desktop could not read that license. Use the original license from your purchase or grant.".into(),
            Self::LicenseConfiguration => "This installation is missing a working license configuration.".into(),
            Self::LicenseUnavailable => "Desktop could not read or save the license information on this computer.".into(),
            _ => error.to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serializes_named_license_errors() {
        for (error, expected) in [
            (ProductError::ActivationInput, "LICENSE_ARGUMENTS"),
            (ProductError::InvalidClaimCode, "LICENSE_INVALID_CLAIM_CODE"),
            (ProductError::Redemption, "LICENSE_REDEMPTION_FAILED"),
            (ProductError::Recovery, "LICENSE_RECOVERY_FAILED"),
            (ProductError::BooksRequired, "LICENSE_BOOKS_REQUIRED"),
            (
                ProductError::LicenseBoundElsewhere,
                "LICENSE_BOUND_ELSEWHERE",
            ),
            (ProductError::ServiceUrlMissing, "LICENSE_CONFIGURATION"),
            (ProductError::ServiceUrlInvalid, "LICENSE_CONFIGURATION"),
            (ProductError::ServiceUrlInsecure, "LICENSE_CONFIGURATION"),
            (ProductError::InvalidLicenseInput, "LICENSE_INVALID"),
            (LicenseError::Signature.into(), "LICENSE_UNVERIFIED"),
            (ProductError::LicenseWorkerFailed, "LICENSE_WORKER_FAILED"),
        ] {
            let response = LicenseResponse::from(&error);
            assert!(response.is_error());
            assert_eq!(error.structured()["code"], expected);
        }
    }
}
