//! Strict listener-wide Host and same-origin validation.

use std::sync::Arc;

use axum::{
    extract::{Request, State},
    http::{HeaderMap, StatusCode, header},
    middleware::Next,
    response::{IntoResponse, Response},
};

/// Local HTTP authorities accepted on the listener's actual bound port.
#[derive(Clone, Debug)]
pub struct LocalBoundary {
    authorities: [Arc<str>; 3],
}

pub(crate) enum LocalOrigin {
    Absent,
    Same(SameOrigin),
}

/// This request supplied the same Origin as its validated local Host.
#[derive(Clone)]
pub(crate) struct SameOrigin(Arc<str>);

impl SameOrigin {
    pub(crate) fn authority(&self) -> &str {
        &self.0
    }
}

impl LocalBoundary {
    /// Construct rules after binding, so port 0 never becomes a wildcard.
    #[must_use]
    pub fn new(port: u16) -> Self {
        Self {
            authorities: [
                format!("127.0.0.1:{port}").into(),
                format!("localhost:{port}").into(),
                format!("[::1]:{port}").into(),
            ],
        }
    }

    /// Exact hosts for the upstream MCP transport's defense in depth.
    pub fn authorities(&self) -> impl Iterator<Item = &str> {
        self.authorities.iter().map(AsRef::as_ref)
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
        self.check(headers).is_some()
    }

    pub(crate) fn check(&self, headers: &HeaderMap) -> Option<LocalOrigin> {
        let mut hosts = headers.get_all(header::HOST).iter();
        let host = hosts.next()?.to_str().ok()?;
        if hosts.next().is_some() {
            return None;
        }
        let authority = self
            .authorities
            .iter()
            .find(|allowed| allowed.as_ref() == host)?;
        let mut origins = headers.get_all(header::ORIGIN).iter();
        let Some(origin) = origins.next() else {
            return Some(LocalOrigin::Absent);
        };
        let origin = origin.to_str().ok()?;
        if origins.next().is_some() || origin.strip_prefix("http://") != Some(host) {
            return None;
        }
        Some(LocalOrigin::Same(SameOrigin(Arc::clone(authority))))
    }
}

pub(crate) async fn guard(
    State(boundary): State<LocalBoundary>,
    mut request: Request,
    next: Next,
) -> Response {
    let pairing_url = request.uri().path() == "/pair";
    let mut response = if let Some(origin) = boundary.check(request.headers()) {
        if let LocalOrigin::Same(proof) = origin {
            request.extensions_mut().insert(proof);
        }
        next.run(request).await
    } else {
        (
            StatusCode::FORBIDDEN,
            "Host or Origin is not allowed on this local listener",
        )
            .into_response()
    };
    // Hide the pairing secret on redemption redirects and error responses.
    // Successful pairing and invite pages contain a same-origin POST form;
    // no-referrer would cause browsers to send Origin: null for that form.
    let referrer_policy = if pairing_url && response.status() != StatusCode::OK {
        "no-referrer"
    } else {
        "same-origin"
    };
    for (name, value) in [
        (
            "content-security-policy",
            "default-src 'self'; style-src 'self'; script-src 'self'; connect-src 'self'; object-src 'none'; base-uri 'none'; frame-ancestors 'none'; form-action 'self'",
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
