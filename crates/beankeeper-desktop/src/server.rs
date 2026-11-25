//! Single-listener Desktop server; ledger and authentication remain upstream.

use std::{
    io,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    path::Path,
    sync::{Arc, Mutex},
};

use axum::{
    Router,
    extract::connect_info::Connected,
    middleware,
    routing::get,
    serve::{IncomingStream, Listener},
};
use beankeeper_cli::{
    cli::Cli,
    db::Db,
    mcp::{
        LedgerTargets, StreamableHttpServerConfig, http_router_with_host_tools,
        http_router_with_ledger_targets,
    },
    passphrase::resolve_passphrase,
};

use crate::{
    DesktopError,
    boundary::{self, LocalBoundary},
    changes,
    demo::DemoBooks,
    instance::{self, BooksPath, InstanceId, ListenerAddress, Registration},
    navigation::{self, Navigation},
    onboarding::DesktopTools,
    pairing::{self, PairingState, owner},
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
        ApplicationMode::default(),
    )
    .map(|(router, _)| router)
}

#[derive(Default)]
struct ApplicationMode {
    id: InstanceId,
    managed_books: Option<std::path::PathBuf>,
    owner_key: Option<owner::OwnerKey>,
}

fn application_with_id(
    cli: &Cli,
    read_only: bool,
    bound: SocketAddr,
    config: StreamableHttpServerConfig,
    pairing: PairingState,
    ApplicationMode {
        id,
        managed_books,
        owner_key,
    }: ApplicationMode,
) -> Result<(Router, Option<DemoBooks>), DesktopError> {
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
    let books = BooksPath::try_from(cli.db.as_path())?;
    let navigation = Navigation::new(pairing.clone(), config.cancellation_token.clone());
    let association_db = Arc::new(Mutex::new(Db::open_snapshot(&cli.db, passphrase.as_ref())?));
    let mut tools = DesktopTools {
        bound,
        navigation: navigation.clone(),
        status_db: Arc::clone(&association_db),
        managed_books,
        demo: None,
        books: books.clone(),
        company: cli.company.clone(),
    };
    let demo;
    let mcp = if let Some(managed_books) = &tools.managed_books {
        let targets = LedgerTargets::new(Db::open_existing(&cli.db, passphrase.as_ref())?);
        demo = Some(DemoBooks::new(
            targets.clone(),
            managed_books,
            config.cancellation_token.clone(),
        ));
        tools.demo.clone_from(&demo);
        http_router_with_ledger_targets(cli, targets, read_only, config, tools)?
    } else {
        demo = None;
        http_router_with_host_tools(cli, passphrase.as_ref(), read_only, config, tools)?
    };
    let db = Db::open_snapshot(&cli.db, passphrase.as_ref())?;
    let review = review::router(
        review_db,
        cli.company.clone(),
        pairing.clone(),
        &cli.db.with_extension("owner-token"),
    )?;
    let changes = changes::router(
        Db::open_existing(&cli.db, passphrase.as_ref())?,
        books,
        cli.company.clone(),
        cli.db.with_extension("owner-token"),
        pairing.clone(),
    );
    drop(passphrase);
    let fallback = cli.agent_token.clone();
    let mcp = mcp.layer(middleware::from_fn_with_state(
        (pairing.clone(), fallback, association_db),
        pairing::mcp_guard,
    ));
    let owner_routes = owner_key.map(|key| owner::router(pairing.clone(), key, id.clone()));
    let router = ui::router(db, cli.company.clone(), pairing.clone())
        .merge(review)
        .merge(changes)
        .merge(navigation::router(navigation, pairing.clone()))
        .merge(pairing::router(pairing))
        .merge(owner_routes.unwrap_or_default())
        .merge(mcp)
        .route("/health", get(move || async move { instance::health(&id) }))
        .layer(middleware::from_fn_with_state(boundary, boundary::guard));
    Ok((router, demo))
}

/// Run the combined listener until Ctrl-C, then cancel MCP and drain HTTP.
///
/// # Errors
/// Returns bind, runtime, passphrase, ledger or server failures.
pub fn serve(cli: &Cli, bind: ValidatedBind, read_only: bool) -> Result<(), DesktopError> {
    let listener = std::net::TcpListener::bind(bind.address())?;
    serve_bound(cli, listener, read_only, InstanceId::default(), None)
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
    serve_bound(cli, listener, false, id, Some(cli.db.clone()))
}

fn serve_bound(
    cli: &Cli,
    listener: std::net::TcpListener,
    read_only: bool,
    id: InstanceId,
    managed_books: Option<std::path::PathBuf>,
) -> Result<(), DesktopError> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?;
    let bound = listener.local_addr()?;
    let config = StreamableHttpServerConfig::default();
    let cancellation = config.cancellation_token.clone();
    let owner_key = ListenerAddress::from_bound(bound)
        .map(|address| owner::OwnerKey::generate().map(|key| (address, key)))
        .transpose()?;
    let (pairing, startup_link) = if managed_books.is_some() {
        (PairingState::for_listener(bound), None)
    } else {
        let (pairing, link) = PairingState::new(bound)?;
        (pairing, Some(link))
    };
    let (app, demo) = application_with_id(
        cli,
        read_only,
        bound,
        config,
        pairing,
        ApplicationMode {
            id: id.clone(),
            managed_books,
            owner_key: owner_key.as_ref().map(|(_, key)| key.clone()),
        },
    )?;
    let directory = crate::product::state_dir().map_err(DesktopError::Product)?;
    let serving = RegisteredListener::publish(
        listener,
        runtime.handle(),
        &directory,
        &cli.db,
        id,
        owner_key,
    )?;
    crate::updates::background(
        directory.join("updates.json"),
        crate::product::update_checks(),
    );
    if let Some(link) = startup_link {
        eprintln!("Pair this browser (one use, expires in 5 minutes): {link}");
        eprintln!("Open the link, then select Pair this browser to confirm.");
    }
    eprintln!(
        "Beankeeper Desktop listening on {} (MCP: /mcp)",
        local_url(bound)
    );
    if !bound.ip().is_loopback() {
        eprintln!(
            "Remote listener enabled: plaintext HTTP, no TLS; Host/Origin remain restricted to localhost, 127.0.0.1 and [::1]"
        );
    }
    runtime.block_on(async move {
        let shutdown = cancellation.clone();
        let result = axum::serve(
            serving,
            app.into_make_service_with_connect_info::<owner::Peer>(),
        )
        .with_graceful_shutdown(async move {
            shutdown_signal().await;
            shutdown.cancel();
        })
        .await;
        cancellation.cancel();
        if let Some(demo) = demo
            && let Err(error) = demo.remove().await
        {
            eprintln!("{error}");
        }
        result
    })?;
    Ok(())
}

struct RegisteredListener {
    // Fields drop in declaration order. Remove the record before the OS can
    // give the port to another launch that would write a record at the same path.
    _registration: Option<Registration>,
    _owner_key: Option<owner::KeyFile>,
    listener: tokio::net::TcpListener,
}

impl RegisteredListener {
    fn publish(
        listener: std::net::TcpListener,
        runtime: &tokio::runtime::Handle,
        state: &Path,
        books: &Path,
        id: InstanceId,
        owner_key: Option<(ListenerAddress, owner::OwnerKey)>,
    ) -> Result<Self, DesktopError> {
        listener.set_nonblocking(true)?;
        let listener = {
            let _enter = runtime.enter();
            tokio::net::TcpListener::from_std(listener)?
        };
        let registration = ListenerAddress::from_bound(listener.local_addr()?)
            .map(|address| Registration::publish(state, books.try_into()?, address, id))
            .transpose()
            .map_err(|error| DesktopError::Service(Box::new(error)))?;
        let key_file = owner_key
            .map(|(address, key)| key.publish(state, address))
            .transpose()
            .map_err(|error| DesktopError::Service(Box::new(error)))?;
        Ok(Self {
            _registration: registration,
            _owner_key: key_file,
            listener,
        })
    }
}

impl Listener for RegisteredListener {
    type Io = tokio::net::TcpStream;
    type Addr = SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        Listener::accept(&mut self.listener).await
    }

    fn local_addr(&self) -> io::Result<Self::Addr> {
        self.listener.local_addr()
    }
}

impl Connected<IncomingStream<'_, RegisteredListener>> for owner::Peer {
    fn connect_info(stream: IncomingStream<'_, RegisteredListener>) -> Self {
        Self(*stream.remote_addr())
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::instance::BooksPath;

    #[test]
    fn registered_listener_serves_health_and_removes_its_record_on_shutdown()
    -> Result<(), Box<dyn std::error::Error>> {
        #[cfg(unix)]
        use std::os::fd::AsRawFd;
        #[cfg(windows)]
        use std::os::windows::io::AsRawSocket;

        let state = tempfile::Builder::new()
            .prefix("bk service café 's owner’s-")
            .tempdir()?;
        let books = state.path().join("books.db");
        std::fs::File::create(&books)?;
        let socket = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
        let bound = socket.local_addr()?;
        #[cfg(unix)]
        let original_socket = socket.as_raw_fd();
        #[cfg(windows)]
        let original_socket = socket.as_raw_socket();
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()?;
        let id = InstanceId::default();
        let serving = RegisteredListener::publish(
            socket,
            runtime.handle(),
            state.path(),
            &books,
            id.clone(),
            None,
        )?;
        #[cfg(unix)]
        assert_eq!(serving.listener.as_raw_fd(), original_socket);
        #[cfg(windows)]
        assert_eq!(serving.listener.as_raw_socket(), original_socket);
        let app = Router::new().route("/health", get(move || async move { instance::health(&id) }));
        let (shutdown, signal) = tokio::sync::oneshot::channel();
        let host = runtime.spawn(async move {
            axum::serve(serving, app)
                .with_graceful_shutdown(async {
                    let _ = signal.await;
                })
                .await
        });

        let books = BooksPath::try_from(books.as_path())?;
        let live = instance::discover(state.path(), &books, None);
        let _ = shutdown.send(());
        runtime.block_on(host)??;

        assert_eq!(
            SocketAddr::from(live?.ok_or("missing live listener")?),
            bound
        );
        let mut records = std::fs::read_dir(state.path().join("instances"))?;
        let directory = records.next().ok_or("missing books registry directory")??;
        assert!(directory.path().read_dir()?.next().is_none());
        Ok(())
    }
}
