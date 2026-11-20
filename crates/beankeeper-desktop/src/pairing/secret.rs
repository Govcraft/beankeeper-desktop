use std::{fmt, io};

use sha2::{Digest, Sha256};

/// An opaque 256-bit capability. Debug output never reveals its value.
#[derive(Clone)]
pub(crate) struct Secret(String);

impl Secret {
    pub(super) fn generate() -> io::Result<Self> {
        let mut bytes = [0; 32];
        getrandom::fill(&mut bytes).map_err(io::Error::other)?;
        let value = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
        Ok(Self(value))
    }

    pub(crate) fn expose(&self) -> &str {
        &self.0
    }

    pub(super) fn digest(value: &str) -> [u8; 32] {
        Sha256::digest(value.as_bytes()).into()
    }

    pub(super) fn matches(&self, value: &str) -> bool {
        // Compare digests, so comparison timing cannot disclose a secret prefix.
        Self::digest(self.expose()) == Self::digest(value)
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret([redacted])")
    }
}
