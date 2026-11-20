//! Strict listener-wide Host and same-origin validation.

use axum::{
    extract::{Request, State},
    http::{HeaderMap, StatusCode, header},
    middleware::Next,
    response::{IntoResponse, Response},
};

/// Local HTTP authorities accepted on the listener's actual bound port.
#[derive(Clone, Debug)]
pub struct LocalBoundary {
    authorities: [String; 3],
}

impl LocalBoundary {
    /// Construct rules after binding, so port 0 never becomes a wildcard.
    #[must_use]
    pub fn new(port: u16) -> Self {
        Self {
            authorities: [
                format!("127.0.0.1:{port}"),
                format!("localhost:{port}"),
                format!("[::1]:{port}"),
            ],
        }
    }

    /// Exact hosts for the upstream MCP transport's defense in depth.
    pub fn authorities(&self) -> impl Iterator<Item = &str> {
        self.authorities.iter().map(String::as_str)
    }

    /// Exact origins for the upstream MCP transport.
    pub fn origins(&self) -> impl Iterator<Item = String> + '_ {
        self.authorities().map(|host| format!("http://{host}"))
    }

    /// Reject missing, repeated, malformed or nonlocal Host and Origin values.
    ///
    /// Origin may be absent for native MCP clients. When present it must match
    /// this request's validated Host, including scheme and actual bound port.
    #[must_use]
    pub fn accepts(&self, headers: &HeaderMap) -> bool {
        let mut hosts = headers.get_all(header::HOST).iter();
        let Some(host) = hosts.next().and_then(|host| host.to_str().ok()) else {
            return false;
        };
        if hosts.next().is_some() || !self.authorities().any(|allowed| allowed == host) {
            return false;
        }
        let mut origins = headers.get_all(header::ORIGIN).iter();
        let Some(origin) = origins.next() else {
            return true;
        };
        origins.next().is_none()
            && origin
                .to_str()
                .is_ok_and(|origin| origin == format!("http://{host}"))
    }
}

pub(crate) async fn guard(
    State(boundary): State<LocalBoundary>,
    request: Request,
    next: Next,
) -> Response {
    let pairing_url = request.uri().path() == "/pair";
    let mut response = if boundary.accepts(request.headers()) {
        next.run(request).await
    } else {
        (
            StatusCode::FORBIDDEN,
            "Host or Origin is not allowed on this local listener",
        )
            .into_response()
    };
    // Hide the pairing secret on redemption redirects and error responses.
    // Successful invite confirmation pages contain a same-origin POST form;
    // no-referrer would cause browsers to send Origin: null for that form.
    let referrer_policy = if pairing_url && response.status() != StatusCode::OK {
        "no-referrer"
    } else {
        "same-origin"
    };
    for (name, value) in [
        (
            "content-security-policy",
            "default-src 'self'; style-src 'self'; script-src 'none'; object-src 'none'; base-uri 'none'; frame-ancestors 'none'; form-action 'self'",
        ),
        ("x-content-type-options", "nosniff"),
        ("x-frame-options", "DENY"),
        ("referrer-policy", referrer_policy),
        ("cross-origin-resource-policy", "same-origin"),
        ("cache-control", "no-store"),
    ] {
        response.headers_mut().insert(
            axum::http::HeaderName::from_static(name),
            axum::http::HeaderValue::from_static(value),
        );
    }
    response
}
