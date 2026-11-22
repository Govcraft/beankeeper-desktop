use std::time::Duration;

use beankeeper_cli::{capability::Capability, identity::IssueRequest};

pub(super) struct HumanCredentialRequest {
    name: &'static str,
    capabilities: [String; 2],
    ttl: Duration,
}

impl Default for HumanCredentialRequest {
    fn default() -> Self {
        Self {
            name: "desktop_human",
            capabilities: [Capability::Read, Capability::Approve]
                .map(|capability| capability.path().to_owned()),
            ttl: Duration::from_hours(12),
        }
    }
}

impl<'a> From<&'a HumanCredentialRequest> for IssueRequest<'a> {
    fn from(request: &'a HumanCredentialRequest) -> Self {
        Self {
            name: request.name,
            capabilities: &request.capabilities,
            ttl: request.ttl,
            public_key: None,
            generate_key: true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paired_human_defaults_to_a_twelve_hour_read_and_approve_credential() {
        let request = HumanCredentialRequest::default();
        let issuance = IssueRequest::from(&request);

        assert_eq!(issuance.name, "desktop_human");
        assert_eq!(issuance.capabilities, ["ledger/read", "ledger/approve"]);
        assert_eq!(issuance.ttl, Duration::from_hours(12));
        assert!(issuance.generate_key);
        assert_eq!(issuance.public_key, None);
    }
}
