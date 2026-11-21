mod common;

use std::{
    io::{BufRead, BufReader, Write},
    net::{SocketAddr, TcpListener},
    process::{Command, Stdio},
    sync::mpsc,
    time::{Duration, Instant},
};

use beankeeper_cli::{
    db::{Actor, Db, get_schema_version},
    identity,
};
use beankeeper_desktop::browser;
use common::{Ledger, Server, TestResult, clean_command, rpc, token};
use serde_json::{Value, json};

#[test]
fn settings_displays_cached_update_outcome_and_readable_utc_attempt() -> TestResult {
    let ledger = Ledger::new()?;
    let state = tempfile::tempdir()?;
    let now = beankeeper_desktop::updates::now();
    let cache = serde_json::to_vec(&json!({
        "desktop_version": env!("CARGO_PKG_VERSION"),
        "last_attempt": now,
        "manifest": null,
        "message": "update download failed; check connectivity and retry after the daily interval"
    }))?;
    std::fs::write(state.path().join("updates.json"), &cache)?;
    let mut command = ledger.command();
    command
        .args(["serve", "--bind", "127.0.0.1:0"])
        .env("BEANKEEPER_NO_UPDATE_CHECK", "0")
        .env("BEANKEEPER_DESKTOP_STATE_DIR", state.path());
    let mut server = Server::launch(command)?;
    server.pair()?;
    let response = server
        .client
        .get(format!("{}/settings", server.base))
        .send()?;
    assert_eq!(response.status(), 200);
    let html = response.text()?;
    let attempted_at = chrono::DateTime::<chrono::Utc>::from_timestamp(now.try_into()?, 0)
        .ok_or("fixture timestamp")?;
    assert!(html.contains("<h1>License and updates</h1>"));
    assert!(html.contains("Could not check for updates:"));
    assert!(html.contains(&format!(
        "Last attempt: {} UTC.",
        attempted_at.format("%Y-%m-%d %H:%M:%S")
    )));
    assert!(!html.contains("Unix seconds"));
    assert!(!html.contains(&now.to_string()));
    assert_eq!(std::fs::read(state.path().join("updates.json"))?, cache);
    Ok(())
}

#[test]
fn settings_ignores_other_version_and_legacy_update_results_without_rewriting_cache() -> TestResult
{
    let ledger = Ledger::new()?;
    for previous in [None, Some("0.1.0-rc.1")] {
        let state = tempfile::tempdir()?;
        let mut cache = json!({
            "last_attempt": beankeeper_desktop::updates::now(),
            "manifest": null,
            "message": "the release server could not be reached",
        });
        if let Some(previous) = previous {
            cache["desktop_version"] = previous.into();
        }
        let bytes = serde_json::to_vec(&cache)?;
        std::fs::write(state.path().join("updates.json"), &bytes)?;
        // Block the startup checker at its process lock so the page can be
        // tested offline before any fresh check finishes.
        std::fs::write(state.path().join("updates.lock"), b"")?;
        let mut command = ledger.command();
        command
            .args(["serve", "--bind", "127.0.0.1:0"])
            .env("BEANKEEPER_NO_UPDATE_CHECK", "0")
            .env("BEANKEEPER_DESKTOP_STATE_DIR", state.path());
        let mut server = Server::launch(command)?;
        server.pair()?;
        let response = server
            .client
            .get(format!("{}/settings", server.base))
            .send()?;
        assert_eq!(response.status(), 200);
        let html = response.text()?;
        assert!(html.contains("Updates have not been checked. Run `bk update-check`."));
        assert!(!html.contains("the release server could not be reached"));
        assert!(!html.contains("Last attempt:"));
        assert_eq!(std::fs::read(state.path().join("updates.json"))?, bytes);
    }
    Ok(())
}

#[test]
fn settings_shows_verified_license_status_and_specific_failures() -> TestResult {
    let ledger = Ledger::new()?;
    let directory = tempfile::tempdir()?;
    let state_dir = directory.path().join("desktop-state");
    let keys_path = directory.path().join("keys.json");
    let input_path = directory.path().join("license.json");
    std::fs::write(
        &keys_path,
        include_bytes!("fixtures/license-public-keys.json"),
    )?;
    std::fs::write(&input_path, include_bytes!("fixtures/license.json"))?;
    let mut command = ledger.command();
    command
        .args(["serve", "--bind", "127.0.0.1:0"])
        .env("BEANKEEPER_DESKTOP_STATE_DIR", &state_dir)
        .env("BEANKEEPER_LICENSE_KEYS_FILE", &keys_path);
    let mut server = Server::launch(command)?;
    let settings = format!("{}/settings", server.base);
    assert_eq!(server.client.get(&settings).send()?.status(), 401);
    server.pair()?;
    let activated = clean_command()
        .env("BEANKEEPER_DESKTOP_STATE_DIR", &state_dir)
        .env("BEANKEEPER_LICENSE_KEYS_FILE", &keys_path)
        .args(["license", "activate"])
        .arg(&input_path)
        .output()?;
    assert!(activated.status.success());
    let html = server.client.get(&settings).send()?.text()?;
    assert!(html.contains("License status: active and valid."));
    assert!(html.contains("lic_01h455vb4pex5vsknk084sn02q"));
    assert!(html.contains("perpetual Desktop use"));
    assert!(html.contains("2027-11-20T12:00:00+00:00 (inclusive)"));
    assert!(html.contains("Automatic update checks are disabled"));

    let license_path = state_dir.join("license.json");
    let original = std::fs::read_to_string(&license_path)?;
    for (license, keys, reason) in [
        (
            original.replace("AHHH0LU", "BHHH0LU"),
            include_str!("fixtures/license-public-keys.json"),
            "invalid license signature",
        ),
        (
            original,
            r#"{"unknown":"6kpsY-KcUgq-9VB7Ey7F-ZVHdq6-vnuSQh7qaRRG0iw"}"#,
            "license issuer is not in the trusted public-key store",
        ),
    ] {
        std::fs::write(&license_path, license)?;
        std::fs::write(&keys_path, keys)?;
        let html = server.client.get(&settings).send()?.text()?;
        assert!(html.contains("License status: unavailable."));
        assert!(html.contains(reason));
        assert!(!html.contains("active and valid"));
    }
    assert!(!state_dir.join("updates.json").exists());
    Ok(())
}

#[test]
fn binary_serves_embedded_views_and_authenticated_mcp_from_an_empty_directory() -> TestResult {
    let ledger = Ledger::new()?;
    let mut server = Server::start(&ledger, &["serve"], None)?;
    server.pair()?;
    // Startup consumes the passphrase once. Requests no longer need the file.
    std::fs::remove_file(&ledger.secret)?;
    for (path, heading) in [
        ("/", "Your books"),
        ("/accounts", "Accounts"),
        ("/transactions", "Transactions"),
        ("/reports", "Reports"),
        ("/settings", "License and updates"),
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
        if path == "/settings" {
            assert!(html.contains("License status: not activated."));
        }
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
        "/accounts",
        "/transactions",
        "/transactions/txn_missing",
        "/reports",
        "/settings",
        "/review",
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
    assert_duplicate_hosts_rejected(authority)?;
    Ok(())
}

#[test]
fn serve_migrates_an_existing_encrypted_ledger_before_opening_snapshots() -> TestResult {
    let ledger = Ledger::new()?;
    {
        let db = ledger.db()?;
        // Undo migration 21 to represent a ledger from the previous release.
        db.conn().execute_batch(
            "BEGIN;
             DROP TABLE setup_sessions;
             DELETE FROM schema_version WHERE version = 21;
             COMMIT;",
        )?;
        assert_eq!(get_schema_version(db.conn())?, 20);
    }

    let mut server = Server::start(&ledger, &["serve", "--read-only"], None)?;
    let passphrase = std::fs::read_to_string(&ledger.secret)?.into();
    let migrated = Db::open_snapshot(&ledger.path, Some(&passphrase))?;
    assert!(get_schema_version(migrated.conn())? >= 21);
    server.initialize(Some(token(&ledger.reader)?))?;
    server.pair()?;
    assert_eq!(server.client.get(&server.base).send()?.status(), 200);
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
fn pairing_gets_are_repeatable_and_only_a_same_origin_post_consumes_the_token() -> TestResult {
    let ledger = Ledger::new()?;
    let mut server = Server::start(&ledger, &["serve"], None)?;
    let url = server
        .pairing_url
        .as_deref()
        .ok_or("pairing URL")?
        .to_owned();
    let pairing_token = url.split_once('=').ok_or("pairing token")?.1;
    for _ in 0..2 {
        let response = server.client.get(&url).send()?;
        assert_eq!(response.status(), 200);
        assert!(response.headers().get("set-cookie").is_none());
        assert!(response.headers().get("location").is_none());
        assert_eq!(response.headers()["cache-control"], "no-store");
        // The form needs the real Origin; no-referrer would produce Origin: null.
        assert_eq!(response.headers()["referrer-policy"], "same-origin");
        assert!(
            response.headers()["content-security-policy"]
                .to_str()?
                .contains("form-action 'self'")
        );
        let html = response.text()?;
        assert!(html.contains("<form method=\"post\" action=\"/pair\">"));
        assert!(html.contains(&format!("name=\"token\" value=\"{pairing_token}\"")));
        assert!(html.contains("<button type=\"submit\">Pair this browser</button>"));
        assert!(!html.contains("http-equiv=\"refresh\""));
    }
    let response = server.client.head(&url).send()?;
    assert_eq!(response.status(), 200);
    assert!(response.headers().get("set-cookie").is_none());
    assert_eq!(server.client.get(&server.base).send()?.status(), 401);

    for (origin, expected) in [
        (None, 401),
        (Some("null"), 403),
        (Some("https://evil.example"), 403),
        (Some("http://127.0.0.1:1"), 403),
    ] {
        let mut request = server
            .client
            .post(format!("{}/pair", server.base))
            .form(&[("token", pairing_token)]);
        if let Some(origin) = origin {
            request = request.header("Origin", origin);
        }
        let response = request.send()?;
        assert_eq!(response.status(), expected);
        assert!(response.headers().get("set-cookie").is_none());
    }
    let response = human_post(&server, "/pair", format!("token={pairing_token}"))
        .header("Origin", &server.base)
        .send()?;
    assert_eq!(response.status(), 403);
    assert!(response.headers().get("set-cookie").is_none());
    let response = human_post(&server, "/pair", format!("token={pairing_token}"))
        .bearer_auth(token(&ledger.writer)?)
        .send()?;
    assert_eq!(response.status(), 401);
    assert!(response.headers().get("set-cookie").is_none());
    for (body, expected) in [
        (String::new(), 422),
        ("token=unknown".to_owned(), 410),
        (format!("token={pairing_token}&token={pairing_token}"), 422),
    ] {
        let response = human_post(&server, "/pair", body).send()?;
        assert_eq!(response.status(), expected);
        assert!(response.headers().get("set-cookie").is_none());
    }
    // Failed writes and speculative reads have left the original link usable.
    server.pair()?;
    assert_eq!(server.client.get(&server.base).send()?.status(), 200);
    let response = human_post(&server, "/pair", format!("token={pairing_token}")).send()?;
    assert_eq!(response.status(), 410);
    assert!(response.headers().get("set-cookie").is_none());
    assert_eq!(response.headers()["referrer-policy"], "no-referrer");
    assert!(!response.text()?.contains(pairing_token));
    Ok(())
}

#[test]
fn raw_origin_and_bearer_tokens_cannot_grant_a_browser_session() -> TestResult {
    let ledger = Ledger::new()?;
    let mut server = Server::start(&ledger, &["serve"], Some(token(&ledger.writer)?))?;
    for path in ["/", "/transactions", "/reports", "/review", "/settings"] {
        let response = server.client.get(format!("{}{path}", server.base)).send()?;
        assert_eq!(response.status(), 401);
        assert!(
            response.headers()["set-cookie"]
                .to_str()?
                .split('=')
                .next()
                .is_some_and(|name| name.ends_with("_return"))
        );
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
    assert_eq!(other.get(&link).send()?.status(), 200);
    let pairing_token = link.split_once('=').ok_or("pairing token")?.1;
    let response = other
        .post(format!("{}/pair", server.base))
        .header("Origin", &server.base)
        .form(&[("token", pairing_token)])
        .send()?;
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
    let invite_token = invite.split_once('=').ok_or("invite")?.1;
    let response = human_post(&server, "/pair", format!("token={invite_token}")).send()?;
    assert_eq!(response.status(), 410);
    assert!(response.headers().get("set-cookie").is_none());
    server.pair()?;
    // Cookie authentication alone cannot call the ledger MCP endpoint.
    assert_eq!(server.request("tools/list", json!({}), None)?.status(), 401);
    let response = server.client.get(&invite).send()?;
    assert_eq!(response.status(), 200);
    assert_eq!(response.headers()["referrer-policy"], "same-origin");
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

#[test]
fn paired_browser_reads_restore_deep_links_and_reject_agent_credentials() -> TestResult {
    let ledger = Ledger::new()?;
    let mut server = Server::start(&ledger, &["serve"], None)?;
    let paths = [
        "/",
        "/accounts?company=acme",
        "/transactions?company=acme&offset=50",
        "/transactions/txn_missing?company=acme",
        "/reports",
        "/settings",
        "/review",
    ];
    // All ledger and placeholder routes reject both anonymous and bearer reads.
    for path in paths {
        for bearer in [None, Some(token(&ledger.reader)?)] {
            let mut request = server.client.get(format!("{}{path}", server.base));
            if let Some(bearer) = bearer {
                request = request.bearer_auth(bearer);
            }
            let response = request.send()?;
            assert_eq!(response.status(), 401, "{path}");
            assert_eq!(response.headers()["referrer-policy"], "same-origin");
            let html = response.text()?;
            assert!(html.contains("Pair this browser"));
            assert!(!html.contains("Acme"));
            assert!(!html.contains(token(&ledger.reader)?));
        }
    }
    // Health and CSS remain public and contain no ledger or pairing secret.
    for path in ["/health", "/assets/style.css"] {
        let response = server.client.get(format!("{}{path}", server.base)).send()?;
        assert_eq!(response.status(), 200);
        assert!(response.headers().get("set-cookie").is_none());
        let body = response.text()?;
        assert!(!body.contains("Acme"));
        assert!(!body.contains(server.pairing_url.as_deref().ok_or("pairing URL")?));
    }
    // The removed access-code path cannot mint credentials.
    assert_eq!(
        server
            .client
            .post(format!("{}/unlock", server.base))
            .send()?
            .status(),
        404
    );
    server.pair()?;
    for path in paths {
        let response = server.client.get(format!("{}{path}", server.base)).send()?;
        assert_eq!(
            response.status(),
            if path.contains("txn_missing") {
                404
            } else {
                200
            },
            "{path}"
        );
        let html = response.text()?;
        assert_eq!(
            html,
            server
                .client
                .get(format!("{}{path}", server.base))
                .send()?
                .text()?
        );
        // Even a paired browser cannot mix agent authority into a page request.
        assert_eq!(
            server
                .client
                .get(format!("{}{path}", server.base))
                .bearer_auth(token(&ledger.reader)?)
                .send()?
                .status(),
            401
        );
    }
    assert!(
        server
            .client
            .get(format!("{}/accounts?company=acme", server.base))
            .send()?
            .text()?
            .contains("Acme")
    );
    assert_paired_browser_boundary(&server)?;
    Ok(())
}

#[cfg(unix)]
#[test]
fn one_shot_passphrase_commands_are_consumed_once_for_both_transports() -> TestResult {
    let ledger = Ledger::new()?;
    let directory = tempfile::tempdir()?;
    let marker = directory.path().join("consumed");
    let quote = |value: &std::path::Path| {
        format!("'{}'", value.display().to_string().replace('\'', "'\\''"))
    };
    let source = format!(
        "test ! -e {0} || exit 1; touch {0}; cat {1}",
        quote(&marker),
        quote(&ledger.secret)
    );
    let mut command = clean_command();
    command
        .arg("--db")
        .arg(&ledger.path)
        .args(["serve", "--bind", "127.0.0.1:0"])
        .env("BEANKEEPER_PASSPHRASE_CMD", source)
        // A nonexistent temp directory makes any secret-file bridge fail.
        .env("TMPDIR", directory.path().join("no-temp-files"));
    let mut server = Server::launch(command)?;
    assert!(marker.exists());
    std::fs::remove_file(&ledger.secret)?;
    server.pair()?;
    assert_eq!(
        server
            .client
            .get(format!("{}/accounts", server.base))
            .send()?
            .status(),
        200
    );
    server.initialize(Some(token(&ledger.reader)?))?;
    assert!(rpc(server.request("tools/list", json!({}), Some(token(&ledger.reader)?))?)?["result"]["tools"].is_array());
    Ok(())
}

#[test]
fn posted_transaction_can_be_followed_from_register_to_detail_and_reloaded() -> TestResult {
    use beankeeper_cli::db::{self, ConflictStrategy, PostEntryParams, PostTransactionParams};
    let ledger = Ledger::new()?;
    let db = ledger.db()?;
    db::create_company(db.conn(), "other", "Other Books", None)?;
    db::create_account(db.conn(), "acme", "1000", "Cash", "asset", None)?;
    db::create_account(db.conn(), "acme", "4000", "Sales", "revenue", None)?;
    let actor =
        Actor::new("owner").with_identity(identity::verify(db.conn(), token(&ledger.writer)?)?);
    let entries = [
        PostEntryParams {
            account_code: "1000".into(),
            direction: "debit".into(),
            amount: 2500,
            memo: None,
            tax_category: None,
        },
        PostEntryParams {
            account_code: "4000".into(),
            direction: "credit".into(),
            amount: 2500,
            memo: None,
            tax_category: None,
        },
    ];
    let db::PostResult::Created(id) = db::post_transaction(
        db.conn(),
        &PostTransactionParams {
            company_slug: "acme",
            description: "Coffee <shop>",
            metadata: Some("{\"source\":\"fixture\"}"),
            currency: "USD",
            date: "2026-11-20",
            entries: &entries,
            correlate: None,
            reference: Some("inv&1"),
            on_conflict: ConflictStrategy::Error,
            actor: &actor,
        },
    )?
    else {
        return Err("transaction was not created".into());
    };
    drop(db);
    let mut server = Server::start(&ledger, &["--company", "other", "serve"], None)?;
    server.pair()?;
    let default = server
        .client
        .get(format!("{}/", server.base))
        .send()?
        .text()?;
    assert!(default.contains("Financial snapshot"));
    assert!(default.contains("href=\"/accounts?company=other\""));
    assert!(!default.contains("<select"));
    assert!(!default.contains("Coffee &lt;shop&gt;"));
    let url = format!(
        "{}/transactions?company=acme&account=1000&from=2026-11-20&to=2026-11-20&min_amount=2500&max_amount=2500&status=uncleared&search=inv%261",
        server.base
    );
    let html = server.client.get(&url).send()?.error_for_status()?.text()?;
    assert!(html.contains("Coffee &lt;shop&gt;"));
    assert!(html.contains("debit $25.00 (uncleared)"));
    assert!(html.contains(&format!("/transactions/{id}?company=acme")));
    assert_eq!(html, server.client.get(&url).send()?.text()?);
    let detail = server
        .client
        .get(format!("{}/transactions/{id}?company=acme", server.base))
        .send()?
        .error_for_status()?
        .text()?;
    for content in [
        "Entries",
        "fixture",
        "Audit trail",
        "agent://",
        ledger.writer["token_jti"].as_str().ok_or("jti")?,
    ] {
        assert!(detail.contains(content), "{content}");
    }
    assert!(detail.contains("class=\"amount\">$25.00</td>"));
    assert!(!detail.contains(token(&ledger.writer)?));
    assert!(!detail.contains("<shop>"));
    let snapshot = server
        .client
        .get(format!("{}/?company=acme", server.base))
        .send()?
        .error_for_status()?
        .text()?;
    assert!(snapshot.contains("Financial snapshot"));
    assert!(snapshot.contains("Net worth is USD 25.00"));
    assert!(snapshot.contains("You can ask your agent"));
    assert!(snapshot.contains("href=\"/reports?company=acme"));
    assert!(!snapshot.contains("Recent activity"));
    Ok(())
}

fn assert_duplicate_hosts_rejected(authority: &str) -> TestResult {
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

fn assert_paired_browser_boundary(server: &Server) -> TestResult {
    // A browser cookie alone cannot authorize MCP; invalid bearers also fail.
    for bearer in [None, Some("invalid")] {
        assert_eq!(
            server.request("tools/list", json!({}), bearer)?.status(),
            401
        );
    }
    for path in [
        "/transactions?from=2026-02-30",
        "/transactions?offset=garbage",
    ] {
        assert_eq!(
            server
                .client
                .get(format!("{}{path}", server.base))
                .send()?
                .status(),
            400
        );
    }
    let html = server.client.get(&server.base).send()?.text()?;
    assert_eq!(
        human_post(server, "/accounts", format!("csrf={}", csrf_from(&html)?))
            .send()?
            .status(),
        405
    );
    Ok(())
}
