mod common;

use std::{
    io::{BufRead, BufReader, Write},
    net::{SocketAddr, TcpListener},
    process::{Command, Stdio},
    sync::mpsc,
    time::{Duration, Instant},
};

use beankeeper_cli::{db::Actor, identity};
use beankeeper_desktop::browser;
use common::{Ledger, Server, TestResult, clean_command, rpc, token};
use serde_json::{Value, json};

#[test]
fn binary_serves_embedded_views_and_authenticated_mcp_from_an_empty_directory() -> TestResult {
    let ledger = Ledger::new()?;
    let mut server = Server::start(&ledger, &["serve"], None)?;
    server.pair()?;
    // Startup consumes the passphrase once. Requests no longer need the file.
    std::fs::remove_file(&ledger.secret)?;
    for (path, heading) in [
        ("/", "Dashboard"),
        ("/accounts", "Accounts"),
        ("/transactions", "Transactions"),
        ("/reports", "Reports"),
        ("/review", "Review queue"),
    ] {
        let response = server.client.get(format!("{}{path}", server.base)).send()?;
        assert_eq!(response.status(), 200);
        assert!(
            response.headers()["content-type"]
                .to_str()?
                .starts_with("text/html")
        );
        assert!(
            response.headers()["content-security-policy"]
                .to_str()?
                .contains("frame-ancestors 'none'")
        );
        let html = response.text()?;
        assert!(html.contains(&format!("<h1>{heading}</h1>")));
        assert_eq!(html.matches("aria-current=\"page\"").count(), 1);
        assert!(!html.contains("{{"));
        assert!(!html.contains(token(&ledger.reader)?));
    }
    let style = server
        .client
        .get(format!("{}/assets/style.css", server.base))
        .send()?;
    assert_eq!(style.status(), 200);
    assert!(
        style.headers()["content-type"]
            .to_str()?
            .starts_with("text/css")
    );
    assert!(style.text()?.contains(":focus-visible"));
    assert_eq!(
        server
            .client
            .get(format!("{}/does-not-exist", server.base))
            .send()?
            .status(),
        404
    );
    assert_eq!(server.request("tools/list", json!({}), None)?.status(), 401);
    assert_eq!(
        server
            .request("tools/list", json!({}), Some("invalid"))?
            .status(),
        401
    );
    server.initialize(Some(token(&ledger.reader)?))?;
    let response = rpc(server.request(
        "tools/call",
        json!({"name":"company_list", "arguments":{}}),
        Some(token(&ledger.reader)?),
    )?)?;
    assert_ne!(response["result"]["isError"], true);
    let payload: Value = if let Some(value) = response["result"].get("structuredContent") {
        value.clone()
    } else {
        serde_json::from_str(
            response["result"]["content"][0]["text"]
                .as_str()
                .ok_or("content")?,
        )?
    };
    assert_eq!(payload[0]["slug"], "acme");
    let db = ledger.db()?;
    identity::revoke(
        db.conn(),
        &Actor::new("owner"),
        ledger.reader["token_jti"].as_str().ok_or("jti")?,
    )?;
    assert_eq!(
        server
            .request("tools/list", json!({}), Some(token(&ledger.reader)?))?
            .status(),
        401
    );
    Ok(())
}

#[test]
fn every_route_rejects_rebinding_hosts_and_cross_origin_requests() -> TestResult {
    let ledger = Ledger::new()?;
    let mut server = Server::start(&ledger, &["serve"], None)?;
    let authority = server.base.trim_start_matches("http://");
    for path in [
        "/",
        "/assets/style.css",
        "/health",
        "/transactions",
        "/missing",
        "/pair",
        "/api/session/logout",
        "/api/session/pairing",
        "/api/session/associate",
        "/mcp",
    ] {
        let url = format!("{}{path}", server.base);
        for host in [
            "evil.example:8750",
            "127.0.0.1:1",
            "localhost",
            "127.0.0.1:0",
        ] {
            assert_eq!(
                server
                    .client
                    .get(&url)
                    .header("Host", host)
                    .send()?
                    .status(),
                403,
                "{path} {host}"
            );
        }
        for origin in [
            "https://evil.example",
            "null",
            "http://127.0.0.1:1",
            "https://localhost:8750",
        ] {
            assert_eq!(
                server
                    .client
                    .get(&url)
                    .header("Origin", origin)
                    .send()?
                    .status(),
                403,
                "{path} {origin}"
            );
            assert_eq!(
                server
                    .client
                    .post(&url)
                    .header("Origin", origin)
                    .send()?
                    .status(),
                403
            );
            assert_eq!(
                server
                    .client
                    .request(reqwest::Method::OPTIONS, &url)
                    .header("Origin", origin)
                    .send()?
                    .status(),
                403
            );
        }
        assert_ne!(
            server
                .client
                .get(&url)
                .header("Origin", format!("http://{authority}"))
                .send()?
                .status(),
            403
        );
    }
    let port = authority.rsplit_once(':').ok_or("port")?.1;
    let response = server
        .client
        .get(&server.base)
        .header("Host", format!("localhost:{port}"))
        .header("Origin", format!("http://localhost:{port}"))
        .send()?;
    // Raw access on another allowed hostname has no paired session.
    assert_eq!(response.status(), 401);
    assert!(
        response
            .headers()
            .get("access-control-allow-origin")
            .is_none()
    );
    // Exercise duplicate Host handling in the actual HTTP parser/guard too.
    let mut stream = std::net::TcpStream::connect(authority)?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    write!(
        stream,
        "GET / HTTP/1.1\r\nHost: {authority}\r\nHost: evil.example\r\nConnection: close\r\n\r\n"
    )?;
    let mut status = String::new();
    BufReader::new(stream).read_line(&mut status)?;
    assert!(status.contains("400") || status.contains("403"), "{status}");
    Ok(())
}

#[test]
fn read_only_ceiling_and_process_fallback_are_preserved() -> TestResult {
    let ledger = Ledger::new()?;
    let mut server = Server::start(
        &ledger,
        &["serve", "--read-only"],
        Some(token(&ledger.writer)?),
    )?;
    server.initialize(None)?;
    let response = rpc(server.request("tools/list", json!({}), None)?)?;
    let tools = response["result"]["tools"].as_array().ok_or("tools")?;
    assert!(tools.iter().any(|tool| tool["name"] == "company_list"));
    assert!(!tools.iter().any(|tool| tool["name"] == "txn_post"));
    // An explicit invalid request token must not inherit the valid fallback.
    assert_eq!(
        server
            .request("tools/list", json!({}), Some("invalid"))?
            .status(),
        401
    );
    identity::revoke(
        ledger.db()?.conn(),
        &Actor::new("owner"),
        ledger.writer["token_jti"].as_str().ok_or("jti")?,
    )?;
    assert_eq!(server.request("tools/list", json!({}), None)?.status(), 401);
    Ok(())
}

#[test]
fn open_probes_before_launching_and_reports_absent_or_unrelated_servers() -> TestResult {
    let ledger = Ledger::new()?;
    let server = Server::start(&ledger, &["serve"], None)?;
    let bind: SocketAddr = server.base.trim_start_matches("http://").parse()?;
    let mut launched = None;
    browser::open_with(bind, |url| {
        launched = Some(url.to_owned());
        Ok(())
    })?;
    assert_eq!(
        launched.as_deref(),
        Some(format!("{}/", server.base).as_str())
    );
    assert!(
        browser::open_with(bind, |_| Err(beankeeper_desktop::DesktopError::Io(
            std::io::Error::other("launcher failed")
        )))
        .is_err()
    );
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let unrelated = listener.local_addr()?;
    let thread = std::thread::spawn(move || -> std::io::Result<()> {
        let (mut stream, _) = listener.accept()?;
        let mut line = String::new();
        let mut reader = BufReader::new(stream.try_clone()?);
        loop {
            line.clear();
            reader.read_line(&mut line)?;
            if line == "\r\n" {
                break;
            }
        }
        stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nConnection: close\r\n\r\nother")
    });
    assert!(
        browser::open_with(unrelated, |_| panic!("must not launch for another service")).is_err()
    );
    thread.join().map_err(|_| "probe thread panicked")??;
    let reserved = TcpListener::bind("127.0.0.1:0")?;
    let absent = reserved.local_addr()?;
    drop(reserved);
    assert!(browser::open_with(absent, |_| panic!("must not launch without a server")).is_err());
    let output = clean_command()
        .args(["open", "--bind", &absent.to_string()])
        .output()?;
    assert!(!output.status.success());
    assert!(String::from_utf8(output.stderr)?.contains("start `bk serve` first"));
    Ok(())
}

#[test]
fn startup_errors_do_not_create_ledgers_and_bind_consent_is_checked_first() -> TestResult {
    let dir = tempfile::tempdir()?;
    let missing = dir.path().join("missing.db");
    let output = clean_command()
        .arg("--db")
        .arg(&missing)
        .args(["serve", "--bind", "0.0.0.0:0"])
        .output()?;
    assert!(!output.status.success());
    assert!(String::from_utf8(output.stderr)?.contains("--allow-remote"));
    assert!(!missing.exists());
    let output = clean_command()
        .arg("--db")
        .arg(&missing)
        .args(["serve", "--bind", "127.0.0.1:0"])
        .output()?;
    assert!(!output.status.success());
    assert!(!missing.exists());
    let ledger = Ledger::new()?;
    let reserved = TcpListener::bind("127.0.0.1:0")?;
    let output = ledger
        .command()
        .args(["serve", "--bind", &reserved.local_addr()?.to_string()])
        .output()?;
    assert!(!output.status.success());
    let output = ledger
        .command()
        .args(["restore", "--input", "missing-backup.db"])
        .output()?;
    // An explicit --db was supplied by the fixture; upstream validates the input.
    assert!(!output.status.success());
    let output = clean_command()
        .args(["restore", "--input", "missing-backup.db"])
        .output()?;
    assert!(String::from_utf8(output.stderr)?.contains("explicit --db"));
    let output = ledger
        .command()
        .args([
            "--agent-token",
            token(&ledger.reader)?,
            "company",
            "list",
            "--json",
        ])
        .output()?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8(output.stdout)?.contains("acme"));
    Ok(())
}

#[test]
fn upstream_http_and_stdio_transports_still_run() -> TestResult {
    let ledger = Ledger::new()?;
    let mut server = Server::start(&ledger, &["mcp", "serve", "--http"], None)?;
    server.initialize(Some(token(&ledger.reader)?))?;
    assert!(rpc(server.request("tools/list", json!({}), Some(token(&ledger.reader)?))?)?["result"]["tools"].is_array());
    let mut child = ledger
        .command()
        .args([
            "--agent-token",
            token(&ledger.reader)?,
            "mcp",
            "serve",
            "--read-only",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    let result = (|| -> TestResult {
        let mut stdin = child.stdin.take().ok_or("stdin")?;
        writeln!(
            stdin,
            "{}",
            json!({"jsonrpc":"2.0", "id":1, "method":"initialize", "params":{"protocolVersion":"2025-11-25", "capabilities":{}, "clientInfo":{"name":"desktop-stdio-test", "version":"1.0"}}})
        )?;
        stdin.flush()?;
        let stdout = child.stdout.take().ok_or("stdout")?;
        let (send, receive) = mpsc::channel();
        std::thread::spawn(move || {
            let mut line = String::new();
            let result = BufReader::new(stdout).read_line(&mut line).map(|_| line);
            let _ = send.send(result);
        });
        let response: Value =
            serde_json::from_str(&receive.recv_timeout(Duration::from_secs(10))??)?;
        assert_eq!(response["result"]["serverInfo"]["name"], "beankeeper");
        Ok(())
    })();
    let _ = child.kill();
    let _ = child.wait();
    result
}

#[cfg(unix)]
#[test]
fn ctrl_c_cancels_live_mcp_sessions_and_releases_the_listener() -> TestResult {
    let ledger = Ledger::new()?;
    let mut server = Server::start(&ledger, &["serve"], None)?;
    server.initialize(Some(token(&ledger.reader)?))?;
    // Keep the SSE stream open while requesting shutdown.
    let response = server
        .client
        .get(format!("{}/mcp", server.base))
        .bearer_auth(token(&ledger.reader)?)
        .header("Accept", "text/event-stream")
        .header(
            "Mcp-Session-Id",
            server.session.as_deref().ok_or("session")?,
        )
        .header("MCP-Protocol-Version", "2025-11-25")
        .send()?;
    assert!(response.status().is_success());
    assert!(
        Command::new("kill")
            .args(["-INT", &server.child.id().to_string()])
            .status()?
            .success()
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(status) = server.child.try_wait()? {
            assert!(status.success());
            break;
        }
        assert!(
            Instant::now() < deadline,
            "server did not shut down with an MCP session open"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    let rebound = TcpListener::bind(server.base.trim_start_matches("http://"))?;
    drop((response, rebound));
    Ok(())
}

#[test]
fn explicit_remote_consent_keeps_the_same_local_host_boundary() -> TestResult {
    let ledger = Ledger::new()?;
    let mut server = Server::start_at(&ledger, &["serve", "--allow-remote"], None, "0.0.0.0:0")?;
    server.pair()?;
    assert_eq!(server.client.get(&server.base).send()?.status(), 200);
    assert_eq!(
        server
            .client
            .get(&server.base)
            .header("Host", "remote.example:8750")
            .send()?
            .status(),
        403
    );
    assert_eq!(
        server
            .client
            .get(&server.base)
            .header("Origin", "https://remote.example")
            .send()?
            .status(),
        403
    );
    Ok(())
}

fn csrf_from(html: &str) -> TestResult<&str> {
    html.split_once("name=\"csrf\" value=\"")
        .and_then(|(_, rest)| rest.split_once('"').map(|(value, _)| value))
        .ok_or_else(|| "missing CSRF form field".into())
}

fn human_post(server: &Server, path: &str, body: String) -> reqwest::blocking::RequestBuilder {
    server
        .client
        .post(format!("{}{path}", server.base))
        .header("Origin", &server.base)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(body)
}

#[test]
fn raw_origin_and_bearer_tokens_cannot_grant_a_browser_session() -> TestResult {
    let ledger = Ledger::new()?;
    let mut server = Server::start(&ledger, &["serve"], Some(token(&ledger.writer)?))?;
    for path in ["/", "/transactions", "/reports", "/review"] {
        let response = server.client.get(format!("{}{path}", server.base)).send()?;
        assert_eq!(response.status(), 401);
        assert!(response.headers().get("set-cookie").is_none());
        let html = response.text()?;
        assert!(html.contains("<h1>Pair this browser</h1>"));
        assert!(!html.contains("name=\"csrf\""));
        assert!(!html.contains("?token="));
        assert!(!html.contains(token(&ledger.writer)?));
    }
    let response = server
        .client
        .get(server.pairing_url.as_deref().ok_or("pairing URL")?)
        .bearer_auth(token(&ledger.writer)?)
        .send()?;
    assert_eq!(response.status(), 401);
    assert!(response.headers().get("set-cookie").is_none());
    server.pair()?;
    let response = server
        .client
        .get(server.pairing_url.as_deref().ok_or("pairing URL")?)
        .send()?;
    assert_eq!(response.status(), 410);
    assert!(response.headers().get("set-cookie").is_none());
    assert_eq!(server.client.get(&server.base).send()?.status(), 200);
    assert_eq!(
        server
            .client
            .get(&server.base)
            .bearer_auth(token(&ledger.writer)?)
            .send()?
            .status(),
        401
    );
    // Explicit process fallback credentials work only on MCP.
    server.initialize(None)?;
    assert!(
        server
            .request("tools/list", json!({}), None)?
            .status()
            .is_success()
    );
    Ok(())
}

#[test]
fn browser_writes_require_origin_and_csrf_and_logout_revokes_the_cookie() -> TestResult {
    let ledger = Ledger::new()?;
    let mut server = Server::start(&ledger, &["serve"], None)?;
    server.pair()?;
    let html = server.client.get(&server.base).send()?.text()?;
    let csrf = csrf_from(&html)?;
    for (origin, candidate, expected) in [
        (None, csrf, 401),
        (Some("null"), csrf, 403),
        (Some("https://evil.example"), csrf, 403),
        (Some(server.base.as_str()), "wrong", 401),
        (Some(server.base.as_str()), "", 401),
    ] {
        for path in [
            "/api/session/logout",
            "/api/session/pairing",
            "/api/session/associate",
        ] {
            let mut request = server
                .client
                .post(format!("{}{path}", server.base))
                .header("Content-Type", "application/x-www-form-urlencoded")
                .body(format!("csrf={candidate}"));
            if let Some(origin) = origin {
                request = request.header("Origin", origin);
            }
            assert_eq!(request.send()?.status(), expected, "{path}");
        }
    }
    assert_eq!(server.client.get(&server.base).send()?.status(), 200);
    assert_eq!(
        human_post(&server, "/api/session/pairing", format!("csrf={csrf}"))
            .bearer_auth(token(&ledger.writer)?)
            .send()?
            .status(),
        401
    );
    assert_eq!(
        human_post(
            &server,
            "/api/session/pairing",
            format!("csrf={csrf}&csrf={csrf}")
        )
        .send()?
        .status(),
        401
    );
    // The actual successful write creates a new one-use link, never an agent token.
    let response = human_post(&server, "/api/session/pairing", format!("csrf={csrf}")).send()?;
    assert_eq!(response.status(), 200);
    let html = response.text()?;
    let link = html
        .split_once("href=\"http://")
        .and_then(|(_, rest)| rest.split_once('"'))
        .map(|(rest, _)| format!("http://{rest}"))
        .ok_or("new browser link")?;
    let other = reqwest::blocking::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .build()?;
    let response = other.get(&link).send()?;
    assert_eq!(response.status(), 303);
    assert!(response.headers().get("set-cookie").is_some());
    assert_eq!(other.get(&link).send()?.status(), 410);
    let response = server
        .client
        .post(format!("{}/api/session/logout", server.base))
        .header("Origin", &server.base)
        .header("X-CSRF-Token", csrf)
        .send()?;
    assert_eq!(response.status(), 303);
    assert!(
        response.headers()["set-cookie"]
            .to_str()?
            .contains("Max-Age=0")
    );
    assert_eq!(server.client.get(&server.base).send()?.status(), 401);
    assert_eq!(
        human_post(&server, "/api/session/pairing", format!("csrf={csrf}"))
            .send()?
            .status(),
        401
    );
    Ok(())
}

#[test]
fn authenticated_mcp_invites_require_human_confirmation_and_keep_agents_isolated() -> TestResult {
    let ledger = Ledger::new()?;
    let mut server = Server::start(&ledger, &["serve"], None)?;
    let response = server.request("tools/list", json!({}), Some("invalid"))?;
    assert_eq!(response.status(), 401);
    assert!(response.headers().get("beankeeper-pairing-url").is_none());
    server.initialize(Some(token(&ledger.reader)?))?;
    let response = server.request("tools/list", json!({}), Some(token(&ledger.reader)?))?;
    let invite = response.headers()["beankeeper-pairing-url"]
        .to_str()?
        .to_owned();
    assert!(!invite.contains(token(&ledger.reader)?));
    assert!(rpc(response)?["result"]["tools"].is_array());
    let response = server.client.get(&invite).send()?;
    assert_eq!(response.status(), 401);
    assert!(response.headers().get("set-cookie").is_none());
    let landing = response.text()?;
    assert!(landing.contains("Continue in this browser"));
    assert!(!landing.contains("name=\"csrf\""));
    assert_eq!(
        server
            .client
            .get(invite.replace("?invite=", "?token="))
            .send()?
            .status(),
        410
    );
    server.pair()?;
    // Cookie authentication alone cannot call the ledger MCP endpoint.
    assert_eq!(server.request("tools/list", json!({}), None)?.status(), 401);
    let response = server.client.get(&invite).send()?;
    assert_eq!(response.status(), 200);
    assert!(response.headers().get("set-cookie").is_none());
    let html = response.text()?;
    let csrf = csrf_from(&html)?;
    let invite_token = invite.split_once('=').ok_or("invite")?.1;
    let form = format!("csrf={csrf}&invite={invite_token}");
    assert_eq!(
        human_post(&server, "/api/session/associate", form.clone())
            .send()?
            .status(),
        303
    );
    assert_eq!(
        human_post(&server, "/api/session/associate", form)
            .send()?
            .status(),
        410
    );
    assert!(
        server
            .client
            .get(&server.base)
            .send()?
            .text()?
            .contains("Your agent is connected")
    );
    assert!(
        server
            .request("tools/list", json!({}), Some(token(&ledger.reader)?))?
            .headers()
            .get("beankeeper-pairing-url")
            .is_none()
    );
    // A second valid bearer on the same transport session never inherits pairing.
    let response = server.request("tools/list", json!({}), Some(token(&ledger.writer)?))?;
    assert!(response.status().is_success());
    assert!(response.headers().get("beankeeper-pairing-url").is_some());
    identity::revoke(
        ledger.db()?.conn(),
        &Actor::new("owner"),
        ledger.reader["token_jti"].as_str().ok_or("jti")?,
    )?;
    let response = server.request("tools/list", json!({}), Some(token(&ledger.reader)?))?;
    assert_eq!(response.status(), 401);
    assert!(response.headers().get("beankeeper-pairing-url").is_none());
    Ok(())
}
