//! Single-listener Desktop server; ledger and authentication remain upstream.

use std::{
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    sync::{Arc, Mutex},
};

use axum::{Router, middleware};
use beankeeper_cli::{
    cli::Cli,
    db::Db,
    mcp::{StreamableHttpServerConfig, http_router_with_host_tools},
    passphrase::resolve_passphrase,
};

use crate::{
    DesktopError,
    boundary::{self, LocalBoundary},
    cli::ServeOptions,
    navigation::{self, Navigation},
    pairing::{self, PairingState},
    review, ui,
};

/// Refuse remote listeners unless explicitly requested.
///
/// # Errors
/// Returns an actionable error for a non-loopback bind without consent.
pub fn validate_bind(bind: SocketAddr, allow_remote: bool) -> Result<(), DesktopError> {
    if bind.ip().is_loopback() && !ui_loopback(bind.ip()) {
        return Err(DesktopError::UnsupportedLoopback(bind));
    }
    if !bind.ip().is_loopback() && !allow_remote {
        return Err(DesktopError::UnsafeBind(bind));
    }
    Ok(())
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
    let navigation = Navigation::new(pairing.clone(), config.cancellation_token.clone());
    let mcp = http_router_with_host_tools(
        cli,
        passphrase.as_ref(),
        read_only,
        config,
        navigation.clone(),
    )?;
    let association_db = Arc::new(Mutex::new(Db::open_snapshot(&cli.db, passphrase.as_ref())?));
    let db = Db::open_snapshot(&cli.db, passphrase.as_ref())?;
    let review = review::router(
        Db::open_existing(&cli.db, passphrase.as_ref())?,
        cli.company.clone(),
        pairing.clone(),
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
        .layer(middleware::from_fn_with_state(boundary, boundary::guard)))
}

/// Run the combined listener until Ctrl-C, then cancel MCP and drain HTTP.
///
/// # Errors
/// Returns bind, runtime, passphrase, ledger or server failures.
pub fn serve(cli: &Cli, options: ServeOptions) -> Result<(), DesktopError> {
    validate_bind(options.bind, options.allow_remote)?;
    let listener = std::net::TcpListener::bind(options.bind)?;
    serve_listener(cli, options, listener)
}

/// Host an already-bound loopback socket, preserving port fallback without a bind race.
///
/// # Errors
/// Returns runtime, ledger or server startup failures.
pub(crate) fn serve_listener(
    cli: &Cli,
    options: ServeOptions,
    listener: std::net::TcpListener,
) -> Result<(), DesktopError> {
    validate_bind(listener.local_addr()?, options.allow_remote)?;
    listener.set_nonblocking(true)?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?;
    let listener = {
        let _enter = runtime.enter();
        tokio::net::TcpListener::from_std(listener)?
    };
    let bound = listener.local_addr()?;
    let config = StreamableHttpServerConfig::default();
    let cancellation = config.cancellation_token.clone();
    let (pairing, pairing_url) = PairingState::new(bound)?;
    let app = application(cli, options.read_only, bound, config, pairing)?;
    if let Ok(directory) = crate::product::state_dir() {
        crate::updates::background(
            directory.join("updates.json"),
            crate::product::updates_disabled(),
        );
    }
    eprintln!("Pair this browser (one use, expires in 5 minutes): {pairing_url}");
    eprintln!("Open the link, then select Pair this browser to confirm.");
    eprintln!(
        "Beankeeper Desktop listening on {} (MCP: /mcp)",
        local_url(bound)
    );
    if !options.bind.ip().is_loopback() {
        eprintln!(
            "Remote listener enabled: plaintext HTTP, no TLS; Host/Origin remain restricted to localhost, 127.0.0.1 and [::1]"
        );
    }
    runtime.block_on(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(async move {
                shutdown_signal().await;
                cancellation.cancel();
            })
            .await
    })?;
    Ok(())
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
