use std::{
    error::Error,
    io::{BufRead, BufReader},
    path::PathBuf,
    process::{Child, Command, Stdio},
    sync::mpsc,
    time::Duration,
};

use beankeeper_cli::{
    capability::Capability,
    db::{self, Actor, Db},
    identity::{self, Admin, Granted, IssueRequest},
};
use reqwest::blocking::{Client, Response};
use secrecy::ExposeSecret;
use serde_json::{Value, json};
use tempfile::TempDir;

pub type TestResult<T = ()> = Result<T, Box<dyn Error>>;
const PASSPHRASE: &str = "desktop-integration-passphrase";

#[must_use]
pub fn audit_query() -> db::audit::ListAuditParams<'static> {
    db::audit::ListAuditParams {
        actor: None,
        agent_uri: None,
        company_slug: None,
        entity: None,
        action: None,
        limit: db::audit::AuditPageLimit::default(),
        offset: db::audit::AuditPageOffset::default(),
    }
}

pub struct Ledger {
    _dir: TempDir,
    pub path: PathBuf,
    pub secret: PathBuf,
    pub reader: Value,
    pub writer: Value,
    owner: Actor,
    owner_token: secrecy::SecretString,
}

impl Ledger {
    /// Create an isolated encrypted ledger and authenticated fixtures.
    ///
    /// # Errors
    /// Returns an error when fixture data or credentials cannot be created.
    pub fn new() -> TestResult<Self> {
        Self::new_with_policy_admin().map(|(ledger, _)| ledger)
    }

    /// Create the fixture ledger and retain its policy administrator.
    ///
    /// # Errors
    /// Returns an error when fixture data or credentials cannot be created.
    pub fn new_with_policy_admin() -> TestResult<(Self, Actor)> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("books.db");
        let secret = dir.path().join("passphrase");
        std::fs::write(&secret, PASSPHRASE)?;
        let db = Db::open(&path, Some(&PASSPHRASE.into()))?;
        let owner = Actor::new("owner".parse()?);
        Granted::<Admin>::with_db(&db, &owner, |grant| {
            db::create_company(grant, &"acme".parse()?, &"Acme".parse()?, None)
        })?;
        Granted::<Admin>::with_db(&db, &owner, identity::init_root)?;
        let issue = |name, capabilities: &[String]| {
            Granted::<Admin>::with_db(&db, &owner, |grant| {
                identity::issue(
                    grant,
                    &IssueRequest {
                        name,
                        capabilities,
                        ttl: Duration::from_secs(3600),
                        public_key: None,
                        generate_key: true,
                    },
                )
            })
        };
        let reader = issue("desktop_reader", &["ledger/read".into()])?;
        let writer = issue(
            "desktop_writer",
            &["ledger/read".into(), "ledger/post".into()],
        )?;
        let admin = issue(
            "desktop_policy_admin",
            &[
                Capability::Admin,
                Capability::Post,
                Capability::Reconcile,
                Capability::Budget,
            ]
            .map(|capability| capability.path().to_owned()),
        )?;
        let owner_token = secrecy::SecretString::from(token(&admin)?);
        let owner = owner.with_identity(identity::verify(db.read(), owner_token.expose_secret())?);
        Granted::<Admin>::with_db(&db, &owner, |grant| db::audit::set_policy(grant, true))?;
        Ok((
            Self {
                _dir: dir,
                path,
                secret,
                reader,
                writer,
                owner: owner.clone(),
                owner_token,
            },
            owner,
        ))
    }

    /// Open the fixture ledger.
    ///
    /// # Errors
    /// Returns an error when the fixture ledger cannot be opened.
    pub fn db(&self) -> TestResult<Db> {
        Ok(Db::open_existing(&self.path, Some(&PASSPHRASE.into()))?)
    }

    #[must_use]
    pub fn owner(&self) -> &Actor {
        &self.owner
    }

    /// Write a private owner credential for the browser approval fixture.
    /// # Errors
    /// Returns I/O or permissions errors from the temporary sidecar.
    pub fn write_owner_token(&self, token: &str) -> TestResult {
        let path = self.path.with_extension("owner-token");
        std::fs::write(&path, token)?;
        beankeeper_desktop::setup::keychain::restrict_file(&path)?;
        Ok(())
    }

    /// Enable browser approval using the fixture owner.
    /// # Errors
    /// Returns I/O or permissions errors from the temporary sidecar.
    pub fn enable_browser_review(&self) -> TestResult {
        self.write_owner_token(self.owner_token.expose_secret())
    }

    /// Open a temporary fixture independently of the ledger API to exercise
    /// schema migration, corruption detection or an injected `SQLite` failure.
    /// # Errors
    /// Returns connection or `SQLCipher` errors.
    pub fn fixture_sql(&self) -> TestResult<rusqlite::Connection> {
        let conn = rusqlite::Connection::open(&self.path)?;
        conn.pragma_update(None, "key", PASSPHRASE)?;
        conn.busy_timeout(Duration::from_secs(5))?;
        Ok(conn)
    }

    #[must_use]
    pub fn command(&self) -> Command {
        let mut command = clean_command();
        command
            .arg("--db")
            .arg(&self.path)
            .arg("--passphrase-file")
            .arg(&self.secret);
        command
    }
}

#[must_use]
pub fn clean_command() -> Command {
    let mut command = Command::new(assert_cmd::cargo::cargo_bin!("bk"));
    command.env("BEANKEEPER_NO_UPDATE_CHECK", "1");
    for name in [
        "BK_AGENT_TOKEN",
        "BEANKEEPER_PASSPHRASE_CMD",
        "BEANKEEPER_COMPANY",
        "BEANKEEPER_DB",
    ] {
        command.env_remove(name);
    }
    command
}

pub struct Server {
    pub child: Child,
    pub base: String,
    pub client: Client,
    pub session: Option<String>,
    pub pairing_url: Option<String>,
    _cwd: TempDir,
}

impl Server {
    /// Start Desktop against the fixture on an ephemeral port.
    ///
    /// # Errors
    /// Returns an error if the process fails to start.
    pub fn start(ledger: &Ledger, args: &[&str], fallback: Option<&str>) -> TestResult<Self> {
        Self::start_at(ledger, args, fallback, "127.0.0.1:0")
    }

    /// Start Desktop against the fixture on a specific local address.
    ///
    /// # Errors
    /// Returns an error if the process fails to start.
    pub fn start_at(
        ledger: &Ledger,
        args: &[&str],
        fallback: Option<&str>,
        bind: &str,
    ) -> TestResult<Self> {
        let mut command = ledger.command();
        command.args(args).args(["--bind", bind]);
        if let Some(token) = fallback {
            command.env("BK_AGENT_TOKEN", token);
        }
        Self::launch(command)
    }

    /// Start a prepared command and read the listener details from its output.
    ///
    /// # Errors
    /// Returns an error if the fixture, process, or listener is unavailable.
    pub fn launch(mut command: Command) -> TestResult<Self> {
        let cwd = tempfile::tempdir()?;
        let desktop_state = command
            .get_envs()
            .find(|(name, _)| *name == "BEANKEEPER_DESKTOP_STATE_DIR")
            .and_then(|(_, value)| value)
            .map_or_else(|| cwd.path().join("desktop-state"), PathBuf::from);
        let child = command
            .current_dir(cwd.path())
            .env("BEANKEEPER_DESKTOP_STATE_DIR", desktop_state)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()?;
        let client = Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(5))
            .build()?;
        let mut server = Self {
            child,
            base: String::new(),
            client,
            session: None,
            pairing_url: None,
            _cwd: cwd,
        };
        let stderr = server.child.stderr.take().ok_or("missing stderr")?;
        let (send, receive) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                let _ = send.send(line);
            }
        });
        loop {
            let line = receive.recv_timeout(Duration::from_secs(15))?;
            if let Some(url) =
                line.strip_prefix("Pair this browser (one use, expires in 5 minutes): ")
            {
                server.pairing_url = Some(url.to_owned());
            }
            if let Some((_, url)) = line.split_once("listening on ") {
                url.split_whitespace()
                    .next()
                    .ok_or("missing URL")?
                    .trim_end_matches("/mcp")
                    .trim_end_matches('/')
                    .clone_into(&mut server.base);
                break;
            }
            if let Some(status) = server.child.try_wait()? {
                return Err(format!("startup exited {status}: {line}").into());
            }
        }
        Ok(server)
    }

    /// Pair the test client using the browser confirmation flow.
    ///
    /// # Errors
    /// Returns an error if the link, cookie, or response cannot be read.
    ///
    /// # Panics
    /// Panics if the server omits the expected pairing response.
    pub fn pair(&mut self) -> TestResult<String> {
        let response = self
            .client
            .get(self.pairing_url.as_deref().ok_or("missing pairing URL")?)
            .send()?;
        assert_eq!(response.status(), 200);
        assert!(response.headers().get("set-cookie").is_none());
        let html = response.text()?;
        let token = html
            .split_once("name=\"token\" value=\"")
            .and_then(|(_, rest)| rest.split_once('"').map(|(value, _)| value))
            .ok_or("missing pairing token")?;
        let response = self
            .client
            .post(format!("{}/pair", self.base))
            .header("Origin", &self.base)
            .form(&[("token", token)])
            .send()?;
        assert_eq!(response.status(), 303);
        let attributes = response
            .headers()
            .get("set-cookie")
            .ok_or("missing cookie")?
            .to_str()?;
        assert!(attributes.contains("HttpOnly"));
        assert!(attributes.contains("SameSite=Strict"));
        assert!(attributes.contains("Path=/"));
        assert!(attributes.contains("Max-Age=43200"));
        assert!(!attributes.contains("Domain="));
        assert_eq!(response.headers()["location"], "/");
        assert_eq!(response.headers()["cache-control"], "no-store");
        assert_eq!(response.headers()["referrer-policy"], "no-referrer");
        let cookie = attributes.split(';').next().ok_or("cookie")?.to_owned();
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(reqwest::header::COOKIE, cookie.parse()?);
        self.client = Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(5))
            .default_headers(headers)
            .build()?;
        Ok(cookie)
    }

    /// Send one JSON-RPC request to the fixture server.
    ///
    /// # Errors
    /// Returns an error when the HTTP transport fails.
    pub fn request(
        &self,
        method: &str,
        params: Value,
        token: Option<&str>,
    ) -> TestResult<Response> {
        let mut body = json!({"jsonrpc":"2.0", "id":1, "method":method});
        body["params"] = params;
        let mut request = self
            .client
            .post(format!("{}/mcp", self.base))
            .header("Accept", "application/json, text/event-stream")
            .header("MCP-Protocol-Version", "2025-11-25")
            .json(&body);
        if let Some(session) = &self.session {
            request = request.header("Mcp-Session-Id", session);
        }
        if let Some(token) = token {
            request = request.bearer_auth(token);
        }
        Ok(request.send()?)
    }

    /// Initialize an authenticated MCP session.
    ///
    /// # Errors
    /// Returns an error when the transport or session metadata is unavailable.
    ///
    /// # Panics
    /// Panics if the server rejects initialization.
    pub fn initialize(&mut self, token: Option<&str>) -> TestResult {
        let response = self.request("initialize", json!({"protocolVersion":"2025-11-25", "capabilities":{}, "clientInfo":{"name":"desktop-test", "version":"1.0"}}), token)?;
        assert!(response.status().is_success());
        self.session = Some(
            response
                .headers()
                .get("Mcp-Session-Id")
                .ok_or("missing session")?
                .to_str()?
                .to_owned(),
        );
        assert_eq!(rpc(response)?["result"]["serverInfo"]["name"], "beankeeper");
        let mut request = self
            .client
            .post(format!("{}/mcp", self.base))
            .header("Accept", "application/json, text/event-stream")
            .header("MCP-Protocol-Version", "2025-11-25")
            .header("Mcp-Session-Id", self.session.as_deref().ok_or("session")?)
            .json(&json!({"jsonrpc":"2.0", "method":"notifications/initialized"}));
        if let Some(token) = token {
            request = request.bearer_auth(token);
        }
        assert!(request.send()?.status().is_success());
        Ok(())
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Extract the bearer token from an issuance fixture.
///
/// # Errors
/// Returns an error when the fixture does not contain a token.
pub fn token(issued: &Value) -> TestResult<&str> {
    issued["token"]
        .as_str()
        .ok_or_else(|| "missing token".into())
}

/// Decode a JSON or event-stream response from the MCP transport.
///
/// # Errors
/// Returns an error if there is no decodable JSON-RPC response.
///
/// # Panics
/// Panics if the response has an unsuccessful HTTP status.
pub fn rpc(response: Response) -> TestResult<Value> {
    assert!(response.status().is_success(), "HTTP {}", response.status());
    let body = response.text()?;
    if let Ok(value) = serde_json::from_str(&body) {
        return Ok(value);
    }
    for line in body.lines() {
        if let Some(data) = line.strip_prefix("data:")
            && let Ok(value) = serde_json::from_str::<Value>(data.trim())
            && value.get("id").is_some()
        {
            return Ok(value);
        }
    }
    Err(format!("missing JSON-RPC response: {body}").into())
}
