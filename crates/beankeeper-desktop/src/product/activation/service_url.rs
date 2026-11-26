use std::{net::IpAddr, str::FromStr};

use reqwest::Url;

use super::ProductError;

/// A license service origin that can safely receive claim and recovery codes.
pub(in crate::product) struct LicenseServiceUrl(Url);

impl FromStr for LicenseServiceUrl {
    type Err = ProductError;

    fn from_str(input: &str) -> Result<Self, Self::Err> {
        let mut url = Url::parse(input).map_err(|_| ProductError::ServiceUrlInvalid)?;
        if !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
        {
            return Err(ProductError::ServiceUrlInvalid);
        }
        match url.scheme() {
            "https" => (),
            "http" if is_loopback(&url) => (),
            _ => return Err(ProductError::ServiceUrlInsecure),
        }
        url.set_path("/");
        Ok(Self(url))
    }
}

impl LicenseServiceUrl {
    pub(in crate::product) fn requires_https(&self) -> bool {
        self.0.scheme() == "https"
    }

    pub(in crate::product) fn bypasses_proxy(&self) -> bool {
        // A validated HTTP origin is already known to be loopback.
        !self.requires_https() || is_loopback(&self.0)
    }

    pub(in crate::product) fn endpoint(&self, path: &str) -> Url {
        let mut url = self.0.clone();
        url.set_path(path);
        url
    }
}

fn is_loopback(url: &Url) -> bool {
    url.host_str().is_some_and(|host| {
        host == "localhost"
            || host
                .trim_matches(['[', ']'])
                .parse::<IpAddr>()
                .is_ok_and(|ip| ip.is_loopback())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_malformed_urls_and_disallowed_components() {
        for input in [
            "not a URL",
            "https://",
            "https://name@example.com/",
            "https://:password@example.com/",
            "https://example.com/?",
            "https://example.com/#",
        ] {
            assert!(
                matches!(
                    input.parse::<LicenseServiceUrl>(),
                    Err(ProductError::ServiceUrlInvalid)
                ),
                "{input}"
            );
        }
    }

    #[test]
    fn rejects_insecure_service_urls() {
        for input in [
            "http://example.com/",
            "http://localhost.example.com/",
            "http://192.0.2.1/",
            "ftp://localhost/",
        ] {
            assert!(
                matches!(
                    input.parse::<LicenseServiceUrl>(),
                    Err(ProductError::ServiceUrlInsecure)
                ),
                "{input}"
            );
        }
    }

    #[test]
    fn normalizes_origins_and_sets_transport_policy() -> Result<(), ProductError> {
        for (input, https, loopback) in [
            ("https://example.com/old/path", true, false),
            ("https://localhost/old/path", true, true),
            ("http://localhost:8080/old/path", false, true),
            ("http://127.0.0.2:8080/old/path", false, true),
            ("http://[::1]:8080/old/path", false, true),
        ] {
            let url: LicenseServiceUrl = input.parse()?;
            assert_eq!(url.requires_https(), https, "{input}");
            assert_eq!(url.bypasses_proxy(), loopback, "{input}");
            assert_eq!(url.0.path(), "/");
            let path = "/api/v1/licenses/redeem";
            assert_eq!(
                url.endpoint(path).as_str(),
                input.replace("/old/path", path)
            );
        }
        Ok(())
    }
}
