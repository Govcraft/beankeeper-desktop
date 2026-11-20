//! Bounded, proxy-free detection before launching the user's default browser.

use std::{net::SocketAddr, time::Duration};

use crate::{DesktopError, server::local_url, ui::HEALTH};

/// Verify the server identity and launch the default browser.
///
/// # Errors
/// Returns an error when Desktop is unavailable or the OS launcher fails.
pub fn open(bind: SocketAddr) -> Result<(), DesktopError> {
    open_with(bind, |url| open::that(url).map_err(DesktopError::from))
}

/// Probe before invoking a supplied browser launcher.
///
/// This boundary lets callers test launch behavior without opening a real tab.
///
/// # Errors
/// Returns invalid-address, connection, server-identity or launcher errors.
pub fn open_with(
    bind: SocketAddr,
    launch: impl FnOnce(&str) -> Result<(), DesktopError>,
) -> Result<(), DesktopError> {
    if !crate::server::ui_loopback(bind.ip()) || bind.port() == 0 {
        return Err(DesktopError::NotRunning(local_url(bind)));
    }
    let url = local_url(bind);
    let client = reqwest::blocking::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(2))
        .build()?;
    let response = client.get(format!("{url}health")).send()?;
    if !response.status().is_success() {
        return Err(DesktopError::NotRunning(url));
    }
    // Bound the body too: unrelated services must not make `bk open` allocate
    // an unbounded response while trying to identify a Desktop listener.
    use std::io::Read;
    let mut body = String::new();
    response.take(64).read_to_string(&mut body)?;
    if body != HEALTH {
        return Err(DesktopError::NotRunning(url));
    }
    launch(&url)
}
