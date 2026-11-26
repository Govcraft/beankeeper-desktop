//! CLI, configuration and local status adapters for isolated licensing/updates.
use crate::{
    license::{self, LicenseError, MAX_LICENSE_INPUT, TrustedKeys, VerifiedLicense},
    private_file::PrivateFile,
    updates::{self, UpdateError},
};
use clap::{Args, Subcommand, ValueEnum};
use secrecy::{ExposeSecret, SecretString};
use std::{
    error::Error,
    fmt::{self, Write as _},
    fs,
    io::{self, Read},
    path::{Path, PathBuf},
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
/// Local license commands, including offline activation and online redemption.
#[derive(Subcommand)]
pub enum LicenseCommand {
    /// Verify and store a license, or redeem a storefront key with a configured service.
    Activate {
        /// Plain-text file containing the license token.
        #[arg(required_unless_present_any = ["token", "code_file"], conflicts_with_all = ["token", "code_file"])]
        file: Option<PathBuf>,
        /// Paste the license token for offline activation.
        #[arg(long, conflicts_with = "code_file")]
        token: Option<String>,
        /// Private text file containing a storefront key or single-use claim code.
        #[arg(long)]
        code_file: Option<PathBuf>,
        /// D10 storefront adapter used only with --code-file.
        #[arg(long, requires = "code_file", ignore_case = true)]
        storefront: Option<Storefront>,
    },
    /// Show the license service URL and issuer keys compiled into this binary.
    Trust {
        /// Print a JSON object with `service_url` and `public_keys`.
        #[arg(long)]
        json: bool,
    },
}

/// Storefront adapters supported for online license redemption.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, ValueEnum)]
pub enum Storefront {
    /// Polar license key (the default).
    #[default]
    Polar,
    /// Gumroad license key.
    Gumroad,
    /// Etsy single-use claim code.
    Etsy,
}

enum LicenseSource {
    File(PathBuf),
    Token(String),
    Code {
        file: PathBuf,
        storefront: Storefront,
    },
}

impl
    TryFrom<(
        Option<PathBuf>,
        Option<String>,
        Option<PathBuf>,
        Option<Storefront>,
    )> for LicenseSource
{
    type Error = ProductError;

    fn try_from(
        (file, token, code_file, storefront): (
            Option<PathBuf>,
            Option<String>,
            Option<PathBuf>,
            Option<Storefront>,
        ),
    ) -> Result<Self, Self::Error> {
        match (file, token, code_file, storefront) {
            (Some(file), None, None, None) => Ok(Self::File(file)),
            (None, Some(token), None, None) => Ok(Self::Token(token)),
            (None, None, Some(file), storefront) => Ok(Self::Code {
                file,
                storefront: storefront.unwrap_or_default(),
            }),
            _ => Err(ProductError::Configuration(
                "select one license file, --token, or --code-file",
            )),
        }
    }
}

impl LicenseSource {
    fn read(self) -> Result<Vec<u8>, ProductError> {
        match self {
            Self::File(file) => bounded_file(&file, MAX_LICENSE_INPUT as u64),
            Self::Token(token) => Ok(token.into_bytes()),
            Self::Code { file, storefront } => redeem(&file, storefront),
        }
    }
}

struct RecoveryToken(SecretString);

impl RecoveryToken {
    fn read_for(code: &SecretString) -> Result<Self, ProductError> {
        let path = std::env::var_os("BEANKEEPER_LICENSE_RECOVERY_TOKEN_FILE")
            .map(PathBuf::from)
            .ok_or(ProductError::Configuration("Etsy redemption requires BEANKEEPER_LICENSE_RECOVERY_TOKEN_FILE; retain this separate private recovery credential before redeeming a single-use code"))?;
        let token = read_redemption_secret(&path)?;
        let value = token.expose_secret().trim();
        if !(32..=512).contains(&value.len()) || value == code.expose_secret().trim() {
            return Err(ProductError::Configuration(
                "Etsy recovery token must contain 32 to 512 bytes and differ from the claim code",
            ));
        }
        Ok(Self(token))
    }
}

enum Redemption {
    Polar(SecretString),
    Gumroad(SecretString),
    Etsy {
        code: SecretString,
        recovery: RecoveryToken,
    },
}

impl Redemption {
    fn from_file(file: &Path, storefront: Storefront) -> Result<Self, ProductError> {
        let code = read_redemption_secret(file)?;
        Ok(match storefront {
            Storefront::Polar => Self::Polar(code),
            Storefront::Gumroad => Self::Gumroad(code),
            Storefront::Etsy => Self::Etsy {
                recovery: RecoveryToken::read_for(&code)?,
                code,
            },
        })
    }

    fn request(&self) -> RedemptionRequest<'_> {
        match self {
            Self::Polar(code) => RedemptionRequest::Polar {
                key: code.expose_secret().trim(),
                recovery_token: (),
            },
            Self::Gumroad(code) => RedemptionRequest::Gumroad {
                key: code.expose_secret().trim(),
                recovery_token: (),
            },
            Self::Etsy { code, recovery } => RedemptionRequest::Etsy {
                key: code.expose_secret().trim(),
                recovery_token: recovery,
            },
        }
    }
}

#[derive(serde::Serialize)]
#[serde(tag = "storefront", rename_all = "lowercase")]
enum RedemptionRequest<'a> {
    Polar {
        key: &'a str,
        recovery_token: (),
    },
    Gumroad {
        key: &'a str,
        recovery_token: (),
    },
    Etsy {
        key: &'a str,
        #[serde(serialize_with = "serialize_recovery_token")]
        recovery_token: &'a RecoveryToken,
    },
}

fn serialize_recovery_token<S: serde::Serializer>(
    token: &RecoveryToken,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    serializer.serialize_str(token.0.expose_secret().trim())
}

fn read_redemption_secret(path: &Path) -> Result<SecretString, ProductError> {
    PrivateFile::open(path)?
        .read_secret(4096)
        .map_err(|error| match error.kind() {
            io::ErrorKind::FileTooLarge => {
                ProductError::Configuration("license input exceeds size limit")
            }
            io::ErrorKind::InvalidData => ProductError::Redemption,
            _ => error.into(),
        })
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
        .ok_or(ProductError::Configuration(
            "set BEANKEEPER_DESKTOP_STATE_DIR or configure a home directory",
        ))
}
/// Issuer keys come from a provisioned build trust map or an explicit local trust file.
///
/// # Errors
/// Returns errors for unreadable, incomplete, or invalid trust maps.
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
fn bounded_file(path: &Path, limit: u64) -> Result<Vec<u8>, ProductError> {
    let mut bytes = Vec::new();
    fs::File::open(path)?
        .take(limit + 1)
        .read_to_end(&mut bytes)?;
    if u64::try_from(bytes.len()).map_or(true, |len| len > limit) {
        return Err(ProductError::Configuration(
            "license input exceeds size limit",
        ));
    }
    Ok(bytes)
}
/// A missing activation is a normal status; verification still happens offline.
///
/// # Errors
/// Returns storage or verification errors for existing activations.
pub fn license_status() -> Result<String, ProductError> {
    let activations = load_activations()?;
    if activations.licenses.is_empty() {
        return Ok(
            "License status: not activated. No Desktop license activated. Paste your license with `bk license activate --token TOKEN` or supply a license file."
                .into(),
        );
    }
    let mut status = describe(&activations.licenses);
    if let Some(error) = activations.first_failure {
        let _ = write!(
            status,
            "\nOne or more license files could not be verified ({error})."
        );
    }
    Ok(status)
}

fn describe(licenses: &[VerifiedLicense]) -> String {
    let mut status = "License status: active and valid.".to_owned();
    for license in licenses {
        let _ = write!(
            status,
            "\nLicense {} covers all {}.x releases forever.",
            license.id(),
            license.eligible_major()
        );
    }
    status
}

#[derive(Default)]
struct Activations {
    licenses: Vec<VerifiedLicense>,
    first_failure: Option<LicenseError>,
}

fn load_activations() -> Result<Activations, ProductError> {
    let paths = license::activation_paths(&state_dir()?)?;
    let mut activations = Activations::default();
    if paths.is_empty() {
        return Ok(activations);
    }
    let keys = trusted_keys()?;
    for path in paths {
        match license::load(&path, &keys) {
            Ok(Some(license))
                if !activations
                    .licenses
                    .iter()
                    .any(|item| item.id() == license.id()) =>
            {
                activations.licenses.push(license);
            }
            Ok(_) => {}
            Err(error) => {
                activations.first_failure.get_or_insert(error);
            }
        }
    }
    if activations.licenses.is_empty()
        && let Some(error) = activations.first_failure
    {
        return Err(error.into());
    }
    Ok(activations)
}

/// Load the authenticated licenses used for installation-wide update authorization.
pub(crate) fn load_update_licenses() -> Result<Vec<VerifiedLicense>, ProductError> {
    Ok(load_activations()?.licenses)
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

fn redeem(file: &Path, storefront: Storefront) -> Result<Vec<u8>, ProductError> {
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
    let redemption = Redemption::from_file(file, storefront)?;
    let mut client = reqwest::blocking::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(15));
    if https {
        client = client.https_only(true);
    }
    let client = client.build().map_err(|_| ProductError::Redemption)?;
    let response = client
        .post(url)
        .json(&redemption.request())
        .send()
        .and_then(reqwest::blocking::Response::error_for_status)
        .map_err(|_| ProductError::Redemption)?;
    let mut bytes = Vec::new();
    response
        .take(MAX_LICENSE_INPUT as u64 + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() > MAX_LICENSE_INPUT {
        return Err(ProductError::Redemption);
    }
    // The issuer returns a plain-text token that activation verifies offline.
    Ok(bytes)
}
/// Execute the explicitly selected license command without touching a ledger.
///
/// # Errors
/// Returns input, trust, redemption, or activation errors.
pub fn run_license(options: LicenseOptions) -> Result<(), ProductError> {
    match options.command {
        None => println!("{}", license_status()?),
        Some(LicenseCommand::Trust { json }) => write_embedded_trust(json)?,
        Some(LicenseCommand::Activate {
            file,
            token,
            code_file,
            storefront,
        }) => {
            let source = LicenseSource::try_from((file, token, code_file, storefront))?;
            // Resolve trust before redeeming a potentially single-use claim.
            let keys = trusted_keys()?;
            let bytes = source.read()?;
            let claims = license::activate_in(&bytes, &keys, &state_dir()?)?;
            println!("{}", describe(std::slice::from_ref(&claims)));
        }
    }
    Ok(())
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

#[cfg(test)]
mod tests {
    use super::{describe, embedded_license_trust, embedded_public_keys};
    use crate::license::{TrustedKeys, verify_offline};

    #[test]
    fn verified_license_status_describes_perpetual_major_version_coverage() {
        let keys =
            TrustedKeys::from_json(include_bytes!("../tests/fixtures/license-public-keys.json"))
                .expect("fixture public keys");
        let claims = verify_offline(
            include_bytes!("../tests/fixtures/license-v4.public.txt"),
            &keys,
        )
        .expect("verified fixture");
        let status = describe(&[claims]);
        assert_eq!(
            status,
            "License status: active and valid.\nLicense lic_01h455vb4pex5vsknk084sn02q covers all 1.x releases forever."
        );
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
