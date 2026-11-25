use std::{
    fmt::{self, Write},
    io,
    str::FromStr,
};

use sha2::{Digest, Sha256};

/// An opaque 256-bit capability. Debug output never reveals its value.
#[derive(Clone)]
pub(crate) struct Secret(String);

impl Secret {
    pub(super) const LENGTH: usize = 64;

    pub(crate) fn generate() -> io::Result<Self> {
        let mut bytes = [0; 32];
        getrandom::fill(&mut bytes).map_err(io::Error::other)?;
        let mut value = String::with_capacity(bytes.len() * 2);
        for byte in bytes {
            let _ = write!(value, "{byte:02x}");
        }
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

impl FromStr for Secret {
    type Err = super::PairingError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if value.len() == Self::LENGTH
            && value
                .bytes()
                .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
        {
            Ok(Self(value.into()))
        } else {
            Err(super::PairingError::InvalidLink)
        }
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret([redacted])")
    }
}
