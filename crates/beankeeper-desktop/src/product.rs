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
    /// Show the license service URL and issuer keys compiled into this binary.
    Trust {
        /// Print a JSON object with service_url and public_keys.
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
        .map_err(|_| ProductError::Configuration("embedded public keys are not valid JSON"))?;
    match value {
        serde_json::Value::Object(map) => Ok(map),
        _ => Err(ProductError::Configuration(
            "embedded public keys must be a JSON object",
        )),
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
            serde_json::to_string(&body).map_err(|_| {
                ProductError::Configuration("cannot encode embedded license trust")
            })?
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
fn license_service_origin() -> Result<String, ProductError> {
    if let Ok(url) = std::env::var("BEANKEEPER_LICENSE_SERVICE_URL")
        && !url.is_empty()
    {
        return Ok(url);
    }
    option_env!("BEANKEEPER_LICENSE_SERVICE_URL")
        .filter(|url| !url.is_empty())
        .map(str::to_owned)
        .ok_or(ProductError::Configuration(
            "set BEANKEEPER_LICENSE_SERVICE_URL at runtime, or build it in with the same variable, before redeeming a claim code",
        ))
}

fn is_loopback_host(host: &str) -> bool {
    matches!(host, "localhost" | "127.0.0.1" | "::1")
        || host
            .trim_matches(['[', ']'])
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

fn redeem(code_file: &std::path::Path, storefront: &str) -> Result<Vec<u8>, ProductError> {
    use secrecy::ExposeSecret;

    let service = license_service_origin()?;
    let mut url = reqwest::Url::parse(&service)
        .map_err(|_| ProductError::Configuration("invalid license service URL"))?;
    let host = url
        .host_str()
        .ok_or(ProductError::Configuration("invalid license service URL"))?;
    let loopback = is_loopback_host(host);
    let https = url.scheme() == "https";
    let http_loopback = url.scheme() == "http" && loopback;
    if !(https || http_loopback)
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(ProductError::Configuration(
            "license service must be an HTTPS origin, or HTTP on loopback, without credentials",
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
    let mut client = reqwest::blocking::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(15));
    if https {
        client = client.https_only(true);
    }
    let client = client.build().map_err(|_| ProductError::Redemption)?;
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
        Some(LicenseCommand::Trust { json }) => write_embedded_trust(json)?,
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

#[cfg(test)]
mod tests {
    use super::{embedded_license_trust, embedded_public_keys};

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
