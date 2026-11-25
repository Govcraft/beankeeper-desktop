use std::{collections::HashMap, time::Instant};

use axum::body::Bytes;
use base64::{Engine as _, prelude::BASE64_URL_SAFE_NO_PAD};
use num_traits::ToPrimitive;
use serde::{Deserialize, Serialize};
use serde_json::{Number, Value, json};
use sha2::{Digest, Sha256};

use super::{
    ImportError, Principal, Ticket,
    request::{FileImport, ReadyImport},
};

pub(super) const MAX_BYTES: usize = 1024 * 1024;
pub(crate) const FILES: &str = "io.modelcontextprotocol/files";
pub(super) const UPLOAD_ROUTE: &str = "/mcp/statement-uploads/{ticket}";

pub(super) fn schema() -> Value {
    json!({
        "type": "object",
        "x-mcp-file": {"accept": [".csv", ".ofx", ".qfx"], "maxSize": MAX_BYTES},
        "required": ["id", "mimeType", "size"],
        "properties": {
            "id": {"type":"string", "minLength":1, "maxLength":StatementFileId::MAX_CHARS},
            "name": {"type":"string"},
            "mimeType": {"type":"string", "minLength":1},
            "size": {"type":"integer", "minimum":1, "maximum":MAX_BYTES},
            "digest": {
                "type":"object",
                "required":["algorithm","value"],
                "properties":{"algorithm":{"const":"sha-256"},"value":{"type":"string"}}
            }
        }
    })
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "Box<str>")]
struct StatementFileId(Box<str>);

impl StatementFileId {
    const MAX_CHARS: usize = 128;
}

impl TryFrom<Box<str>> for StatementFileId {
    type Error = &'static str;

    fn try_from(id: Box<str>) -> Result<Self, Self::Error> {
        if id.is_empty() || id.chars().take(Self::MAX_CHARS + 1).count() > Self::MAX_CHARS {
            return Err("Statement file ID must contain 1 to 128 characters.");
        }
        Ok(Self(id))
    }
}

#[derive(PartialEq, Eq, Deserialize)]
#[serde(try_from = "Box<str>")]
struct StatementMimeType(Box<str>);

impl TryFrom<Box<str>> for StatementMimeType {
    type Error = &'static str;

    fn try_from(value: Box<str>) -> Result<Self, Self::Error> {
        if value.is_empty() {
            return Err("Statement MIME type must be nonempty.");
        }
        Ok(Self(value))
    }
}

#[derive(PartialEq, Eq, Deserialize)]
#[serde(try_from = "Number")]
struct StatementSize(usize);

impl TryFrom<Number> for StatementSize {
    type Error = &'static str;

    fn try_from(value: Number) -> Result<Self, Self::Error> {
        let size = value.as_u64().or_else(|| {
            let number = value.as_f64()?;
            if number.fract() == 0.0 {
                number.to_u32().map(u64::from)
            } else {
                None
            }
        });
        size.and_then(|size| usize::try_from(size).ok())
            .ok_or(Self::ERROR)?
            .try_into()
    }
}

impl StatementSize {
    const ERROR: &str = "Expected a nonempty statement of at most 1 MiB.";
}

impl TryFrom<usize> for StatementSize {
    type Error = &'static str;

    fn try_from(size: usize) -> Result<Self, Self::Error> {
        if (1..=MAX_BYTES).contains(&size) {
            Ok(Self(size))
        } else {
            Err(Self::ERROR)
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FileDigest {
    algorithm: Sha256Algorithm,
    value: String,
}

#[derive(Deserialize)]
enum Sha256Algorithm {
    #[serde(rename = "sha-256")]
    Sha256,
}

#[derive(PartialEq, Eq, Deserialize)]
#[serde(try_from = "FileDigest")]
struct StatementDigest([u8; 32]);

impl TryFrom<FileDigest> for StatementDigest {
    type Error = &'static str;

    fn try_from(input: FileDigest) -> Result<Self, Self::Error> {
        let FileDigest {
            algorithm: Sha256Algorithm::Sha256,
            value,
        } = input;
        BASE64_URL_SAFE_NO_PAD
            .decode(value)
            .ok()
            .and_then(|bytes| bytes.try_into().ok())
            .map(Self)
            .ok_or("Invalid SHA-256 digest.")
    }
}

#[derive(PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct FileSpec {
    id: StatementFileId,
    name: Option<Box<str>>,
    mime_type: StatementMimeType,
    size: StatementSize,
    digest: Option<StatementDigest>,
}

/// Immutable UTF-8 statement within the size limit. Filenames never become paths.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "Box<str>")]
pub(super) struct StatementText(Box<str>);

impl TryFrom<Box<str>> for StatementText {
    type Error = &'static str;

    fn try_from(text: Box<str>) -> Result<Self, Self::Error> {
        StatementSize::try_from(text.len())?;
        Ok(Self(text))
    }
}

impl StatementText {
    const TASK_THRESHOLD: usize = 64 * 1024;

    pub(super) fn uses_background_task(&self) -> bool {
        self.0.len() >= Self::TASK_THRESHOLD
    }
}

impl TryFrom<(&FileSpec, Bytes)> for StatementText {
    type Error = ImportError;

    fn try_from((file, bytes): (&FileSpec, Bytes)) -> Result<Self, Self::Error> {
        if file.size.0 != bytes.len() {
            return Err(ImportError::Validation(
                "Uploaded size differs from the declared size.",
            ));
        }
        if file
            .digest
            .as_ref()
            .is_some_and(|expected| expected.0 != <[u8; 32]>::from(Sha256::digest(&bytes)))
        {
            return Err(ImportError::Validation(
                "Uploaded file does not match its SHA-256 digest.",
            ));
        }
        let text = std::str::from_utf8(&bytes)
            .map_err(|_| ImportError::Validation("Statement files must contain UTF-8 text."))?;
        Ok(Self(text.into()))
    }
}

#[derive(Serialize, Deserialize)]
enum UploadResultType {
    #[serde(rename = "upload_required")]
    UploadRequired,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct UploadTarget {
    file_id: StatementFileId,
    path: String,
    credentials: String,
    method: String,
    expires_at: String,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct UploadRequired {
    result_type: UploadResultType,
    uploads: [UploadTarget; 1],
    request_state: String,
}

struct Upload {
    principal: Principal,
    request: FileImport,
    transfer: Ticket,
    expires: Instant,
    text: Option<StatementText>,
}

#[derive(Default)]
pub(super) struct Uploads(HashMap<Ticket, Upload>);

impl Uploads {
    const CAPACITY: usize = 16;

    pub(super) fn prune(&mut self, now: Instant) {
        self.0.retain(|_, upload| now < upload.expires);
    }

    pub(super) fn issue(
        &mut self,
        principal: Principal,
        request: FileImport,
    ) -> Result<UploadRequired, ImportError> {
        let now = Instant::now();
        self.prune(now);
        if self.0.len() >= Self::CAPACITY {
            return Err(ImportError::Busy);
        }
        let (state, state_secret) = Ticket::generate()?;
        let (transfer, transfer_secret) = Ticket::generate()?;
        let result = UploadRequired {
            result_type: UploadResultType::UploadRequired,
            uploads: [UploadTarget {
                file_id: request.file().id.clone(),
                path: format!("/statement-uploads/{}", transfer_secret.expose()),
                credentials: "include".into(),
                method: "PUT".into(),
                expires_at: (chrono::Utc::now() + super::RETENTION).to_rfc3339(),
            }],
            request_state: state_secret.expose().into(),
        };
        self.0.insert(
            state,
            Upload {
                principal,
                request,
                transfer,
                expires: now + super::RETENTION,
                text: None,
            },
        );
        Ok(result)
    }

    fn target(
        &mut self,
        transfer: &Ticket,
        principal: &Principal,
    ) -> Result<&mut Upload, ImportError> {
        self.prune(Instant::now());
        self.0
            .values_mut()
            .find(|upload| &upload.transfer == transfer && &upload.principal == principal)
            .ok_or(ImportError::Unknown)
    }

    pub(super) fn expected_size(
        &mut self,
        transfer: &Ticket,
        principal: &Principal,
    ) -> Result<usize, ImportError> {
        Ok(self.target(transfer, principal)?.request.file().size.0)
    }

    pub(super) fn stage(
        &mut self,
        transfer: &Ticket,
        principal: &Principal,
        bytes: Bytes,
    ) -> Result<(), ImportError> {
        let upload = self.target(transfer, principal)?;
        let confirmed = StatementText::try_from((upload.request.file(), bytes))?;
        if let Some(staged) = &upload.text {
            if *staged == confirmed {
                return Ok(());
            }
            return Err(ImportError::Validation(
                "This target already received a different file.",
            ));
        }
        upload.text = Some(confirmed);
        Ok(())
    }

    pub(super) fn confirm(
        &mut self,
        state: &Ticket,
        principal: &Principal,
        request: &FileImport,
    ) -> Result<ReadyImport, ImportError> {
        self.prune(Instant::now());
        let upload = self
            .0
            .get(state)
            .filter(|upload| &upload.principal == principal && &upload.request == request)
            .ok_or(ImportError::Unknown)?;
        // Exact repeats are idempotent under the ledger's import reference.
        let text = upload.text.as_ref().ok_or(ImportError::Validation(
            "The statement file has not arrived.",
        ))?;
        Ok(upload.request.with_content(text.clone()))
    }
}
