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
    // Startup consumes the passphrase once. Requests no longer need the file.
    std::fs::remove_file(&ledger.secret)?;
    for (path, heading) in [
        ("/", "Beankeeper is running"),
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
    let server = Server::start(&ledger, &["serve"], None)?;
    let authority = server.base.trim_start_matches("http://");
    for path in [
        "/",
        "/assets/style.css",
        "/health",
        "/transactions",
        "/missing",
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
    assert_eq!(response.status(), 200);
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
    let server = Server::start_at(&ledger, &["serve", "--allow-remote"], None, "0.0.0.0:0")?;
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
