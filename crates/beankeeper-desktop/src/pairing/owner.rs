//! Local owner authority for issuing browser links on demand.

use std::{
    fs,
    io::Read,
    net::SocketAddr,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use axum::{
    Router,
    extract::{ConnectInfo, State},
    http::{HeaderMap, HeaderValue, header},
    routing::post,
};
use reqwest::{Url, blocking::Client, redirect::Policy};
use secrecy::ExposeSecret;

use super::{LinkKind, PAIR_ROUTE, PairingError, PairingState, secret::Secret};
use crate::{
    instance::{ID_HEADER, InstanceId, ListenerAddress, LiveListener},
    private_file::PrivateFile,
    service::{ServiceError, private_write},
    setup::keychain,
};

const ROUTE: &str = "/api/local/browser-link";
const OWNER_HEADER: &str = "x-beankeeper-browser-owner";

/// The peer address reported by the accepting socket.
#[derive(Clone)]
pub(crate) struct Peer(pub(crate) SocketAddr);

/// A per-launch credential dedicated to requesting browser links.
#[derive(Clone)]
pub(crate) struct OwnerKey(Secret);

impl OwnerKey {
    pub(crate) fn generate() -> Result<Self, PairingError> {
        Secret::generate().map(Self).map_err(Into::into)
    }

    fn path(state: &Path, address: ListenerAddress) -> PathBuf {
        state
            .join("browser-pairing")
            .join(address.filename())
            .with_extension("key")
    }

    pub(crate) fn publish(
        &self,
        state: &Path,
        address: ListenerAddress,
    ) -> Result<KeyFile, ServiceError> {
        let directory = state.join("browser-pairing");
        fs::create_dir_all(&directory)?;
        keychain::restrict_directory(&directory)?;
        let path = Self::path(state, address);
        private_write(&path, self.0.expose().as_bytes())?;
        Ok(KeyFile(path))
    }

    fn read(state: &Path, address: ListenerAddress) -> Result<Self, ServiceError> {
        let key = PrivateFile::open(&Self::path(state, address))
            .and_then(|file| file.read_secret(Secret::LENGTH))
            .map_err(|_| ServiceError::BrowserPairing)?;
        key.expose_secret()
            .parse()
            .map(Self)
            .map_err(|_| ServiceError::BrowserPairing)
    }
}

pub(crate) struct KeyFile(PathBuf);

impl Drop for KeyFile {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

/// Proof that the native request presented the listener's private owner key.
struct OwnerPairing<'a>(&'a PairingState);

#[derive(Clone)]
struct Authority {
    state: PairingState,
    key: OwnerKey,
    id: InstanceId,
}

impl<'a> TryFrom<(&'a Authority, Peer, &HeaderMap)> for OwnerPairing<'a> {
    type Error = PairingError;

    fn try_from(
        (authority, peer, headers): (&'a Authority, Peer, &HeaderMap),
    ) -> Result<Self, Self::Error> {
        let mut keys = headers.get_all(OWNER_HEADER).iter();
        let mut ids = headers.get_all(ID_HEADER).iter();
        let value = keys
            .next()
            .and_then(|value| value.to_str().ok())
            .ok_or(PairingError::Unauthorized)?;
        let id = ids
            .next()
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<InstanceId>().ok());
        if !peer.0.ip().is_loopback()
            || keys.next().is_some()
            || ids.next().is_some()
            || id.as_ref() != Some(&authority.id)
            || [header::AUTHORIZATION, header::COOKIE, header::ORIGIN]
                .iter()
                .any(|name| headers.contains_key(name))
            || !authority.key.0.matches(value)
        {
            return Err(PairingError::Unauthorized);
        }
        Ok(Self(&authority.state))
    }
}

impl OwnerPairing<'_> {
    fn issue(self, now: Instant) -> Result<String, PairingError> {
        let mut registry = self.0.lock(now)?;
        let link = PairingState::insert_link(&mut registry, LinkKind::Browser, now)?;
        Ok(link.secret.expose().into())
    }
}

pub(crate) fn router(state: PairingState, key: OwnerKey, id: InstanceId) -> Router {
    Router::new()
        .route(ROUTE, post(issue))
        .with_state(Authority { state, key, id })
}

async fn issue(
    State(authority): State<Authority>,
    ConnectInfo(peer): ConnectInfo<Peer>,
    headers: HeaderMap,
) -> Result<String, PairingError> {
    OwnerPairing::try_from((&authority, peer, &headers))?.issue(Instant::now())
}

/// Request a token and construct the URL locally on the selected listener.
pub(crate) fn request_link(state: &Path, listener: &LiveListener) -> Result<Url, ServiceError> {
    let address = ListenerAddress::from(listener);
    let id = listener.id().ok_or(ServiceError::BrowserPairing)?;
    let key = OwnerKey::read(state, address)?;
    let endpoint = address.url(ROUTE)?;
    let mut credential =
        HeaderValue::from_str(key.0.expose()).map_err(|_| ServiceError::BrowserPairing)?;
    credential.set_sensitive(true);
    let response = Client::builder()
        .no_proxy()
        .redirect(Policy::none())
        .timeout(Duration::from_secs(2))
        .build()
        .and_then(|client| {
            client
                .post(endpoint)
                .header(OWNER_HEADER, credential)
                .header(ID_HEADER, id.to_string())
                .send()
        })
        .map_err(|_| ServiceError::BrowserPairing)?;
    if !response.status().is_success() {
        return Err(ServiceError::BrowserPairing);
    }
    let mut value = String::new();
    response
        .take(Secret::LENGTH as u64 + 1)
        .read_to_string(&mut value)
        .map_err(|_| ServiceError::BrowserPairing)?;
    let token: Secret = value.parse().map_err(|_| ServiceError::BrowserPairing)?;
    let mut url = address.url(PAIR_ROUTE)?;
    url.query_pairs_mut().append_pair("token", token.expose());
    Ok(url)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pairing::LINK_LIFETIME;

    #[test]
    fn owner_authority_issues_a_fresh_link_after_startup_links_expire()
    -> Result<(), Box<dyn std::error::Error>> {
        let address = ListenerAddress::default();
        let (state, _) = PairingState::new(address.into())?;
        let authority = Authority {
            state,
            key: OwnerKey::generate()?,
            id: InstanceId::default(),
        };
        let peer = || Peer(address.into());
        let mut headers = HeaderMap::new();
        assert!(OwnerPairing::try_from((&authority, peer(), &headers)).is_err());
        headers.insert(OWNER_HEADER, authority.key.0.expose().parse()?);
        headers.insert(ID_HEADER, InstanceId::default().to_string().parse()?);
        assert!(OwnerPairing::try_from((&authority, peer(), &headers)).is_err());
        headers.insert(ID_HEADER, authority.id.to_string().parse()?);
        let now = Instant::now() + LINK_LIFETIME;
        let token = OwnerPairing::try_from((&authority, peer(), &headers))?.issue(now)?;
        let digest = Secret::digest(&token);
        let last_valid = now
            + LINK_LIFETIME
                .checked_sub(Duration::from_secs(1))
                .ok_or("pairing lifetime must exceed a second")?;
        let registry = authority.state.lock(last_valid)?;
        assert_eq!(registry.links.len(), 1);
        assert!(registry.links.contains_key(&digest));
        drop(registry);
        assert!(authority.state.lock(now + LINK_LIFETIME)?.links.is_empty());

        headers.append(OWNER_HEADER, authority.key.0.expose().parse()?);
        assert!(OwnerPairing::try_from((&authority, peer(), &headers)).is_err());
        Ok(())
    }
}
