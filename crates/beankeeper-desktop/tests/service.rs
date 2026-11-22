//! Lifecycle discovery remains usable without a running ledger server.

pub mod common;

use common::{TestResult, clean_command};
use serde_json::{Value, json};
use std::{
    io::{BufRead, BufReader, Write},
    process::Stdio,
    sync::mpsc,
    time::Duration,
};

fn isolated(root: &std::path::Path) -> std::process::Command {
    let mut command = clean_command();
    command
        .env("BEANKEEPER_SERVICE_DATA_DIR", root.join("data"))
        .env("BEANKEEPER_DESKTOP_STATE_DIR", root.join("state"))
        .env("BEANKEEPER_SERVICE_LOG_DIR", root.join("logs"));
    command
}

#[cfg(target_os = "linux")]
fn without_session(root: &std::path::Path) -> std::process::Command {
    let mut command = isolated(root);
    command
        .env_remove("XDG_RUNTIME_DIR")
        .env_remove("DBUS_SESSION_BUS_ADDRESS");
    command
}

#[test]
fn stopped_service_status_is_structured_and_has_no_side_effects() -> TestResult {
    let root = tempfile::tempdir()?;
    let output = isolated(root.path())
        .args(["service", "status", "--json"])
        .output()?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let status: Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(status["installed"], false);
    assert_eq!(status["running"], false);
    assert!(status["agent_access"].is_null());
    assert!(status["mcp_endpoint"].is_null());
    assert!(status["version"].is_null());
    assert_eq!(
        status["books"],
        root.path().join("data/books.db").to_string_lossy().as_ref()
    );
    assert!(!root.path().join("data").exists());
    assert!(!root.path().join("state").exists());
    Ok(())
}

#[test]
fn local_lifecycle_mcp_lists_all_commands_and_reports_named_errors_when_stopped() -> TestResult {
    let root = tempfile::tempdir()?;
    check_lifecycle(isolated(root.path()), "SERVICE_MANAGER")
}

fn check_lifecycle(mut command: std::process::Command, start_error: &str) -> TestResult {
    let mut child = command
        .args(["service", "mcp"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let result = (|| -> TestResult {
        let mut input = child.stdin.take().ok_or("missing stdin")?;
        let output = child.stdout.take().ok_or("missing stdout")?;
        let (send, receive) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(output).lines() {
                if send.send(line).is_err() {
                    break;
                }
            }
        });
        let mut request = |message: Value| -> Result<Value, Box<dyn std::error::Error>> {
            writeln!(input, "{message}")?;
            input.flush()?;
            Ok(serde_json::from_str(
                &receive.recv_timeout(Duration::from_secs(15))??,
            )?)
        };
        let initialize = request(
            json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"service-test","version":"1"}}}),
        )?;
        assert_eq!(
            initialize["result"]["serverInfo"]["name"],
            "beankeeper-service"
        );
        // This notification has no reply; send it independently.
        drop(request);
        writeln!(
            input,
            "{}",
            json!({"jsonrpc":"2.0","method":"notifications/initialized"})
        )?;
        input.flush()?;
        check_lifecycle_tools(&mut input, &receive, start_error)?;
        drop(input);
        Ok(())
    })();
    // Reap on both assertion-independent failure and success paths.
    let _ = child.kill();
    let _ = child.wait();
    result
}

fn check_lifecycle_tools(
    input: &mut impl Write,
    receive: &mpsc::Receiver<std::io::Result<String>>,
    start_error: &str,
) -> TestResult {
    for (id, method, params) in [
        (2, "tools/list", json!({})),
        (
            3,
            "tools/call",
            json!({"name":"service_status","arguments":{}}),
        ),
        (
            4,
            "tools/call",
            json!({"name":"service_start","arguments":{}}),
        ),
        (
            5,
            "tools/call",
            json!({"name":"service_status","arguments":{"unexpected":true}}),
        ),
    ] {
        writeln!(
            input,
            "{}",
            json!({"jsonrpc":"2.0","id":id,"method":method,"params":params})
        )?;
        input.flush()?;
        let reply: Value = serde_json::from_str(&receive.recv_timeout(Duration::from_secs(15))??)?;
        assert_eq!(reply["id"], id);
        match id {
            2 => {
                let names: Vec<_> = reply["result"]["tools"]
                    .as_array()
                    .ok_or("missing tools")?
                    .iter()
                    .map(|t| t["name"].as_str().unwrap_or_default())
                    .collect();
                for name in [
                    "service_install",
                    "service_uninstall",
                    "service_start",
                    "service_stop",
                    "service_status",
                ] {
                    assert!(names.contains(&name));
                }
            }
            3 => assert_eq!(reply["result"]["structuredContent"]["running"], false),
            4 => {
                assert_eq!(reply["result"]["isError"], true);
                assert_eq!(reply["result"]["structuredContent"]["code"], start_error);
            }
            5 => {
                assert_eq!(reply["result"]["isError"], true);
                assert_eq!(
                    reply["result"]["structuredContent"]["code"],
                    "SERVICE_ARGUMENTS"
                );
            }
            _ => unreachable!(),
        }
    }
    Ok(())
}

#[test]
fn managed_bridge_fails_with_actionable_service_error_when_stopped() -> TestResult {
    let root = tempfile::tempdir()?;
    let output = isolated(root.path())
        .args(["mcp", "bridge", "--service"])
        .output()?;
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("SERVICE_NOT_READY"));
    Ok(())
}

#[cfg(target_os = "linux")]
#[test]
fn managed_host_publishes_fallback_endpoint_and_rejects_stale_runtime_identity() -> TestResult {
    use beankeeper_desktop::setup::keychain::{restrict_directory, restrict_file};
    use common::Server;
    use std::{
        fs,
        net::{Ipv4Addr, TcpListener},
    };
    let root = tempfile::tempdir()?;
    let data = root.path().join("data");
    let state = root.path().join("state");
    let home = root.path().join("home");
    let config = root.path().join("config");
    let bin = root.path().join("bin");
    for path in [&data, &state, &home, &bin, &config.join("systemd/user")] {
        fs::create_dir_all(path)?;
        restrict_directory(path)?;
    }
    let books = data.join("books.db");
    let configured = || {
        let mut command = without_session(root.path());
        command
            .env("HOME", &home)
            .env("XDG_CONFIG_HOME", &config)
            .env(
                "PATH",
                format!(
                    "{}:{}",
                    bin.display(),
                    std::env::var("PATH").unwrap_or_default()
                ),
            );
        command
    };
    // Let the real install provision the ledger, then take over from the stub manager.
    write_stub_manager(&bin)?;
    let output = configured()
        .env("DBUS_SESSION_BUS_ADDRESS", "unix:path=/stub-systemd-bus")
        .args(["service", "install", "--file-store", "--no-agents"])
        .output()?;
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("OS service manager refused"));
    let occupied = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
    let service_config = state.join("service.json");
    let mut registration: Value = serde_json::from_slice(&fs::read(&service_config)?)?;
    registration["port"] = json!(occupied.local_addr()?.port());
    fs::write(&service_config, serde_json::to_vec(&registration)?)?;
    restrict_file(&service_config)?;
    fs::write(
        config.join("systemd/user/beankeeper-desktop.service"),
        "test registration",
    )?;
    let mut host = configured();
    host.args(["service", "run"]);
    let server = Server::launch(host)?;
    assert!(
        !server
            .base
            .ends_with(&occupied.local_addr()?.port().to_string())
    );
    let output = configured()
        .args(["service", "status", "--json"])
        .output()?;
    let status: Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(status["running"], true);
    assert_eq!(status["version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(status["mcp_endpoint"], format!("{}/mcp", server.base));
    assert_fallback_link(&configured, &state, &server.base)?;
    assert_eq!(status["agent_access"]["preset"], "bookkeeper");
    assert_eq!(status["agent_access"]["write_mode"], "direct");
    let agent_command = || {
        let mut command = configured();
        command
            .env("HOME", &home)
            .env("CODEX_HOME", home.join("managed-codex"));
        command
    };
    assert_agent_reaches_service(&agent_command, &status["mcp_endpoint"])?;
    let duplicate = configured().args(["service", "run"]).output()?;
    assert!(!duplicate.status.success());
    assert!(String::from_utf8_lossy(&duplicate.stderr).contains("already running"));
    assert_stale_runtime_fails(&configured, &agent_command, &state)?;
    assert_managed_stdio_launchers(&configured, &state, &server.base)?;
    assert_managed_write_modes(&configured, &books, &status["agent_access"])?;
    assert_managed_auth_failure(&configured, &agent_command, &books)?;
    drop(server);
    let output = configured()
        .args(["service", "status", "--json"])
        .output()?;
    assert_eq!(
        serde_json::from_slice::<Value>(&output.stdout)?["running"],
        false
    );
    Ok(())
}

#[cfg(target_os = "linux")]
fn assert_fallback_link(
    configured: &impl Fn() -> std::process::Command,
    state: &std::path::Path,
    base: &str,
) -> TestResult {
    let persisted: Value = serde_json::from_slice(&std::fs::read(state.join("service.json"))?)?;
    assert_eq!(
        persisted["port"],
        reqwest::Url::parse(base)?.port().ok_or("fallback port")?
    );
    let output = configured()
        .args(["--json", "ui", "link", "--view-id", "accounts"])
        .output()?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let link: Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(link["url"], format!("{base}/accounts"));
    Ok(())
}

#[cfg(target_os = "linux")]
fn assert_agent_reaches_service(
    agent_command: &impl Fn() -> std::process::Command,
    endpoint: &Value,
) -> TestResult {
    let output = agent_command()
        .args(["agents", "connect", "codex", "--json"])
        .output()?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    let output = agent_command()
        .args(["agents", "doctor", "codex", "--json"])
        .output()?;
    let diagnostic: Value = serde_json::from_slice(&output.stdout)?;
    assert!(
        output.status.success(),
        "{diagnostic}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(diagnostic["data"]["agents"][0]["state"], "configured");
    assert_eq!(diagnostic["data"]["service"]["state"], "ready");
    assert_eq!(&diagnostic["data"]["service"]["endpoint"], endpoint);
    Ok(())
}

#[cfg(target_os = "linux")]
fn assert_stale_runtime_fails(
    configured: &impl Fn() -> std::process::Command,
    agent_command: &impl Fn() -> std::process::Command,
    state: &std::path::Path,
) -> TestResult {
    use std::fs;
    let runtime_path = state.join("service-runtime.json");
    let runtime = fs::read(&runtime_path)?;
    let mut wrong_process: Value = serde_json::from_slice(&runtime)?;
    wrong_process["pid"] = json!(std::process::id());
    fs::write(&runtime_path, serde_json::to_vec(&wrong_process)?)?;
    let output = configured()
        .args(["service", "status", "--json"])
        .output()?;
    assert_eq!(
        serde_json::from_slice::<Value>(&output.stdout)?["running"],
        false
    );
    let output = agent_command()
        .args(["agents", "doctor", "codex", "--json"])
        .output()?;
    let diagnostic: Value = serde_json::from_slice(&output.stdout)?;
    assert!(!output.status.success());
    assert_eq!(
        diagnostic["data"]["service"]["error"]["code"],
        "SERVICE_ENDPOINT_IDENTITY"
    );
    fs::write(&runtime_path, &runtime)?;
    Ok(())
}

#[cfg(target_os = "linux")]
fn assert_managed_auth_failure(
    configured: &impl Fn() -> std::process::Command,
    agent_command: &impl Fn() -> std::process::Command,
    books: &std::path::Path,
) -> TestResult {
    use std::fs;
    let file = books.with_extension("agent-token");
    let mut revoked = secrecy::zeroize::Zeroizing::new(fs::read(&file)?);
    let first = revoked.first_mut().ok_or("missing agent credential")?;
    *first = if *first == b'A' { b'B' } else { b'A' };
    fs::write(file, revoked.as_slice())?;

    for (mut command, args, error_at) in [
        (
            configured(),
            &["service", "status", "--json"][..],
            "/error_code",
        ),
        (
            agent_command(),
            &["agents", "doctor", "codex", "--json"][..],
            "/data/service/error/code",
        ),
    ] {
        let output = command.args(args).output()?;
        let diagnostic: Value = serde_json::from_slice(&output.stdout)?;
        assert_eq!(
            diagnostic.pointer(error_at).and_then(Value::as_str),
            Some("AGENT_MCP_REJECTED")
        );
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn assert_managed_stdio_launchers(
    configured: &impl Fn() -> std::process::Command,
    state: &std::path::Path,
    base: &str,
) -> TestResult {
    // Both stdio launchers discover the same running service. The extension
    // leaves its installation alone and adds its MCP onboarding instructions.
    for args in [
        &["mcp", "bridge", "--service"][..],
        &["mcp", "extension"][..],
    ] {
        let mut transport = configured()
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()?;
        let mut input = transport.stdin.take().ok_or("stdin")?;
        for request in [
            json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"managed-test","version":"1"}}}),
            json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
            json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}),
            json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"ui_get_link","arguments":{
                "view_id":"income-statement","state":{"currency":"USD","from":"2026-11-01","to":"2026-11-30"}
            }}}),
            json!({"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"ui_show_view","arguments":{"view_id":"income-statement"}}}),
        ] {
            writeln!(input, "{request}")?;
        }
        drop(input);
        let output = transport.wait_with_output()?;
        assert!(output.status.success());
        let replies: Vec<Value> = String::from_utf8(output.stdout)?
            .lines()
            .map(serde_json::from_str)
            .collect::<Result<_, _>>()?;
        let [initialized, tools, link, navigation] = replies.as_slice() else {
            return Err(format!(
                "expected initialize, list, link, and navigation replies: {replies:?}"
            )
            .into());
        };
        assert_eq!(initialized["result"]["serverInfo"]["name"], "beankeeper");
        let instructions = initialized["result"]["instructions"]
            .as_str()
            .ok_or("instructions")?;
        assert!(instructions.contains("minor units"));
        assert_eq!(
            instructions.contains("setup_status"),
            args.ends_with(&["extension"])
        );
        assert!(!state.join("claude-extension").exists());
        assert_managed_link(tools, link, navigation, base)?;
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn assert_managed_link(tools: &Value, link: &Value, navigation: &Value, base: &str) -> TestResult {
    let tools = tools["result"]["tools"].as_array().ok_or("missing tools")?;
    let advertised = tools
        .iter()
        .find(|tool| tool["name"] == "ui_get_link")
        .ok_or("managed ledger connection omitted ui_get_link")?;
    assert_eq!(advertised["annotations"]["readOnlyHint"], true);
    assert!(tools.iter().any(|tool| tool["name"] == "txn_post"));
    for name in [
        "ui_show_view",
        "ui_highlight",
        "ui_status",
        "txn_approve",
        "txn_reverse",
    ] {
        assert!(!tools.iter().any(|tool| tool["name"] == name));
    }

    assert_eq!(link["result"]["isError"], false, "{link}");
    let detail = &link["result"]["structuredContent"];
    let path = "/reports/income-statement?from=2026-11-01&to=2026-11-30&currency=USD";
    assert_eq!(detail["url"], format!("{base}{path}"));
    assert_eq!(detail["path"], path);
    assert_eq!(detail["view_id"], "income-statement");
    assert_eq!(detail["state"]["from"], "2026-11-01");
    assert_eq!(
        navigation["result"]["structuredContent"]["code"], "FORBIDDEN",
        "{navigation}"
    );
    Ok(())
}

#[cfg(target_os = "linux")]
fn assert_managed_write_modes(
    configured: &impl Fn() -> std::process::Command,
    books: &std::path::Path,
    direct_access: &Value,
) -> TestResult {
    use beankeeper_cli::{
        db::{self, Actor, Db},
        identity,
    };
    use secrecy::{ExposeSecret, SecretString};
    use std::fs;

    let db = Db::open_existing(
        books,
        Some(&SecretString::from(fs::read_to_string(
            books.with_extension("passphrase"),
        )?)),
    )?;
    let owner_file = books.with_extension("owner-token");
    let owner_token = SecretString::from(fs::read_to_string(&owner_file)?);
    let owner = identity::verify(db.read(), owner_token.expose_secret())?;
    let actor = Actor::new("managed-test-owner".parse()?).with_identity(owner.clone());
    create_managed_accounts(&db, &actor)?;
    let agent_file = books.with_extension("agent-token");
    let token = SecretString::from(fs::read_to_string(&agent_file)?);
    let agent = identity::verify(db.read(), token.expose_secret())?;
    let limits = db::spend_limits::list(db.read(), agent.uri(), None)?;
    assert_eq!(
        limits.first().map(db::spend_limits::SpendLimit::agent_uri),
        None
    );

    let bridge = || {
        let mut command = configured();
        command.args(["mcp", "bridge", "--service"]);
        command
    };
    let mut lifecycle = configured();
    lifecycle.args(["service", "mcp"]);
    let saved_owner_file = books.with_extension("owner-token.offline");
    fs::rename(&owner_file, &saved_owner_file)?;
    assert_eq!(
        stdio_tool(lifecycle, "service_status", &json!({}))?["agent_access"],
        *direct_access
    );
    assert_eq!(
        stdio_tool(bridge(), "setup_status", &json!({}))?["agent_access"],
        *direct_access
    );

    let posting = |reference| {
        json!({
            "company": "personal", "date": "2026-11-22", "description": "Managed sale",
            "reference": reference,
            "debits": [{"account": "1000", "amount": "1250.00"}],
            "credits": [{"account": "4000", "amount": "1250.00"}]
        })
    };
    let posted = stdio_tool(bridge(), "txn_post", &posting("managed-direct"))?;
    assert_eq!(posted["created"], true);
    let posted_id = posted["id"].as_i64().ok_or("posted transaction ID")?;
    assert_transaction_actor(&db, posted_id, &agent)?;

    let proposal_credential = issue_sign_off_grant(&db, &actor)?;
    fs::write(&agent_file, proposal_credential.token().expose_secret())?;
    let output = configured()
        .args(["service", "status", "--json"])
        .output()?;
    let status: Value = serde_json::from_slice(&output.stdout)?;
    let access = &status["agent_access"];
    assert_eq!(access["preset"], "bookkeeper");
    assert_eq!(access["write_mode"], "propose-only");
    assert!(
        access["note"]
            .as_str()
            .ok_or("write mode note")?
            .contains("sign-off")
    );
    assert_eq!(
        stdio_tool(bridge(), "setup_status", &json!({}))?["agent_access"],
        *access
    );
    let proposed = stdio_tool(bridge(), "txn_post", &posting("managed-proposal"))?;
    assert_eq!(proposed["status"], "pending");
    assert!(proposed["transaction_id"].is_null());
    assert_eq!(
        proposed["proposer_uri"],
        proposal_credential.agent_uri.as_str()
    );
    let count = || {
        db.read()
            .query_row("SELECT COUNT(*) FROM transactions", [], |row| {
                row.get::<_, i64>(0)
            })
    };
    assert_eq!(count()?, 1);

    fs::rename(saved_owner_file, owner_file)?;
    let mut undo = configured();
    undo.env("BK_AGENT_TOKEN", owner_token.expose_secret())
        .arg("--db")
        .arg(books);
    let reversal_id = reverse_managed_post(undo, posted_id)?;
    assert_ne!(reversal_id, posted_id);
    assert_transaction_actor(&db, reversal_id, &owner)?;
    assert_eq!(count()?, 2);
    Ok(())
}

#[cfg(target_os = "linux")]
fn issue_sign_off_grant(
    db: &beankeeper_cli::db::Db,
    owner: &beankeeper_cli::db::Actor,
) -> Result<beankeeper_cli::identity::IssuedCredential, beankeeper_cli::error::CliError> {
    use beankeeper_cli::{
        capability::Capability,
        identity::{self, Admin, Granted, IssueRequest, WriteMode},
    };

    Granted::<Admin>::with_db(db, owner, |grant| {
        identity::issue_credential_with_mode(
            grant,
            &IssueRequest {
                name: "test_signoff",
                capabilities: &[Capability::Read, Capability::Post, Capability::Propose]
                    .map(|capability| capability.path().to_owned()),
                ttl: Duration::from_secs(3600),
                public_key: None,
                generate_key: true,
            },
            WriteMode::ProposeOnly,
        )
    })
}

#[cfg(target_os = "linux")]
fn create_managed_accounts(
    db: &beankeeper_cli::db::Db,
    actor: &beankeeper_cli::db::Actor,
) -> TestResult {
    use beankeeper::types::AccountType;
    use beankeeper_cli::{
        db,
        identity::{Admin, Granted},
    };

    let company = "personal".parse()?;
    for (code, name, account_type) in [
        ("1000", "Checking", AccountType::Asset),
        ("4000", "Sales", AccountType::Revenue),
    ] {
        Granted::<Admin>::with_db(db, actor, |grant| {
            db::create_account(
                grant,
                &db::CreateAccountParams {
                    company_slug: &company,
                    code,
                    name,
                    account_type,
                    default_tax_category: None,
                },
            )
        })?;
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn reverse_managed_post(mut command: std::process::Command, posted_id: i64) -> TestResult<i64> {
    let output = command
        .args([
            "--company",
            "personal",
            "--json",
            "txn",
            "reverse",
            &posted_id.to_string(),
            "--date",
            "2026-11-22",
        ])
        .output()?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let reversed: Value = serde_json::from_slice(&output.stdout)?;
    Ok(reversed["data"]["id"]
        .as_i64()
        .ok_or("reversal transaction ID")?)
}

#[cfg(target_os = "linux")]
fn assert_transaction_actor(
    db: &beankeeper_cli::db::Db,
    id: i64,
    agent: &beankeeper_cli::identity::VerifiedAgent,
) -> TestResult {
    let (uri, jti): (String, String) = db.read().query_row(
        "SELECT agent_uri, token_jti FROM audit_log
         WHERE company_slug = 'personal' AND entity = 'transaction' AND action = 'create' AND entity_id = ?",
        [id.to_string()],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    assert_eq!(uri, agent.uri());
    assert_eq!(jti, agent.jti());
    Ok(())
}

#[cfg(target_os = "linux")]
fn stdio_tool(
    mut command: std::process::Command,
    name: &str,
    arguments: &Value,
) -> TestResult<Value> {
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let mut input = child.stdin.take().ok_or("stdio input")?;
    let output = child.stdout.take().ok_or("stdio output")?;
    let (send, receive) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(output).lines() {
            if send.send(line).is_err() {
                break;
            }
        }
    });
    let result = (|| -> TestResult<Value> {
        writeln!(
            input,
            "{}",
            json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"managed-write-test","version":"1"}}})
        )?;
        input.flush()?;
        let initialized: Value =
            serde_json::from_str(&receive.recv_timeout(Duration::from_secs(15))??)?;
        assert!(
            initialized["result"]["serverInfo"].is_object(),
            "{initialized}"
        );
        for message in [
            json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
            json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":name,"arguments":arguments}}),
        ] {
            writeln!(input, "{message}")?;
        }
        input.flush()?;
        Ok(serde_json::from_str(
            &receive.recv_timeout(Duration::from_secs(15))??,
        )?)
    })();
    drop(input);
    if result.is_err() {
        let _ = child.kill();
    }
    let output = child.wait_with_output()?;
    let mut reply = result.map_err(|error| {
        format!(
            "{name}: {error}. {}",
            String::from_utf8_lossy(&output.stderr)
        )
    })?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(reply["id"], 2, "{reply}");
    assert_ne!(reply["result"]["isError"], true, "{reply}");
    let data = reply["result"]["structuredContent"].take();
    assert!(!data.is_null(), "{reply}");
    Ok(data)
}

#[cfg(target_os = "linux")]
fn write_stub_manager(bin: &std::path::Path) -> TestResult {
    use std::{fs, os::unix::fs::PermissionsExt};
    // Read-only discovery must work even when systemctl cannot reach a bus.
    fs::write(bin.join("systemctl"), "#!/bin/sh\nexit 1\n")?;
    fs::set_permissions(bin.join("systemctl"), fs::Permissions::from_mode(0o755))?;
    // CI can run these fixture processes as root. The real manager is stubbed.
    fs::write(bin.join("id"), "#!/bin/sh\nprintf '1000\\n'\n")?;
    fs::set_permissions(bin.join("id"), fs::Permissions::from_mode(0o755))?;
    Ok(())
}

#[cfg(target_os = "linux")]
#[test]
fn unusable_session_is_reported_before_managing_services_and_after_agent_registration() -> TestResult
{
    use beankeeper_desktop::setup::keychain::restrict_file;
    use std::{fs, os::unix::net::UnixListener};

    let root = tempfile::tempdir()?;
    let config = root.path().join("config/systemd/user");
    let state = root.path().join("state");
    fs::create_dir_all(&config)?;
    fs::create_dir_all(&state)?;
    let home = root.path().join("home");
    fs::create_dir_all(&home)?;
    let runtime = root.path().join("runtime");
    let command = || {
        let mut command = isolated(root.path());
        command
            .env("HOME", &home)
            .env("CODEX_HOME", home.join("codex"))
            .env("XDG_CONFIG_HOME", root.path().join("config"))
            .env("XDG_RUNTIME_DIR", &runtime)
            .env_remove("DBUS_SESSION_BUS_ADDRESS");
        command
    };
    let install = command()
        .args(["service", "install", "--file-store", "--no-agents"])
        .output()?;
    assert!(!install.status.success());
    let message = String::from_utf8(install.stderr)?;
    for expected in [
        "SESSION_ENV_UNAVAILABLE",
        "XDG_RUNTIME_DIR",
        "DBUS_SESSION_BUS_ADDRESS",
    ] {
        assert!(message.contains(expected));
    }
    assert!(!root.path().join("data/books.db").exists());
    let descriptor = state.join("service.json");
    fs::write(
        &descriptor,
        serde_json::to_vec(&json!({
            "executable": assert_cmd::cargo::cargo_bin!("bk"),
            "books": root.path().join("data/books.db"),
            "port": 8750,
        }))?,
    )?;
    restrict_file(&descriptor)?;
    fs::write(config.join("beankeeper-desktop.service"), [])?;
    for action in ["connect", "doctor"] {
        let output = command()
            .args(["agents", action, "codex", "--json"])
            .output()?;
        let report: Value = serde_json::from_slice(&output.stdout)?;
        assert_eq!(report["data"]["agents"][0]["state"], "configured");
        assert_eq!(
            report["data"]["service"]["error"]["code"],
            "SESSION_ENV_UNAVAILABLE"
        );
    }
    let bridge = command().args(["mcp", "bridge", "--service"]).output()?;
    assert!(!bridge.status.success());
    assert!(String::from_utf8(bridge.stderr)?.contains("SESSION_ENV_UNAVAILABLE"));
    check_lifecycle(command(), "SESSION_ENV_UNAVAILABLE")?;
    fs::create_dir(&runtime)?;
    let _bus = UnixListener::bind(runtime.join("bus"))?;
    assert_stop_clears_stale_runtime(&command, &state)?;
    Ok(())
}

#[cfg(target_os = "linux")]
fn assert_stop_clears_stale_runtime(
    configured: &impl Fn() -> std::process::Command,
    state: &std::path::Path,
) -> TestResult {
    use std::{fs, os::unix::fs::PermissionsExt};
    let runtime = state.join("service-runtime.json");
    fs::write(&runtime, b"descriptor left by a terminated host")?;
    let manager = state.join("systemctl");
    fs::write(
        &manager,
        "#!/bin/sh\ncase \"$1 $2\" in '--user show'|'--user disable'|'--user mask') exit 0;; esac\nexit 1\n",
    )?;
    fs::set_permissions(manager, fs::Permissions::from_mode(0o755))?;
    let output = configured()
        .env("PATH", state)
        .args(["service", "stop"])
        .output()?;
    assert!(output.status.success());
    assert!(!runtime.exists());
    Ok(())
}
