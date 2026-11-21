//! Per-launch browser capabilities, CSRF checks and agent/browser associations.

mod agent_key;
mod browser_session_id;
mod routes;

pub(crate) const PAIR_ROUTE: &str = "/pair";

mod secret;

pub use agent_key::AgentKey;
pub use browser_session_id::BrowserSessionId;
pub(crate) use routes::{human_guard, human_post};
pub(crate) use routes::{mcp_guard, router};

use std::{
    collections::HashMap,
    error::Error,
    fmt, io,
    sync::{Arc, Mutex, MutexGuard},
    time::{Duration, Instant},
};

use axum::http::{HeaderMap, header};
use secret::Secret;

use crate::boundary::SameOrigin;

const LINK_LIFETIME: Duration = Duration::from_secs(300);
const SESSION_LIFETIME: Duration = Duration::from_hours(12);
const MAX_ENTRIES: usize = 64;

/// Pairing and human-session failures, without credentials in diagnostics.
#[derive(Debug)]
pub enum PairingError {
    /// A pairing URL is invalid, expired, or already consumed.
    InvalidLink,
    /// A browser credential, authority, Origin, or CSRF token was not valid.
    Unauthorized,
    /// The bounded per-launch registry is full.
    Capacity,
    /// Shared state is unavailable.
    Unavailable,
    /// The operating system could not generate a credential.
    Entropy(io::Error),
}

impl fmt::Display for PairingError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::InvalidLink => "Pairing link expired or already used. Use a fresh link from the terminal or a paired browser.",
            Self::Unauthorized => "Pair this browser using the link printed by `bk serve`. Human writes also require the same Origin and a CSRF token.",
            Self::Capacity => "Too many pairing links or browser sessions. Wait for a link to expire or sign out a browser.",
            Self::Unavailable => "Browser session state is unavailable. Restart `bk serve`.",
            Self::Entropy(_) => "Could not generate a browser credential.",
        })
    }
}

impl Error for PairingError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Entropy(error) => Some(error),
            _ => None,
        }
    }
}

impl From<io::Error> for PairingError {
    fn from(error: io::Error) -> Self {
        Self::Entropy(error)
    }
}

struct Link {
    secret: Secret,
    expires: Instant,
    kind: LinkKind,
}

enum LinkKind {
    Browser,
    AgentInvite(AgentKey),
}

#[derive(Clone, Debug)]
struct Session {
    id: BrowserSessionId,
    csrf: Secret,
    authority: String,
    expires: Instant,
    binding: Binding,
}

#[derive(Clone, Debug)]
enum Binding {
    BrowserOnly,
    Agent(AgentKey),
}

/// A session authenticated by its private pairing cookie and exact Host.
#[derive(Clone)]
pub(crate) struct HumanSession {
    session: Session,
    cookie_key: [u8; 32],
}

impl fmt::Debug for HumanSession {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("HumanSession").field(&self.session).finish()
    }
}

impl HumanSession {
    pub(crate) fn id(&self) -> &BrowserSessionId {
        &self.session.id
    }

    pub(crate) fn csrf_token(&self) -> &str {
        self.session.csrf.expose()
    }

    pub(crate) fn agent(&self) -> Option<&AgentKey> {
        match &self.session.binding {
            Binding::BrowserOnly => None,
            Binding::Agent(agent) => Some(agent),
        }
    }
}

/// Proof that a human session also presented the same Origin and its CSRF token.
#[derive(Clone)]
pub(crate) struct HumanWrite(HumanSession);

impl HumanWrite {
    pub(crate) fn session(&self) -> &HumanSession {
        &self.0
    }
}

struct Destination {
    path: String,
    expires: Instant,
}

#[derive(Default)]
struct Registry {
    links: HashMap<[u8; 32], Link>,
    sessions: HashMap<[u8; 32], Session>,
    destinations: HashMap<[u8; 32], Destination>,
}

impl Registry {
    fn prune(&mut self, now: Instant) {
        self.links.retain(|_, link| now < link.expires);
        self.sessions.retain(|_, session| now < session.expires);
        self.destinations
            .retain(|_, destination| now < destination.expires);
    }
}

/// Shared, bounded state owned by exactly one Desktop launch.
#[derive(Clone)]
pub struct PairingState {
    registry: Arc<Mutex<Registry>>,
    base: String,
    cookie_name: String,
}

impl PairingState {
    pub(crate) fn base_url(&self) -> &str {
        &self.base
    }

    /// Generate the launch's single-use browser pairing URL (valid five minutes).
    ///
    /// # Errors
    /// Returns an entropy or state error; no weak credential fallback exists.
    pub fn new(bound: std::net::SocketAddr) -> Result<(Self, String), PairingError> {
        let state = Self {
            registry: Arc::default(),
            base: crate::server::local_url(bound),
            cookie_name: format!("bk_desktop_{}", bound.port()),
        };
        let url = state.browser_link(Instant::now())?;
        Ok((state, url))
    }

    fn destination_cookie_name(&self) -> String {
        format!("{}_return", self.cookie_name)
    }

    fn destination_key(&self, headers: &HeaderMap) -> Option<[u8; 32]> {
        let name = self.destination_cookie_name();
        let mut values = headers
            .get_all(header::COOKIE)
            .iter()
            .filter_map(|header| header.to_str().ok())
            .flat_map(|header| header.split(';'))
            .filter_map(|cookie| cookie.trim().split_once('='))
            .filter(|(key, _)| *key == name)
            .map(|(_, value)| value);
        let value = values.next()?;
        if values.next().is_some() {
            return None;
        }
        Some(Secret::digest(value))
    }

    pub(crate) fn remember_destination(
        &self,
        headers: &HeaderMap,
        path: String,
        now: Instant,
    ) -> Result<String, PairingError> {
        let mut registry = self.lock(now)?;
        let key = self.destination_key(headers);
        if let Some(destination) = key.and_then(|key| registry.destinations.get_mut(&key)) {
            destination.path = path;
            return Ok(String::new());
        }
        let key = Secret::generate()?;
        // A remembered view is optional convenience, so capacity must never
        // replace the pairing page. Expiries are fixed at insertion, making the
        // earliest expiry the oldest entry without tracking another timestamp.
        if registry.destinations.len() >= MAX_ENTRIES
            && let Some(oldest) = registry
                .destinations
                .iter()
                .min_by_key(|(_, destination)| destination.expires)
                .map(|(key, _)| *key)
        {
            registry.destinations.remove(&oldest);
        }
        registry.destinations.insert(
            Secret::digest(key.expose()),
            Destination {
                path,
                expires: now + LINK_LIFETIME,
            },
        );
        Ok(format!(
            "{}={}; Path=/; HttpOnly; SameSite=Strict; Max-Age=300",
            self.destination_cookie_name(),
            key.expose()
        ))
    }

    pub(crate) fn take_destination(
        &self,
        headers: &HeaderMap,
        now: Instant,
    ) -> Result<String, PairingError> {
        let mut registry = self.lock(now)?;
        Ok(self
            .destination_key(headers)
            .and_then(|key| registry.destinations.remove(&key))
            .map_or_else(|| "/".into(), |destination| destination.path))
    }

    pub(crate) fn clear_destination_cookie(&self) -> String {
        format!(
            "{}=; Path=/; HttpOnly; SameSite=Strict; Max-Age=0",
            self.destination_cookie_name()
        )
    }

    fn lock(&self, now: Instant) -> Result<MutexGuard<'_, Registry>, PairingError> {
        let mut registry = self
            .registry
            .lock()
            .map_err(|_| PairingError::Unavailable)?;
        registry.prune(now);
        Ok(registry)
    }

    fn insert_link(
        &self,
        registry: &mut Registry,
        kind: LinkKind,
        now: Instant,
    ) -> Result<String, PairingError> {
        if registry.links.len() >= MAX_ENTRIES {
            return Err(PairingError::Capacity);
        }
        let link = Link {
            secret: Secret::generate()?,
            expires: now + LINK_LIFETIME,
            kind,
        };
        let url = self.link_url(&link);
        registry
            .links
            .insert(Secret::digest(link.secret.expose()), link);
        Ok(url)
    }

    fn link_url(&self, link: &Link) -> String {
        let kind = match link.kind {
            LinkKind::Browser => "token",
            LinkKind::AgentInvite(_) => "invite",
        };
        format!("{}pair?{kind}={}", self.base, link.secret.expose())
    }

    fn browser_link(&self, now: Instant) -> Result<String, PairingError> {
        self.insert_link(&mut *self.lock(now)?, LinkKind::Browser, now)
    }

    /// Return a five-minute, one-use agent association link for D4 navigation.
    ///
    /// The link cannot create a human session. A human must first pair using
    /// the terminal URL, then confirm the association in that browser.
    /// Repeated requests reuse an outstanding link without extending its life.
    ///
    /// # Errors
    /// Returns a capacity, entropy or state error.
    pub fn agent_pairing_url(&self, agent: AgentKey, now: Instant) -> Result<String, PairingError> {
        let mut registry = self.lock(now)?;
        if let Some(link) = registry
            .links
            .values()
            .find(|link| matches!(&link.kind, LinkKind::AgentInvite(owner) if *owner == agent))
        {
            return Ok(self.link_url(link));
        }
        self.insert_link(&mut registry, LinkKind::AgentInvite(agent), now)
    }

    /// Browser routing targets for an authenticated agent, excluding expired sessions.
    ///
    /// Browser profiles share their cookie across tabs. D4 can attach individual
    /// tab connections to these session IDs without exposing cookie credentials.
    ///
    /// # Errors
    /// Returns an error if shared state is unavailable.
    pub fn paired_sessions(
        &self,
        agent: &AgentKey,
        now: Instant,
    ) -> Result<Vec<BrowserSessionId>, PairingError> {
        Ok(self
            .lock(now)?
            .sessions
            .values()
            .filter(|session| matches!(&session.binding, Binding::Agent(owner) if owner == agent))
            .map(|session| session.id.clone())
            .collect())
    }

    fn redeem(
        &self,
        token: &str,
        origin: &SameOrigin,
        now: Instant,
    ) -> Result<String, PairingError> {
        let mut registry = self.lock(now)?;
        let digest = Secret::digest(token);
        let link = registry
            .links
            .get(&digest)
            .ok_or(PairingError::InvalidLink)?;
        if !matches!(link.kind, LinkKind::Browser) {
            return Err(PairingError::InvalidLink);
        }
        if registry.sessions.len() >= MAX_ENTRIES {
            return Err(PairingError::Capacity);
        }
        let cookie = Secret::generate()?;
        let session = Session {
            id: BrowserSessionId::new(),
            csrf: Secret::generate()?,
            authority: origin.authority().into(),
            expires: now + SESSION_LIFETIME,
            binding: Binding::BrowserOnly,
        };
        registry.links.remove(&digest);
        registry
            .sessions
            .insert(Secret::digest(cookie.expose()), session);
        Ok(format!(
            "{}={}; Path=/; HttpOnly; SameSite=Strict; Max-Age={}",
            self.cookie_name,
            cookie.expose(),
            SESSION_LIFETIME.as_secs()
        ))
    }

    fn cookie<'a>(&self, headers: &'a HeaderMap) -> Result<&'a str, PairingError> {
        let mut found = None;
        for value in headers.get_all(header::COOKIE) {
            for cookie in value
                .to_str()
                .map_err(|_| PairingError::Unauthorized)?
                .split(';')
            {
                if let Some((name, value)) = cookie.trim().split_once('=')
                    && name == self.cookie_name
                    && found.replace(value).is_some()
                {
                    return Err(PairingError::Unauthorized);
                }
            }
        }
        found.ok_or(PairingError::Unauthorized)
    }

    pub(crate) fn session(
        &self,
        headers: &HeaderMap,
        now: Instant,
    ) -> Result<HumanSession, PairingError> {
        // Bearer credentials never confer human authority, even with a cookie.
        if headers.contains_key(header::AUTHORIZATION) {
            return Err(PairingError::Unauthorized);
        }
        let cookie_key = Secret::digest(self.cookie(headers)?);
        let registry = self.lock(now)?;
        let session = registry
            .sessions
            .get(&cookie_key)
            .ok_or(PairingError::Unauthorized)?;
        let mut hosts = headers.get_all(header::HOST).iter();
        if hosts.next().and_then(|value| value.to_str().ok()) != Some(&session.authority)
            || hosts.next().is_some()
        {
            return Err(PairingError::Unauthorized);
        }
        Ok(HumanSession {
            session: session.clone(),
            cookie_key,
        })
    }

    fn associate(
        &self,
        write: &HumanWrite,
        invite: &str,
        now: Instant,
    ) -> Result<(), PairingError> {
        let session = write.session();
        let mut registry = self.lock(now)?;
        let digest = Secret::digest(invite);
        let link = registry
            .links
            .get(&digest)
            .ok_or(PairingError::InvalidLink)?;
        let LinkKind::AgentInvite(agent) = &link.kind else {
            return Err(PairingError::InvalidLink);
        };
        let agent = agent.clone();
        let target = registry
            .sessions
            .get_mut(&session.cookie_key)
            .filter(|target| &target.id == session.id())
            .ok_or(PairingError::Unauthorized)?;
        target.binding = Binding::Agent(agent);
        registry.links.remove(&digest);
        Ok(())
    }

    fn authorize_write(
        &self,
        session: HumanSession,
        origin: &SameOrigin,
        csrf: &str,
        now: Instant,
    ) -> Result<HumanWrite, PairingError> {
        if origin.authority() != session.session.authority
            || !session.session.csrf.matches(csrf)
            || !self.lock(now)?.sessions.contains_key(&session.cookie_key)
        {
            return Err(PairingError::Unauthorized);
        }
        Ok(HumanWrite(session))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::boundary::{LocalBoundary, LocalOrigin};
    use axum::http::HeaderValue;

    type TestResult = Result<(), Box<dyn Error>>;

    fn capability(url: &str) -> Result<&str, Box<dyn Error>> {
        url.split_once('=')
            .map(|(_, value)| value)
            .ok_or_else(|| "missing capability".into())
    }

    fn headers(cookie: &str) -> Result<HeaderMap, Box<dyn Error>> {
        let mut headers = HeaderMap::new();
        headers.insert(header::HOST, "127.0.0.1:8750".parse()?);
        headers.insert(header::ORIGIN, "http://127.0.0.1:8750".parse()?);
        headers.insert(
            header::COOKIE,
            cookie.split(';').next().ok_or("cookie")?.parse()?,
        );
        Ok(headers)
    }

    fn write_proof(
        state: &PairingState,
        headers: &HeaderMap,
        csrf: &str,
        now: Instant,
    ) -> Result<HumanWrite, PairingError> {
        let session = state.session(headers, now)?;
        state.authorize_write(session, &verified_origin(headers)?, csrf, now)
    }

    fn verified_origin(headers: &HeaderMap) -> Result<SameOrigin, PairingError> {
        match LocalBoundary::new(8750).check(headers) {
            Some(LocalOrigin::Same(origin)) => Ok(origin),
            _ => Err(PairingError::Unauthorized),
        }
    }

    fn origin() -> Result<SameOrigin, PairingError> {
        let mut headers = HeaderMap::new();
        headers.insert(header::HOST, HeaderValue::from_static("127.0.0.1:8750"));
        headers.insert(
            header::ORIGIN,
            HeaderValue::from_static("http://127.0.0.1:8750"),
        );
        verified_origin(&headers)
    }

    #[test]
    fn links_are_random_single_use_and_expire_at_five_minutes() -> TestResult {
        let origin = origin()?;
        let (state, launch) = PairingState::new("127.0.0.1:8750".parse()?)?;
        let now = Instant::now();
        let next = state.browser_link(now)?;
        assert_ne!(launch, next);
        assert_eq!(capability(&launch)?.len(), 64);
        let cookie = state.redeem(capability(&launch)?, &origin, now)?;
        assert!(!cookie.contains(capability(&launch)?));
        assert!(matches!(
            state.redeem(capability(&launch)?, &origin, now),
            Err(PairingError::InvalidLink)
        ));
        assert!(matches!(
            state.redeem(capability(&next)?, &origin, now + LINK_LIFETIME),
            Err(PairingError::InvalidLink)
        ));
        let fresh = state.browser_link(now + LINK_LIFETIME)?;
        assert!(
            state
                .redeem(capability(&fresh)?, &origin, now + LINK_LIFETIME)
                .is_ok()
        );
        Ok(())
    }

    #[test]
    fn concurrent_redemption_creates_exactly_one_session() -> TestResult {
        let origin = origin()?;
        let (state, url) = PairingState::new("127.0.0.1:8750".parse()?)?;
        let token = capability(&url)?.to_owned();
        let barrier = Arc::new(std::sync::Barrier::new(8));
        let threads: Vec<_> = (0..8)
            .map(|_| {
                let state = state.clone();
                let origin = origin.clone();
                let token = token.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    state.redeem(&token, &origin, Instant::now()).is_ok()
                })
            })
            .collect();
        let successes = threads
            .into_iter()
            .map(|thread| thread.join().map_err(|_| "thread panicked"))
            .collect::<Result<Vec<_>, _>>()?;
        assert_eq!(successes.into_iter().filter(|success| *success).count(), 1);
        Ok(())
    }

    #[test]
    fn human_sessions_require_correct_host_origin_and_csrf_and_do_not_survive_restart() -> TestResult
    {
        let origin = origin()?;
        let (state, url) = PairingState::new("127.0.0.1:8750".parse()?)?;
        let now = Instant::now();
        let cookie = state.redeem(capability(&url)?, &origin, now)?;
        let mut headers = headers(&cookie)?;
        let session = state.session(&headers, now)?;
        let csrf = session.csrf_token();
        assert!(write_proof(&state, &headers, csrf, now).is_ok());
        headers.remove(header::ORIGIN);
        assert!(write_proof(&state, &headers, csrf, now).is_err());
        headers.insert(header::ORIGIN, "http://127.0.0.1:8750".parse()?);
        assert!(write_proof(&state, &headers, "incorrect", now).is_err());
        headers.append(header::ORIGIN, "http://127.0.0.1:8750".parse()?);
        assert!(write_proof(&state, &headers, csrf, now).is_err());
        headers.remove(header::ORIGIN);
        headers.insert(header::HOST, "localhost:8750".parse()?);
        assert!(state.session(&headers, now).is_err());
        headers.insert(header::HOST, "127.0.0.1:8750".parse()?);
        headers.append(
            header::COOKIE,
            cookie.split(';').next().ok_or("cookie")?.parse()?,
        );
        assert!(state.session(&headers, now).is_err());
        headers.remove(header::COOKIE);
        headers.insert(
            header::COOKIE,
            cookie.split(';').next().ok_or("cookie")?.parse()?,
        );
        headers.insert(header::AUTHORIZATION, "Bearer agent-token".parse()?);
        assert!(state.session(&headers, now).is_err());
        headers.remove(header::AUTHORIZATION);
        let (restarted, _) = PairingState::new("127.0.0.1:8750".parse()?)?;
        assert!(restarted.session(&headers, now).is_err());
        assert!(restarted.redeem(capability(&url)?, &origin, now).is_err());
        assert!(state.session(&headers, now + SESSION_LIFETIME).is_err());
        assert!(!format!("{session:?}").contains(csrf));
        Ok(())
    }

    #[test]
    fn agent_invites_cannot_create_human_sessions_and_route_only_to_the_confirming_browsers()
    -> TestResult {
        let origin = origin()?;
        let (state, launch) = PairingState::new("127.0.0.1:8750".parse()?)?;
        let now = Instant::now();
        let agent = AgentKey::for_test("verified-agent-one");
        let other = AgentKey::for_test("verified-agent-two");
        assert_eq!(
            state.paired_sessions(&agent, now)?,
            Vec::<BrowserSessionId>::new()
        );
        let invite = state.agent_pairing_url(agent.clone(), now)?;
        assert_eq!(
            invite,
            state.agent_pairing_url(agent.clone(), now + Duration::from_secs(1))?
        );
        assert!(state.redeem(capability(&invite)?, &origin, now).is_err());
        let cookie = state.redeem(capability(&launch)?, &origin, now)?;
        let first_headers = headers(&cookie)?;
        let first_session = state.session(&first_headers, now)?;
        let first_csrf = first_session.csrf_token();
        let first = write_proof(&state, &first_headers, first_csrf, now)?;
        state.associate(&first, capability(&invite)?, now)?;
        assert_eq!(state.paired_sessions(&agent, now)?.len(), 1);
        assert_eq!(
            state.paired_sessions(&other, now)?,
            Vec::<BrowserSessionId>::new()
        );
        assert!(state.associate(&first, capability(&invite)?, now).is_err());
        let second = state.browser_link(now)?;
        let cookie2 = state.redeem(capability(&second)?, &origin, now)?;
        let invite2 = state.agent_pairing_url(agent.clone(), now)?;
        let second_headers = headers(&cookie2)?;
        let second_session = state.session(&second_headers, now)?;
        let second = write_proof(&state, &second_headers, second_session.csrf_token(), now)?;
        state.associate(&second, capability(&invite2)?, now)?;
        assert!(write_proof(&state, &second_headers, first_csrf, now).is_err());
        let targets = state.paired_sessions(&agent, now)?;
        assert_eq!(targets.len(), 2);
        assert_ne!(targets[0], targets[1]);
        assert_eq!(
            state.paired_sessions(&agent, now + SESSION_LIFETIME)?,
            Vec::<BrowserSessionId>::new()
        );
        Ok(())
    }

    #[test]
    fn pending_invites_are_bounded_and_expiration_releases_capacity() -> TestResult {
        let (state, _) = PairingState::new("127.0.0.1:8750".parse()?)?;
        let now = Instant::now();
        for _ in 1..MAX_ENTRIES {
            state.browser_link(now)?;
        }
        assert!(matches!(
            state.browser_link(now),
            Err(PairingError::Capacity)
        ));
        assert!(state.browser_link(now + LINK_LIFETIME).is_ok());
        Ok(())
    }
    #[test]
    fn agent_invite_expiry_is_not_extended_by_repeat_requests() -> TestResult {
        let (state, _) = PairingState::new("127.0.0.1:8750".parse()?)?;
        let now = Instant::now();
        let agent = AgentKey::for_test("verified-token");
        let invite = state.agent_pairing_url(agent.clone(), now)?;
        let remaining = LINK_LIFETIME
            .checked_sub(Duration::from_secs(1))
            .ok_or("fixture lifetime")?;
        assert_eq!(
            invite,
            state.agent_pairing_url(agent.clone(), now + remaining)?
        );
        assert_ne!(invite, state.agent_pairing_url(agent, now + LINK_LIFETIME)?);
        Ok(())
    }

    #[test]
    fn browser_sessions_are_bounded_and_absolute_expiry_frees_capacity() -> TestResult {
        let origin = origin()?;
        let (state, launch) = PairingState::new("127.0.0.1:8750".parse()?)?;
        let now = Instant::now();
        state.redeem(capability(&launch)?, &origin, now)?;
        for _ in 1..MAX_ENTRIES {
            let link = state.browser_link(now)?;
            state.redeem(capability(&link)?, &origin, now)?;
        }
        let link = state.browser_link(now)?;
        assert!(matches!(
            state.redeem(capability(&link)?, &origin, now),
            Err(PairingError::Capacity)
        ));
        let fresh = state.browser_link(now + SESSION_LIFETIME)?;
        assert!(
            state
                .redeem(capability(&fresh)?, &origin, now + SESSION_LIFETIME)
                .is_ok()
        );
        Ok(())
    }
}
