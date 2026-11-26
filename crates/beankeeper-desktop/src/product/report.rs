//! Shared license status and activation output, with only public claims rendered.

use std::{fmt, process::ExitCode, sync::Arc};

use serde::Serialize;

use super::{Activations, BooksSelection, ProductError};
use crate::{
    entitlement::LedgerLicenseStatus,
    license::{LicenseError, LicenseId, UnrevokedLicense},
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
        code: &'static str,
    },
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
enum Status {
    NotActivated,
    Active,
}

#[derive(Serialize)]
struct Coverage<'a> {
    license_id: &'a LicenseId,
    eligible_major: u32,
}

impl<'a> From<&'a UnrevokedLicense> for Coverage<'a> {
    fn from(license: &'a UnrevokedLicense) -> Self {
        Self {
            license_id: license.id(),
            eligible_major: license.eligible_major().into(),
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
            LicenseInventory::Active {
                first, additional, ..
            } => (
                Status::Active,
                std::iter::once(first.as_ref())
                    .chain(additional)
                    .map(Coverage::from)
                    .collect(),
            ),
        };
        Self::new(Outcome::Success { status, licenses }, inventory.to_string())
    }
}

impl From<&ProductError> for LicenseResponse<'_> {
    fn from(error: &ProductError) -> Self {
        let (code, message) = error_details(error);
        Self::new(Outcome::Failure { code }, message)
    }
}

pub(super) enum LicenseInventory {
    NotActivated,
    Active {
        first: Box<UnrevokedLicense>,
        additional: Vec<UnrevokedLicense>,
        warning: Option<Arc<LicenseError>>,
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
            },
            None => Self::NotActivated,
        }
    }
}

impl fmt::Display for LicenseInventory {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotActivated => {
                f.write_str("License status: not activated. No Desktop license is active.")
            }
            Self::Active {
                first,
                additional,
                warning,
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
                Ok(())
            }
        }
    }
}

fn error_details(error: &ProductError) -> (&'static str, String) {
    match error {
        ProductError::ActivationInput => ("LICENSE_ARGUMENTS", error.to_string()),
        ProductError::InvalidClaimCode => ("LICENSE_INVALID_CLAIM_CODE", error.to_string()),
        ProductError::Redemption => ("LICENSE_REDEMPTION_FAILED", error.to_string()),
        ProductError::Recovery => ("LICENSE_RECOVERY_FAILED", error.to_string()),
        ProductError::BooksRequired => ("LICENSE_BOOKS_REQUIRED", error.to_string()),
        ProductError::LicenseBoundElsewhere => ("LICENSE_BOUND_ELSEWHERE", error.to_string()),
        ProductError::LicenseRevoked(_) => ("LICENSE_REVOKED", error.to_string()),
        ProductError::InvalidLicenseInput => invalid_license(),
        ProductError::License(error) => license_error_details(error),
        ProductError::Configuration(_) | ProductError::BooksConfiguration => configuration_error(),
        _ => unavailable(),
    }
}

fn license_error_details(error: &LicenseError) -> (&'static str, String) {
    match error {
        LicenseError::Signature | LicenseError::UnknownKey => (
            "LICENSE_UNVERIFIED",
            "Desktop could not verify that license. Use the original license from your purchase or grant.".into(),
        ),
        LicenseError::InvalidToken | LicenseError::InvalidFooter | LicenseError::InvalidClaims
        | LicenseError::TooLarge => invalid_license(),
        LicenseError::Unconfigured | LicenseError::TrustConfiguration | LicenseError::InvalidRevocations => {
            configuration_error()
        }
        LicenseError::WrongMajor(_) => ("LICENSE_WRONG_MAJOR", error.to_string()),
        LicenseError::Revoked(_) => ("LICENSE_REVOKED", error.to_string()),
        LicenseError::Io(_) => unavailable(),
    }
}

fn invalid_license() -> (&'static str, String) {
    (
        "LICENSE_INVALID",
        "Desktop could not read that license. Use the original license from your purchase or grant.".into(),
    )
}

fn configuration_error() -> (&'static str, String) {
    (
        "LICENSE_CONFIGURATION",
        "This installation is missing a working license configuration.".into(),
    )
}

fn unavailable() -> (&'static str, String) {
    (
        "LICENSE_UNAVAILABLE",
        "Desktop could not read or save the license information on this computer.".into(),
    )
}
