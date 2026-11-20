use std::{
    error::Error,
    io::{BufRead, BufReader},
    path::PathBuf,
    process::{Child, Command, Stdio},
    sync::mpsc,
    time::Duration,
};

use beankeeper_cli::{
    db::{self, Actor, Db},
    identity::{self, IssueRequest},
};
use reqwest::blocking::{Client, Response};
use serde_json::{Value, json};
use tempfile::TempDir;

pub type TestResult<T = ()> = Result<T, Box<dyn Error>>;
const PASSPHRASE: &str = "desktop-integration-passphrase";

pub struct Ledger {
    _dir: TempDir,
    pub path: PathBuf,
    pub secret: PathBuf,
    pub reader: Value,
    pub writer: Value,
}

impl Ledger {
    pub fn new() -> TestResult<Self> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("books.db");
        let secret = dir.path().join("passphrase");
        std::fs::write(&secret, PASSPHRASE)?;
        let db = Db::open(&path, Some(&PASSPHRASE.into()))?;
        db::create_company(db.conn(), "acme", "Acme", None)?;
        identity::init_root(db.conn(), &Actor::new("owner"))?;
        let issue = |name, capabilities: &[String]| {
            identity::issue(
                db.conn(),
                &Actor::new("owner"),
                &IssueRequest {
                    name,
                    capabilities,
                    ttl: Duration::from_secs(3600),
                    public_key: None,
                    generate_key: true,
                },
            )
        };
        let reader = issue("desktop_reader", &["ledger/read".into()])?;
        let writer = issue(
            "desktop_writer",
            &["ledger/read".into(), "ledger/post".into()],
        )?;
        db::audit::set_policy(db.conn(), &Actor::new("owner"), true)?;
        Ok(Self {
            _dir: dir,
            path,
            secret,
            reader,
            writer,
        })
    }

    pub fn db(&self) -> TestResult<Db> {
        Ok(Db::open_existing(&self.path, Some(&PASSPHRASE.into()))?)
    }

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

pub fn clean_command() -> Command {
    let mut command = Command::new(assert_cmd::cargo::cargo_bin!("bk"));
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
    pub fn start(ledger: &Ledger, args: &[&str], fallback: Option<&str>) -> TestResult<Self> {
        Self::start_at(ledger, args, fallback, "127.0.0.1:0")
    }

    pub fn start_at(
        ledger: &Ledger,
        args: &[&str],
        fallback: Option<&str>,
        bind: &str,
    ) -> TestResult<Self> {
        let cwd = tempfile::tempdir()?;
        let mut command = ledger.command();
        command.args(args).args(["--bind", bind]);
        if let Some(token) = fallback {
            command.env("BK_AGENT_TOKEN", token);
        }
        let child = command
            .current_dir(cwd.path())
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
                server.base = url
                    .split_whitespace()
                    .next()
                    .ok_or("missing URL")?
                    .trim_end_matches("/mcp")
                    .trim_end_matches('/')
                    .to_owned();
                break;
            }
            if let Some(status) = server.child.try_wait()? {
                return Err(format!("startup exited {status}: {line}").into());
            }
        }
        Ok(server)
    }

    pub fn pair(&mut self) -> TestResult {
        let response = self
            .client
            .get(self.pairing_url.as_deref().ok_or("missing pairing URL")?)
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
        Ok(())
    }

    pub fn request(
        &self,
        method: &str,
        params: Value,
        token: Option<&str>,
    ) -> TestResult<Response> {
        let mut request = self
            .client
            .post(format!("{}/mcp", self.base))
            .header("Accept", "application/json, text/event-stream")
            .header("MCP-Protocol-Version", "2025-11-25")
            .json(&json!({"jsonrpc":"2.0", "id":1, "method":method, "params":params}));
        if let Some(session) = &self.session {
            request = request.header("Mcp-Session-Id", session);
        }
        if let Some(token) = token {
            request = request.bearer_auth(token);
        }
        Ok(request.send()?)
    }

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

pub fn token(issued: &Value) -> TestResult<&str> {
    issued["token"]
        .as_str()
        .ok_or_else(|| "missing token".into())
}

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
