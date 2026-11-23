use std::{error::Error, fmt};

use serde::Deserialize;
use serde_json::{Map, Value};

/// A missing or empty environment adds no overrides.
#[derive(Default, Deserialize, PartialEq, Eq)]
#[serde(try_from = "Value")]
pub(super) struct EnvOverrides(Option<Map<String, Value>>);

impl TryFrom<Value> for EnvOverrides {
    type Error = InvalidEnvOverrides;

    fn try_from(value: Value) -> Result<Self, Self::Error> {
        match value {
            Value::Object(env) => Ok(env.into()),
            _ => Err(InvalidEnvOverrides),
        }
    }
}

impl From<Map<String, Value>> for EnvOverrides {
    fn from(env: Map<String, Value>) -> Self {
        Self((!env.is_empty()).then_some(env))
    }
}

#[derive(Debug)]
pub(super) struct InvalidEnvOverrides;

impl fmt::Display for InvalidEnvOverrides {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("env must be a JSON object")
    }
}

impl Error for InvalidEnvOverrides {}
