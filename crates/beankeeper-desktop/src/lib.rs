//! Local Desktop host for the upstream Beankeeper CLI and MCP router.

pub mod agents;
pub mod boundary;
pub mod bridge;
pub mod browser;
pub mod changes;
pub mod cli;
pub mod demo;
mod instance;
mod ledger_rows;
pub mod license;
pub mod links;
mod markup;
mod money;
mod navigation;
pub mod onboarding;
mod output;
pub mod pairing;
mod private_file;
pub mod product;
pub mod reports;
mod review;
pub mod server;
pub mod service;
pub mod setup;
mod ui;
pub mod updates;
mod views;

use std::{error::Error, fmt, io, net::SocketAddr};

/// Startup and browser-launch failures owned by the Desktop host.
#[derive(Debug)]
pub enum DesktopError {
    /// A requested view cannot apply these filters.
    Link(links::LinkError),
    /// Per-user service management failed.
    Service(Box<service::ServiceError>),
    /// License or update command failed.
    Product(product::ProductError),
    /// Onboarding could not finish safely.
    Setup(Box<setup::SetupError>),
    /// The stdio-to-HTTP bridge failed.
    Bridge(bridge::BridgeError),
    /// A non-loopback listener needs explicit consent.
    UnsafeBind(SocketAddr),
    /// The UI supports the canonical IPv4 and IPv6 loopback addresses.
    UnsupportedLoopback(SocketAddr),
    /// A socket, runtime, or browser launcher failed.
    Io(io::Error),
    /// The upstream ledger/router could not be opened.
    Ledger(beankeeper_cli::error::CliError),
    /// The target did not identify itself as a running Desktop server.
    NotRunning(String),
    /// The bounded local health probe failed.
    Probe(reqwest::Error),
    /// Browser capabilities could not be created.
    Pairing(pairing::PairingError),
}

impl fmt::Display for DesktopError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Link(error) => write!(f, "VALIDATION: {error}"),
            Self::Service(error) => write!(f, "{}: {error}", error.code()),
            Self::Product(error) => write!(f, "{error}"),
            Self::Pairing(error) => write!(f, "{error}"),
            Self::Setup(error) => write!(f, "{error}"),
            Self::Bridge(error) => write!(f, "{error}"),
            Self::UnsafeBind(bind) => write!(
                f,
                "non-loopback bind {bind} requires --allow-remote (plaintext HTTP, no TLS)"
            ),
            Self::UnsupportedLoopback(bind) => write!(
                f,
                "unsupported loopback address {bind}; use 127.0.0.1 or [::1]"
            ),
            Self::Io(error) => write!(f, "Desktop I/O failed: {error}"),
            Self::Ledger(error) => write!(f, "{error}"),
            Self::NotRunning(url) => write!(
                f,
                "Beankeeper Desktop is not running at {url}; start `bk serve` first"
            ),
            Self::Probe(error) => write!(
                f,
                "cannot reach Beankeeper Desktop; start `bk serve` first: {error}"
            ),
        }
    }
}

impl Error for DesktopError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Link(error) => Some(error),
            Self::Service(error) => Some(error),
            Self::Product(error) => Some(error),
            Self::Pairing(error) => Some(error),
            Self::Setup(error) => Some(error),
            Self::Bridge(error) => Some(error),
            Self::Io(error) => Some(error),
            Self::Ledger(error) => Some(error),
            Self::Probe(error) => Some(error),
            Self::UnsafeBind(_) | Self::UnsupportedLoopback(_) | Self::NotRunning(_) => None,
        }
    }
}

impl From<pairing::PairingError> for DesktopError {
    fn from(error: pairing::PairingError) -> Self {
        Self::Pairing(error)
    }
}

impl From<io::Error> for DesktopError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}
impl From<beankeeper_cli::error::CliError> for DesktopError {
    fn from(error: beankeeper_cli::error::CliError) -> Self {
        Self::Ledger(error)
    }
}
impl From<reqwest::Error> for DesktopError {
    fn from(error: reqwest::Error) -> Self {
        Self::Probe(error)
    }
}
