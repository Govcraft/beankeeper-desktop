use std::str::FromStr;

use beankeeper::types::{AccountCode, Currency};
use beankeeper_cli::{
    commands::setup::{SetupReference, StatementFormat},
    db::{companies::CompanySlug, holds::HoldMatchOptions},
    error::CliError,
};
use clap::Args;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Map, Value};
use serde_with::{As, DisplayFromStr};

use super::{
    ImportError,
    summary::Scope,
    upload::{FileSpec, StatementText},
};

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(transparent)]
struct ImportReference(Box<str>);

impl From<&SetupReference> for ImportReference {
    fn from(reference: &SetupReference) -> Self {
        // SetupReference exposes its canonical spelling through Display.
        Self(reference.to_string().into_boxed_str())
    }
}

impl FromStr for ImportReference {
    type Err = CliError;

    fn from_str(wire: &str) -> Result<Self, Self::Err> {
        wire.parse::<SetupReference>()
            .map(|reference| Self::from(&reference))
    }
}

impl<'de> Deserialize<'de> for ImportReference {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let wire = String::deserialize(deserializer)?;
        wire.parse().map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum PositiveAmount {
    Inflow,
    Outflow,
}

/// Only caller-supplied overrides. The audited importer owns the mapping defaults.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CsvMappingInput {
    #[serde(
        default,
        deserialize_with = "decode_override",
        skip_serializing_if = "Option::is_none"
    )]
    date_col: Option<String>,
    #[serde(
        default,
        deserialize_with = "decode_override",
        skip_serializing_if = "Option::is_none"
    )]
    description_col: Option<String>,
    #[serde(
        default,
        deserialize_with = "decode_override",
        skip_serializing_if = "Option::is_none"
    )]
    amount_col: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reference_col: Option<String>,
    #[serde(
        default,
        deserialize_with = "decode_override",
        skip_serializing_if = "Option::is_none"
    )]
    date_format: Option<String>,
    #[serde(
        default,
        deserialize_with = "decode_override",
        skip_serializing_if = "Option::is_none"
    )]
    positive_amount: Option<PositiveAmount>,
}

fn decode_override<'de, T: Deserialize<'de>, D: Deserializer<'de>>(
    input: D,
) -> Result<Option<T>, D::Error> {
    T::deserialize(input).map(Some)
}

impl FromStr for CsvMappingInput {
    type Err = serde_json::Error;

    fn from_str(wire: &str) -> Result<Self, Self::Err> {
        serde_json::from_str::<Map<String, Value>>(wire)
            .and_then(|fields| Self::deserialize(Value::Object(fields)))
    }
}

#[derive(Deserialize)]
#[serde(untagged)]
enum CsvMappingWire {
    Object(CsvMappingInput),
    Text(String),
}

impl TryFrom<CsvMappingWire> for CsvMappingInput {
    type Error = serde_json::Error;

    fn try_from(value: CsvMappingWire) -> Result<Self, Self::Error> {
        match value {
            CsvMappingWire::Object(mapping) => Ok(mapping),
            CsvMappingWire::Text(wire) => wire.parse(),
        }
    }
}

fn decode_mapping<'de, D: Deserializer<'de>>(
    input: D,
) -> Result<Option<CsvMappingInput>, D::Error> {
    Option::<CsvMappingWire>::deserialize(input)?
        .map(CsvMappingInput::try_from)
        .transpose()
        .map_err(serde::de::Error::custom)
}

#[derive(Args, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ImportFields {
    /// Book that owns the bank account.
    #[arg(long = "book")]
    company: CompanySlug,
    /// Stable import reference. Reuse it when retrying the same statement.
    #[arg(long)]
    reference: ImportReference,
    /// Bank account code.
    #[arg(long)]
    #[serde(with = "As::<DisplayFromStr>")]
    bank: AccountCode,
    /// Account to use for entries without a category rule.
    #[arg(long)]
    #[serde(with = "As::<DisplayFromStr>")]
    suspense: AccountCode,
    /// Statement currency.
    #[arg(long)]
    #[serde(with = "As::<DisplayFromStr>")]
    currency: Currency,
    /// Source format. Use ofx for OFX or QFX.
    #[arg(long, value_enum)]
    #[serde(rename = "format")]
    statement_format: StatementFormat,
    /// Optional CSV column mapping as a JSON object.
    #[arg(long)]
    #[serde(
        default,
        deserialize_with = "decode_mapping",
        skip_serializing_if = "Option::is_none"
    )]
    csv: Option<CsvMappingInput>,
    #[command(flatten)]
    #[serde(flatten)]
    matching: HoldMatchOptions,
}

#[derive(Deserialize)]
struct WireImport {
    #[serde(flatten)]
    fields: ImportFields,
    #[serde(default, deserialize_with = "decode_override")]
    file: Option<FileSpec>,
    #[serde(default, deserialize_with = "decode_override")]
    content: Option<StatementText>,
}

pub(super) enum ImportRequest {
    File(FileImport),
    Inline(ReadyImport),
}

#[derive(PartialEq, Eq)]
pub(super) struct FileImport {
    fields: ImportFields,
    file: FileSpec,
}

impl FileImport {
    pub(super) fn file(&self) -> &FileSpec {
        &self.file
    }

    pub(super) fn with_content(&self, content: StatementText) -> ReadyImport {
        ReadyImport {
            fields: self.fields.clone(),
            content,
        }
    }
}

#[derive(Serialize)]
pub(super) struct ReadyImport {
    #[serde(flatten)]
    fields: ImportFields,
    content: StatementText,
}

impl ReadyImport {
    pub(super) fn content(&self) -> &StatementText {
        &self.content
    }

    pub(super) fn summary_scope(&self) -> Scope {
        Scope::from((&self.fields.company, &self.fields.suspense))
    }
}

impl TryFrom<&Map<String, Value>> for ImportRequest {
    type Error = ImportError;

    fn try_from(arguments: &Map<String, Value>) -> Result<Self, Self::Error> {
        let entries = arguments.iter().map(|(key, value)| (key.as_str(), value));
        let wire = WireImport::deserialize(serde::de::value::MapDeserializer::new(entries))
        .map_err(|_| {
            ImportError::Validation(
                "Invalid statement arguments. Check the source, format, accounts, and reference.",
            )
        })?;
        match (wire.file, wire.content) {
            (Some(file), None) => Ok(Self::File(FileImport {
                fields: wire.fields,
                file,
            })),
            (None, Some(content)) => Ok(Self::Inline(ReadyImport {
                fields: wire.fields,
                content,
            })),
            _ => Err(ImportError::Validation(
                "Supply exactly one statement source.",
            )),
        }
    }
}

impl From<(&ImportFields, StatementText)> for ReadyImport {
    fn from((fields, content): (&ImportFields, StatementText)) -> Self {
        Self {
            fields: fields.clone(),
            content,
        }
    }
}
