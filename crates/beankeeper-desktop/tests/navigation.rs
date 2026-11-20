mod common;

use beankeeper_cli::{
    db::{self, Actor},
    identity::{self, IssueRequest},
};
use common::{Ledger, Server, TestResult, rpc, token};
use reqwest::blocking::Response;
use serde_json::{Value, json};
use std::{
    io::{BufRead, BufReader},
    time::Duration,
};

fn issue(ledger: &Ledger, name: &str) -> TestResult<Value> {
    Ok(identity::issue(
        ledger.db()?.conn(),
        &Actor::new("owner"),
        &IssueRequest {
            name,
            capabilities: &["ui/navigate".into()],
            ttl: Duration::from_secs(3600),
            public_key: None,
            generate_key: true,
        },
    )?)
}

fn call(server: &Server, bearer: &str, name: &str, arguments: Value) -> TestResult<Value> {
    rpc(server.request(
        "tools/call",
        json!({"name":name,"arguments":arguments}),
        Some(bearer),
    )?)
}

fn content(result: &Value) -> &Value {
    &result["result"]["structuredContent"]
}
fn text(result: &Value) -> &str {
    result["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_default()
}

fn csrf(server: &Server) -> TestResult<String> {
    let html = server
        .client
        .get(format!("{}/", server.base))
        .send()?
        .text()?;
    Ok(html
        .split("name=\"csrf\" value=\"")
        .nth(1)
        .and_then(|v| v.split('"').next())
        .ok_or("CSRF")?
        .into())
}

fn associate(server: &Server, bearer: &str, csrf: &str) -> TestResult {
    let result = call(server, bearer, "ui_status", json!({}))?;
    let url = content(&result)["pairing_url"].as_str().ok_or("invite")?;
    let invite = url.split("invite=").nth(1).ok_or("invite")?;
    let response = server
        .client
        .post(format!("{}/api/session/associate", server.base))
        .header("Origin", &server.base)
        .form(&[("csrf", csrf), ("invite", invite)])
        .send()?;
    assert_eq!(response.status(), 303);
    Ok(())
}

fn update(
    server: &Server,
    csrf: &str,
    id: Option<&str>,
    url: &str,
    highlight: &str,
) -> TestResult<String> {
    let mut form = vec![
        ("csrf", csrf),
        ("url", url),
        ("targets", "main\ntransaction-1\nverification"),
        ("highlight", highlight),
    ];
    if let Some(id) = id {
        form.push(("tab_id", id));
    }
    let response = server
        .client
        .post(format!("{}/api/ui/tab", server.base))
        .header("Origin", &server.base)
        .form(&form)
        .send()?;
    assert_eq!(response.status(), 200);
    Ok(response.json::<Value>()?["tab_id"]
        .as_str()
        .ok_or("tab_id")?
        .into())
}

fn stream(server: &Server, id: &str) -> TestResult<BufReader<Response>> {
    let response = server
        .client
        .get(format!("{}/api/ui/events?tab_id={id}", server.base))
        .send()?;
    assert_eq!(response.status(), 200);
    assert!(
        response.headers()["content-type"]
            .to_str()?
            .starts_with("text/event-stream")
    );
    Ok(BufReader::new(response))
}

fn event(reader: &mut impl BufRead, kind: &str) -> TestResult<Value> {
    for _ in 0..80 {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 {
            return Err("SSE closed".into());
        }
        if let Some(data) = line.strip_prefix("data:") {
            let value: Value = serde_json::from_str(data.trim())?;
            if value["kind"] == kind {
                return Ok(value);
            }
        }
    }
    Err("missing SSE event".into())
}

#[test]
fn tools_use_live_upstream_grants_and_give_text_without_a_tab() -> TestResult {
    let ledger = Ledger::new()?;
    let navigator = issue(&ledger, "navigator")?;
    let nav = token(&navigator)?;
    let reader = token(&ledger.reader)?;
    let mut server = Server::start(&ledger, &["serve"], Some(reader))?;
    server.initialize(Some(reader))?;
    for enforce in [true, false] {
        db::audit::set_policy(ledger.db()?.conn(), &Actor::new("owner"), enforce)?;
        for bearer in [reader, token(&ledger.writer)?, nav] {
            let tools = rpc(server.request("tools/list", json!({}), Some(bearer))?)?;
            let tools = tools["result"]["tools"].as_array().ok_or("tools")?;
            for name in ["ui_show_view", "ui_highlight", "ui_status"] {
                assert_eq!(tools.iter().any(|tool| tool["name"] == name), bearer == nav);
                let arguments = match name {
                    "ui_show_view" => {
                        json!({"view_id":"transactions","state":{"company":"acme","account":"1000"}})
                    }
                    "ui_highlight" => json!({"target":"main"}),
                    _ => json!({}),
                };
                let result = call(&server, bearer, name, arguments)?;
                if bearer != nav {
                    assert_eq!(content(&result)["code"], "FORBIDDEN");
                } else {
                    assert_ne!(result["result"]["isError"], true);
                    assert!(text(&result).contains("nothing") || text(&result).contains("Nothing"));
                    assert!(text(&result).contains("/pair?invite="));
                    assert!(content(&result)["pairing_url"].as_str().is_some());
                }
            }
        }
    }
    let unavailable = call(
        &server,
        nav,
        "ui_show_view",
        json!({"view_id":"reports","state":{"report_kind":"balance_sheet","period":"2026"}}),
    )?;
    assert!(text(&unavailable).contains("View not available"));
    assert!(content(&unavailable)["pairing_url"].as_str().is_some());
    for arguments in [
        json!({"view_id":"https://evil.example"}),
        json!({"view_id":"transactions","state":{"from":"2026-02-30"}}),
        json!({"view_id":"transaction"}),
        json!({"view_id":"review","state":{"account":"1000"}}),
        json!({"view_id":"dashboard","injected":true}),
    ] {
        assert_eq!(
            call(&server, nav, "ui_show_view", arguments)?["result"]["isError"],
            true
        );
    }
    assert_eq!(
        server
            .request("tools/list", json!({}), Some("invalid"))?
            .status(),
        401
    );
    identity::revoke(
        ledger.db()?.conn(),
        &Actor::new("owner"),
        navigator["token_jti"].as_str().ok_or("jti")?,
    )?;
    assert_eq!(
        server
            .request(
                "tools/call",
                json!({"name":"ui_status","arguments":{}}),
                Some(nav)
            )?
            .status(),
        401
    );
    Ok(())
}

#[test]
fn read_only_removes_navigation_mutations_but_keeps_status() -> TestResult {
    let ledger = Ledger::new()?;
    let navigator = issue(&ledger, "navigator")?;
    let nav = token(&navigator)?;
    let mut server = Server::start(&ledger, &["serve", "--read-only"], Some(nav))?;
    server.initialize(None)?;
    let tools = rpc(server.request("tools/list", json!({}), None)?)?;
    let tools = tools["result"]["tools"].as_array().ok_or("tools")?;
    assert!(tools.iter().any(|tool| tool["name"] == "ui_status"));
    for name in ["ui_show_view", "ui_highlight"] {
        assert!(!tools.iter().any(|tool| tool["name"] == name));
        let result = call(
            &server,
            nav,
            name,
            json!({"view_id":"dashboard","target":"main"}),
        )?;
        assert!(result.get("error").is_some() || result["result"]["isError"] == true);
    }
    assert!(text(&call(&server, nav, "ui_status", json!({}))?).contains("/pair?invite="));
    Ok(())
}

#[test]
fn paired_sse_delivers_scoped_navigation_highlight_and_confirmed_status() -> TestResult {
    let ledger = Ledger::new()?;
    let navigator = issue(&ledger, "navigator")?;
    let other = issue(&ledger, "other")?;
    let nav = token(&navigator)?;
    let mut server = Server::start(&ledger, &["serve"], None)?;
    server.initialize(Some(token(&ledger.reader)?))?;
    server.pair()?;
    let csrf = csrf(&server)?;
    associate(&server, nav, &csrf)?;
    let tab = update(&server, &csrf, None, "/?company=acme", "")?;
    let mut events = stream(&server, &tab)?;
    event(&mut events, "heartbeat")?;
    let result = call(
        &server,
        nav,
        "ui_show_view",
        json!({"view_id":"transactions","state":{"company":"acme","from":"2026-01-01","search":"rent & fees"}}),
    )?;
    assert_eq!(content(&result)["queued"], true);
    let navigation = event(&mut events, "navigate")?;
    let url = navigation["url"].as_str().ok_or("url")?;
    assert_eq!(
        url,
        "/transactions?company=acme&from=2026-01-01&search=rent%20%26%20fees"
    );
    let before = call(&server, nav, "ui_status", json!({}))?;
    assert_eq!(content(&before)["tabs"][0]["url"], "/?company=acme");
    let response = server.client.get(format!("{}{url}", server.base)).send()?;
    assert_eq!(response.status(), 200);
    assert!(response.text()?.contains("<h1>Transactions</h1>"));
    assert_eq!(update(&server, &csrf, Some(&tab), url, "")?, tab);
    let confirmed = call(&server, nav, "ui_status", json!({}))?;
    assert_eq!(content(&confirmed)["tabs"][0]["url"], url);
    assert_eq!(content(&confirmed)["paired"], true);
    let missing = call(
        &server,
        nav,
        "ui_highlight",
        json!({"target":"missing-row"}),
    )?;
    assert_eq!(content(&missing)["queued"], false);
    assert!(text(&missing).contains("No such row"));
    let highlight = call(
        &server,
        nav,
        "ui_highlight",
        json!({"target":"transaction-1"}),
    )?;
    assert_eq!(content(&highlight)["queued"], true);
    assert_eq!(event(&mut events, "highlight")?["target"], "transaction-1");
    update(&server, &csrf, Some(&tab), url, "transaction-1")?;
    assert_eq!(
        content(&call(&server, nav, "ui_status", json!({}))?)["tabs"][0]["highlight"],
        "transaction-1"
    );
    let other_status = call(&server, token(&other)?, "ui_status", json!({}))?;
    assert_eq!(content(&other_status)["paired"], false);
    assert_eq!(content(&other_status)["tabs"], json!([]));
    let targeted = call(
        &server,
        token(&other)?,
        "ui_show_view",
        json!({"view_id":"review","tab_id":tab}),
    )?;
    assert_eq!(content(&targeted)["queued"], false);
    let second = update(&server, &csrf, None, "/review?company=acme", "")?;
    let mut second_events = stream(&server, &second)?;
    event(&mut second_events, "heartbeat")?;
    let ambiguous = call(&server, nav, "ui_show_view", json!({"view_id":"accounts"}))?;
    assert_eq!(content(&ambiguous)["code"], "AMBIGUOUS_TAB");
    let exact = call(
        &server,
        nav,
        "ui_show_view",
        json!({"view_id":"review_detail","state":{"company":"acme","proposal_id":"proposal_01h455vb4pex5vsknk084sn02q"},"tab_id":second}),
    )?;
    assert_eq!(content(&exact)["queued"], true);
    assert_eq!(
        event(&mut second_events, "navigate")?["url"],
        "/review/proposal_01h455vb4pex5vsknk084sn02q?company=acme"
    );
    assert_eq!(event(&mut events, "heartbeat")?["kind"], "heartbeat");
    let response = server
        .client
        .post(format!("{}/api/session/logout", server.base))
        .header("Origin", &server.base)
        .form(&[("csrf", &csrf)])
        .send()?;
    assert_eq!(response.status(), 303);
    let status = call(&server, nav, "ui_status", json!({}))?;
    assert_eq!(content(&status)["tabs"], json!([]));
    let mut rest = String::new();
    std::io::Read::read_to_string(&mut events, &mut rest)?;
    assert!(!rest.contains("navigate"));
    Ok(())
}

#[test]
fn tab_routes_require_cookie_origin_csrf_and_never_accept_bearer_authority() -> TestResult {
    let ledger = Ledger::new()?;
    let mut server = Server::start(&ledger, &["serve"], None)?;
    assert_eq!(
        server
            .client
            .get(format!("{}/api/ui/events?tab_id=tab_fake", server.base))
            .send()?
            .status(),
        401
    );
    server.pair()?;
    let csrf = csrf(&server)?;
    let form = [("csrf", csrf.as_str()), ("url", "/"), ("targets", "main")];
    for (origin, token) in [
        (None, None),
        (Some("https://evil.example"), None),
        (Some(server.base.as_str()), Some(token(&ledger.reader)?)),
    ] {
        let mut request = server
            .client
            .post(format!("{}/api/ui/tab", server.base))
            .form(&form);
        if let Some(origin) = origin {
            request = request.header("Origin", origin);
        }
        if let Some(token) = token {
            request = request.bearer_auth(token);
        }
        assert!(!request.send()?.status().is_success());
    }
    assert_eq!(
        server
            .client
            .post(format!("{}/api/ui/tab", server.base))
            .header("Origin", &server.base)
            .form(&[("csrf", "bad"), ("url", "/"), ("targets", "main")])
            .send()?
            .status(),
        401
    );
    for url in [
        "https://evil.example",
        "//evil.example",
        "/api/session/logout",
        "/review/x/approve",
    ] {
        assert_eq!(
            server
                .client
                .post(format!("{}/api/ui/tab", server.base))
                .header("Origin", &server.base)
                .form(&[("csrf", csrf.as_str()), ("url", url), ("targets", "main")])
                .send()?
                .status(),
            400
        );
    }
    let tab = update(&server, &csrf, None, "/", "")?;
    assert_eq!(
        server
            .client
            .get(format!(
                "{}/api/ui/events?tab_id=tab_01h455vb4pex5vsknk084sn02q",
                server.base
            ))
            .send()?
            .status(),
        401
    );
    assert_eq!(
        server
            .client
            .get(format!("{}/api/ui/events?tab_id={tab}", server.base))
            .bearer_auth(token(&ledger.reader)?)
            .send()?
            .status(),
        401
    );
    Ok(())
}

#[cfg(unix)]
#[test]
fn shutdown_closes_live_browser_stream_and_drains_the_listener() -> TestResult {
    let ledger = Ledger::new()?;
    let mut server = Server::start(&ledger, &["serve"], None)?;
    server.pair()?;
    let csrf = csrf(&server)?;
    let tab = update(&server, &csrf, None, "/", "")?;
    let mut events = stream(&server, &tab)?;
    event(&mut events, "heartbeat")?;
    assert!(
        std::process::Command::new("kill")
            .args(["-INT", &server.child.id().to_string()])
            .status()?
            .success()
    );
    let mut rest = String::new();
    std::io::Read::read_to_string(&mut events, &mut rest)?;
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    loop {
        if let Some(status) = server.child.try_wait()? {
            assert!(status.success());
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "Server failed to drain SSE"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    Ok(())
}

#[test]
fn closed_tabs_are_removed_and_another_browser_cannot_claim_their_stream() -> TestResult {
    let ledger = Ledger::new()?;
    let navigator = issue(&ledger, "navigator")?;
    let nav = token(&navigator)?;
    let mut server = Server::start(&ledger, &["serve"], None)?;
    server.initialize(Some(nav))?;
    server.pair()?;
    let csrf = csrf(&server)?;
    associate(&server, nav, &csrf)?;
    let tab = update(&server, &csrf, None, "/", "")?;
    let mut events = stream(&server, &tab)?;
    event(&mut events, "heartbeat")?;
    assert_eq!(
        content(&call(&server, nav, "ui_status", json!({}))?)["tabs"]
            .as_array()
            .ok_or("tabs")?
            .len(),
        1
    );
    let link_html = server
        .client
        .post(format!("{}/api/session/pairing", server.base))
        .header("Origin", &server.base)
        .form(&[("csrf", &csrf)])
        .send()?
        .text()?;
    let url = link_html
        .split("href=\"")
        .filter_map(|value| value.split('"').next())
        .find(|url| url.starts_with(&format!("{}/pair?token=", server.base)))
        .ok_or("browser link")?;
    let other = reqwest::blocking::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(5))
        .build()?;
    let redeemed = other.get(url).send()?;
    assert_eq!(redeemed.status(), 303);
    let cookie = redeemed.headers()["set-cookie"]
        .to_str()?
        .split(';')
        .next()
        .ok_or("cookie")?;
    assert_eq!(
        other
            .get(format!("{}/api/ui/events?tab_id={tab}", server.base))
            .header("Cookie", cookie)
            .send()?
            .status(),
        401
    );
    drop(events);
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    loop {
        let result = call(&server, nav, "ui_status", json!({}))?;
        if content(&result)["tabs"] == json!([]) {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "Closed tab remained connected"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    let result = call(&server, nav, "ui_show_view", json!({"view_id":"accounts"}))?;
    assert_eq!(content(&result)["queued"], false);
    assert!(text(&result).contains("nothing was displayed"));
    for path in [
        "/transactions/999999?company=acme",
        "/review/not_a_proposal?company=acme",
    ] {
        let response = server.client.get(format!("{}{path}", server.base)).send()?;
        assert!(!response.status().is_success());
        let html = response.text()?;
        assert!(html.contains("/assets/navigation.js"));
        assert!(
            html.contains("aria-labelledby=\"session\""),
            "Error view lost its navigation session"
        );
    }
    Ok(())
}
