//! CLI, configuration and local status adapters for isolated licensing/updates.
use crate::{
    license::{self, LicenseClaims, LicenseError, TrustedKeys},
    updates::{self, UpdateError},
};
use chrono::{DateTime, Utc};
use clap::{Args, Subcommand};
use std::{
    error::Error,
    fmt, fs,
    io::{self, Read},
    path::PathBuf,
    time::Duration,
};

/// Product command errors, with secret-bearing service diagnostics discarded.
#[derive(Debug)]
pub enum ProductError {
    License(LicenseError),
    Update(UpdateError),
    Io(io::Error),
    Configuration(&'static str),
    Redemption,
}
impl fmt::Display for ProductError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::License(e) => write!(f, "{e}"),
            Self::Update(e) => write!(f, "{e}"),
            Self::Io(_) => f.write_str("cannot access Desktop license configuration"),
            Self::Configuration(reason) => f.write_str(reason),
            Self::Redemption => {
                f.write_str("license redemption failed; check the service URL and claim code")
            }
        }
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
        Self::License(e)
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
/// Sources for an offline file or an explicitly requested online redemption.
#[derive(Subcommand)]
pub enum LicenseCommand {
    /// Verify and store a license, or redeem a storefront key with a configured service.
    Activate {
        /// License JSON file. Offline activation makes no network request.
        #[arg(required_unless_present = "code_file", conflicts_with = "code_file")]
        file: Option<PathBuf>,
        /// Private text file containing a storefront key or single-use claim code.
        #[arg(long)]
        code_file: Option<PathBuf>,
        /// D10 storefront adapter used only with --code-file.
        #[arg(long, default_value = "polar", requires = "code_file")]
        storefront: Option<String>,
    },
}

/// Per-user Desktop state, independent of any ledger, with an explicit test override.
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
        .ok_or(ProductError::Configuration(
            "set BEANKEEPER_DESKTOP_STATE_DIR or configure a home directory",
        ))
}
/// Issuer keys come from a provisioned build trust map or an explicit local trust file.
pub fn trusted_keys() -> Result<TrustedKeys, ProductError> {
    let bytes = if let Some(path) = std::env::var_os("BEANKEEPER_LICENSE_KEYS_FILE") {
        bounded_file(&PathBuf::from(path), 32_768)?
    } else {
        option_env!("BEANKEEPER_LICENSE_PUBLIC_KEYS_JSON")
            .unwrap_or("{}")
            .as_bytes()
            .to_vec()
    };
    Ok(TrustedKeys::from_json(&bytes)?)
}
fn bounded_file(path: &std::path::Path, limit: u64) -> Result<Vec<u8>, ProductError> {
    let mut bytes = Vec::new();
    fs::File::open(path)?
        .take(limit + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit {
        return Err(ProductError::Configuration(
            "license input exceeds size limit",
        ));
    }
    Ok(bytes)
}
/// A missing activation is a normal status; verification still happens offline.
pub fn license_status() -> Result<String, ProductError> {
    let path = state_dir()?.join("license.json");
    if !path.try_exists()? {
        return Ok("No Desktop license activated. Use `bk license activate FILE`.".into());
    }
    let claims = license::load(&path, &trusted_keys()?)?
        .ok_or(ProductError::Configuration("activated license is missing"))?;
    Ok(describe(&claims))
}
fn describe(claims: &LicenseClaims) -> String {
    format!(
        "License {}: perpetual Desktop use; updates through {} (inclusive).",
        claims.license_id,
        claims.updates_until.to_rfc3339()
    )
}
/// Check release eligibility using an authenticated manifest timestamp.
pub fn require_update_entitlement(released_at: DateTime<Utc>) -> Result<(), ProductError> {
    let claims = license::load(&state_dir()?.join("license.json"), &trusted_keys()?)?.ok_or(
        ProductError::Configuration("activate a Desktop license before self-update"),
    )?;
    if license::entitlement(&claims, released_at).update_eligible {
        Ok(())
    } else {
        Err(ProductError::Configuration(
            "this release is outside the license update window",
        ))
    }
}
fn redeem(code_file: &std::path::Path, storefront: &str) -> Result<Vec<u8>, ProductError> {
    use secrecy::ExposeSecret;

    let service = std::env::var("BEANKEEPER_LICENSE_SERVICE_URL").map_err(|_| {
        ProductError::Configuration(
            "set BEANKEEPER_LICENSE_SERVICE_URL before redeeming a claim code",
        )
    })?;
    let mut url = reqwest::Url::parse(&service)
        .map_err(|_| ProductError::Configuration("invalid license service URL"))?;
    if url.scheme() != "https"
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(ProductError::Configuration(
            "license service must be an HTTPS origin without credentials",
        ));
    }
    url.set_path("/api/v1/licenses/redeem");
    crate::setup::keychain::validate_private_file(code_file)?;
    let code = secrecy::SecretString::from(
        String::from_utf8(bounded_file(code_file, 4096)?).map_err(|_| ProductError::Redemption)?,
    );
    let recovery_token = if storefront == "etsy" {
        let path = std::env::var_os("BEANKEEPER_LICENSE_RECOVERY_TOKEN_FILE")
            .map(PathBuf::from)
            .ok_or(ProductError::Configuration("Etsy redemption requires BEANKEEPER_LICENSE_RECOVERY_TOKEN_FILE; retain this separate private recovery credential before redeeming a single-use code"))?;
        crate::setup::keychain::validate_private_file(&path)?;
        let token = secrecy::SecretString::from(
            String::from_utf8(bounded_file(&path, 4096)?).map_err(|_| ProductError::Redemption)?,
        );
        let value = token.expose_secret().trim();
        if !(32..=512).contains(&value.len()) || value == code.expose_secret().trim() {
            return Err(ProductError::Configuration(
                "Etsy recovery token must contain 32 to 512 bytes and differ from the claim code",
            ));
        }
        Some(token)
    } else {
        None
    };
    let client = reqwest::blocking::Client::builder()
        .https_only(true)
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(15))
        .build()
        .map_err(|_| ProductError::Redemption)?;
    let response = client
        .post(url)
        .json(&serde_json::json!({
            "storefront": storefront,
            "key": code.expose_secret().trim(),
            "recovery_token": recovery_token.as_ref().map(|token| token.expose_secret().trim())
        }))
        .send()
        .and_then(reqwest::blocking::Response::error_for_status)
        .map_err(|_| ProductError::Redemption)?;
    let mut bytes = Vec::new();
    response.take(32_769).read_to_end(&mut bytes)?;
    if bytes.len() > 32_768 {
        return Err(ProductError::Redemption);
    }
    // D10 returns the direct LicenseFile JSON attachment. The caller verifies
    // the envelope independently before storing any activation.
    Ok(bytes)
}
/// Execute the explicitly selected license command without touching a ledger.
pub fn run_license(options: LicenseOptions) -> Result<(), ProductError> {
    match options.command {
        None => println!("{}", license_status()?),
        Some(LicenseCommand::Activate {
            file,
            code_file,
            storefront,
        }) => {
            // Resolve trust before redeeming a potentially single-use claim.
            let keys = trusted_keys()?;
            let bytes = match (file, code_file) {
                (Some(file), None) => bounded_file(&file, 32_768)?,
                (None, Some(code)) => redeem(&code, storefront.as_deref().unwrap_or("polar"))?,
                _ => {
                    return Err(ProductError::Configuration(
                        "select one license file or --code-file",
                    ));
                }
            };
            let claims = license::activate(&bytes, &keys, &state_dir()?.join("license.json"))?;
            println!("{}", describe(&claims));
        }
    }
    Ok(())
}
/// Opt-out applies before update disk/network activity. Set to `1` or `true`.
pub fn updates_disabled() -> bool {
    std::env::var("BEANKEEPER_NO_UPDATE_CHECK")
        .is_ok_and(|v| v == "1" || v.eq_ignore_ascii_case("true"))
}
/// Safe cached status for a paired browser page, without triggering a request.
pub fn status_text() -> String {
    let license = license_status().unwrap_or_else(|e| e.to_string());
    let updates = state_dir()
        .and_then(|dir| {
            Ok(updates::status(
                &dir.join("updates.json"),
                updates_disabled(),
            )?)
        })
        .unwrap_or_else(|e| e.to_string());
    format!("{license}\n{updates}")
}
