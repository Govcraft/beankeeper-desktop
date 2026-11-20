use std::fmt;

use mti::prelude::*;

/// A browser session's public routing identifier, never its cookie credential.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct BrowserSessionId(MagicTypeId);

impl BrowserSessionId {
    pub(super) fn new() -> Self {
        Self("browser".create_type_id::<V7>())
    }
}

impl fmt::Display for BrowserSessionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}
