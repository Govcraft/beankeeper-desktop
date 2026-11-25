use std::{collections::HashMap, time::Instant};

use axum::body::Bytes;
use base64::{Engine as _, prelude::BASE64_URL_SAFE_NO_PAD};
use serde::Deserialize;
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

use super::{ImportError, Principal, Ticket};

pub(super) const MAX_BYTES: usize = 1024 * 1024;
pub(crate) const FILES: &str = "io.modelcontextprotocol/files";
pub(super) const UPLOAD_ROUTE: &str = "/mcp/statement-uploads/{ticket}";

pub(super) fn schema() -> Value {
    json!({
        "type": "object",
        "x-mcp-file": {"accept": [".csv", ".ofx", ".qfx"], "maxSize": MAX_BYTES},
        "required": ["id", "mimeType", "size"],
        "properties": {
            "id": {"type":"string", "minLength":1, "maxLength":128},
            "name": {"type":"string"},
            "mimeType": {"type":"string"},
            "size": {"type":"integer", "minimum":1, "maximum":MAX_BYTES},
            "digest": {
                "type":"object",
                "required":["algorithm","value"],
                "properties":{"algorithm":{"const":"sha-256"},"value":{"type":"string"}}
            }
        }
    })
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct FileInput {
    id: String,
    mime_type: String,
    size: usize,
    digest: Option<FileDigest>,
}

#[derive(Deserialize)]
struct FileDigest {
    algorithm: Sha256Algorithm,
    value: String,
}

#[derive(Deserialize)]
enum Sha256Algorithm {
    #[serde(rename = "sha-256")]
    Sha256,
}

pub(super) struct FileSpec {
    id: String,
    size: usize,
    digest: Option<[u8; 32]>,
}

impl TryFrom<&Value> for FileSpec {
    type Error = ImportError;

    fn try_from(value: &Value) -> Result<Self, Self::Error> {
        if value.get("data").is_some() {
            return Err(ImportError::Validation(
                "Send the file through its upload target.",
            ));
        }
        let input = FileInput::deserialize(value)
            .map_err(|_| ImportError::Validation("Invalid statement file metadata."))?;
        if input.id.is_empty()
            || input.id.chars().take(129).count() > 128
            || input.size == 0
            || input.size > MAX_BYTES
            || input.mime_type.is_empty()
        {
            return Err(ImportError::Validation(
                "Expected a nonempty statement of at most 1 MiB and a valid file ID.",
            ));
        }
        let digest = input
            .digest
            .map(|digest| {
                let FileDigest {
                    algorithm: Sha256Algorithm::Sha256,
                    value,
                } = digest;
                BASE64_URL_SAFE_NO_PAD
                    .decode(value)
                    .ok()
                    .and_then(|bytes| bytes.try_into().ok())
                    .ok_or(ImportError::Validation("Invalid SHA-256 digest."))
            })
            .transpose()?;
        Ok(Self {
            id: input.id,
            size: input.size,
            digest,
        })
    }
}

/// A length-checked immutable copy of the transfer. No client filename becomes a path.
struct StatementText(Box<str>);

impl TryFrom<(&FileSpec, Bytes)> for StatementText {
    type Error = ImportError;

    fn try_from((file, bytes): (&FileSpec, Bytes)) -> Result<Self, Self::Error> {
        if file.size != bytes.len() {
            return Err(ImportError::Validation(
                "Uploaded size differs from the declared size.",
            ));
        }
        if file
            .digest
            .is_some_and(|expected| expected != <[u8; 32]>::from(Sha256::digest(&bytes)))
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

struct Upload {
    principal: Principal,
    arguments: Map<String, Value>,
    file: FileSpec,
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
        arguments: Map<String, Value>,
        file: FileSpec,
    ) -> Result<Value, ImportError> {
        let now = Instant::now();
        self.prune(now);
        if self.0.len() >= Self::CAPACITY {
            return Err(ImportError::Busy);
        }
        let (state, state_secret) = Ticket::generate()?;
        let (transfer, transfer_secret) = Ticket::generate()?;
        let result = json!({
            "resultType": "upload_required",
            "uploads": [{
                "fileId": file.id,
                "path": format!("/statement-uploads/{}", transfer_secret.expose()),
                "credentials": "include",
                "method": "PUT",
                "expiresAt": (chrono::Utc::now() + super::RETENTION).to_rfc3339()
            }],
            "requestState": state_secret.expose()
        });
        self.0.insert(
            state,
            Upload {
                principal,
                arguments,
                file,
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
        Ok(self.target(transfer, principal)?.file.size)
    }

    pub(super) fn stage(
        &mut self,
        transfer: &Ticket,
        principal: &Principal,
        bytes: Bytes,
    ) -> Result<(), ImportError> {
        let upload = self.target(transfer, principal)?;
        let confirmed = StatementText::try_from((&upload.file, bytes))?;
        if let Some(staged) = &upload.text {
            if staged.0 == confirmed.0 {
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
        arguments: &Map<String, Value>,
    ) -> Result<Map<String, Value>, ImportError> {
        self.prune(Instant::now());
        let upload = self
            .0
            .get(state)
            .filter(|upload| {
                &upload.principal == principal && same_object(&upload.arguments, arguments)
            })
            .ok_or(ImportError::Unknown)?;
        // Execute the bound request. Exact repeats are idempotent under the ledger's import reference.
        let text = upload.text.as_ref().ok_or(ImportError::Validation(
            "The statement file has not arrived.",
        ))?;
        let mut arguments = upload.arguments.clone();
        arguments.remove("file");
        arguments.insert("content".into(), Value::String(text.0.to_string()));
        Ok(arguments)
    }
}

fn same_object(left: &Map<String, Value>, right: &Map<String, Value>) -> bool {
    left.len() == right.len()
        && left.iter().all(|(key, value)| {
            right
                .get(key)
                .is_some_and(|other| same_arguments(value, other))
        })
}

fn same_arguments(left: &Value, right: &Value) -> bool {
    match (left, right) {
        (Value::Object(left), Value::Object(right)) => same_object(left, right),
        (Value::Array(left), Value::Array(right)) => {
            left.len() == right.len()
                && left
                    .iter()
                    .zip(right)
                    .all(|(value, other)| same_arguments(value, other))
        }
        (Value::Number(left), Value::Number(right)) if left != right => {
            let left = left.to_string();
            let right = right.to_string();
            // serde_json retains a decimal point for integral floats. Compare
            // their spelling rather than rounding integer values through f64.
            integral_spelling(&left) == integral_spelling(&right)
        }
        _ => left == right,
    }
}

fn integral_spelling(number: &str) -> &str {
    let number = number.strip_suffix(".0").unwrap_or(number);
    if number == "-0" { "0" } else { number }
}
