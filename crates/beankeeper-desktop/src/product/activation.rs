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

pub(in crate::product) mod service_url;
use service_url::LicenseServiceUrl;

const CODE_LIMIT: usize = 512;
const RECOVERY_MIN: usize = 32;

/// Storefront adapters supported for online license redemption.
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum, Deserialize, JsonSchema, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Storefront {
    /// Polar license key.
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
    Trial,
    Stdin,
    File(LicensePath),
    Claim(ClaimCode),
    CodeFile(CodeFile),
}

struct CodeFile {
    path: PathBuf,
    storefront: Option<Storefront>,
}

impl From<(PathBuf, Option<Storefront>)> for CodeFile {
    fn from((path, storefront): (PathBuf, Option<Storefront>)) -> Self {
        Self { path, storefront }
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
    pub(super) fn trial() -> Self {
        Self(Source::Trial)
    }

    pub(super) fn verify(
        self,
        keys: &TrustedKeys,
        state: &Path,
    ) -> Result<VerifiedLicense, ProductError> {
        let input = match self.0 {
            Source::Trial => {
                let input = LicenseService::configured()?.trial(state)?;
                let license = verify_offline(input, keys)?;
                return match license.entitlement() {
                    crate::license::Entitlement::Trial { .. } => Ok(license),
                    crate::license::Entitlement::Perpetual { .. } => {
                        Err(ProductError::TrialUnavailable)
                    }
                };
            }
            Source::Pasted(token) => LicenseInput::Pasted(token),
            Source::Stdin => read_license(io::stdin())?,
            Source::File(path) => path.read()?,
            Source::Claim(code) => {
                LicenseService::configured()?.redeem(&code, state, &RecoveryStorage::Desktop)?
            }
            Source::CodeFile(file) => {
                let client = LicenseService::configured()?;
                let secret = read_secret(&file.path)?;
                let code = ClaimCode::try_from((secret.expose_secret(), file.storefront))?;
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

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum IssuerErrorCode {
    TrialAlreadyUsed,
    TrialExpired,
    TrialRateLimited,
    ReturnNotApplicable,
    CompRequestConflict,
    InvalidLicenseProof,
    ReturnWindowClosed,
    RefundUnavailable,
    UpdateIneligible,
    LicenseRevoked,
    UnsupportedLicenseFormat,
    UnsupportedCheckoutProduct,
    #[serde(other)]
    Unknown,
}
#[derive(Serialize)]
struct TrialRequest<'a> {
    machine_token: &'a str,
}
impl<'a> From<&'a MachineToken> for TrialRequest<'a> {
    fn from(token: &'a MachineToken) -> Self {
        Self {
            machine_token: token.as_ref(),
        }
    }
}

struct LicenseService {
    origin: LicenseServiceUrl,
    http: reqwest::blocking::Client,
}

impl LicenseService {
    fn trial(&self, state: &Path) -> Result<LicenseInput, ProductError> {
        let token = MachineToken::load_or_create(state)?;
        let response = self
            .http
            .post(self.origin.endpoint("/api/v1/licenses/trial"))
            .json(&TrialRequest::from(&token))
            .send()
            .map_err(|_| ProductError::TrialUnavailable)?;
        match response.status().as_u16() {
            200 => read_license(response).map_err(|_| ProductError::TrialUnavailable),
            409 => {
                #[derive(Deserialize)]
                struct Conflict {
                    code: IssuerErrorCode,
                }
                let mut body = Vec::new();
                response
                    .take(4097)
                    .read_to_end(&mut body)
                    .map_err(|_| ProductError::TrialUnavailable)?;
                match serde_json::from_slice::<Conflict>(&body) {
                    Ok(Conflict {
                        code: IssuerErrorCode::TrialAlreadyUsed,
                    }) => Err(ProductError::TrialAlreadyUsed),
                    _ => Err(ProductError::TrialUnavailable),
                }
            }
            429 => {
                let retry = response
                    .headers()
                    .get(reqwest::header::RETRY_AFTER)
                    .and_then(|value| value.try_into().ok());
                Err(ProductError::TrialRateLimited(retry))
            }
            _ => Err(ProductError::TrialUnavailable),
        }
    }

    fn configured() -> Result<Self, ProductError> {
        let configured_url = std::env::var("BEANKEEPER_LICENSE_SERVICE_URL")
            .ok()
            .filter(|url| !url.is_empty())
            .or_else(|| {
                super::embedded_license_trust()
                    .service_url
                    .map(str::to_owned)
            })
            .ok_or(ProductError::ServiceUrlMissing)?;
        let origin: LicenseServiceUrl = configured_url.parse()?;
        let mut builder = reqwest::blocking::Client::builder()
            .https_only(origin.requires_https())
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(15));
        if origin.bypasses_proxy() {
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
        let response = self
            .http
            .post(self.origin.endpoint(path))
            .json(request)
            .send()
            .map_err(|_| ProductError::Redemption)?;
        if !response.status().is_success() {
            return Err(ProductError::Redemption);
        }
        read_license(response).map_err(|_| ProductError::Redemption)
    }
}

/// Opaque random installation identity, never derived from hardware.
struct MachineToken(SecretString);
impl std::str::FromStr for MachineToken {
    type Err = ProductError;
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if !(32..=256).contains(&value.len())
            || value.trim() != value
            || value.chars().any(char::is_control)
        {
            return Err(ProductError::MachineIdentity);
        }
        Ok(Self(value.into()))
    }
}
impl AsRef<str> for MachineToken {
    fn as_ref(&self) -> &str {
        self.0.expose_secret()
    }
}
impl MachineToken {
    fn read(file: PrivateFile) -> Result<Self, ProductError> {
        file.read_secret(256)?.expose_secret().parse()
    }

    fn load_or_create(state: &Path) -> Result<Self, ProductError> {
        let path = state.join("machine-token");
        match PrivateFile::open(&path) {
            Ok(file) => return Self::read(file),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
        let mut random = Zeroizing::new([0; 32]);
        getrandom::fill(&mut *random).map_err(|_| ProductError::MachineIdentity)?;
        let encoded = Zeroizing::new(URL_SAFE_NO_PAD.encode(*random));
        let token: Self = encoded.parse()?;
        match crate::license::write_private(
            token.as_ref().as_bytes(),
            &path,
            crate::license::SaveMode::Create,
        ) {
            Ok(()) => Ok(token),
            Err(crate::license::LicenseError::Io(e))
                if e.kind() == io::ErrorKind::AlreadyExists =>
            {
                Self::read(PrivateFile::open(&path)?)
            }
            Err(e) => Err(e.into()),
        }
    }
}

#[cfg(test)]
mod trial_tests {
    use super::*;
    use std::{net::TcpListener, thread};

    #[test]
    fn machine_identity_is_private_persistent_and_validated()
    -> Result<(), Box<dyn std::error::Error>> {
        let state = tempfile::tempdir()?;
        let first = MachineToken::load_or_create(state.path())?;
        let next = MachineToken::load_or_create(state.path())?;
        assert!(first.as_ref().eq(next.as_ref()));
        assert!(matches!(
            "short".parse::<MachineToken>(),
            Err(ProductError::MachineIdentity)
        ));
        assert!(
            format!(" {}", first.as_ref())
                .parse::<MachineToken>()
                .is_err()
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(state.path().join("machine-token"))?
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
        Ok(())
    }

    fn mock(
        status: u16,
        body: &str,
        extra: &str,
    ) -> Result<(LicenseService, thread::JoinHandle<String>), Box<dyn std::error::Error>> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let url = format!("http://{}", listener.local_addr()?);
        let response = format!(
            "HTTP/1.1 {status} Mock\r\nContent-Length: {}\r\nConnection: close\r\n{extra}\r\n{body}",
            body.len()
        );
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            stream
                .set_read_timeout(Some(Duration::from_secs(3)))
                .expect("timeout");
            let mut bytes = Vec::new();
            let mut buffer = [0; 1024];
            loop {
                let count = stream.read(&mut buffer).expect("request");
                if count == 0 {
                    break;
                }
                bytes.extend_from_slice(&buffer[..count]);
                if let Some(end) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                    let header = String::from_utf8_lossy(&bytes[..end]);
                    let length = header
                        .lines()
                        .find_map(|line| {
                            line.to_ascii_lowercase()
                                .strip_prefix("content-length: ")
                                .and_then(|v| v.parse::<usize>().ok())
                        })
                        .unwrap_or_default();
                    if bytes.len() >= end + 4 + length {
                        break;
                    }
                }
            }
            stream.write_all(response.as_bytes()).expect("reply");
            String::from_utf8(bytes).expect("utf8")
        });
        Ok((
            LicenseService {
                origin: url.parse()?,
                http: reqwest::blocking::Client::builder()
                    .no_proxy()
                    .redirect(reqwest::redirect::Policy::none())
                    .build()?,
            },
            handle,
        ))
    }

    #[test]
    fn trial_service_reuses_identity_and_maps_safe_retry_errors()
    -> Result<(), Box<dyn std::error::Error>> {
        let state = tempfile::tempdir()?;
        let token = MachineToken::load_or_create(state.path())?;
        for (status, body, extra) in [
            (200, "signed-test-response", ""),
            (409, "{\"code\":\"trial_already_used\"}", ""),
            (409, "{\"code\":\"future_issuer_code\"}", ""),
            (409, "{\"code\":\"comp_request_conflict\"}", ""),
            (409, "invalid JSON", ""),
            (
                429,
                "{\"error\":\"trial_rate_limited\"}",
                "Retry-After: 60\r\n",
            ),
            (503, "private server details", ""),
        ] {
            let (service, server) = mock(status, body, extra)?;
            let result = service.trial(state.path());
            match status {
                200 => assert_eq!(result?.as_ref(), body.as_bytes()),
                409 if body.contains("trial_already_used") => {
                    assert!(matches!(result, Err(ProductError::TrialAlreadyUsed)));
                }
                429 => assert!(matches!(
                    result,
                    Err(ProductError::TrialRateLimited(Some(retry))) if retry == "60".parse()?
                )),
                _ => assert!(matches!(result, Err(ProductError::TrialUnavailable))),
            }
            let request = server.join().map_err(|_| "mock panic")?;
            assert!(request.starts_with("POST /api/v1/licenses/trial "));
            let (_, body) = request.split_once("\r\n\r\n").ok_or("body")?;
            let sent: serde_json::Value = serde_json::from_str(body)?;
            assert!(
                sent["machine_token"]
                    .as_str()
                    .is_some_and(|value| value.eq(token.as_ref()))
            );
            assert_eq!(sent.as_object().ok_or("object")?.len(), 1);
        }
        Ok(())
    }
}
