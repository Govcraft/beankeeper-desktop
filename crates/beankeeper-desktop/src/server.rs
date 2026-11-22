//! Single-listener Desktop server; ledger and authentication remain upstream.

use std::{
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    sync::{Arc, Mutex},
};

use axum::{Router, middleware, routing::get};
use beankeeper_cli::{
    cli::Cli,
    db::Db,
    mcp::{StreamableHttpServerConfig, http_router_with_host_tools},
    passphrase::resolve_passphrase,
};

use crate::{
    DesktopError,
    boundary::{self, LocalBoundary},
    instance::{self, InstanceId, ListenerAddress, Registration},
    navigation::{self, Navigation},
    onboarding::DesktopTools,
    pairing::{self, PairingState},
    review, ui,
};

/// Address accepted by Desktop's local/remote listener policy.
#[derive(Debug, Clone, Copy)]
pub struct ValidatedBind(SocketAddr);

impl ValidatedBind {
    /// The accepted socket address.
    #[must_use]
    pub fn address(self) -> SocketAddr {
        self.0
    }
}

/// Refuse remote listeners unless explicitly requested.
///
/// # Errors
/// Returns an actionable error for a non-loopback bind without consent.
pub fn validate_bind(bind: SocketAddr, allow_remote: bool) -> Result<ValidatedBind, DesktopError> {
    if bind.ip().is_loopback() && !ui_loopback(bind.ip()) {
        return Err(DesktopError::UnsupportedLoopback(bind));
    }
    if !bind.ip().is_loopback() && !allow_remote {
        return Err(DesktopError::UnsafeBind(bind));
    }
    Ok(ValidatedBind(bind))
}

pub(crate) fn ui_loopback(ip: IpAddr) -> bool {
    ip == IpAddr::V4(Ipv4Addr::LOCALHOST) || ip == IpAddr::V6(Ipv6Addr::LOCALHOST)
}

/// Local UI URL for a bound address, including wildcard or remote listeners.
#[must_use]
pub fn local_url(bound: SocketAddr) -> String {
    let ip = if bound.is_ipv6() {
        IpAddr::V6(Ipv6Addr::LOCALHOST)
    } else {
        IpAddr::V4(Ipv4Addr::LOCALHOST)
    };
    format!("http://{}/", SocketAddr::new(ip, bound.port()))
}

/// Merge upstream MCP with embedded UI routes and a listener-wide guard.
///
/// Call during synchronous startup: the upstream factory performs blocking I/O.
/// The caller must cancel `config.cancellation_token` on shutdown.
///
/// # Errors
/// Returns upstream passphrase or existing-ledger startup errors.
pub fn application(
    cli: &Cli,
    read_only: bool,
    bound: SocketAddr,
    config: StreamableHttpServerConfig,
    pairing: PairingState,
) -> Result<Router, DesktopError> {
    application_with_id(
        cli,
        read_only,
        bound,
        config,
        pairing,
        InstanceId::default(),
    )
}

fn application_with_id(
    cli: &Cli,
    read_only: bool,
    bound: SocketAddr,
    config: StreamableHttpServerConfig,
    pairing: PairingState,
    id: InstanceId,
) -> Result<Router, DesktopError> {
    let boundary = LocalBoundary::new(bound.port());
    let config = config
        .with_allowed_hosts(boundary.authorities())
        .with_allowed_origins(boundary.origins())
        .enforce_origin_validation();
    let passphrase = resolve_passphrase(
        cli.passphrase.passphrase_fd,
        cli.passphrase.passphrase_file.as_deref(),
        false,
    )?;
    // Migrate through the review connection before opening the read-only snapshots.
    let review_db = Db::open_existing(&cli.db, passphrase.as_ref())?;
    let navigation = Navigation::new(pairing.clone(), config.cancellation_token.clone());
    let association_db = Arc::new(Mutex::new(Db::open_snapshot(&cli.db, passphrase.as_ref())?));
    let mcp = http_router_with_host_tools(
        cli,
        passphrase.as_ref(),
        read_only,
        config,
        DesktopTools {
            bound,
            navigation: navigation.clone(),
            status_db: Arc::clone(&association_db),
        },
    )?;
    let db = Db::open_snapshot(&cli.db, passphrase.as_ref())?;
    let review = review::router(
        review_db,
        cli.company.clone(),
        pairing.clone(),
        &cli.db.with_extension("owner-token"),
    )?;
    drop(passphrase);
    let fallback = cli.agent_token.clone();
    let mcp = mcp.layer(middleware::from_fn_with_state(
        (pairing.clone(), fallback, association_db),
        pairing::mcp_guard,
    ));
    Ok(ui::router(db, cli.company.clone(), pairing.clone())
        .merge(review)
        .merge(navigation::router(navigation, pairing.clone()))
        .merge(pairing::router(pairing))
        .merge(mcp)
        .route("/health", get(move || async move { instance::health(&id) }))
        .layer(middleware::from_fn_with_state(boundary, boundary::guard)))
}

/// Run the combined listener until Ctrl-C, then cancel MCP and drain HTTP.
///
/// # Errors
/// Returns bind, runtime, passphrase, ledger or server failures.
pub fn serve(cli: &Cli, bind: ValidatedBind, read_only: bool) -> Result<(), DesktopError> {
    let listener = std::net::TcpListener::bind(bind.address())?;
    serve_bound(cli, listener, read_only, InstanceId::default())
}

/// Host an already-bound loopback socket, preserving port fallback without a bind race.
///
/// # Errors
/// Returns runtime, ledger or server startup failures.
pub(crate) fn serve_listener(
    cli: &Cli,
    listener: std::net::TcpListener,
    id: InstanceId,
) -> Result<(), DesktopError> {
    validate_bind(listener.local_addr()?, false)?;
    serve_bound(cli, listener, false, id)
}

fn serve_bound(
    cli: &Cli,
    listener: std::net::TcpListener,
    read_only: bool,
    id: InstanceId,
) -> Result<(), DesktopError> {
    listener.set_nonblocking(true)?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?;
    let serving = {
        let _enter = runtime.enter();
        tokio::net::TcpListener::from_std(listener.try_clone()?)?
    };
    let bound = listener.local_addr()?;
    let config = StreamableHttpServerConfig::default();
    let cancellation = config.cancellation_token.clone();
    let (pairing, pairing_url) = PairingState::new(bound)?;
    let app = application_with_id(cli, read_only, bound, config, pairing, id.clone())?;
    let directory = crate::product::state_dir().map_err(DesktopError::Product)?;
    let registration = ListenerAddress::from_bound(bound)
        .map(|address| Registration::publish(&directory, cli.db.as_path().try_into()?, address, id))
        .transpose()
        .map_err(|error| DesktopError::Service(Box::new(error)))?;
    crate::updates::background(
        directory.join("updates.json"),
        crate::product::update_checks(),
    );
    eprintln!("Pair this browser (one use, expires in 5 minutes): {pairing_url}");
    eprintln!("Open the link, then select Pair this browser to confirm.");
    eprintln!(
        "Beankeeper Desktop listening on {} (MCP: /mcp)",
        local_url(bound)
    );
    if !bound.ip().is_loopback() {
        eprintln!(
            "Remote listener enabled: plaintext HTTP, no TLS; Host/Origin remain restricted to localhost, 127.0.0.1 and [::1]"
        );
    }
    let result = runtime.block_on(async move {
        axum::serve(serving, app)
            .with_graceful_shutdown(async move {
                shutdown_signal().await;
                cancellation.cancel();
            })
            .await
    });
    drop(registration);
    drop(listener);
    result.map_err(Into::into)
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        if let Ok(mut terminate) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            tokio::select! { _ = tokio::signal::ctrl_c() => {}, _ = terminate.recv() => {} }
            return;
        }
    }
    let _ = tokio::signal::ctrl_c().await;
}
