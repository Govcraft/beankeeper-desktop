//! Open Desktop in the OS user's default browser.

use std::{net::SocketAddr, path::Path};

use crate::{
    DesktopError,
    cli::OpenOptions,
    instance::{Health, ListenerAddress, LiveListener},
    pairing::owner,
    server::local_url,
    service::{self, Paths, ServiceError},
};

/// Open the managed listener when running, or a specific foreground listener.
///
/// # Errors
/// Returns an error when Desktop is unavailable or the OS launcher fails.
pub fn open(options: OpenOptions) -> Result<(), DesktopError> {
    let paths = Paths::discover().map_err(service_error)?;
    let listener = match options.bind {
        Some(address) => probe(address)?,
        None => match service::browser_listener(&paths, None) {
            Ok(listener) => listener,
            Err(ServiceError::NotInstalled | ServiceError::NotRunning) => {
                probe(ListenerAddress::default())?
            }
            Err(error) => return Err(service_error(error)),
        },
    };
    launch_at(&paths.state, &listener, launch).map_err(service_error)
}

/// Open the managed service from an OS-authority connection.
pub(crate) fn open_managed(
    paths: &Paths,
    expected_books: Option<&Path>,
) -> Result<(), ServiceError> {
    let listener = service::browser_listener(paths, expected_books)?;
    launch_at(&paths.state, &listener, launch)
}

fn service_error(error: ServiceError) -> DesktopError {
    DesktopError::Service(Box::new(error))
}

fn launch(url: &str) -> Result<(), ServiceError> {
    open::that_detached(url).map_err(|_| ServiceError::BrowserLaunch)
}

/// Verify the listener and request a fresh link before invoking a browser launcher.
///
/// # Errors
/// Returns invalid-address, connection, pairing or launcher errors.
pub fn open_with(
    bind: SocketAddr,
    state: &Path,
    launch: impl FnOnce(&str) -> Result<(), ServiceError>,
) -> Result<(), DesktopError> {
    let address =
        ListenerAddress::try_from(bind).map_err(|_| DesktopError::NotRunning(local_url(bind)))?;
    launch_at(state, &probe(address)?, launch).map_err(service_error)
}

fn probe(address: ListenerAddress) -> Result<LiveListener, DesktopError> {
    let health =
        Health::probe(address).map_err(|_| DesktopError::NotRunning(local_url(address.into())))?;
    Ok((&health).into())
}

fn launch_at(
    state: &Path,
    listener: &LiveListener,
    launch: impl FnOnce(&str) -> Result<(), ServiceError>,
) -> Result<(), ServiceError> {
    let url = owner::request_link(state, listener)?;
    launch(url.as_str())
}
