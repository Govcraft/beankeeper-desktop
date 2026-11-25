//! Response envelopes for commands owned by Desktop.

use beankeeper_cli::{error::CliError, output::json::EnvelopeError};
use chrono::{SecondsFormat, Utc};
use serde::Serialize;

#[derive(Clone, Serialize)]
pub(crate) struct Metadata<C> {
    command: C,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) agent: Option<String>,
    timestamp: String,
}

impl<C> Metadata<C> {
    pub(crate) fn new(command: C) -> Self {
        Self {
            command,
            agent: None,
            timestamp: Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true),
        }
    }
}

#[derive(Serialize)]
pub(crate) struct Envelope<C, T, E> {
    pub(crate) ok: bool,
    pub(crate) meta: Metadata<C>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) data: Option<T>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) error: Option<E>,
}

impl<C> Envelope<C, (), EnvelopeError> {
    pub(crate) fn failure(meta: Metadata<C>, error: &CliError) -> Self {
        Self {
            ok: false,
            meta,
            data: None,
            error: Some(EnvelopeError {
                code: error.code(),
                message: error.to_string(),
                // Desktop setup status and audit commands cannot reconcile statements.
                details: None,
            }),
        }
    }
}
