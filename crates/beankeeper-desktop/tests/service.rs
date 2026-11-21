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
    let mut child = isolated(root.path())
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
            let reply: Value =
                serde_json::from_str(&receive.recv_timeout(Duration::from_secs(15))??)?;
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
                    assert_eq!(
                        reply["result"]["structuredContent"]["code"],
                        "SERVICE_MANAGER"
                    );
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
        drop(input);
        Ok(())
    })();
    // Reap on both assertion-independent failure and success paths.
    let _ = child.kill();
    let _ = child.wait();
    result
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
        os::unix::fs::PermissionsExt,
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
    let output = isolated(root.path())
        .arg("--db")
        .arg(&books)
        .args([
            "setup",
            "--non-interactive",
            "--no-start",
            "--no-open",
            "--secret-store-file",
        ])
        .arg(data.join("passphrase"))
        .arg("--config-home")
        .arg(&home)
        .output()?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let executable = std::path::PathBuf::from(assert_cmd::cargo::cargo_bin!("bk"));
    let occupied = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
    fs::write(
        state.join("service.json"),
        serde_json::to_vec(&json!({
            "executable":executable,"books":books,"port":occupied.local_addr()?.port()
        }))?,
    )?;
    restrict_file(&state.join("service.json"))?;
    fs::write(
        config.join("systemd/user/beankeeper-desktop.service"),
        "test registration",
    )?;
    // Only the manager liveness query is replaced. The actual encrypted server,
    // port fallback, authenticated MCP handshake and bridge run unchanged.
    fs::write(
        bin.join("systemctl"),
        "#!/bin/sh\n[ \"$*\" = '--user is-active --quiet beankeeper-desktop.service' ]\n",
    )?;
    fs::set_permissions(bin.join("systemctl"), fs::Permissions::from_mode(0o755))?;
    let configured = || {
        let mut command = isolated(root.path());
        command.env("XDG_CONFIG_HOME", &config).env(
            "PATH",
            format!(
                "{}:{}",
                bin.display(),
                std::env::var("PATH").unwrap_or_default()
            ),
        );
        command
    };
    let server = Server::launch({
        let mut command = configured();
        command.args(["service", "run"]);
        command
    })?;
    assert!(
        !server
            .base
            .ends_with(&occupied.local_addr()?.port().to_string())
    );
    let output = configured()
        .args(["service", "status", "--json"])
        .output()?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let status: Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(status["running"], true);
    assert_eq!(status["version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(status["mcp_endpoint"], format!("{}/mcp", server.base));
    let duplicate = configured().args(["service", "run"]).output()?;
    assert!(!duplicate.status.success());
    assert!(String::from_utf8_lossy(&duplicate.stderr).contains("already running"));
    let runtime_path = state.join("service-runtime.json");
    let runtime = fs::read(&runtime_path)?;
    let mut stale: Value = serde_json::from_slice(&runtime)?;
    stale["pid"] = json!(std::process::id());
    fs::write(&runtime_path, serde_json::to_vec(&stale)?)?;
    let output = configured()
        .args(["service", "status", "--json"])
        .output()?;
    assert_eq!(
        serde_json::from_slice::<Value>(&output.stdout)?["running"],
        false
    );
    fs::write(&runtime_path, &runtime)?;
    // Both stdio launchers discover the same running service. The extension
    // leaves its installation alone and adds its MCP onboarding instructions.
    for args in [
        &["mcp", "bridge", "--service"][..],
        &["mcp", "extension"][..],
    ] {
        let mut bridge = configured()
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()?;
        let mut input = bridge.stdin.take().ok_or("stdin")?;
        writeln!(
            input,
            "{}",
            json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"managed-test","version":"1"}}})
        )?;
        drop(input);
        let output = bridge.wait_with_output()?;
        assert!(output.status.success());
        let initialized: Value = serde_json::from_slice(&output.stdout)?;
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
    }
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
