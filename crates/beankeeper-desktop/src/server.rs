//! Single-listener Desktop server; ledger and authentication remain upstream.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

use axum::{Router, middleware};
use beankeeper_cli::{
    cli::Cli,
    db::Db,
    passphrase::resolve_passphrase,
    mcp::{StreamableHttpServerConfig, http_router},
};
use clap::Parser;
use secrecy::ExposeSecret;

use crate::{
    DesktopError,
    boundary::{self, LocalBoundary},
    cli::ServeOptions,
    pairing::{self, AgentKey, PairingState},
    ui,
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
    let mcp = ledger_router(cli, read_only, config, passphrase.as_ref())?;
    let db = Db::open_snapshot(&cli.db, passphrase.as_ref())?;
    drop(passphrase);
    let fallback = cli
        .agent_token
        .as_ref()
        .map(|token| AgentKey::from_verified_token(token.expose_secret()));
    let mcp = mcp.layer(middleware::from_fn_with_state(
        (pairing.clone(), fallback),
        pairing::mcp_guard,
    ));
    Ok(ui::router(db, cli.company.clone(), pairing.clone())
        .merge(pairing::router(pairing))
        .merge(mcp)
        .layer(middleware::from_fn_with_state(boundary, boundary::guard)))
}

// The upstream factory resolves a source itself. Feed the already-resolved
// secret through an anonymous pipe so one-shot commands and FDs run only once.
fn ledger_router(
    cli: &Cli,
    read_only: bool,
    config: StreamableHttpServerConfig,
    passphrase: Option<&secrecy::SecretString>,
) -> Result<Router, DesktopError> {
    let Some(passphrase) = passphrase else {
        return Ok(http_router(cli, read_only, config)?);
    };
    let mut options = Cli::try_parse_from(["bk", "mcp", "serve"])
        .map_err(|error| std::io::Error::other(error.to_string()))?;
    options.db.clone_from(&cli.db);
    options.agent_token.clone_from(&cli.agent_token);
    router_with_passphrase(&mut options, read_only, config, passphrase)
}

#[cfg(unix)]
fn router_with_passphrase(
    options: &mut Cli,
    read_only: bool,
    config: StreamableHttpServerConfig,
    passphrase: &secrecy::SecretString,
) -> Result<Router, DesktopError> {
    use std::{io::Write, os::fd::AsRawFd};

    let (reader, mut writer) = std::io::pipe()?;
    options.passphrase.passphrase_fd = Some(reader.as_raw_fd());
    std::thread::scope(|scope| {
        let writing = scope.spawn(move || writer.write_all(passphrase.expose_secret().as_bytes()));
        let router = http_router(options, read_only, config);
        drop(reader);
        writing
            .join()
            .map_err(|_| std::io::Error::other("passphrase pipe writer failed"))??;
        Ok(router?)
    })
}

#[cfg(not(unix))]
fn router_with_passphrase(
    _options: &mut Cli,
    _read_only: bool,
    _config: StreamableHttpServerConfig,
    _passphrase: &secrecy::SecretString,
) -> Result<Router, DesktopError> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "encrypted Desktop ledgers require Unix passphrase FDs until the upstream router accepts a resolved secret",
    ).into())
}

/// Run the combined listener until Ctrl-C, then cancel MCP and drain HTTP.
///
/// # Errors
/// Returns bind, runtime, passphrase, ledger or server failures.
pub fn serve(cli: &Cli, options: ServeOptions) -> Result<(), DesktopError> {
    validate_bind(options.bind, options.allow_remote)?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?;
    let listener = runtime.block_on(tokio::net::TcpListener::bind(options.bind))?;
    let bound = listener.local_addr()?;
    let config = StreamableHttpServerConfig::default();
    let cancellation = config.cancellation_token.clone();
    let (pairing, pairing_url) = PairingState::new(bound)?;
    let app = application(cli, options.read_only, bound, config, pairing)?;
    eprintln!("Pair this browser (one use, expires in 5 minutes): {pairing_url}");
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
                let _ = tokio::signal::ctrl_c().await;
                cancellation.cancel();
            })
            .await
    })?;
    Ok(())
}
