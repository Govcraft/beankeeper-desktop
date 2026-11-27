//! CLI, configuration and local status adapters for isolated licensing/updates.
use crate::{
    entitlement::{
        Bindings, CheckedActivation, ENFORCEMENT, Entitlement, LedgerAccess, LedgerLicenseStatus,
        OFFICIAL_BUILD,
    },
    instance::BooksPath,
    license::{self, ActiveLicense, LicenseError, LicenseId, TrustedKeys, UnrevokedLicense},
    release_policy::EnforcementPolicy,
    updates::{self, UpdateError},
};
use clap::{Args, Subcommand};
use std::{
    borrow::Cow,
    error::Error,
    fmt, fs,
    io::{self, Read},
    path::{Path, PathBuf},
    process::ExitCode,
    sync::Arc,
};

mod activation;
pub(crate) mod mcp;
pub(crate) mod removal;
mod report;
mod retry_after;
pub use report::LicenseResponse;
pub use retry_after::RetryAfter;
pub(crate) mod returns;

pub use activation::Storefront;
use activation::{ActivationInputs, ActivationSource};
use report::{LicenseInventory, LicenseReport, TrialNotice, TrialNoticeStore};

/// Product command errors, with secret-bearing service diagnostics discarded.
#[derive(Debug)]
pub enum ProductError {
    License(Arc<LicenseError>),
    Update(UpdateError),
    Io(io::Error),
    ServiceUrlMissing,
    ServiceUrlInvalid,
    ServiceUrlInsecure,
    EmbeddedPublicKeysInvalid,
    EmbeddedPublicKeysNotObject,
    EmbeddedTrustEncoding,
    StateDirectoryMissing,
    TrustFileTooLarge,
    UpdateChecksDisabled,
    LicenseWorkerFailed,
    LicenseBoundElsewhere,
    LicenseRevoked(LicenseId),
    BooksRequired,
    BooksConfiguration,
    Redemption,
    ActivationInput,
    InvalidLicenseInput,
    InvalidClaimCode,
    Recovery,
    TrialAlreadyUsed,
    TrialRateLimited(Option<RetryAfter>),
    TrialUnavailable,
    MachineIdentity,
}
impl fmt::Display for ProductError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TrialAlreadyUsed => write!(f, "This computer's trial has ended. Your books remain readable and exportable. Buy a $79 license at https://beankeeper.app/#pricing and ask your agent to activate it."),
            Self::TrialRateLimited(retry) => {
                f.write_str("The trial service is busy. Retry later")?;
                if let Some(retry) = retry { write!(f, " {retry}")?; }
                f.write_str(".")
            },
            Self::TrialUnavailable => f.write_str("Desktop could not reach the trial service. Your books can still be created, read and exported. Ask your agent to retry later; ordinary writes wait for a license."),
            Self::License(e) => write!(f, "{e}"),
            Self::Update(e) => write!(f, "{e}"),
            Self::Io(_) => f.write_str("cannot access Desktop license configuration"),
            Self::ServiceUrlMissing => f.write_str(
                "BEANKEEPER_LICENSE_SERVICE_URL is not configured for this installation.",
            ),
            Self::ServiceUrlInvalid => f.write_str("The configured license service URL is invalid."),
            Self::ServiceUrlInsecure => f.write_str(
                "The license service must use HTTPS or be on this computer.",
            ),
            Self::EmbeddedPublicKeysInvalid => f.write_str("embedded public keys are not valid JSON"),
            Self::EmbeddedPublicKeysNotObject => f.write_str("embedded public keys must be a JSON object"),
            Self::EmbeddedTrustEncoding => f.write_str("cannot encode embedded license trust"),
            Self::StateDirectoryMissing => f.write_str(
                "set BEANKEEPER_DESKTOP_STATE_DIR or configure a home directory",
            ),
            Self::TrustFileTooLarge => f.write_str("license input exceeds size limit"),
            Self::UpdateChecksDisabled => f.write_str(
                "update checks are disabled; unset BEANKEEPER_NO_UPDATE_CHECK first",
            ),
            Self::LicenseWorkerFailed => f.write_str("Desktop could not complete the license request."),
            Self::LicenseBoundElsewhere => f.write_str(
                "This license is already bound to another set of books. Choose the original books or use a different license.",
            ),
            Self::LicenseRevoked(id) => write!(
                f,
                "Desktop license {id} was revoked. Use another license for these books."
            ),
            Self::BooksRequired => f.write_str(
                "Desktop needs an existing set of books to attach this license to. Set up your books and try again.",
            ),
            Self::BooksConfiguration => f.write_str("Desktop could not find the selected books. Check the local Desktop setup."),
            Self::Redemption => {
                f.write_str("The license service could not redeem that code. Check the code or try again later.")
            }
            Self::ActivationInput => f.write_str("Provide exactly one signed license, saved license file, or claim code."),
            Self::InvalidLicenseInput => f.write_str("Desktop could not read that license. Use the original license or file you received."),
            Self::InvalidClaimCode => f.write_str("That claim code is incomplete or has an invalid format."),
            Self::MachineIdentity => f.write_str("Desktop could not save or read its private trial machine identity."),
            Self::Recovery => f.write_str("Desktop could not save or read its private license recovery information."),
        }
    }
}
impl ProductError {
    /// A named, secret-free error for machine-readable CLI output.
    #[must_use]
    pub fn structured(&self) -> serde_json::Value {
        serde_json::json!(report::LicenseResponse::from(self))
    }
}

impl Error for ProductError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::License(e) => Some(e),
            Self::Update(e) => Some(e),
            Self::Io(e) => Some(e),
            _ => None,
        }
    }
}
impl From<LicenseError> for ProductError {
    fn from(e: LicenseError) -> Self {
        match e {
            LicenseError::Revoked(id) => Self::LicenseRevoked(id),
            error => Self::License(Arc::new(error)),
        }
    }
}
impl From<UpdateError> for ProductError {
    fn from(e: UpdateError) -> Self {
        Self::Update(e)
    }
}
impl From<io::Error> for ProductError {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

/// Optional license activation subcommand; no subcommand displays local status.
#[derive(Args)]
pub struct LicenseOptions {
    #[command(subcommand)]
    pub command: Option<LicenseCommand>,
}
/// Local license commands, including offline activation and online redemption.
#[derive(Subcommand)]
pub enum LicenseCommand {
    /// Start this computer's email-free 30-day trial.
    Trial,
    /// Show local license status.
    Status,
    /// Remove a license from this computer without a return or refund.
    Remove {
        /// Installed license to remove.
        license_id: LicenseId,
    },
    /// Request a license return and refund without changing your books.
    Return {
        /// Select an installed license when more than one is present.
        #[arg(long)]
        license_id: Option<LicenseId>,
        /// Confirm the return and refund after reading the return policy.
        #[arg(long)]
        yes: bool,
    },
    /// Verify and store a license, or redeem a storefront key with a configured service.
    Activate {
        /// Signed license file. Offline activation makes no network request.
        file: Option<PathBuf>,
        /// Read a pasted signed license from standard input.
        #[arg(long)]
        token_stdin: bool,
        /// Private text file containing a storefront key or single-use claim code.
        #[arg(long)]
        code_file: Option<PathBuf>,
        /// Storefront for --code-file. Stripe checkout codes and Etsy claim codes are inferred.
        #[arg(long, ignore_case = true)]
        storefront: Option<Storefront>,
    },
    /// Show the license service URL and issuer keys compiled into this binary.
    Trust {
        /// Print a JSON object with `service_url` and `public_keys`.
        #[arg(long)]
        json: bool,
    },
}

/// Compile-time license trust, ignoring runtime environment overrides.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EmbeddedLicenseTrust {
    /// Origin compiled into the binary, if the release build provided one.
    pub service_url: Option<&'static str>,
    /// JSON object of key id to public key; "{}" when none were compiled in.
    pub public_keys_json: &'static str,
}

/// Values forwarded by build.rs via cargo:rustc-env, not process environment.
#[must_use]
pub fn embedded_license_trust() -> EmbeddedLicenseTrust {
    EmbeddedLicenseTrust {
        service_url: option_env!("BEANKEEPER_LICENSE_SERVICE_URL").filter(|url| !url.is_empty()),
        public_keys_json: option_env!("BEANKEEPER_LICENSE_PUBLIC_KEYS_JSON").unwrap_or("{}"),
    }
}

fn embedded_public_keys() -> Result<serde_json::Map<String, serde_json::Value>, ProductError> {
    let value = serde_json::from_str(embedded_license_trust().public_keys_json)
        .map_err(|_| ProductError::EmbeddedPublicKeysInvalid)?;
    match value {
        serde_json::Value::Object(map) => Ok(map),
        _ => Err(ProductError::EmbeddedPublicKeysNotObject),
    }
}

fn write_embedded_trust(as_json: bool) -> Result<(), ProductError> {
    let trust = embedded_license_trust();
    let keys = embedded_public_keys()?;
    if as_json {
        let body = serde_json::json!({
            "service_url": trust.service_url,
            "public_keys": keys,
        });
        println!(
            "{}",
            serde_json::to_string(&body).map_err(|_| ProductError::EmbeddedTrustEncoding)?
        );
        return Ok(());
    }
    match trust.service_url {
        Some(url) => println!("Embedded license service: {url}"),
        None => println!("No license service URL was compiled into this binary."),
    }
    if keys.is_empty() {
        println!("No issuer public keys were compiled into this binary.");
    } else {
        println!("Embedded issuer public keys:");
        for (key_id, value) in &keys {
            match value.as_str() {
                Some(key) => println!("  {key_id}: {key}"),
                None => println!("  {key_id}: {value}"),
            }
        }
    }
    Ok(())
}

/// Per-user Desktop state, independent of any ledger, with an explicit test override.
///
/// # Errors
/// Returns an error when the platform state directory is unknown.
pub fn state_dir() -> Result<PathBuf, ProductError> {
    if let Some(path) = std::env::var_os("BEANKEEPER_DESKTOP_STATE_DIR") {
        return Ok(PathBuf::from(path));
    }
    #[cfg(windows)]
    let base = std::env::var_os("LOCALAPPDATA").map(PathBuf::from);
    #[cfg(target_os = "macos")]
    let base = std::env::var_os("HOME")
        .map(|home| PathBuf::from(home).join("Library/Application Support"));
    #[cfg(not(any(windows, target_os = "macos")))]
    let base = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/state")));
    base.map(|path| path.join("beankeeper-desktop"))
        .ok_or(ProductError::StateDirectoryMissing)
}
/// Issuer keys come from a provisioned build trust map or an explicit local trust file.
///
/// # Errors
/// Returns errors for unreadable, incomplete, or invalid trust maps.
pub fn trusted_keys() -> Result<TrustedKeys, ProductError> {
    let bytes =
        if !OFFICIAL_BUILD && let Some(path) = std::env::var_os("BEANKEEPER_LICENSE_KEYS_FILE") {
            bounded_file(&PathBuf::from(path), 32_768)?
        } else {
            option_env!("BEANKEEPER_LICENSE_PUBLIC_KEYS_JSON")
                .unwrap_or("{}")
                .as_bytes()
                .to_vec()
        };
    Ok(TrustedKeys::from_json(&bytes)?)
}
fn bounded_file(path: &Path, limit: u64) -> Result<Vec<u8>, ProductError> {
    let mut bytes = Vec::new();
    fs::File::open(path)?
        .take(limit + 1)
        .read_to_end(&mut bytes)?;
    if u64::try_from(bytes.len()).map_or(true, |len| len > limit) {
        return Err(ProductError::TrustFileTooLarge);
    }
    Ok(bytes)
}
/// A missing activation is a normal status; verification still happens offline.
///
/// # Errors
/// Returns storage or verification errors for existing activations.
pub fn license_status() -> Result<String, ProductError> {
    Ok(LicenseRequest::Status
        .execute(None)?
        .inventory()?
        .to_string())
}

enum LicenseRequest {
    Trial,
    Status,
    Activate(ActivationSource),
}

impl LicenseRequest {
    fn run(self, target: &BooksSelection<'_>) -> Result<LicenseReport, ProductError> {
        let books = target.resolve(&self)?;
        self.execute(books.as_ref())
    }

    fn execute(self, books: Option<&BooksPath>) -> Result<LicenseReport, ProductError> {
        self.execute_at(books, &state_dir()?)
    }

    fn execute_at(
        self,
        books: Option<&BooksPath>,
        state: &Path,
    ) -> Result<LicenseReport, ProductError> {
        match self {
            Self::Status => {
                let snapshot = ActivationSnapshot::load(state);
                let access = books.map(|books| {
                    let access = LedgerAccess::from(books.clone());
                    access.status_from(state, |id| match &snapshot {
                        Ok(snapshot) => snapshot.for_id(state, id),
                        Err(ProductError::License(error)) => Err(Arc::clone(error)),
                        Err(_) => Err(Arc::new(LicenseError::Io(io::Error::other(
                            "Desktop activation storage is unavailable",
                        )))),
                    })
                });
                let inventory = snapshot.and_then(|snapshot| snapshot.installed().map(Into::into));
                let trial_notice = books
                    .map(|books| TrialNoticeStore::from((state, books)).load())
                    .transpose()?
                    .flatten();
                Ok(LicenseReport {
                    inventory,
                    access,
                    trial_notice,
                })
            }
            Self::Trial => {
                let snapshot = ActivationSnapshot::load(state)?;
                let installed = match snapshot.installed() {
                    Ok(installed) => installed,
                    Err(ProductError::License(_)) => Activations::default(),
                    Err(error) => return Err(error),
                };
                for license in installed.licenses {
                    let major = ENFORCEMENT.activation_major(&license);
                    if license.entitlement().is_expired(chrono::Utc::now())
                        || license.eligible_major() != major
                    {
                        continue;
                    }
                    if let Some(books) = books
                        && !Bindings(state).allows(license.id(), books)?
                    {
                        if matches!(license.entitlement(), license::Entitlement::Trial { .. }) {
                            return Err(ProductError::LicenseBoundElsewhere);
                        }
                        continue;
                    }
                    let active = ActiveLicense::try_from((license, major))?;
                    if let Some(books) = books {
                        Bindings(state).bind(&active, books)?;
                        TrialNoticeStore::from((state, books)).clear()?;
                    }
                    return Self::Status.execute_at(books, state);
                }
                Self::Activate(ActivationSource::trial()).execute_at(books, state)
            }
            Self::Activate(source) => {
                // Establish trust and the target before spending a potentially single-use claim.
                let keys = trusted_keys()?;
                let revocations = license::release_revocations(&keys)?;
                let verified = source.verify(&keys, state)?;
                let license = revocations.admit(verified)?;
                let major = ENFORCEMENT.activation_major(&license);
                let active = ActiveLicense::try_from((license, major))?;
                if let Some(books) = books {
                    Bindings(state).bind(&active, books)?;
                }
                active.persist(state)?;
                if let Some(books) = books {
                    TrialNoticeStore::from((state, books)).clear()?;
                }
                if matches!(
                    active.as_ref().entitlement(),
                    license::Entitlement::Perpetual { .. }
                ) {
                    remove_trials(state, &keys, active.id())?;
                }
                let inventory = LicenseInventory::Active {
                    first: Box::new(active.as_ref().clone()),
                    additional: Vec::new(),
                    warning: None,
                    revoked: Vec::new(),
                };
                let access = books.map(|_| match ENFORCEMENT {
                    EnforcementPolicy::Free => LedgerLicenseStatus::Free,
                    EnforcementPolicy::Enforced(_) => {
                        LedgerLicenseStatus::Enforced(Entitlement::Active(active))
                    }
                });
                Ok(LicenseReport {
                    inventory: Ok(inventory),
                    access,
                    trial_notice: None,
                })
            }
        }
    }
}

#[derive(Default)]
struct Activations {
    licenses: Vec<UnrevokedLicense>,
    first_failure: Option<Arc<LicenseError>>,
    revoked: Vec<license::VerifiedLicense>,
}

/// One set of disk reads for installation coverage and the selected books' write access.
type CheckedStanding = Result<Option<license::LicenseStanding>, Arc<LicenseError>>;

#[derive(Default)]
struct ActivationSnapshot(Vec<(PathBuf, CheckedStanding)>);

impl ActivationSnapshot {
    fn load(state: &Path) -> Result<Self, ProductError> {
        let paths = license::activation_paths(state)?;
        if paths.is_empty() {
            return Ok(Self::default());
        }
        let keys = trusted_keys()?;
        let revocations = license::release_revocations(&keys)?;
        let checked = paths
            .into_iter()
            .map(|path| {
                let result = license::load(&path, &keys)
                    .map(|license| license.map(|license| revocations.standing(license)))
                    .map_err(Arc::new);
                (path, result)
            })
            .collect();
        Ok(Self(checked))
    }

    fn for_id(&self, state: &Path, id: &LicenseId) -> CheckedActivation {
        let path = license::activation_path(state, id);
        self.0
            .iter()
            .find(|(stored, _)| *stored == path)
            .map_or(Ok(None), |(_, activation)| {
                activation.clone().and_then(|license| {
                    license
                        .map(UnrevokedLicense::try_from)
                        .transpose()
                        .map_err(Arc::new)
                })
            })
    }

    fn installed(self) -> Result<Activations, ProductError> {
        let mut activations = Activations::default();
        for (_, result) in self.0 {
            match result {
                Ok(Some(license::LicenseStanding::Revoked(license))) => {
                    activations.revoked.push(license);
                }
                Ok(Some(license::LicenseStanding::Valid(license)))
                    if !activations
                        .licenses
                        .iter()
                        .any(|item| item.id() == license.id()) =>
                {
                    activations.licenses.push(license);
                }
                Err(error) if !matches!(error.as_ref(), LicenseError::Revoked(_)) => {
                    activations.first_failure.get_or_insert(error);
                }
                _ => {}
            }
        }
        if activations.licenses.is_empty()
            && let Some(error) = activations.first_failure
        {
            return Err(ProductError::License(error));
        }
        // A perpetual license takes precedence even after an interrupted trial cleanup.
        activations.licenses.sort_by_key(|license| {
            matches!(license.entitlement(), license::Entitlement::Trial { .. })
        });
        Ok(activations)
    }
}

/// Load the authenticated licenses used for installation-wide update authorization.
pub(crate) fn load_update_licenses() -> Result<Vec<UnrevokedLicense>, ProductError> {
    Ok(ActivationSnapshot::load(&state_dir()?)?
        .installed()?
        .licenses)
}

/// Execute the explicitly selected license command without touching a ledger.
///
/// # Errors
/// Returns input, trust, redemption, or activation errors.
pub fn run_license(
    options: LicenseOptions,
    books_path: &Path,
    requirement: BooksRequirement,
    json: bool,
) -> Result<ExitCode, ProductError> {
    let request = match options.command {
        None | Some(LicenseCommand::Status) => LicenseRequest::Status,
        Some(LicenseCommand::Trial) => LicenseRequest::Trial,
        Some(LicenseCommand::Remove { license_id }) => {
            let target = BooksSelection::try_from((Some(books_path), requirement))?;
            let response = removal::remove(license_id, &target)?;
            if json {
                println!("{}", serde_json::json!(response));
            } else {
                println!("{}", response.message());
            }
            return Ok(ExitCode::SUCCESS);
        }
        Some(LicenseCommand::Return { license_id, yes }) => {
            return run_return(license_id, yes.into(), json);
        }
        Some(LicenseCommand::Trust { json }) => {
            if OFFICIAL_BUILD {
                license::release_revocations(&trusted_keys()?)?;
            }
            write_embedded_trust(json)?;
            return Ok(ExitCode::SUCCESS);
        }
        Some(LicenseCommand::Activate {
            file,
            token_stdin,
            code_file,
            storefront,
        }) => {
            let source = ActivationInputs {
                token_stdin,
                file,
                code_file,
                storefront,
                ..ActivationInputs::default()
            }
            .try_into()?;
            LicenseRequest::Activate(source)
        }
    };
    let target = BooksSelection::try_from((Some(books_path), requirement))?;
    let report = request.run(&target)?;
    report.write_cli(json, &target)
}
fn run_return(
    license_id: Option<LicenseId>,
    confirmation: returns::ReturnConfirmation,
    json: bool,
) -> Result<ExitCode, ProductError> {
    let request = returns::ReturnRequest::new(license_id.clone(), confirmation);
    let mut response = request.execute();
    if response.needs_confirmation() && !json {
        use std::io::Write;
        eprintln!("{}", returns::POLICY);
        eprint!("Request the return and refund? [y/N] ");
        io::stderr().flush()?;
        let mut answer = String::new();
        io::stdin().read_line(&mut answer)?;
        let confirmed = answer
            .parse::<returns::ReturnConfirmation>()
            .unwrap_or_else(|never| match never {});
        response = returns::ReturnRequest::new(license_id, confirmed).execute();
    }
    let response = response.into_cli();
    if json {
        println!("{}", serde_json::json!(response));
    } else {
        println!("{}", response.message());
    }
    Ok(if response.is_error() || response.needs_confirmation() {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    })
}

/// Whether a license command requires its selected books to exist.
#[derive(Clone, Copy)]
pub enum BooksRequirement {
    /// The default books might not exist yet.
    Optional,
    /// The selected books must exist.
    Required,
}

impl From<bool> for BooksRequirement {
    fn from(explicit: bool) -> Self {
        if explicit {
            Self::Required
        } else {
            Self::Optional
        }
    }
}

impl BooksRequirement {
    fn for_activation(self, policy: EnforcementPolicy) -> Self {
        match policy {
            EnforcementPolicy::Enforced(_) => Self::Required,
            EnforcementPolicy::Free => self,
        }
    }
}

struct BooksSelection<'a> {
    path: Cow<'a, Path>,
    requirement: BooksRequirement,
}

impl<'a> TryFrom<(Option<&'a Path>, BooksRequirement)> for BooksSelection<'a> {
    type Error = ProductError;

    fn try_from(
        (fallback, requirement): (Option<&'a Path>, BooksRequirement),
    ) -> Result<Self, Self::Error> {
        let managed = match requirement {
            BooksRequirement::Required => None,
            BooksRequirement::Optional => {
                crate::service::managed_books().map_err(|_| ProductError::BooksConfiguration)?
            }
        };
        let path = match (managed, fallback) {
            (Some(path), _) => Cow::Owned(path),
            (None, Some(path)) => Cow::Borrowed(path),
            (None, None) => Cow::Owned(
                crate::service::default_books().map_err(|_| ProductError::BooksConfiguration)?,
            ),
        };
        Ok(Self { path, requirement })
    }
}

impl BooksSelection<'_> {
    fn resolve(&self, request: &LicenseRequest) -> Result<Option<BooksPath>, ProductError> {
        let requirement = match request {
            LicenseRequest::Status => self.requirement,
            LicenseRequest::Trial | LicenseRequest::Activate(_) => {
                self.requirement.for_activation(ENFORCEMENT)
            }
        };
        match BooksPath::try_from(self.path.as_ref()) {
            Ok(books) => Ok(Some(books)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => match requirement {
                BooksRequirement::Optional => Ok(None),
                BooksRequirement::Required => Err(ProductError::BooksRequired),
            },
            Err(error) => Err(error.into()),
        }
    }

    fn settings_url(&self) -> Option<String> {
        use crate::{
            server,
            service::{self, LinkBooks},
        };
        service::link_address(LinkBooks::Explicit(&self.path), None)
            .ok()
            .map(|listener| format!("{}settings", server::local_url(listener.into())))
    }
}

/// Opt-out applies before update disk/network activity. Set to `1` or `true`.
#[must_use]
pub fn update_checks() -> updates::UpdateChecks {
    if std::env::var("BEANKEEPER_NO_UPDATE_CHECK")
        .is_ok_and(|v| v == "1" || v.eq_ignore_ascii_case("true"))
    {
        updates::UpdateChecks::Disabled
    } else {
        updates::UpdateChecks::Enabled
    }
}
/// Safe cached status for a paired browser page, without triggering a request.
#[must_use]
pub fn status_text() -> String {
    let license = license_status().unwrap_or_else(|e| format!("License status: unavailable. {e}."));
    let updates = state_dir()
        .and_then(|dir| Ok(updates::status(&dir.join("updates.json"), update_checks())?))
        .unwrap_or_else(|e| e.to_string());
    format!("{license}\n{updates}")
}

fn remove_trials(
    state: &Path,
    keys: &TrustedKeys,
    paid_id: &LicenseId,
) -> Result<(), ProductError> {
    for path in license::activation_paths(state)? {
        if let Ok(Some(installed)) = license::load(&path, keys)
            && installed.id() != paid_id
            && matches!(installed.entitlement(), license::Entitlement::Trial { .. })
        {
            removal::remove_at(state, installed.id())?;
        }
    }
    Ok(())
}

/// Best-effort shared setup path: inability to obtain a trial never prevents setup.
pub(crate) fn setup_trial(books: &crate::setup::OwnerBooks<'_>) -> String {
    match state_dir() {
        Ok(state) => setup_trial_at(books, &state),
        Err(error) => error.to_string(),
    }
}

/// Setup uses its explicit installation state, including in isolated service tests.
pub(crate) fn setup_trial_at(books: &crate::setup::OwnerBooks<'_>, state: &Path) -> String {
    let target = BooksSelection {
        path: Cow::Borrowed(books.as_ref()),
        requirement: BooksRequirement::Optional,
    };
    let books = match target.resolve(&LicenseRequest::Trial) {
        Ok(books) => books,
        Err(error) => return error.to_string(),
    };
    let store = books
        .as_ref()
        .map(|books| TrialNoticeStore::from((state, books)));
    match LicenseRequest::Trial.execute_at(books.as_ref(), state) {
        Ok(report) => report.response(None).to_string(),
        Err(error) => match store
            .map(|store| store.save(&TrialNotice::from(&error)))
            .transpose()
        {
            Ok(_) => error.to_string(),
            Err(storage) => format!("{error}\n{storage}"),
        },
    }
}

/// Shared public, secret-free status for lifecycle tools.
pub(crate) fn report_for_books(books: &Path, state: &Path) -> LicenseResponse {
    let target = BooksSelection {
        path: Cow::Borrowed(books),
        requirement: BooksRequirement::Optional,
    };
    match target
        .resolve(&LicenseRequest::Status)
        .and_then(|books| LicenseRequest::Status.execute_at(books.as_ref(), state))
    {
        Ok(report) => report.response(target.settings_url().as_deref()),
        Err(error) => LicenseResponse::from(&error),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ActivationSnapshot, LicenseInventory, LicenseReport, embedded_license_trust,
        embedded_public_keys,
    };
    use crate::license::{TrustedKeys, VerifiedRevocations, verify_offline};

    #[test]
    fn verified_license_status_describes_perpetual_major_version_coverage()
    -> Result<(), Box<dyn std::error::Error>> {
        let keys =
            TrustedKeys::from_json(include_bytes!("../tests/fixtures/license-public-keys.json"))?;
        let claims = verify_offline(
            include_bytes!("../tests/fixtures/license-v4.public.txt"),
            &keys,
        )?;
        let revocations = VerifiedRevocations::verify(
            include_bytes!("../tests/fixtures/revocations-empty.json"),
            &keys,
        )?;
        let status = LicenseInventory::Active {
            first: Box::new(revocations.admit(claims)?),
            additional: Vec::new(),
            warning: None,
            revoked: Vec::new(),
        }
        .to_string();
        assert_eq!(
            status,
            "License status: active and valid.\nLicense lic_01h455vb4pex5vsknk084sn02q covers all 1.x releases forever."
        );
        Ok(())
    }

    #[test]
    fn signed_revoked_inventory_is_visible_and_cannot_grant_write_access()
    -> Result<(), Box<dyn std::error::Error>> {
        let keys =
            TrustedKeys::from_json(include_bytes!("../tests/fixtures/license-public-keys.json"))?;
        let revoked = VerifiedRevocations::verify(
            include_bytes!("../tests/fixtures/revocations.json"),
            &keys,
        )?;
        let first = verify_offline(
            include_bytes!("../tests/fixtures/license-v4.public.txt"),
            &keys,
        )?;
        let second = verify_offline(
            include_bytes!("../tests/fixtures/license-major-2-v4.public.txt"),
            &keys,
        )?;
        let snapshot = ActivationSnapshot(vec![
            ("first".into(), Ok(Some(revoked.standing(first)))),
            ("second".into(), Ok(Some(revoked.standing(second)))),
        ]);
        let inventory: LicenseInventory = snapshot.installed()?.into();
        let report = LicenseReport {
            inventory: Ok(inventory),
            access: None,
            trial_notice: None,
        };
        let response = serde_json::to_value(report.response(None))?;
        assert_eq!(response["status"], "active");
        assert_eq!(response["licenses"][0]["standing"], "valid");
        assert_eq!(response["licenses"][1]["standing"], "revoked");
        assert!(
            response["message"]
                .as_str()
                .ok_or("message")?
                .contains("was revoked")
        );
        let only = verify_offline(
            include_bytes!("../tests/fixtures/license-v4.public.txt"),
            &keys,
        )?;
        let inventory: LicenseInventory =
            ActivationSnapshot(vec![("first".into(), Ok(Some(revoked.standing(only))))])
                .installed()?
                .into();
        let report = LicenseReport {
            inventory: Ok(inventory),
            access: None,
            trial_notice: None,
        };
        assert_eq!(
            serde_json::to_value(report.response(None))?["status"],
            "revoked"
        );
        Ok(())
    }

    #[test]
    fn storefront_has_a_closed_case_insensitive_cli_vocabulary()
    -> Result<(), Box<dyn std::error::Error>> {
        use super::{LicenseCommand, LicenseOptions, Storefront};
        use clap::FromArgMatches;

        for (spelling, expected) in [
            ("polar", Storefront::Polar),
            ("gumroad", Storefront::Gumroad),
            ("Etsy", Storefront::Etsy),
        ] {
            let matches = crate::cli::command().try_get_matches_from([
                "bk",
                "license",
                "activate",
                "--code-file",
                "code.txt",
                "--storefront",
                spelling,
            ])?;
            let options = matches
                .subcommand_matches("license")
                .ok_or("license command")?;
            let Some(LicenseCommand::Activate {
                storefront: Some(storefront),
                ..
            }) = LicenseOptions::from_arg_matches(options)?.command
            else {
                return Err("missing storefront option".into());
            };
            assert_eq!(storefront, expected);
        }
        assert!(
            crate::cli::command()
                .try_get_matches_from([
                    "bk",
                    "license",
                    "activate",
                    "--code-file",
                    "code.txt",
                    "--storefront",
                    "unknown",
                ])
                .is_err()
        );
        Ok(())
    }

    #[test]
    fn embedded_trust_matches_compile_time_env() {
        let trust = embedded_license_trust();
        assert_eq!(
            trust.service_url,
            option_env!("BEANKEEPER_LICENSE_SERVICE_URL").filter(|url| !url.is_empty())
        );
        assert_eq!(
            trust.public_keys_json,
            option_env!("BEANKEEPER_LICENSE_PUBLIC_KEYS_JSON").unwrap_or("{}")
        );
        embedded_public_keys().expect("embedded keys must be a JSON object");
    }
}
