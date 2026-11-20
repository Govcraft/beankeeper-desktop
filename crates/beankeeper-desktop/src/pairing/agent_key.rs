use std::fmt;

use super::secret::Secret;

/// A non-reversible credential fingerprint used to route an agent to browsers.
///
/// This is not an authentication credential. Upstream must verify the bearer
/// token on every MCP request, including after a browser has been associated.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct AgentKey([u8; 32]);

impl AgentKey {
    /// Derive a routing key from an upstream-verified agent bearer credential.
    #[must_use]
    pub fn from_verified_token(token: &str) -> Self {
        Self(Secret::digest(token))
    }
}

impl fmt::Debug for AgentKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("AgentKey([redacted])")
    }
}
