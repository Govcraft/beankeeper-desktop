//! Typed license returns, isolated from books and recovery material.
use super::{
    ProductError,
    activation::{Storefront, service_url::LicenseServiceUrl},
};
use crate::{
    license::{self, LicenseId},
    private_file::PrivateFile,
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use chrono::{DateTime, Utc};
use mti::prelude::MagicTypeId;
use schemars::JsonSchema;
use secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Serialize};
use std::{
    fmt, fs,
    io::{self, Read},
    path::Path,
    str::FromStr,
    time::Duration,
};

pub(crate) const POLICY: &str = "Returns within 60 days of purchase are refunded automatically. If the 60-day window has closed, the return is not accepted, your license stays active and nothing changes. Confirming requests the return and refund. Paid features stop on this computer on success. Your books stay yours and readable. Your books are never touched.";

#[derive(Clone, Copy, Default, Deserialize, JsonSchema)]
#[serde(transparent)]
pub(crate) struct ReturnConfirmation(bool);
impl From<bool> for ReturnConfirmation {
    fn from(value: bool) -> Self {
        Self(value)
    }
}
impl ReturnConfirmation {
    pub(crate) fn is_confirmed(self) -> bool {
        self.0
    }
    fn confirmed(self) -> Option<ConfirmedReturn> {
        self.0.then_some(ConfirmedReturn)
    }
}
impl FromStr for ReturnConfirmation {
    type Err = std::convert::Infallible;
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Ok(Self(matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "y" | "yes"
        )))
    }
}

struct ConfirmedReturn;

pub(crate) struct ReturnRequest {
    license_id: Option<LicenseId>,
    confirmation: ReturnConfirmation,
}
impl ReturnRequest {
    pub(crate) fn new(license_id: Option<LicenseId>, confirmation: ReturnConfirmation) -> Self {
        Self {
            license_id,
            confirmation,
        }
    }
    pub(crate) fn execute(self) -> ReturnResponse {
        let Some(confirmed) = self.confirmation.confirmed() else {
            return Outcome::ConfirmationRequired.into();
        };
        let result = (|| {
            let state = super::state_dir().map_err(|_| Outcome::LocalUnavailable)?;
            let service = ReturnService::configured()?;
            self.execute_at(&state, &service, confirmed)
        })();
        result.unwrap_or_else(Into::into)
    }
    fn execute_at(
        self,
        state: &Path,
        service: &ReturnService,
        confirmed: ConfirmedReturn,
    ) -> Result<ReturnResponse, Outcome> {
        let (id, selected_proof) = if let Some(id) = self.license_id {
            (id, None)
        } else {
            let paths = license::activation_paths(state).map_err(|_| Outcome::LocalUnavailable)?;
            let [path] = paths.as_slice() else {
                return Err(Outcome::ChooseLicense);
            };
            let proof = Proof::read(path)?;
            (proof.id.clone(), Some(proof))
        };
        let event = state.join("license-returns").join(format!("{id}.json"));
        if event.exists() {
            let bytes =
                super::bounded_file(&event, 16_384).map_err(|_| Outcome::LocalUnavailable)?;
            let receipt: Receipt =
                serde_json::from_slice(&bytes).map_err(|_| Outcome::LocalUnavailable)?;
            remove_activation(state, &id)?;
            return Ok(receipt.into());
        }
        let proof = match selected_proof {
            Some(proof) => proof,
            None => Proof::read(&license::activation_path(state, &id))?,
        };
        if proof.id != id {
            return Err(Outcome::InvalidLicenseProof);
        }
        finish_return(state, &proof, &event, service, confirmed)
    }
}
fn finish_return(
    state: &Path,
    proof: &Proof,
    event: &Path,
    service: &ReturnService,
    confirmed: ConfirmedReturn,
) -> Result<ReturnResponse, Outcome> {
    let receipt = service.return_license(proof, confirmed)?;
    commit_receipt(state, &proof.id, event, &receipt)?;
    Ok(receipt.into())
}

fn commit_receipt(
    state: &Path,
    id: &LicenseId,
    event: &Path,
    receipt: &Receipt,
) -> Result<(), Outcome> {
    // This immutable receipt is the installation's durable license-return audit event.
    // Write it before removing the activation so a local I/O failure is recoverable.
    let directory = event.parent().ok_or(Outcome::LocalUnavailable)?;
    fs::create_dir_all(directory).map_err(|_| Outcome::LocalUnavailable)?;
    crate::setup::keychain::restrict_directory(directory).map_err(|_| Outcome::LocalUnavailable)?;
    let mut pending =
        tempfile::NamedTempFile::new_in(directory).map_err(|_| Outcome::LocalUnavailable)?;
    crate::setup::keychain::restrict_file(pending.path()).map_err(|_| Outcome::LocalUnavailable)?;
    serde_json::to_writer(&mut pending, &receipt).map_err(|_| Outcome::LocalUnavailable)?;
    pending
        .as_file()
        .sync_all()
        .map_err(|_| Outcome::LocalUnavailable)?;
    match pending.persist_noclobber(event) {
        Ok(_) => {}
        Err(error) if error.error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(_) => return Err(Outcome::LocalUnavailable),
    }
    remove_activation(state, id)
}
fn remove_activation(state: &Path, id: &LicenseId) -> Result<(), Outcome> {
    match fs::remove_file(license::activation_path(state, id)) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(_) => Err(Outcome::LocalUnavailable),
    }
}
struct Proof {
    id: LicenseId,
    token: SecretString,
}
impl Proof {
    fn read(path: &Path) -> Result<Self, Outcome> {
        let token = PrivateFile::open(path)
            .and_then(|file| file.read_secret(license::MAX_LICENSE_INPUT))
            .map_err(|_| Outcome::LocalUnavailable)?;
        let token = SecretString::from(token.expose_secret().trim());
        let keys = super::trusted_keys().map_err(|_| Outcome::InvalidLicenseProof)?;
        let verified = license::verify_offline(token.expose_secret(), &keys)
            .map_err(|_| Outcome::InvalidLicenseProof)?;
        Ok(Self {
            id: verified.id().clone(),
            token,
        })
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
struct ReceiptId(MagicTypeId);
impl FromStr for ReceiptId {
    type Err = Outcome;
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let id = MagicTypeId::from_str(value).map_err(|_| Outcome::ServiceUnavailable)?;
        if id.prefix().as_str() != "ret" || id != value {
            return Err(Outcome::ServiceUnavailable);
        }
        Ok(Self(id))
    }
}
impl TryFrom<String> for ReceiptId {
    type Error = Outcome;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        value.parse()
    }
}
impl From<ReceiptId> for String {
    fn from(value: ReceiptId) -> Self {
        value.0.to_string()
    }
}
impl fmt::Display for ReceiptId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

#[derive(Deserialize, Serialize)]
#[serde(try_from = "String", into = "String")]
struct Challenge([u8; 32]);
impl FromStr for Challenge {
    type Err = Outcome;
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let mut bytes = [0; 32];
        if value.len() != 43 || URL_SAFE_NO_PAD.decode_slice(value, &mut bytes).ok() != Some(32) {
            return Err(Outcome::ServiceUnavailable);
        }
        Ok(Self(bytes))
    }
}
impl TryFrom<String> for Challenge {
    type Error = Outcome;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        value.parse()
    }
}
impl Clone for Challenge {
    fn clone(&self) -> Self {
        Self(self.0)
    }
}
impl From<Challenge> for String {
    fn from(value: Challenge) -> Self {
        URL_SAFE_NO_PAD.encode(value.0)
    }
}
#[derive(Deserialize)]
struct ChallengeResponse {
    challenge: Challenge,
    #[serde(rename = "expires_at")]
    _expires_at: DateTime<Utc>,
}
#[derive(Serialize)]
struct ReturnBody<'a> {
    license: &'a str,
    challenge: &'a Challenge,
}
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum RefundStatus {
    Pending,
    Submitted,
}
#[derive(Serialize, Deserialize)]
struct Receipt {
    #[serde(rename = "receipt_id")]
    id: ReceiptId,
    returned_at: DateTime<Utc>,
    storefront: Storefront,
    refund_status: RefundStatus,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum Outcome {
    ConfirmationRequired,
    Returned,
    ReturnWindowClosed,
    ReturnNotApplicable,
    InvalidLicenseProof,
    LicenseRevoked,
    RefundUnavailable,
    ServiceUnavailable,
    LocalUnavailable,
    ChooseLicense,
}
impl Outcome {
    fn message(self) -> &'static str {
        match self {
            Self::ConfirmationRequired => POLICY,
            Self::Returned => {
                "Your license was returned. Paid features have stopped on this computer. Your books stay yours and readable and were never touched."
            }
            Self::ReturnWindowClosed => {
                "The 60-day refund window has closed; your license is unchanged and still active."
            }
            Self::ReturnNotApplicable => {
                "This complimentary license has no refund or in-app return. Contact support@beankeeper.app. Your license is unchanged."
            }
            Self::InvalidLicenseProof => {
                "Your license or return request could not be verified. Your local license was kept. Contact support@beankeeper.app."
            }
            Self::LicenseRevoked => {
                "This license was already returned or replaced. Your local license was kept. Contact support@beankeeper.app if you need help."
            }
            Self::RefundUnavailable => {
                "The refund is unavailable right now. Your license is still active. Try again later or contact support@beankeeper.app."
            }
            Self::ServiceUnavailable => {
                "The return service could not complete your request. Your local license was kept. Try again or contact support@beankeeper.app."
            }
            Self::LocalUnavailable => {
                "Desktop could not finish updating its local license records. Your books were never touched. Try again or contact support@beankeeper.app."
            }
            Self::ChooseLicense => {
                "Choose the license to return using license_id. Use license_status to see your installed licenses."
            }
        }
    }
}
impl fmt::Display for Outcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.message())
    }
}
impl std::error::Error for Outcome {}

#[derive(Serialize)]
pub(crate) struct ReturnResponse {
    desktop_path: &'static str,
    code: Outcome,
    message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    receipt: Option<Receipt>,
}
impl From<Outcome> for ReturnResponse {
    fn from(code: Outcome) -> Self {
        Self {
            desktop_path: "/settings",
            code,
            message: if code == Outcome::ConfirmationRequired {
                format!(
                    "{POLICY} To request the return and refund, call license_return again with confirm: true, or use bk license return --yes."
                )
            } else {
                code.message().to_owned()
            },
            receipt: None,
        }
    }
}
impl From<Receipt> for ReturnResponse {
    fn from(receipt: Receipt) -> Self {
        let message = match receipt.refund_status {
            RefundStatus::Pending => {
                "Your return was recorded; the refund is being submitted automatically. Paid features have stopped on this computer. Your books stay yours and readable and were never touched."
            }
            RefundStatus::Submitted => {
                "Your license was returned and the refund was submitted. Paid features have stopped on this computer. Your books stay yours and readable and were never touched."
            }
        };
        Self {
            desktop_path: "/settings",
            code: Outcome::Returned,
            message: format!("{message} Receipt: {}.", receipt.id),
            receipt: Some(receipt),
        }
    }
}
impl ReturnResponse {
    pub(crate) fn message(&self) -> &str {
        &self.message
    }
    pub(crate) fn is_error(&self) -> bool {
        !matches!(self.code, Outcome::Returned | Outcome::ConfirmationRequired)
    }
    pub(crate) fn needs_confirmation(&self) -> bool {
        self.code == Outcome::ConfirmationRequired
    }
}

struct ReturnService {
    origin: LicenseServiceUrl,
    http: reqwest::blocking::Client,
}
impl ReturnService {
    fn configured() -> Result<Self, Outcome> {
        let configured = std::env::var("BEANKEEPER_LICENSE_SERVICE_URL")
            .ok()
            .filter(|url| !url.is_empty())
            .or_else(|| {
                super::embedded_license_trust()
                    .service_url
                    .map(str::to_owned)
            })
            .ok_or(Outcome::ServiceUnavailable)?;
        Self::try_from(configured.as_str())
    }
    fn return_license(
        &self,
        proof: &Proof,
        _confirmed: ConfirmedReturn,
    ) -> Result<Receipt, Outcome> {
        let challenge: ChallengeResponse = self.post(
            "/api/v1/licenses/return/challenge",
            &proof.token.expose_secret(),
        )?;
        let body = ReturnBody {
            license: proof.token.expose_secret(),
            challenge: &challenge.challenge,
        };
        let result = self.post("/api/v1/licenses/return", &body);
        match result {
            Err(Outcome::ServiceUnavailable) => self.post("/api/v1/licenses/return", &body),
            other => other,
        }
    }
    fn post<T: serde::de::DeserializeOwned>(
        &self,
        path: &str,
        body: &impl Serialize,
    ) -> Result<T, Outcome> {
        let response = self
            .http
            .post(self.origin.endpoint(path))
            .json(body)
            .send()
            .map_err(|_| Outcome::ServiceUnavailable)?;
        let status = response.status().as_u16();
        let mut bytes = Vec::new();
        response
            .take(16_385)
            .read_to_end(&mut bytes)
            .map_err(|_| Outcome::ServiceUnavailable)?;
        if bytes.len() > 16_384 {
            return Err(Outcome::ServiceUnavailable);
        }
        if status == 200 {
            return serde_json::from_slice(&bytes).map_err(|_| Outcome::ServiceUnavailable);
        }
        Err(map_failure(status, &bytes))
    }
}
impl TryFrom<&str> for ReturnService {
    type Error = Outcome;
    fn try_from(value: &str) -> Result<Self, Self::Error> {
        let origin: LicenseServiceUrl = value
            .parse()
            .map_err(|_: ProductError| Outcome::ServiceUnavailable)?;
        let mut builder = reqwest::blocking::Client::builder()
            .https_only(origin.requires_https())
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(15));
        if origin.bypasses_proxy() {
            builder = builder.no_proxy();
        }
        let http = builder.build().map_err(|_| Outcome::ServiceUnavailable)?;
        Ok(Self { origin, http })
    }
}
#[derive(Deserialize)]
struct FailureBody {
    code: Outcome,
}
fn map_failure(status: u16, bytes: &[u8]) -> Outcome {
    let Ok(FailureBody { code }) = serde_json::from_slice(bytes) else {
        return Outcome::ServiceUnavailable;
    };
    match (status, code) {
        (401, Outcome::InvalidLicenseProof)
        | (403, Outcome::LicenseRevoked)
        | (422, Outcome::ReturnWindowClosed | Outcome::ReturnNotApplicable)
        | (503, Outcome::RefundUnavailable) => code,
        _ => Outcome::ServiceUnavailable,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io::{BufRead, BufReader, Write},
        net::TcpListener,
        thread,
    };
    const RECEIPT: &str = r#"{"receipt_id":"ret_01h455vb4pex5vsknk084sn02q","returned_at":"2026-11-26T00:00:00Z","storefront":"polar","refund_status":"pending"}"#;

    #[test]
    fn every_server_failure_has_a_closed_typed_outcome() {
        for (status, code, expected) in [
            (401, "invalid_license_proof", Outcome::InvalidLicenseProof),
            (403, "license_revoked", Outcome::LicenseRevoked),
            (422, "return_window_closed", Outcome::ReturnWindowClosed),
            (422, "return_not_applicable", Outcome::ReturnNotApplicable),
            (503, "refund_unavailable", Outcome::RefundUnavailable),
        ] {
            assert_eq!(
                map_failure(
                    status,
                    format!(r#"{{"code":"{code}","message":"ignored untrusted message"}}"#)
                        .as_bytes()
                ),
                expected
            );
        }
        assert_eq!(map_failure(422, b"{}"), Outcome::ServiceUnavailable);
        assert_eq!(
            map_failure(200, br#"{"code":"license_revoked"}"#),
            Outcome::ServiceUnavailable
        );
        assert_eq!(
            map_failure(401, br#"{"code":"unknown"}"#),
            Outcome::ServiceUnavailable
        );
    }
    #[test]
    fn confirmation_defaults_to_no_and_does_no_work() {
        let response = ReturnRequest::new(None, ReturnConfirmation::default()).execute();
        assert!(response.needs_confirmation());
        assert!(response.message().starts_with(POLICY));
        assert!(!ReturnConfirmation::default().is_confirmed());
        assert!(
            "YES"
                .parse::<ReturnConfirmation>()
                .is_ok_and(ReturnConfirmation::is_confirmed)
        );
        assert!(
            "no".parse::<ReturnConfirmation>()
                .is_ok_and(|value| !value.is_confirmed())
        );
    }
    #[test]
    fn receipt_and_challenge_validate_at_the_boundary() -> Result<(), Box<dyn std::error::Error>> {
        let receipt: Receipt = serde_json::from_str(RECEIPT)?;
        assert_eq!(receipt.id.to_string(), "ret_01h455vb4pex5vsknk084sn02q");
        assert!(serde_json::from_str::<Receipt>(&RECEIPT.replace("ret_", "lic_")).is_err());
        assert!(serde_json::from_str::<Receipt>(&RECEIPT.replace("pending", "unknown")).is_err());
        assert!(
            serde_json::from_str::<Receipt>(&RECEIPT.replace("2026-11-26T00:00:00Z", "yesterday"))
                .is_err()
        );
        assert!("invalid".parse::<Challenge>().is_err());
        assert!(
            ReturnResponse::from(receipt)
                .message()
                .contains("submitted automatically")
        );
        let submitted: Receipt = serde_json::from_str(&RECEIPT.replace("pending", "submitted"))?;
        assert!(
            ReturnResponse::from(submitted)
                .message()
                .contains("refund was submitted")
        );
        Ok(())
    }

    fn mock(
        replies: Vec<(u16, String)>,
    ) -> Result<(ReturnService, thread::JoinHandle<()>), Box<dyn std::error::Error>> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let service =
            ReturnService::try_from(format!("http://{}", listener.local_addr()?).as_str())?;
        let worker = thread::spawn(move || {
            for (index, (status, body)) in replies.into_iter().enumerate() {
                let (mut stream, _) = listener.accept().expect("accept request");
                let mut reader = BufReader::new(stream.try_clone().expect("clone stream"));
                let mut line = String::new();
                reader.read_line(&mut line).expect("request line");
                assert!(line.starts_with(if index == 0 {
                    "POST /api/v1/licenses/return/challenge "
                } else {
                    "POST /api/v1/licenses/return "
                }));
                let mut length = 0;
                loop {
                    line.clear();
                    reader.read_line(&mut line).expect("headers");
                    if line == "\r\n" {
                        break;
                    }
                    if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                        length = value.trim().parse::<usize>().expect("length");
                    }
                }
                let mut request = vec![0; length];
                reader.read_exact(&mut request).expect("body");
                let value: serde_json::Value =
                    serde_json::from_slice(&request).expect("typed JSON request");
                if index == 0 {
                    assert!(value.is_string());
                } else {
                    assert_eq!(value["challenge"], URL_SAFE_NO_PAD.encode([7; 32]));
                }
                write!(stream, "HTTP/1.1 {status} Mock\r\nContent-Length: {}\r\nConnection: close\r\nContent-Type: application/json\r\n\r\n{body}", body.len()).expect("reply");
            }
        });
        Ok((service, worker))
    }
    fn challenge() -> String {
        format!(
            r#"{{"challenge":"{}","expires_at":"2026-11-26T00:05:00Z"}}"#,
            URL_SAFE_NO_PAD.encode([7; 32])
        )
    }
    fn proof() -> Result<Proof, Box<dyn std::error::Error>> {
        Ok(Proof {
            id: "lic_01h455vb4pex5vsknk084sn02q".parse()?,
            token: SecretString::from("private-license"),
        })
    }

    #[test]
    fn mock_success_removes_only_license_and_records_receipt_and_replays()
    -> Result<(), Box<dyn std::error::Error>> {
        let state = tempfile::tempdir()?;
        let proof = proof()?;
        let activation = license::activation_path(state.path(), &proof.id);
        fs::create_dir_all(activation.parent().ok_or("activation parent")?)?;
        fs::write(&activation, "private-license")?;
        fs::write(state.path().join("books"), "untouched")?;
        fs::write(state.path().join("recovery-kit"), "untouched")?;
        let (service, worker) = mock(vec![
            (200, challenge()),
            (200, RECEIPT.into()),
            (200, RECEIPT.into()),
        ])?;
        let event = state
            .path()
            .join("license-returns")
            .join(format!("{}.json", proof.id));
        let response = finish_return(state.path(), &proof, &event, &service, ConfirmedReturn)?;
        let receipt = response.receipt.ok_or("receipt missing")?;
        let replay: Receipt = service.post(
            "/api/v1/licenses/return",
            &ReturnBody {
                license: proof.token.expose_secret(),
                challenge: &URL_SAFE_NO_PAD.encode([7; 32]).parse()?,
            },
        )?;
        assert_eq!(receipt.id.to_string(), replay.id.to_string());
        let event = state
            .path()
            .join("license-returns")
            .join(format!("{}.json", proof.id));
        commit_receipt(state.path(), &proof.id, &event, &receipt)?;
        commit_receipt(state.path(), &proof.id, &event, &replay)?;
        assert!(!activation.exists());
        assert!(fs::read_to_string(event)?.contains("ret_01h455vb4pex5vsknk084sn02q"));
        assert_eq!(fs::read_to_string(state.path().join("books"))?, "untouched");
        assert_eq!(
            fs::read_to_string(state.path().join("recovery-kit"))?,
            "untouched"
        );
        worker.join().map_err(|_| "mock panicked")?;
        Ok(())
    }
    #[test]
    fn mock_all_failures_keep_local_license() -> Result<(), Box<dyn std::error::Error>> {
        for (status, code) in [
            (401, "invalid_license_proof"),
            (403, "license_revoked"),
            (422, "return_window_closed"),
            (422, "return_not_applicable"),
            (503, "refund_unavailable"),
        ] {
            let state = tempfile::tempdir()?;
            let proof = proof()?;
            let activation = license::activation_path(state.path(), &proof.id);
            fs::create_dir_all(activation.parent().ok_or("activation parent")?)?;
            fs::write(&activation, "private-license")?;
            let (service, worker) = mock(vec![
                (200, challenge()),
                (status, format!(r#"{{"code":"{code}"}}"#)),
            ])?;
            let event = state
                .path()
                .join("license-returns")
                .join(format!("{}.json", proof.id));
            assert!(
                finish_return(state.path(), &proof, &event, &service, ConfirmedReturn).is_err()
            );
            assert!(!event.exists());
            assert_eq!(fs::read_to_string(activation)?, "private-license");
            worker.join().map_err(|_| "mock panicked")?;
        }
        Ok(())
    }
}
