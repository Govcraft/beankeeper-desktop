//! Private listener records for the books each local Desktop launch serves.

use std::{
    fmt, fs,
    io::{self, Read},
    net::{Ipv4Addr, Ipv6Addr, SocketAddr},
    num::NonZeroU32,
    path::{Path, PathBuf},
    str::FromStr,
    time::Duration,
};

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use mti::prelude::*;
use reqwest::{Url, blocking::Client, redirect::Policy};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::service::{ServiceError, private_json, read_private};

pub(crate) const ID_HEADER: &str = "x-beankeeper-desktop-instance";
pub(crate) const HEALTH: &str = "beankeeper-desktop-v1";

/// A local listener address with a port assigned by the OS.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "SocketAddr", into = "SocketAddr")]
pub(crate) struct ListenerAddress(SocketAddr);

impl Default for ListenerAddress {
    fn default() -> Self {
        Self(SocketAddr::from((Ipv4Addr::LOCALHOST, 8750)))
    }
}

impl TryFrom<SocketAddr> for ListenerAddress {
    type Error = &'static str;

    fn try_from(address: SocketAddr) -> Result<Self, Self::Error> {
        if crate::server::ui_loopback(address.ip()) && address.port() != 0 {
            Ok(Self(address))
        } else {
            Err("Expected 127.0.0.1 or [::1] with a nonzero port.")
        }
    }
}

impl FromStr for ListenerAddress {
    type Err = &'static str;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        value
            .parse::<SocketAddr>()
            .map_err(|_| "Expected an IP socket address.")?
            .try_into()
    }
}

impl From<ListenerAddress> for SocketAddr {
    fn from(address: ListenerAddress) -> Self {
        address.0
    }
}

impl ListenerAddress {
    pub(crate) fn from_bound(bound: SocketAddr) -> Option<Self> {
        let address = if bound.ip().is_unspecified() {
            SocketAddr::new(
                if bound.is_ipv6() {
                    Ipv6Addr::LOCALHOST.into()
                } else {
                    Ipv4Addr::LOCALHOST.into()
                },
                bound.port(),
            )
        } else {
            bound
        };
        address.try_into().ok()
    }

    pub(crate) fn url(self, path: &str) -> Result<Url, ServiceError> {
        let mut url =
            Url::parse("http://localhost/").map_err(|_| ServiceError::EndpointIdentity)?;
        url.set_ip_host(self.0.ip())
            .map_err(|()| ServiceError::EndpointIdentity)?;
        url.set_port(Some(self.0.port()))
            .map_err(|()| ServiceError::EndpointIdentity)?;
        url.set_path(path);
        Ok(url)
    }

    fn filename(self) -> String {
        let family = if self.0.is_ipv6() { 6 } else { 4 };
        format!("v{family}-{}.json", self.0.port())
    }
}

/// An absolute books path as recorded by its owner.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "PathBuf", into = "PathBuf")]
pub(crate) struct BooksPath(PathBuf);

impl TryFrom<PathBuf> for BooksPath {
    type Error = &'static str;

    fn try_from(path: PathBuf) -> Result<Self, Self::Error> {
        if path.is_absolute() {
            Ok(Self(path))
        } else {
            Err("The books path must be absolute.")
        }
    }
}

impl TryFrom<&Path> for BooksPath {
    type Error = io::Error;

    fn try_from(path: &Path) -> Result<Self, Self::Error> {
        fs::canonicalize(path).map(Self)
    }
}

impl From<BooksPath> for PathBuf {
    fn from(path: BooksPath) -> Self {
        path.0
    }
}

impl AsRef<Path> for BooksPath {
    fn as_ref(&self) -> &Path {
        &self.0
    }
}

impl BooksPath {
    fn records(&self, state: &Path) -> PathBuf {
        let digest = Sha256::digest(self.0.as_os_str().as_encoded_bytes());
        state.join("instances").join(URL_SAFE_NO_PAD.encode(digest))
    }
}

/// Public launch identity, independent of browser and MCP credentials.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub(crate) struct InstanceId(MagicTypeId);

impl InstanceId {
    const PREFIX: &str = "desktop";
}

impl Default for InstanceId {
    fn default() -> Self {
        Self(Self::PREFIX.create_type_id::<V7>())
    }
}

impl fmt::Display for InstanceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl FromStr for InstanceId {
    type Err = &'static str;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let id: MagicTypeId = value
            .parse()
            .map_err(|_| "Expected a Desktop instance ID.")?;
        if id.prefix().as_str() == Self::PREFIX {
            Ok(Self(id))
        } else {
            Err("Expected a Desktop instance ID.")
        }
    }
}

impl TryFrom<String> for InstanceId {
    type Error = &'static str;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        value.parse()
    }
}

impl From<InstanceId> for String {
    fn from(id: InstanceId) -> Self {
        id.to_string()
    }
}

const PID_HEADER: &str = "x-beankeeper-desktop-pid";
const VERSION_HEADER: &str = "x-beankeeper-desktop-version";

pub(crate) fn health(id: &InstanceId) -> ([(&'static str, String); 3], &'static str) {
    (
        [
            (VERSION_HEADER, env!("CARGO_PKG_VERSION").into()),
            (PID_HEADER, std::process::id().to_string()),
            (ID_HEADER, id.to_string()),
        ],
        HEALTH,
    )
}

pub(crate) struct Health {
    address: ListenerAddress,
    pid: NonZeroU32,
    id: Option<InstanceId>,
    pub(crate) version: String,
}

impl Health {
    pub(crate) fn probe(address: ListenerAddress) -> Result<Self, ServiceError> {
        let url = address.url("/health")?;
        let response = Client::builder()
            .no_proxy()
            .redirect(Policy::none())
            .timeout(Duration::from_secs(2))
            .build()
            .and_then(|client| client.get(url).send())
            .map_err(|_| ServiceError::EndpointConnect)?;
        let header = |key| {
            response
                .headers()
                .get(key)
                .and_then(|value| value.to_str().ok())
        };
        let pid = header(PID_HEADER)
            .and_then(|value| value.parse().ok())
            .ok_or(ServiceError::EndpointIdentity)?;
        let version = header(VERSION_HEADER)
            .filter(|version| !version.is_empty())
            .ok_or(ServiceError::EndpointIdentity)?
            .to_owned();
        let id = header(ID_HEADER)
            .map(str::parse)
            .transpose()
            .map_err(|_| ServiceError::EndpointIdentity)?;
        if !response.status().is_success() {
            return Err(ServiceError::EndpointIdentity);
        }
        let mut body = String::new();
        response
            .take(64)
            .read_to_string(&mut body)
            .map_err(|_| ServiceError::EndpointConnect)?;
        if body != HEALTH {
            return Err(ServiceError::EndpointIdentity);
        }
        Ok(Self {
            address,
            pid,
            id,
            version,
        })
    }

    pub(crate) fn verify(
        &self,
        address: ListenerAddress,
        pid: NonZeroU32,
        id: Option<&InstanceId>,
    ) -> Option<LiveListener> {
        if self.address == address && self.pid == pid && self.id.as_ref() == id {
            Some(LiveListener(address))
        } else {
            None
        }
    }
}

/// The listener matched the recorded process at the time of discovery.
#[derive(Clone, Copy)]
pub(crate) struct LiveListener(ListenerAddress);

impl From<LiveListener> for ListenerAddress {
    fn from(listener: LiveListener) -> Self {
        listener.0
    }
}

impl From<LiveListener> for SocketAddr {
    fn from(listener: LiveListener) -> Self {
        listener.0.into()
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Record {
    books: BooksPath,
    address: ListenerAddress,
    pid: NonZeroU32,
    id: InstanceId,
}

impl Record {
    fn verify(&self, health: &Health) -> Option<LiveListener> {
        health.verify(self.address, self.pid, Some(&self.id))
    }
}

pub(crate) struct Registration(PathBuf);

impl Registration {
    pub(crate) fn publish(
        state: &Path,
        books: BooksPath,
        address: ListenerAddress,
        id: InstanceId,
    ) -> Result<Self, ServiceError> {
        let directory = books.records(state);
        for path in [state.join("instances"), directory.clone()] {
            fs::create_dir_all(&path)?;
            crate::setup::keychain::restrict_directory(&path)?;
        }
        let path = directory.join(address.filename());
        let pid =
            NonZeroU32::try_from(std::process::id()).map_err(|_| ServiceError::EndpointIdentity)?;
        private_json(
            &path,
            &Record {
                books,
                address,
                pid,
                id,
            },
        )?;
        Ok(Self(path))
    }
}

impl Drop for Registration {
    fn drop(&mut self) {
        // The caller holds the listener through this cleanup. A crash can
        // leave a descriptor, but a later launch will have a different ID.
        let _ = fs::remove_file(&self.0);
    }
}

pub(crate) fn discover(
    state: &Path,
    books: &BooksPath,
    preferred: Option<ListenerAddress>,
) -> Result<Option<LiveListener>, ServiceError> {
    let directory = books.records(state);
    let mut live = None;
    for path in registry_entries(&directory)? {
        let Ok(record) = read_private::<Record>(&path) else {
            continue;
        };
        if record.books == *books
            && preferred.is_none_or(|address| record.address == address)
            && path
                .file_name()
                .is_some_and(|name| name == record.address.filename().as_str())
            && let Ok(health) = Health::probe(record.address)
            && let Some(listener) = record.verify(&health)
            && live.replace(listener).is_some()
        {
            return Err(ServiceError::MultipleInstances);
        }
    }
    Ok(live)
}

fn registry_entries(directory: &Path) -> io::Result<Vec<PathBuf>> {
    match fs::read_dir(directory) {
        Ok(entries) => entries
            .map(|entry| entry.map(|entry| entry.path()))
            .collect(),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(error) => Err(error),
    }
}

/// Safe to include in JSON or show to a local operator.
#[derive(Debug, Serialize)]
pub struct PortWarning {
    code: PortWarningCode,
    address: SocketAddr,
    books: Option<BooksPath>,
    message: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
enum PortWarningCode {
    DesktopDefaultPortOccupied,
}

impl PortWarning {
    pub(crate) fn message(&self) -> &str {
        &self.message
    }
}

pub(crate) fn default_port_warning(
    state: &Path,
    service_books: &Path,
    service_address: Option<ListenerAddress>,
) -> Option<PortWarning> {
    let address = ListenerAddress::default();
    if service_address == Some(address) {
        return None;
    }
    port_warning(state, service_books, address, &Health::probe(address).ok()?)
}

fn port_warning(
    state: &Path,
    service_books: &Path,
    address: ListenerAddress,
    health: &Health,
) -> Option<PortWarning> {
    let books = registry_entries(&state.join("instances"))
        .unwrap_or_default()
        .into_iter()
        .filter_map(|dir| read_private::<Record>(&dir.join(address.filename())).ok())
        .find(|record| record.verify(health).is_some())
        .map(|record| record.books)
        .or_else(|| legacy_books(health));
    if books
        .as_ref()
        .is_some_and(|books| books.as_ref() == service_books)
    {
        return None;
    }
    let address = SocketAddr::from(address);
    let message = match &books {
        Some(path) => format!(
            "Another Beankeeper Desktop instance is listening at {address} for books at {}.",
            path.as_ref().display()
        ),
        None => format!(
            "Another Beankeeper Desktop instance is listening at {address}. It has no matching private instance record, so its books path is unavailable."
        ),
    };
    Some(PortWarning {
        code: PortWarningCode::DesktopDefaultPortOccupied,
        address,
        books,
        message,
    })
}

#[cfg(target_os = "linux")]
fn legacy_books(health: &Health) -> Option<BooksPath> {
    use std::{
        ffi::OsStr,
        os::unix::{ffi::OsStrExt, fs::MetadataExt},
    };

    // Older foreground releases published a health PID but no books record.
    // The reported process must be this user's bk with an explicit database.
    if health.id.is_some() {
        return None;
    }
    let process = Path::new("/proc").join(health.pid.to_string());
    if process.metadata().ok()?.uid() != rustix::process::geteuid().as_raw()
        || fs::read_link(process.join("exe")).ok()?.file_name()? != "bk"
    {
        return None;
    }
    let mut command_line = Vec::new();
    fs::File::open(process.join("cmdline"))
        .ok()?
        .take(65_537)
        .read_to_end(&mut command_line)
        .ok()?;
    if command_line.len() > 65_536 {
        return None;
    }
    let mut args = command_line.split(|byte| *byte == 0).skip(1);
    let argument = loop {
        match args.next()? {
            b"--" => return None,
            b"--db" => break args.next()?,
            arg if arg.starts_with(b"--db=") => break &arg[5..],
            _ => {}
        }
    };
    if argument.is_empty() {
        return None;
    }
    let path = Path::new(OsStr::from_bytes(argument));
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        fs::read_link(process.join("cwd")).ok()?.join(path)
    };
    BooksPath::try_from(path.as_path()).ok()
}

#[cfg(not(target_os = "linux"))]
fn legacy_books(_: &Health) -> Option<BooksPath> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn competing_port_warning_uses_only_the_record_matching_live_health()
    -> Result<(), Box<dyn std::error::Error>> {
        let state = tempfile::tempdir()?;
        let occupied = state.path().join("other-books.db");
        // The warning uses local records. The books file need not be readable
        // or even present when constructing the diagnostics.
        let books = BooksPath::try_from(occupied.clone())?;
        let address = ListenerAddress::default();
        let id = InstanceId::default();
        let _registration = Registration::publish(state.path(), books, address, id.clone())?;
        let pid = NonZeroU32::try_from(std::process::id())?;
        let mut health = Health {
            address,
            pid,
            id: Some(id),
            version: env!("CARGO_PKG_VERSION").into(),
        };
        let managed = state.path().join("managed.db");
        let warning =
            port_warning(state.path(), &managed, address, &health).ok_or("missing port warning")?;
        let json = serde_json::to_value(&warning)?;
        assert_eq!(json["code"], "DESKTOP_DEFAULT_PORT_OCCUPIED");
        assert_eq!(json["books"], serde_json::to_value(&occupied)?);
        assert!(port_warning(state.path(), &occupied, address, &health).is_none());

        health.id = Some(InstanceId::default());
        let warning = port_warning(state.path(), &managed, address, &health)
            .ok_or("missing warning for unregistered instance")?;
        assert!(warning.books.is_none());
        assert!(!occupied.exists());
        Ok(())
    }
}
