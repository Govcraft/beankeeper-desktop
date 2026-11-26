//! Input types and shared offline activation and online redemption.

use std::{
    fs::{self, File},
    io::{self, Read, Write},
    path::{Path, PathBuf},
    time::Duration,
};

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use clap::ValueEnum;
use schemars::JsonSchema;
use secrecy::{ExposeSecret, SecretString, zeroize::Zeroizing};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{
    license::{MAX_LICENSE_INPUT, TrustedKeys, VerifiedLicense, verify_offline},
    private_file::PrivateFile,
    setup::keychain::{restrict_directory, restrict_file},
};

use super::ProductError;

const CODE_LIMIT: usize = 512;
const RECOVERY_MIN: usize = 32;

/// Storefront adapters supported for online license redemption.
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, ValueEnum, Deserialize, JsonSchema, Serialize,
)]
#[serde(rename_all = "lowercase")]
pub enum Storefront {
    /// Polar license key (the CLI default).
    #[default]
    Polar,
    /// Gumroad license key.
    Gumroad,
    /// Etsy single-use claim code.
    Etsy,
    /// Stripe checkout code.
    Stripe,
}

impl Storefront {
    fn for_claim(code: &str) -> Self {
        if code.starts_with("cs_live_") || code.starts_with("cs_test_") {
            Self::Stripe
        } else {
            Self::Etsy
        }
    }
}

enum LicenseInput {
    Pasted(SecretString),
    Read(Zeroizing<Vec<u8>>),
}

impl AsRef<[u8]> for LicenseInput {
    fn as_ref(&self) -> &[u8] {
        match self {
            Self::Pasted(token) => token.expose_secret().as_bytes(),
            Self::Read(bytes) => bytes,
        }
    }
}

struct LicensePath(PathBuf);

impl TryFrom<PathBuf> for LicensePath {
    type Error = ProductError;

    fn try_from(path: PathBuf) -> Result<Self, Self::Error> {
        if path.as_os_str().is_empty() {
            return Err(ProductError::InvalidLicenseInput);
        }
        Ok(Self(path))
    }
}

impl LicensePath {
    fn read(&self) -> Result<LicenseInput, ProductError> {
        let mut options = File::options();
        options.read(true);
        #[cfg(unix)]
        {
            use rustix::fs::OFlags;
            use std::os::unix::fs::OpenOptionsExt;

            options.custom_flags(
                i32::try_from((OFlags::NONBLOCK | OFlags::NOCTTY).bits())
                    .map_err(io::Error::other)?,
            );
        }
        let file = options.open(&self.0)?;
        if !file.metadata()?.is_file() {
            return Err(ProductError::InvalidLicenseInput);
        }
        read_license(file)
    }
}

fn read_license(input: impl Read) -> Result<LicenseInput, ProductError> {
    let mut bytes = Zeroizing::new(Vec::new());
    input
        .take(MAX_LICENSE_INPUT as u64 + 1)
        .read_to_end(&mut bytes)?;
    Ok(LicenseInput::Read(bytes))
}

struct ClaimCode {
    secret: SecretString,
    storefront: Storefront,
}

impl TryFrom<(&str, Option<Storefront>)> for ClaimCode {
    type Error = ProductError;

    fn try_from((input, storefront): (&str, Option<Storefront>)) -> Result<Self, Self::Error> {
        let code = input.trim();
        let storefront = storefront.unwrap_or_else(|| Storefront::for_claim(code));
        let minimum = if storefront == Storefront::Etsy {
            RECOVERY_MIN
        } else {
            1
        };
        if !(minimum..=CODE_LIMIT).contains(&code.len()) || code.chars().any(char::is_control) {
            return Err(ProductError::InvalidClaimCode);
        }
        Ok(Self {
            secret: SecretString::from(code),
            storefront,
        })
    }
}

pub(super) struct ActivationSource(Source);

#[derive(Default)]
pub(super) struct ActivationInputs {
    pub(super) pasted: Option<SecretString>,
    pub(super) token_stdin: bool,
    pub(super) file: Option<PathBuf>,
    pub(super) claim: Option<SecretString>,
    pub(super) code_file: Option<PathBuf>,
    pub(super) storefront: Option<Storefront>,
}

enum Source {
    Pasted(SecretString),
    Stdin,
    File(LicensePath),
    Claim(ClaimCode),
    CodeFile(CodeFile),
}

struct CodeFile {
    path: PathBuf,
    storefront: Storefront,
}

impl From<(PathBuf, Option<Storefront>)> for CodeFile {
    fn from((path, storefront): (PathBuf, Option<Storefront>)) -> Self {
        Self {
            path,
            storefront: storefront.unwrap_or_default(),
        }
    }
}

impl TryFrom<ActivationInputs> for ActivationSource {
    type Error = ProductError;

    fn try_from(input: ActivationInputs) -> Result<Self, Self::Error> {
        let source = match (
            input.pasted,
            input.token_stdin,
            input.file,
            input.claim,
            input.code_file,
            input.storefront,
        ) {
            (Some(token), false, None, None, None, None) => Source::Pasted(token),
            (None, true, None, None, None, None) => Source::Stdin,
            (None, false, Some(path), None, None, None) => Source::File(path.try_into()?),
            (None, false, None, Some(code), None, storefront) => {
                Source::Claim((code.expose_secret(), storefront).try_into()?)
            }
            (None, false, None, None, Some(path), storefront) => {
                Source::CodeFile((path, storefront).into())
            }
            _ => return Err(ProductError::ActivationInput),
        };
        Ok(Self(source))
    }
}

impl ActivationSource {
    pub(super) fn verify(
        self,
        keys: &TrustedKeys,
        state: &Path,
    ) -> Result<VerifiedLicense, ProductError> {
        let input = match self.0 {
            Source::Pasted(token) => LicenseInput::Pasted(token),
            Source::Stdin => read_license(io::stdin())?,
            Source::File(path) => path.read()?,
            Source::Claim(code) => {
                LicenseService::configured()?.redeem(&code, state, &RecoveryStorage::Desktop)?
            }
            Source::CodeFile(file) => {
                let client = LicenseService::configured()?;
                let secret = read_secret(&file.path)?;
                let code = ClaimCode::try_from((secret.expose_secret(), Some(file.storefront)))?;
                client.redeem(&code, state, &RecoveryStorage::from_environment())?
            }
        };
        Ok(verify_offline(input, keys)?)
    }
}

fn read_secret(path: &Path) -> Result<SecretString, ProductError> {
    PrivateFile::open(path)?
        .read_secret(CODE_LIMIT + 2)
        .map_err(|error| match error.kind() {
            io::ErrorKind::FileTooLarge | io::ErrorKind::InvalidData => {
                ProductError::InvalidClaimCode
            }
            _ => error.into(),
        })
}

enum RecoveryStorage {
    Desktop,
    Custom(PathBuf),
}

impl RecoveryStorage {
    fn from_environment() -> Self {
        std::env::var_os("BEANKEEPER_LICENSE_RECOVERY_TOKEN_FILE")
            .map_or(Self::Desktop, |path| Self::Custom(path.into()))
    }

    fn token_for(&self, code: &ClaimCode, state: &Path) -> Result<RecoveryToken, ProductError> {
        match self {
            Self::Desktop => RecoveryToken::load_or_create(code, state),
            Self::Custom(path) => RecoveryToken::try_from((
                read_secret(path)
                    .map_err(|_| ProductError::Recovery)?
                    .expose_secret(),
                code,
            )),
        }
    }
}

struct RecoveryToken(SecretString);

impl TryFrom<(&str, &ClaimCode)> for RecoveryToken {
    type Error = ProductError;

    fn try_from((input, code): (&str, &ClaimCode)) -> Result<Self, Self::Error> {
        let input = input.trim();
        if !(RECOVERY_MIN..=CODE_LIMIT).contains(&input.len())
            || input == code.secret.expose_secret()
            || input.chars().any(char::is_control)
        {
            return Err(ProductError::Recovery);
        }
        Ok(Self(SecretString::from(input)))
    }
}

impl RecoveryToken {
    fn load_or_create(code: &ClaimCode, state: &Path) -> Result<Self, ProductError> {
        let directory = state.join("license-recovery");
        let digest = Sha256::digest(code.secret.expose_secret().as_bytes());
        let path = directory.join(URL_SAFE_NO_PAD.encode(digest));
        match PrivateFile::open(&path) {
            Ok(file) => return Self::read(file, code),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(_) => return Err(ProductError::Recovery),
        }
        Self::create(&directory, &path, code)
    }

    fn read(file: PrivateFile, code: &ClaimCode) -> Result<Self, ProductError> {
        let value = file
            .read_secret(CODE_LIMIT)
            .map_err(|_| ProductError::Recovery)?;
        Self::try_from((value.expose_secret(), code))
    }

    fn create(directory: &Path, path: &Path, code: &ClaimCode) -> Result<Self, ProductError> {
        let setup = || {
            fs::create_dir_all(directory)?;
            restrict_directory(directory)?;
            let file = tempfile::NamedTempFile::new_in(directory)?;
            restrict_file(file.path())?;
            Ok::<_, io::Error>(file)
        };
        let mut pending = setup().map_err(|_| ProductError::Recovery)?;
        let mut random = Zeroizing::new([0; 32]);
        getrandom::fill(&mut *random).map_err(|_| ProductError::Recovery)?;
        let token = Zeroizing::new(URL_SAFE_NO_PAD.encode(*random));
        let token = Self::try_from((token.as_str(), code))?;
        pending
            .write_all(token.0.expose_secret().as_bytes())
            .and_then(|()| pending.as_file().sync_all())
            .map_err(|_| ProductError::Recovery)?;
        match pending.persist_noclobber(path) {
            Ok(_) => Ok(token),
            Err(error) if error.error.kind() == io::ErrorKind::AlreadyExists => Self::read(
                PrivateFile::open(path).map_err(|_| ProductError::Recovery)?,
                code,
            ),
            Err(_) => Err(ProductError::Recovery),
        }
    }
}

#[derive(Serialize)]
struct RedemptionRequest<'a> {
    storefront: Storefront,
    key: &'a str,
    recovery_token: Option<&'a str>,
}

#[derive(Serialize)]
struct RecoveryRequest<'a> {
    recovery_token: &'a str,
}

struct LicenseService {
    origin: reqwest::Url,
    http: reqwest::blocking::Client,
}

impl LicenseService {
    fn configured() -> Result<Self, ProductError> {
        let origin = std::env::var("BEANKEEPER_LICENSE_SERVICE_URL")
            .ok()
            .filter(|url| !url.is_empty())
            .or_else(|| {
                super::embedded_license_trust()
                    .service_url
                    .map(str::to_owned)
            })
            .ok_or(ProductError::Configuration(
                "BEANKEEPER_LICENSE_SERVICE_URL is not configured for this installation.",
            ))?;
        let mut origin = reqwest::Url::parse(&origin).map_err(|_| {
            ProductError::Configuration("The configured license service URL is invalid.")
        })?;
        let https = origin.scheme() == "https";
        let loopback = origin.host_str().is_some_and(is_loopback_host);
        if !(https || (origin.scheme() == "http" && loopback))
            || !origin.username().is_empty()
            || origin.password().is_some()
            || origin.query().is_some()
            || origin.fragment().is_some()
        {
            return Err(ProductError::Configuration(
                "The license service must use HTTPS or be on this computer.",
            ));
        }
        origin.set_path("/");
        let mut builder = reqwest::blocking::Client::builder()
            .https_only(https)
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(15));
        if loopback {
            builder = builder.no_proxy();
        }
        let http = builder.build().map_err(|_| ProductError::Redemption)?;
        Ok(Self { origin, http })
    }

    fn redeem(
        &self,
        code: &ClaimCode,
        state: &Path,
        storage: &RecoveryStorage,
    ) -> Result<LicenseInput, ProductError> {
        let recovery = (code.storefront == Storefront::Etsy)
            .then(|| storage.token_for(code, state))
            .transpose()?;
        let recovery_token = recovery.as_ref().map(|token| token.0.expose_secret());
        let request = RedemptionRequest {
            storefront: code.storefront,
            key: code.secret.expose_secret(),
            recovery_token,
        };
        let result = self.send("/api/v1/licenses/redeem", &request);
        // A retry after a lost response can recover the already redeemed license.
        if matches!(result, Err(ProductError::Redemption))
            && let Some(recovery_token) = recovery_token
        {
            return self.send(
                "/api/v1/licenses/recover",
                &RecoveryRequest { recovery_token },
            );
        }
        result
    }

    fn send(&self, path: &str, request: &impl Serialize) -> Result<LicenseInput, ProductError> {
        let mut url = self.origin.clone();
        url.set_path(path);
        let response = self
            .http
            .post(url)
            .json(request)
            .send()
            .map_err(|_| ProductError::Redemption)?;
        if !response.status().is_success() {
            return Err(ProductError::Redemption);
        }
        read_license(response).map_err(|_| ProductError::Redemption)
    }
}

fn is_loopback_host(host: &str) -> bool {
    host == "localhost"
        || host
            .trim_matches(['[', ']'])
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}
