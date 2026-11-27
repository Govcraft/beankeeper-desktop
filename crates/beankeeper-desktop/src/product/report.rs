//! Shared license status and activation output, with only public claims rendered.

use super::RetryAfter;
use secrecy::ExposeSecret;
use std::{fmt, io, path::Path, process::ExitCode, sync::Arc};

use serde::{Deserialize, Serialize};

use super::{Activations, BooksSelection, ProductError};
use crate::{
    entitlement::LedgerLicenseStatus,
    license::{LicenseError, LicenseId, MajorVersion, UnrevokedLicense},
};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(super) struct TrialNotice {
    code: LicenseErrorCode,
    retry_after_seconds: Option<RetryAfter>,
}
impl From<&ProductError> for TrialNotice {
    fn from(error: &ProductError) -> Self {
        Self {
            code: error.into(),
            retry_after_seconds: match error {
                ProductError::TrialRateLimited(retry) => *retry,
                _ => None,
            },
        }
    }
}

impl TrialNotice {
    const PATH: &str = "trial-notice.json";
    pub(super) fn load(state: &Path) -> Result<Option<Self>, ProductError> {
        let file = match crate::private_file::PrivateFile::open(&state.join(Self::PATH)) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let bytes = file.read_bytes(4096)?;
        serde_json::from_slice(bytes.expose_secret())
            .map(Some)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error).into())
    }
    pub(super) fn save(&self, state: &Path) -> Result<(), ProductError> {
        let bytes = serde_json::to_vec(self).map_err(io::Error::other)?;
        crate::license::write_private(
            &bytes,
            &state.join(Self::PATH),
            crate::license::SaveMode::Replace,
        )?;
        Ok(())
    }
    pub(super) fn clear(state: &Path) -> Result<(), ProductError> {
        crate::private_file::remove_file(&state.join(Self::PATH))?;
        Ok(())
    }
}
impl fmt::Display for TrialNotice {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.code {
            LicenseErrorCode::TrialRateLimited => {
                ProductError::TrialRateLimited(self.retry_after_seconds).fmt(f)
            }
            code => code.fmt(f),
        }
    }
}

pub(super) struct LicenseReport {
    pub(super) inventory: Result<LicenseInventory, ProductError>,
    pub(super) access: Option<LedgerLicenseStatus>,
    pub(super) trial_notice: Option<TrialNotice>,
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
        if let Some(notice) = self.trial_notice {
            println!("{notice}");
        }
        Ok(ExitCode::SUCCESS)
    }

    pub(super) fn response(&self, settings_url: Option<&str>) -> LicenseResponse {
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
        if let Some(notice) = &self.trial_notice {
            response.message = format!("{}\n{notice}", response.message);
            response.trial_notice = Some(notice.into());
        }
        response.settings_url = settings_url.map(str::to_owned);
        response
    }
}

#[derive(Debug, Serialize)]
/// Owned, secret-free license status shared by CLI, MCP and service status.
pub struct LicenseResponse {
    #[serde(flatten)]
    outcome: Outcome,
    message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    writable: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    summary: Option<String>,
    desktop_path: &'static str,
    settings_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    retry_after_seconds: Option<RetryAfter>,
    #[serde(skip_serializing_if = "Option::is_none")]
    trial_notice: Option<TrialNoticeResponse>,
}

#[derive(Debug, Serialize)]
struct TrialNoticeResponse {
    #[serde(flatten)]
    notice: TrialNotice,
    message: String,
}
impl From<&TrialNotice> for TrialNoticeResponse {
    fn from(notice: &TrialNotice) -> Self {
        Self {
            notice: notice.clone(),
            message: notice.to_string(),
        }
    }
}

#[derive(Debug, Serialize)]
#[serde(untagged)]
enum Outcome {
    Success {
        status: Status,
        kind: LicenseKind,
        licenses: Vec<Coverage>,
    },
    Failure {
        code: LicenseErrorCode,
    },
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "snake_case")]
enum Status {
    NotActivated,
    Expired,
    Active,
    Revoked,
}

#[derive(Debug, Serialize)]
struct Coverage {
    license_id: LicenseId,
    eligible_major: MajorVersion,
    storefront: crate::license::Storefront,
    standing: Standing,
    kind: LicenseKind,
    expires_at: Option<crate::license::LicenseTime>,
    days_remaining: Option<crate::license::DaysRemaining>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "snake_case")]
enum Standing {
    Expired,
    Valid,
    Revoked,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "lowercase")]
enum LicenseKind {
    Paid,
    Trial,
    None,
}
impl From<crate::license::Entitlement> for LicenseKind {
    fn from(term: crate::license::Entitlement) -> Self {
        match term {
            crate::license::Entitlement::Perpetual { .. } => Self::Paid,
            crate::license::Entitlement::Trial { .. } => Self::Trial,
        }
    }
}
impl Coverage {
    fn revoked(
        license: &crate::license::VerifiedLicense,
        now: chrono::DateTime<chrono::Utc>,
    ) -> Self {
        Self {
            license_id: license.id().clone(),
            eligible_major: license.eligible_major(),
            storefront: license.storefront(),
            standing: Standing::Revoked,
            kind: license.entitlement().into(),
            expires_at: license.entitlement().expires_at(),
            days_remaining: license.entitlement().days_remaining(now),
        }
    }
}

impl Coverage {
    fn valid(license: &UnrevokedLicense, now: chrono::DateTime<chrono::Utc>) -> Self {
        Self {
            license_id: license.id().clone(),
            eligible_major: license.eligible_major(),
            storefront: license.storefront(),
            standing: if license.entitlement().is_expired(now) {
                Standing::Expired
            } else {
                Standing::Valid
            },
            kind: license.entitlement().into(),
            expires_at: license.entitlement().expires_at(),
            days_remaining: license.entitlement().days_remaining(now),
        }
    }
}

impl LicenseResponse {
    fn new(outcome: Outcome, message: String) -> Self {
        Self {
            outcome,
            message,
            writable: None,
            summary: None,
            desktop_path: "/settings",
            settings_url: None,
            retry_after_seconds: None,
            trial_notice: None,
        }
    }

    pub(super) fn is_error(&self) -> bool {
        matches!(self.outcome, Outcome::Failure { .. })
    }

    pub(super) fn message(&self) -> &str {
        &self.message
    }
}

impl From<&LicenseInventory> for LicenseResponse {
    fn from(inventory: &LicenseInventory) -> Self {
        Self::at(inventory, chrono::Utc::now())
    }
}
impl LicenseResponse {
    fn at(inventory: &LicenseInventory, now: chrono::DateTime<chrono::Utc>) -> Self {
        let (status, licenses) = match inventory {
            LicenseInventory::NotActivated => (Status::NotActivated, Vec::new()),
            LicenseInventory::Revoked(licenses) => (
                Status::Revoked,
                licenses
                    .iter()
                    .map(|license| Coverage::revoked(license, now))
                    .collect(),
            ),
            LicenseInventory::Active {
                first,
                additional,
                revoked,
                ..
            } => (
                if std::iter::once(first.as_ref())
                    .chain(additional)
                    .all(|license| license.entitlement().is_expired(now))
                {
                    Status::Expired
                } else {
                    Status::Active
                },
                std::iter::once(first.as_ref())
                    .chain(additional)
                    .map(|license| Coverage::valid(license, now))
                    .chain(
                        revoked
                            .iter()
                            .map(|license| Coverage::revoked(license, now)),
                    )
                    .collect(),
            ),
        };
        let kind = match inventory {
            LicenseInventory::NotActivated => LicenseKind::None,
            LicenseInventory::Revoked(licenses) => licenses
                .first()
                .map_or(LicenseKind::None, |license| license.entitlement().into()),
            LicenseInventory::Active { first, .. } => first.entitlement().into(),
        };
        Self::new(
            Outcome::Success {
                status,
                kind,
                licenses,
            },
            InventoryDisplay { inventory, now }.to_string(),
        )
    }
}

impl From<&ProductError> for LicenseResponse {
    fn from(error: &ProductError) -> Self {
        let code = LicenseErrorCode::from(error);
        let message = code.message(error);
        let mut response = Self::new(Outcome::Failure { code }, message);
        if let ProductError::TrialRateLimited(retry) = error {
            response.retry_after_seconds = *retry;
        }
        response
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
        InventoryDisplay {
            inventory: self,
            now: chrono::Utc::now(),
        }
        .fmt(f)
    }
}
struct InventoryDisplay<'a> {
    inventory: &'a LicenseInventory,
    now: chrono::DateTime<chrono::Utc>,
}
impl fmt::Display for InventoryDisplay<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let now = self.now;
        match self.inventory {
            LicenseInventory::NotActivated => {
                f.write_str("License status: not activated. No Desktop license is active.")
            }
            LicenseInventory::Revoked(licenses) => {
                f.write_str("License status: revoked. No Desktop license is active.")?;
                write_revoked(f, licenses)
            }
            LicenseInventory::Active {
                first,
                additional,
                warning,
                revoked,
            } => {
                if std::iter::once(first.as_ref())
                    .chain(additional)
                    .all(|license| license.entitlement().is_expired(now))
                {
                    f.write_str("License status: trial expired.")?;
                } else {
                    f.write_str("License status: active and valid.")?;
                }
                for license in std::iter::once(first.as_ref()).chain(additional) {
                    if let Some(expiry) = license.entitlement().expires_at() {
                        write!(
                            f,
                            "\nTrial {} expires {expiry}; {} days remaining.",
                            license.id(),
                            license
                                .entitlement()
                                .days_remaining(now)
                                .ok_or(fmt::Error)?
                        )?;
                        if license.entitlement().is_expired(now) {
                            write!(f, "\n{}", LicenseError::TrialExpired)?;
                        }
                    } else {
                        write!(
                            f,
                            "\nLicense {} covers all {}.x releases forever.",
                            license.id(),
                            license.eligible_major()
                        )?;
                    }
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

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub(super) enum LicenseErrorCode {
    LicenseArguments,
    TrialAlreadyUsed,
    TrialRateLimited,
    TrialUnavailable,
    TrialExpired,
    LicenseInvalidClaimCode,
    LicenseRedemptionFailed,
    LicenseRecoveryFailed,
    LicenseMachineIdentity,
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

impl fmt::Display for LicenseErrorCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TrialAlreadyUsed => ProductError::TrialAlreadyUsed.fmt(f),
            Self::TrialRateLimited => ProductError::TrialRateLimited(None).fmt(f),
            Self::TrialUnavailable => ProductError::TrialUnavailable.fmt(f),
            Self::TrialExpired => LicenseError::TrialExpired.fmt(f),
            Self::LicenseArguments => ProductError::ActivationInput.fmt(f),
            Self::LicenseInvalidClaimCode => ProductError::InvalidClaimCode.fmt(f),
            Self::LicenseRedemptionFailed => ProductError::Redemption.fmt(f),
            Self::LicenseRecoveryFailed => ProductError::Recovery.fmt(f),
            Self::LicenseMachineIdentity => ProductError::MachineIdentity.fmt(f),
            Self::LicenseBooksRequired => ProductError::BooksRequired.fmt(f),
            Self::LicenseBoundElsewhere => ProductError::LicenseBoundElsewhere.fmt(f),
            Self::LicenseRevoked => f.write_str("This Desktop license was revoked and does not allow writes."),
            Self::LicenseWrongMajor => f.write_str("This Desktop license does not cover the current release."),
            Self::LicenseInvalid => f.write_str("Desktop could not read that license. Use the original license from your purchase or grant."),
            Self::LicenseUnverified => f.write_str("Desktop could not verify that license. Use the original license from your purchase or grant."),
            Self::LicenseConfiguration => f.write_str("This installation is missing a working license configuration."),
            Self::LicenseUnavailable => f.write_str("Desktop could not read or save the license information on this computer."),
            Self::WorkerFailed => ProductError::LicenseWorkerFailed.fmt(f),
        }
    }
}

impl From<&ProductError> for LicenseErrorCode {
    fn from(error: &ProductError) -> Self {
        match error {
            ProductError::TrialAlreadyUsed => Self::TrialAlreadyUsed,
            ProductError::TrialRateLimited(_) => Self::TrialRateLimited,
            ProductError::TrialUnavailable => Self::TrialUnavailable,
            ProductError::ActivationInput => Self::LicenseArguments,
            ProductError::InvalidClaimCode => Self::LicenseInvalidClaimCode,
            ProductError::Redemption => Self::LicenseRedemptionFailed,
            ProductError::Recovery => Self::LicenseRecoveryFailed,
            ProductError::MachineIdentity => Self::LicenseMachineIdentity,
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
            LicenseError::TrialExpired => Self::TrialExpired,
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
            Self::LicenseUnverified
            | Self::LicenseInvalid
            | Self::LicenseConfiguration
            | Self::LicenseUnavailable => self.to_string(),
            _ => error.to_string(),
        }
    }
}

impl fmt::Display for LicenseResponse {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notice_store_derives_messages_and_reports_failures() -> Result<(), Box<dyn std::error::Error>>
    {
        let state = tempfile::tempdir()?;
        assert!(TrialNotice::load(state.path())?.is_none());
        for error in [
            ProductError::TrialRateLimited(Some("60".parse()?)),
            ProductError::TrialAlreadyUsed,
            ProductError::TrialUnavailable,
            ProductError::LicenseBoundElsewhere,
            ProductError::MachineIdentity,
        ] {
            let notice = TrialNotice::from(&error);
            notice.save(state.path())?;
            let loaded = TrialNotice::load(state.path())?.ok_or("missing notice")?;
            assert_eq!(loaded.to_string(), error.to_string());
            let stored: serde_json::Value =
                serde_json::from_slice(&std::fs::read(state.path().join(TrialNotice::PATH))?)?;
            assert!(stored.get("message").is_none());
            let response = serde_json::to_value(TrialNoticeResponse::from(&loaded))?;
            assert_eq!(response["message"], error.to_string());
        }
        let configuration = TrialNotice::from(&ProductError::ServiceUrlMissing);
        assert_eq!(
            configuration.to_string(),
            "This installation is missing a working license configuration."
        );
        let path = state.path().join(TrialNotice::PATH);
        std::fs::write(
            &path,
            br#"{"code":"TRIAL_RATE_LIMITED","retry_after_seconds":60,"message":"stale message"}"#,
        )?;
        assert_eq!(
            TrialNotice::load(state.path())?
                .ok_or("missing notice")?
                .to_string(),
            ProductError::TrialRateLimited(Some("60".parse()?)).to_string()
        );
        std::fs::write(
            &path,
            br#"{"code":"TRIAL_RATE_LIMITED","retry_after_seconds":0}"#,
        )?;
        assert!(TrialNotice::load(state.path()).is_err());
        TrialNotice::clear(state.path())?;
        TrialNotice::clear(state.path())?;
        assert!(TrialNotice::load(state.path())?.is_none());
        std::fs::create_dir(state.path().join(TrialNotice::PATH))?;
        assert!(TrialNotice::load(state.path()).is_err());
        assert!(
            TrialNotice::from(&ProductError::TrialUnavailable)
                .save(state.path())
                .is_err()
        );
        assert!(TrialNotice::clear(state.path()).is_err());
        Ok(())
    }

    #[test]
    fn serializes_named_license_errors() {
        for (error, expected) in [
            (ProductError::ActivationInput, "LICENSE_ARGUMENTS"),
            (ProductError::InvalidClaimCode, "LICENSE_INVALID_CLAIM_CODE"),
            (ProductError::Redemption, "LICENSE_REDEMPTION_FAILED"),
            (ProductError::Recovery, "LICENSE_RECOVERY_FAILED"),
            (ProductError::MachineIdentity, "LICENSE_MACHINE_IDENTITY"),
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
