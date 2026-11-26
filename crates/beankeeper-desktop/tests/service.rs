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
    check_lifecycle(isolated(root.path()), "SERVICE_NOT_INSTALLED", false)
}

#[test]
fn license_mcp_activates_for_selected_books_and_matches_cli_status() -> TestResult {
    use std::fs;
    let root = tempfile::tempdir()?;
    let keys = root.path().join("issuer.json");
    fs::write(&keys, include_bytes!("fixtures/license-public-keys.json"))?;
    fs::File::create(root.path().join("beankeeper.db"))?;
    let command = || {
        let mut command = isolated(root.path());
        command
            .current_dir(root.path())
            .env("BEANKEEPER_LICENSE_KEYS_FILE", &keys);
        command
    };
    let mut pipe = McpPipe::start(command().args(["service", "mcp"]))?;
    let listed = pipe.request("tools/list", &json!({}))?;
    let tools = listed["tools"].as_array().ok_or("missing license tools")?;
    for name in ["license_status", "license_activate", "license_return"] {
        assert_eq!(tools.iter().filter(|tool| tool["name"] == name).count(), 1);
    }
    let status = pipe.tool("license_status", &json!({}))?;
    let cli = command().arg("license").output()?;
    assert!(cli.status.success());
    assert_eq!(status["structuredContent"]["status"], "not_activated");
    assert_eq!(status["structuredContent"]["writable"], true);
    assert_eq!(
        status["structuredContent"]["message"],
        std::str::from_utf8(&cli.stdout)?.trim_end()
    );

    let cli_json = command().args(["--json", "license"]).output()?;
    assert!(cli_json.status.success());
    assert_eq!(
        status["structuredContent"],
        serde_json::from_slice::<Value>(&cli_json.stdout)?
    );

    let token = include_str!("fixtures/license-v4.public.txt").trim_end();
    assert_invalid_license_inputs(&mut pipe, token)?;
    let wrong_keys = root.path().join("different-issuer.json");
    fs::write(
        &wrong_keys,
        br#"{"unknown":"6kpsY-KcUgq-9VB7Ey7F-ZVHdq6-vnuSQh7qaRRG0iw"}"#,
    )?;
    let mut wrong_issuer = McpPipe::start(
        command()
            .env("BEANKEEPER_LICENSE_KEYS_FILE", wrong_keys)
            .args(["service", "mcp"]),
    )?;
    let response = wrong_issuer.tool("license_activate", &json!({"license": token}))?;
    assert_eq!(response["structuredContent"]["code"], "LICENSE_UNVERIFIED");
    assert!(!response.to_string().contains(token));
    assert!(!root.path().join("state").exists());

    let activated = pipe.tool("license_activate", &json!({"license": token}))?;
    let report = &activated["structuredContent"];
    assert_eq!(report["status"], "active");
    assert_eq!(report["licenses"][0]["eligible_major"], 1);
    assert_eq!(report["desktop_path"], "/settings");
    assert!(!activated.to_string().contains(token));
    let status = pipe.tool("license_status", &json!({}))?;
    let cli = command().arg("license").output()?;
    assert!(cli.status.success());
    assert_eq!(status["structuredContent"], *report);
    assert_eq!(
        report["message"],
        std::str::from_utf8(&cli.stdout)?.trim_end()
    );

    let file = root.path().join("saved-license");
    let comp = include_str!("fixtures/license-major-2-v4.public.txt");
    fs::write(&file, comp)?;
    let activated = pipe.tool("license_activate", &json!({"file_path": file}))?;
    assert_eq!(activated["structuredContent"]["status"], "active");
    assert_eq!(
        activated["structuredContent"]["licenses"][0]["eligible_major"],
        2
    );
    assert!(!activated.to_string().contains(comp.trim_end()));

    fs::write(&file, token)?;
    let other_books = root.path().join("other.db");
    fs::File::create(&other_books)?;
    let already_bound = command()
        .arg("--db")
        .arg(&other_books)
        .args(["license", "activate"])
        .arg(&file)
        .output()?;
    assert!(!already_bound.status.success());
    assert!(std::str::from_utf8(&already_bound.stderr)?.contains("already bound"));
    assert!(!std::str::from_utf8(&already_bound.stderr)?.contains(token));
    let failure = command().args(["--json", "license", "activate"]).output()?;
    assert!(!failure.status.success());
    let payload: Value = serde_json::from_slice(&failure.stderr)?;
    assert_eq!(payload["code"], "LICENSE_ARGUMENTS");
    Ok(())
}

#[test]
fn license_return_cli_and_mcp_share_confirmation_and_receipt() -> TestResult {
    use std::{fs, net::TcpListener};
    let root = tempfile::tempdir()?;
    let keys = root.path().join("issuer.json");
    fs::write(&keys, include_bytes!("fixtures/license-public-keys.json"))?;
    let listener = TcpListener::bind("127.0.0.1:0")?;
    listener.set_nonblocking(true)?;
    let origin = format!("http://{}", listener.local_addr()?);
    let (stop, shutdown) = tokio::sync::oneshot::channel();
    let server = std::thread::spawn(move || serve_return_mock(listener, shutdown));
    let command = || {
        let mut command = isolated(root.path());
        command
            .env("BEANKEEPER_LICENSE_KEYS_FILE", &keys)
            .env("BEANKEEPER_LICENSE_SERVICE_URL", &origin);
        command
    };
    let mut pipe = McpPipe::start(command().args(["service", "mcp"]))?;
    let initial = pipe.tool("license_return", &json!({}))?;
    let cli = command().args(["--json", "license", "return"]).output()?;
    assert!(!cli.status.success());
    assert_eq!(
        initial["structuredContent"]["code"],
        serde_json::from_slice::<Value>(&cli.stdout)?["code"]
    );
    assert_eq!(
        initial["structuredContent"]["code"],
        "confirmation_required"
    );
    assert!(
        initial["structuredContent"]["message"]
            .as_str()
            .ok_or("policy missing")?
            .contains("Returns within 60 days of purchase are refunded automatically.")
    );
    assert!(!root.path().join("state").exists());

    let id = "lic_01h455vb4pex5vsknk084sn02q";
    let activation = root.path().join(format!("state/licenses/{id}.license"));
    fs::create_dir_all(activation.parent().ok_or("license parent")?)?;
    fs::write(
        &activation,
        include_bytes!("fixtures/license-v4.public.txt"),
    )?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&activation, fs::Permissions::from_mode(0o600))?;
    }
    let books = root.path().join("books.db");
    fs::write(&books, b"books must not be touched")?;
    let returned = pipe.tool("license_return", &json!({"confirm":true}))?;
    assert_ne!(returned["isError"], true, "{returned}");
    assert_eq!(returned["structuredContent"]["code"], "returned");
    assert!(!activation.exists());
    assert_eq!(fs::read(&books)?, b"books must not be touched");
    // A single installed license is also selected automatically by the CLI.
    fs::write(
        &activation,
        include_bytes!("fixtures/license-v4.public.txt"),
    )?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&activation, fs::Permissions::from_mode(0o600))?;
    }
    let replay = command()
        .args(["--json", "license", "return", "--yes"])
        .output()?;
    assert!(
        replay.status.success(),
        "{}",
        String::from_utf8_lossy(&replay.stderr)
    );
    assert_eq!(
        returned["structuredContent"],
        serde_json::from_slice::<Value>(&replay.stdout)?
    );
    let already = pipe.tool("license_return", &json!({"confirm":true}))?;
    let cli = command()
        .args(["--json", "license", "return", "--yes"])
        .output()?;
    assert!(cli.status.success());
    assert_eq!(
        already["structuredContent"],
        serde_json::from_slice::<Value>(&cli.stdout)?
    );
    assert_eq!(already["structuredContent"]["code"], "already_returned");
    assert_eq!(
        already["structuredContent"]["receipt"]["receipt_id"],
        returned["structuredContent"]["receipt"]["receipt_id"]
    );
    let _ = stop.send(());
    server.join().map_err(|_| "return server panicked")??;
    Ok(())
}

#[test]
fn license_return_empty_and_multiple_match_across_cli_and_mcp() -> TestResult {
    let root = tempfile::tempdir()?;
    let mut pipe = McpPipe::start(isolated(root.path()).args(["service", "mcp"]))?;
    for (count, code) in [(0, "no_license_installed"), (2, "choose_license")] {
        if count == 2 {
            let directory = root.path().join("state/licenses");
            std::fs::create_dir_all(&directory)?;
            for name in ["first.license", "second.license"] {
                std::fs::write(directory.join(name), "not read until selected")?;
            }
        }
        let mcp = pipe.tool("license_return", &json!({"confirm":true}))?;
        let cli = isolated(root.path())
            .args(["--json", "license", "return", "--yes"])
            .output()?;
        assert_eq!(
            mcp["structuredContent"],
            serde_json::from_slice::<Value>(&cli.stdout)?
        );
        assert_eq!(mcp["structuredContent"]["code"], code);
        assert_eq!(cli.status.success(), count == 0);
    }
    Ok(())
}

#[test]
fn license_return_closed_stdin_gives_cli_retry_guidance() -> TestResult {
    let root = tempfile::tempdir()?;
    let output = isolated(root.path())
        .args(["license", "return"])
        .stdin(std::process::Stdio::null())
        .output()?;
    assert!(!output.status.success());
    let stdout = String::from_utf8(output.stdout)?;
    let stderr = String::from_utf8(output.stderr)?;
    assert!(stdout.contains("rerun bk license return --yes"));
    assert!(!stdout.contains("license_return"));
    assert_eq!(
        format!("{stdout}{stderr}")
            .matches("Returns within 60 days")
            .count(),
        1
    );
    Ok(())
}

#[test]
fn license_return_elicits_once_and_decline_keeps_local_state() -> TestResult {
    let root = tempfile::tempdir()?;
    let mut pipe = McpPipe::spawn(isolated(root.path()).args(["service", "mcp"]))?;
    pipe.request(
        "initialize",
        &json!({
            "protocolVersion":"2025-11-25", "capabilities":{"elicitation":{"form":{}}},
            "clientInfo":{"name":"return-elicitation-test","version":"1"}
        }),
    )?;
    writeln!(
        pipe.input,
        "{}",
        json!({"jsonrpc":"2.0","method":"notifications/initialized"})
    )?;
    pipe.id += 1;
    let request_id = pipe.id;
    writeln!(
        pipe.input,
        "{}",
        json!({"jsonrpc":"2.0","id":request_id,
        "method":"tools/call","params":{"name":"license_return","arguments":{}}})
    )?;
    pipe.input.flush()?;
    let elicitation: Value =
        serde_json::from_str(&pipe.frames.recv_timeout(Duration::from_secs(20))??)?;
    assert_eq!(elicitation["method"], "elicitation/create");
    assert!(
        elicitation["params"]["message"]
            .as_str()
            .ok_or("missing policy")?
            .contains("Confirming requests the return and refund.")
    );
    assert_eq!(
        elicitation["params"]["requestedSchema"]["properties"]["confirm"]["type"],
        "boolean"
    );
    writeln!(
        pipe.input,
        "{}",
        json!({"jsonrpc":"2.0","id":elicitation["id"],
        "result":{"action":"decline"}})
    )?;
    pipe.input.flush()?;
    let result: Value = serde_json::from_str(&pipe.frames.recv_timeout(Duration::from_secs(20))??)?;
    assert_eq!(result["id"], request_id);
    assert_eq!(
        result["result"]["structuredContent"]["code"],
        "confirmation_required"
    );
    assert!(!root.path().join("state").exists());
    Ok(())
}

fn serve_return_mock(
    listener: std::net::TcpListener,
    shutdown: tokio::sync::oneshot::Receiver<()>,
) -> std::io::Result<()> {
    use axum::{Json, Router, routing::post};
    tokio::runtime::Builder::new_current_thread().enable_all().build()?.block_on(async {
        let token = include_str!("fixtures/license-v4.public.txt").trim_end();
        let challenge = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
        let app = Router::new()
            .route("/api/v1/licenses/return/challenge", post(move |Json(body): Json<String>| async move {
                assert_eq!(body.trim_end(), token);
                Json(json!({"challenge":challenge,"expires_at":"2099-01-01T00:00:00Z"}))
            }))
            .route("/api/v1/licenses/return", post(move |Json(body): Json<Value>| async move {
                assert_eq!(body["license"].as_str().map(str::trim_end), Some(token));
                assert_eq!(body["challenge"], challenge);
                Json(json!({"receipt_id":"ret_01h455vb4pex5vsknk084sn02q","returned_at":"2026-11-26T00:00:00Z","storefront":"polar","refund_status":"pending"}))
            }));
        axum::serve(tokio::net::TcpListener::from_std(listener)?, app)
            .with_graceful_shutdown(async { let _ = shutdown.await; }).await
    })
}

fn assert_invalid_license_inputs(pipe: &mut McpPipe, token: &str) -> TestResult {
    let tampered = token.replacen("v4.public.eyJ", "v4.public.fyJ", 1);
    for (input, code) in [
        (json!({}), "LICENSE_ARGUMENTS"),
        (
            json!({"license": token, "claim_code": "different"}),
            "LICENSE_ARGUMENTS",
        ),
        (
            json!({"license": token, "recovery_token": "unexpected"}),
            "LICENSE_ARGUMENTS",
        ),
        (json!({"claim_code": "short"}), "LICENSE_INVALID_CLAIM_CODE"),
        (
            json!({"license": "v4.public.invalid.token"}),
            "LICENSE_INVALID",
        ),
        (json!({"license": tampered}), "LICENSE_UNVERIFIED"),
    ] {
        let failed = pipe.tool("license_activate", &input)?;
        assert_eq!(failed["isError"], true);
        assert_eq!(failed["structuredContent"]["code"], code);
        assert!(!failed.to_string().contains(token));
        assert!(!failed.to_string().contains(&tampered));
    }
    Ok(())
}

#[test]
fn license_mcp_recovers_a_claim_with_the_same_private_credential() -> TestResult {
    use std::{fs, net::TcpListener};

    let root = tempfile::tempdir()?;
    let issuer = root.path().join("issuer.json");
    fs::write(&issuer, include_bytes!("fixtures/license-public-keys.json"))?;
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let origin = format!("http://{}", listener.local_addr()?);
    listener.set_nonblocking(true)?;

    let (requests, observed) = mpsc::channel::<Value>();
    let (stop, shutdown) = tokio::sync::oneshot::channel::<()>();
    let server = std::thread::spawn(move || serve_license_mock(listener, requests, shutdown));
    let mut command = isolated(root.path());
    command
        .current_dir(root.path())
        .env("BEANKEEPER_LICENSE_KEYS_FILE", &issuer)
        .env("BEANKEEPER_LICENSE_SERVICE_URL", origin)
        .env_remove("BEANKEEPER_LICENSE_RECOVERY_TOKEN_FILE");
    let mut pipe = McpPipe::start(command.args(["service", "mcp"]))?;

    let claim = "0123456789abcdefghijklmnopqrstuv";
    let activated = pipe.tool("license_activate", &json!({"claim_code":claim}))?;
    assert_eq!(activated["structuredContent"]["status"], "active");
    let first = observed.recv_timeout(Duration::from_secs(2))?;
    let recovered = observed.recv_timeout(Duration::from_secs(2))?;
    assert_eq!(first["storefront"], "etsy");
    assert_eq!(first["key"], claim);
    let credential = first["recovery_token"]
        .as_str()
        .ok_or("missing recovery credential")?;
    assert!(credential.len() >= 32);
    assert_ne!(credential, claim);
    assert_eq!(recovered, json!({"recovery_token":credential}));
    assert!(!activated.to_string().contains(credential));
    assert!(!activated.to_string().contains(claim));
    assert!(
        !activated
            .to_string()
            .contains(include_str!("fixtures/license-v4.public.txt").trim_end())
    );

    let mut files = fs::read_dir(root.path().join("state/license-recovery"))?;
    let file = files.next().ok_or("missing private recovery file")??;
    assert!(files.next().is_none());
    assert_eq!(fs::read_to_string(file.path())?, credential);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(file.metadata()?.permissions().mode() & 0o777, 0o600);
    }
    assert!(root.path().join("state/licenses").is_dir());
    let checkout = "cs_live_mock_checkout";
    let stripe = pipe.tool("license_activate", &json!({"claim_code":checkout}))?;
    assert_eq!(stripe["structuredContent"]["status"], "active");
    assert!(!stripe.to_string().contains(checkout));
    let stripe_request = observed.recv_timeout(Duration::from_secs(2))?;
    assert_eq!(
        stripe_request,
        json!({"storefront": "stripe", "key": checkout, "recovery_token": null})
    );
    let _ = stop.send(());
    server
        .join()
        .map_err(|_| "mock license server panicked")??;
    Ok(())
}

fn serve_license_mock(
    listener: std::net::TcpListener,
    requests: mpsc::Sender<Value>,
    shutdown: tokio::sync::oneshot::Receiver<()>,
) -> std::io::Result<()> {
    use axum::{Json, Router, extract::State, http::StatusCode, routing::post};

    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?
        .block_on(async {
            let app =
                    Router::new()
                        .route(
                            "/api/v1/licenses/redeem",
                            post(
                                |State(seen): State<mpsc::Sender<Value>>,
                                 Json(body): Json<Value>| async move {
                                    let stripe = body["storefront"] == "stripe";
                                    let _ = seen.send(body);
                                    if stripe {
                                        (
                                            StatusCode::OK,
                                            include_str!("fixtures/license-v4.public.txt"),
                                        )
                                    } else {
                                        (StatusCode::CONFLICT, "")
                                    }
                                },
                            ),
                        )
                        .route(
                            "/api/v1/licenses/recover",
                            post(
                                |State(seen): State<mpsc::Sender<Value>>,
                                 Json(body): Json<Value>| async move {
                                    let _ = seen.send(body);
                                    include_str!("fixtures/license-v4.public.txt")
                                },
                            ),
                        )
                        .with_state(requests);
            axum::serve(tokio::net::TcpListener::from_std(listener)?, app)
                .with_graceful_shutdown(async {
                    let _ = shutdown.await;
                })
                .await
        })
}

fn check_lifecycle(
    mut command: std::process::Command,
    start_error: &str,
    installed: bool,
) -> TestResult {
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
        let instructions = initialize["result"]["instructions"]
            .as_str()
            .ok_or("missing lifecycle MCP instructions")?;
        assert!(instructions.contains("“let my agent post” in chat"));
        assert!(instructions.contains("access_set"));
        assert!(instructions.contains("Restarting cannot grant posting access"));
        // This notification has no reply; send it independently.
        writeln!(
            input,
            "{}",
            json!({"jsonrpc":"2.0","method":"notifications/initialized"})
        )?;
        input.flush()?;
        check_lifecycle_tools(&mut input, &receive, start_error, installed)?;
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
    installed: bool,
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
        (
            6,
            "tools/call",
            json!({"name":"service_install","arguments":{"file_store":"yes"}}),
        ),
        (
            7,
            "tools/call",
            json!({"name":"service_open","arguments":{}}),
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
                let tools = reply["result"]["tools"].as_array().ok_or("missing tools")?;
                let install = tools
                    .iter()
                    .find(|tool| tool["name"] == "service_install")
                    .ok_or("missing install schema")?;
                let schema = &install["inputSchema"];
                assert_eq!(schema["additionalProperties"], false);
                for name in ["file_store", "no_agents"] {
                    assert_eq!(schema["properties"][name]["type"], "boolean");
                    assert_eq!(schema["properties"][name]["default"], false);
                }
            }
            3 => {
                let status = &reply["result"]["structuredContent"];
                assert_eq!(status["installed"], installed);
                assert_eq!(status["running"], false);
                if installed {
                    assert_eq!(status["error_code"], "SERVICE_NOT_RUNNING");
                }
            }
            4 => {
                assert_eq!(reply["result"]["isError"], true);
                assert_eq!(reply["result"]["structuredContent"]["code"], start_error);
            }
            5 | 6 => {
                assert_eq!(reply["result"]["isError"], true);
                assert_eq!(
                    reply["result"]["structuredContent"]["code"],
                    "SERVICE_ARGUMENTS"
                );
            }
            7 => {
                let code = if installed {
                    "SERVICE_NOT_RUNNING"
                } else {
                    "SERVICE_NOT_INSTALLED"
                };
                assert_eq!(reply["result"]["isError"], true);
                assert_eq!(reply["result"]["structuredContent"]["code"], code);
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

#[test]
fn extension_discovers_setup_in_an_empty_home_and_rejects_model_supplied_commands() -> TestResult {
    let root = tempfile::tempdir()?;
    let mut command = isolated(root.path());
    command.args(["mcp", "extension"]);
    let mut pipe = McpPipe::start(&mut command)?;
    let listed = pipe.request("tools/list", &json!({}))?;
    let tools = listed["tools"].as_array().ok_or("missing setup tools")?;
    assert_eq!(
        tools.iter().map(|tool| &tool["name"]).collect::<Vec<_>>(),
        [
            "setup",
            "setup_status",
            "doctor",
            "agents_extension",
            "update_check",
            "license_status",
            "license_activate",
            "license_return",
            "license_remove",
        ]
    );
    for tool in tools {
        assert_eq!(tool["inputSchema"]["additionalProperties"], false);
    }
    let status = pipe.tool("setup_status", &json!({}))?;
    assert_eq!(status["structuredContent"]["state"], "not_started");
    let cli = isolated(root.path())
        .args(["--format", "json", "service", "extension", "status"])
        .output()?;
    assert!(cli.status.success());
    assert_eq!(
        serde_json::from_slice::<Value>(&cli.stdout)?,
        status["structuredContent"]
    );
    let rejected = pipe.tool("setup", &json!({"command":"fixture", "url":"fixture"}))?;
    assert_eq!(rejected["structuredContent"]["code"], "SERVICE_ARGUMENTS");
    let unavailable = pipe.tool("demo_start", &json!({}))?;
    assert_eq!(
        unavailable["structuredContent"]["code"],
        "EXTENSION_SETUP_REQUIRED"
    );
    assert!(!root.path().join("state").exists());
    assert!(!root.path().join("data").exists());
    Ok(())
}

#[test]
fn extension_diagnostics_match_cli_and_mcp_without_cosign_or_a_ledger() -> TestResult {
    use std::fs;
    let root = tempfile::tempdir()?;
    let profile = root.path().join("claude");
    fs::create_dir(&profile)?;
    let registry = profile.join("extensions-installations.json");
    for version in [None, Some("0.0.0"), Some(env!("CARGO_PKG_VERSION"))] {
        let entries = version.map_or_else(
            || json!({}),
            |version| {
                json!({
                    "local.mcpb.govcraft.beankeeper": {"version":version}
                })
            },
        );
        let bytes = serde_json::to_vec(&json!({"extensions":entries}))?;
        fs::write(&registry, &bytes)?;
        let command = || {
            let mut command = isolated(root.path());
            command
                .env("CLAUDE_USER_DATA_DIR", &profile)
                .env("BEANKEEPER_NO_UPDATE_CHECK", "1");
            command
        };
        let output = command().args(["doctor", "--json"]).output()?;
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let cli: Value = serde_json::from_slice(&output.stdout)?;
        let state = if version.is_none() {
            "not_installed"
        } else if version == Some("0.0.0") {
            "behind"
        } else {
            "current"
        };
        assert_eq!(cli["extension"]["state"], state);
        let output = command().args(["update-check", "--json"]).output()?;
        assert!(output.status.success());
        let updates: Value = serde_json::from_slice(&output.stdout)?;
        assert_eq!(cli["extension"], updates["extension"]);
        assert_eq!(updates["desktop_updates"]["state"], "disabled");
        for args in [["service", "mcp"], ["mcp", "extension"]] {
            let mut pipe = McpPipe::start(command().args(args))?;
            let listed = pipe.request("tools/list", &json!({}))?;
            for name in ["doctor", "update_check"] {
                assert_eq!(
                    listed["tools"]
                        .as_array()
                        .ok_or("missing tools")?
                        .iter()
                        .filter(|tool| tool["name"] == name)
                        .count(),
                    1
                );
                let report = pipe.tool(name, &json!({}))?;
                assert_eq!(report["structuredContent"]["extension"], cli["extension"]);
                if name == "update_check" {
                    assert_eq!(
                        report["structuredContent"]["desktop_updates"],
                        updates["desktop_updates"]
                    );
                }
            }
        }
        assert_eq!(fs::read(&registry)?, bytes);
        assert!(!root.path().join("data").exists());
    }
    Ok(())
}

#[test]
fn local_mcp_connections_advertise_extension_install_and_reject_custom_download_sources()
-> TestResult {
    let root = tempfile::tempdir()?;
    for args in [["service", "mcp"], ["mcp", "extension"]] {
        let mut command = isolated(root.path());
        command.args(args);
        let mut pipe = McpPipe::start(&mut command)?;
        let listed = pipe.request("tools/list", &json!({}))?;
        let tool = listed["tools"]
            .as_array()
            .and_then(|tools| tools.iter().find(|tool| tool["name"] == "agents_extension"))
            .ok_or("missing agents_extension")?;
        assert_eq!(tool["inputSchema"]["additionalProperties"], false);
        assert_eq!(
            tool["inputSchema"]["properties"]["path_only"]["type"],
            "boolean"
        );
        assert_eq!(
            tool["inputSchema"]["properties"]["path_only"]["default"],
            false
        );
        for args in [
            json!({"url": "https://example.invalid/file.mcpb"}),
            json!({"path_only": "true"}),
        ] {
            let reply = pipe.tool("agents_extension", &args)?;
            assert_eq!(reply["isError"], true);
            assert_eq!(
                reply["structuredContent"]["code"],
                "AGENT_EXTENSION_ARGUMENTS"
            );
        }
    }
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
    provision_through_lifecycle_mcp(configured(), &home, &books)?;
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
fn provision_through_lifecycle_mcp(
    mut command: std::process::Command,
    home: &std::path::Path,
    books: &std::path::Path,
) -> TestResult {
    command
        .env("DBUS_SESSION_BUS_ADDRESS", "unix:path=/stub-systemd-bus")
        .env("CODEX_HOME", home.join("codex"))
        .args(["service", "mcp"]);
    let install = stdio_tool_response(
        command,
        "service_install",
        &json!({"file_store":true,"no_agents":true}),
    )?;
    assert_eq!(install["isError"], true, "{install}");
    assert_eq!(install["structuredContent"]["code"], "SERVICE_MANAGER");
    assert!(
        install["structuredContent"]["message"]
            .as_str()
            .is_some_and(|message| message.contains("OS service manager refused"))
    );
    assert!(books.with_extension("passphrase").is_file());
    assert_eq!(std::fs::read_dir(home)?.count(), 0);
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
        .args(["agents", "connect", "chatgpt-codex", "--json"])
        .output()?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    let output = agent_command()
        .args(["agents", "doctor", "chatgpt-codex", "--json"])
        .output()?;
    let diagnostic: Value = serde_json::from_slice(&output.stdout)?;
    assert!(
        output.status.success(),
        "{diagnostic}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let agent = &diagnostic["data"]["agents"][0];
    assert_eq!(agent["state"], "configured");
    assert_eq!(agent["skill_installed"], true);
    assert_eq!(diagnostic["data"]["service"]["state"], "ready");
    assert_eq!(&diagnostic["data"]["service"]["endpoint"], endpoint);
    let config = agent["config"].as_str().ok_or("missing Codex config")?;
    let document: toml_edit::DocumentMut = std::fs::read_to_string(config)?.parse()?;
    let servers = &document["mcp_servers"];
    let mut ledger = McpPipe::start(&mut registered_stdio(
        agent_command(),
        &servers["beankeeper"],
    )?)?;
    let listed = ledger.request("tools/list", &json!({}))?;
    let tools = listed["tools"].as_array().ok_or("missing ledger tools")?;
    for name in [
        "setup_status",
        "txn_post",
        "txn_recategorize",
        "audit_undo",
        "ui_get_link",
    ] {
        assert!(
            tools.iter().any(|tool| tool["name"] == name),
            "missing {name}"
        );
    }
    let companies = ledger.tool("company_list", &json!({}))?;
    assert_ne!(companies["isError"], true, "{companies}");
    let setup = ledger.tool("setup_status", &json!({}))?;
    assert_ne!(setup["isError"], true, "{setup}");
    assert_eq!(
        demo_payload(&setup)?["agent_access"]["write_mode"],
        "direct"
    );
    assert_codex_demo_workflow(&mut ledger)?;
    assert_eq!(
        demo_payload(&ledger.tool("company_list", &json!({}))?)?,
        demo_payload(&companies)?
    );
    let link = ledger.tool(
        "ui_get_link",
        &json!({"view_id":"income-statement","state":{
            "from":"2026-11-01","to":"2026-11-30","currency":"USD"
        }}),
    )?;
    assert_eq!(
        demo_payload(&link)?["path"],
        "/reports/income-statement?from=2026-11-01&to=2026-11-30&currency=USD"
    );
    let mut lifecycle = McpPipe::start(&mut registered_stdio(
        agent_command(),
        &servers["beankeeper-service"],
    )?)?;
    let service = lifecycle.tool("service_status", &json!({}))?;
    assert_eq!(service["structuredContent"]["mcp_endpoint"], *endpoint);
    Ok(())
}

#[cfg(target_os = "linux")]
fn assert_codex_demo_workflow(ledger: &mut McpPipe) -> TestResult {
    let demo = ledger.tool("demo_start", &json!({}))?;
    assert_eq!(demo_payload(&demo)?["state"], "active", "{demo}");
    let mut tool = |name, args: Value| {
        let response = ledger.tool(name, &args)?;
        assert_ne!(response["isError"], true, "{name}: {response}");
        demo_payload(&response)
    };
    let posted = tool(
        "txn_post",
        json!({
            "company":"personal", "reference":"codex-first-run", "description":"Practice purchase",
            "date":"2026-11-20", "currency":"USD",
            "debits":[{"account":"5200","amount":"12.50"}],
            "credits":[{"account":"1000","amount":"12.50"}]
        }),
    )?;
    assert_eq!(posted["created"], true, "{posted}");
    let moved = tool(
        "txn_recategorize",
        json!({
            "company":"personal", "reference":"codex-first-run", "posting_reference":"codex-category",
            "from_account":"5200", "to_account":"5100"
        }),
    )?;
    assert_eq!(moved["original_id"], posted["id"]);
    assert_eq!(moved["amount"], 1250);
    let receipt = moved["receipt_id"]
        .as_i64()
        .ok_or("missing category receipt")?;
    let undo = tool(
        "audit_undo",
        json!({"company":"personal","receipt":receipt}),
    )?;
    assert_eq!(undo["undo_of"], receipt);
    assert_eq!(undo["kind"], "reversed");
    let summary = tool(
        "financial_summary",
        json!({"company":"personal","to":"2026-11-30"}),
    )?;
    assert!(
        summary["data"]["comparison"]["series"].is_array(),
        "{summary}"
    );
    let removed = tool("demo_remove", json!({}))?;
    assert_eq!(removed["state"], "removed");
    Ok(())
}

#[cfg(target_os = "linux")]
fn registered_stdio(
    mut command: std::process::Command,
    entry: &toml_edit::Item,
) -> TestResult<std::process::Command> {
    let executable = entry["command"].as_str().ok_or("missing stdio command")?;
    assert_eq!(command.get_program(), executable);
    assert_eq!(
        entry.as_table_like().ok_or("missing server table")?.len(),
        2
    );
    for argument in entry["args"].as_array().ok_or("missing stdio arguments")? {
        command.arg(argument.as_str().ok_or("invalid stdio argument")?);
    }
    Ok(command)
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
    // Both stdio launchers give the same access guidance alongside the
    // upstream ledger instructions.
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
        assert_eq!(
            initialized["result"]["capabilities"]["extensions"][rmcp::model::SKILLS_EXTENSION_ID],
            json!({})
        );
        let instructions = initialized["result"]["instructions"]
            .as_str()
            .ok_or("instructions")?;
        assert!(instructions.contains("minor units"));
        assert!(instructions.contains("setup_status"));
        assert!(instructions.contains("Restarting cannot grant posting access"));
        assert!(instructions.contains("“let my agent post” in chat"));
        assert!(instructions.contains("access_set"));
        assert!(instructions.contains("Respect the owner's choice to stay read-only"));
        assert!(!instructions.contains("docs_url"));
        assert!(!state.join("claude-extension").exists());
        assert_managed_link(tools, link, navigation, base)?;
        assert_managed_skill_discovery(configured().args(args))?;
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn assert_managed_skill_discovery(command: &mut std::process::Command) -> TestResult {
    use rmcp::model::SKILLS_EXTENSION_ID;

    let mut pipe = McpPipe::spawn(command)?;
    let discovered = pipe.request("server/discover", &inline_params(json!({})))?;
    assert_eq!(
        discovered["capabilities"]["extensions"][SKILLS_EXTENSION_ID],
        json!({})
    );
    assert_eq!(discovered["capabilities"]["resources"], json!({}));
    let listed = pipe.request("skills/list", &inline_params(json!({})))?;
    let uri = &listed["skills"][0]["uri"];
    assert_eq!(uri, "skill://beankeeper-desktop/SKILL.md");
    let resources = pipe.request("resources/read", &inline_params(json!({"uri":uri})))?;
    assert_eq!(
        resources["contents"][0]["text"],
        include_str!("../../../SKILL.md")
    );
    let list = pipe.request("tools/list", &inline_params(json!({})))?;
    let tools = list["tools"].as_array().ok_or("missing managed tools")?;
    assert!(
        tools.iter().any(|tool| tool["name"] == "company_list"),
        "{list}"
    );
    assert!(
        tools.iter().any(|tool| tool["name"] == "setup_status"),
        "{list}"
    );
    for name in ["license_status", "license_activate", "license_return"] {
        assert_eq!(tools.iter().filter(|tool| tool["name"] == name).count(), 1);
    }
    let license = pipe.request(
        "tools/call",
        &inline_params(json!({"name":"license_status","arguments":{}})),
    )?;
    let license = &license["structuredContent"];
    assert_eq!(license["status"], "not_activated");
    assert_eq!(license["writable"], true);
    assert!(
        license["settings_url"]
            .as_str()
            .is_some_and(|url| url.ends_with("/settings"))
    );
    let companies = pipe.request(
        "tools/call",
        &inline_params(json!({"name":"company_list","arguments":{}})),
    )?;
    assert_ne!(companies["isError"], true, "{companies}");
    assert_eq!(companies["content"][0]["type"], "text");
    assert_inline_demo_target(&mut pipe, &companies)?;
    Ok(())
}

#[cfg(target_os = "linux")]
fn assert_inline_demo_target(pipe: &mut McpPipe, original_companies: &Value) -> TestResult {
    use beankeeper_cli::mcp::LEDGER_TARGET_META_KEY;

    let transactions = inline_params(json!({"name":"txn_list","arguments":{"company":"personal"}}));
    let original_transactions = demo_payload(&pipe.request("tools/call", &transactions)?)?;
    let started = pipe.request("tools/call", &inline_params(json!({"name":"demo_start"})))?;
    assert_eq!(started["structuredContent"]["state"], "active", "{started}");
    let target = &started["structuredContent"]["target"];
    let request = inline_params(json!({"name":"company_list"}));
    let demo = pipe.request("tools/call", &request)?;
    assert_eq!(demo["_meta"][LEDGER_TARGET_META_KEY], *target);
    assert_ne!(demo_payload(&demo)?, demo_payload(original_companies)?);

    let post = inline_params(json!({"name":"txn_post","arguments":demo_posting()}));
    let posted = pipe.request("tools/call", &post)?;
    assert_eq!(posted["_meta"][LEDGER_TARGET_META_KEY], *target);
    assert_eq!(demo_payload(&posted)?["created"], true, "{posted}");

    let mut demo_request = request.clone();
    demo_request["_meta"][LEDGER_TARGET_META_KEY] = json!("missing-demo");
    let missing = pipe.request("tools/call", &demo_request)?;
    assert_eq!(missing["structuredContent"]["code"], "DEMO_UNAVAILABLE");
    assert_eq!(missing["_meta"][LEDGER_TARGET_META_KEY], "missing-demo");
    demo_request["_meta"][LEDGER_TARGET_META_KEY] = target.clone();
    let explicit = pipe.request("tools/call", &demo_request)?;
    assert_eq!(explicit["_meta"][LEDGER_TARGET_META_KEY], *target);

    let removed = pipe.request("tools/call", &inline_params(json!({"name":"demo_remove"})))?;
    assert_eq!(removed["structuredContent"]["state"], "removed");
    let startup = pipe.request("tools/call", &request)?;
    assert!(startup["_meta"].get(LEDGER_TARGET_META_KEY).is_none());
    assert_eq!(demo_payload(&startup)?, demo_payload(original_companies)?);
    let startup = pipe.request("tools/call", &transactions)?;
    assert!(startup["_meta"].get(LEDGER_TARGET_META_KEY).is_none());
    assert_eq!(demo_payload(&startup)?, original_transactions);
    let stale = pipe.request("tools/call", &demo_request)?;
    assert_eq!(stale["structuredContent"]["code"], "DEMO_UNAVAILABLE");
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
    for name in ["ui_show_view", "ui_highlight", "ui_status", "txn_approve"] {
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
    let direct_note = direct_access["note"].as_str().ok_or("direct access note")?;
    assert!(direct_note.contains("audit_undo by receipt ID or txn_reverse by transaction ID"));
    assert!(direct_note.contains("Desktop links only for viewing"));
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
    let proposal_note = access["note"].as_str().ok_or("propose-only access note")?;
    assert!(proposal_note.contains("audit_undo by receipt ID or txn_reverse by transaction ID"));
    assert!(proposal_note.contains("Undo and reversal also return pending proposals"));
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
    let count = || beankeeper_cli::commands::setup::status(&db).map(|status| status.transactions);
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
    use beankeeper_cli::db::audit::{AuditAction, AuditEntity, ListAuditParams, list_audit};
    let rows = list_audit(
        db.read(),
        &ListAuditParams {
            company_slug: Some(&"personal".parse()?),
            entity: Some(AuditEntity::Transaction),
            action: Some(AuditAction::Create),
            ..common::audit_query()
        },
    )?;
    let id = id.to_string();
    let row = rows
        .iter()
        .find(|row| row.entity_id == id)
        .ok_or("posting audit row")?;
    assert_eq!(row.agent_uri.as_deref(), Some(agent.uri()));
    assert_eq!(row.token_jti.as_deref(), Some(agent.jti()));
    Ok(())
}

#[cfg(target_os = "linux")]
fn stdio_tool(command: std::process::Command, name: &str, arguments: &Value) -> TestResult<Value> {
    let mut reply = stdio_tool_response(command, name, arguments)?;
    assert_ne!(reply["isError"], true, "{reply}");
    let data = reply["structuredContent"].take();
    assert!(!data.is_null(), "{reply}");
    Ok(data)
}

#[cfg(target_os = "linux")]
fn stdio_tool_response(
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
    Ok(reply["result"].take())
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
    check_lifecycle(command(), "SESSION_ENV_UNAVAILABLE", true)?;
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

#[cfg(target_os = "linux")]
struct LegacyService {
    root: tempfile::TempDir,
    books: std::path::PathBuf,
}

#[cfg(target_os = "linux")]
impl LegacyService {
    fn new() -> TestResult<Self> {
        Self::with_port(8750)
    }

    fn with_port(port: u16) -> TestResult<Self> {
        use beankeeper_desktop::setup::keychain::{restrict_directory, restrict_file};
        use std::fs;

        let root = tempfile::tempdir()?;
        let books = root.path().join("data/books.db");
        let fixture = Self { root, books };
        let root = fixture.root.path();
        let home = root.join("home");
        let state = root.join("state");
        let definition = root.join("config/systemd/user/beankeeper-desktop.service");
        let data = root.join("data");
        fs::create_dir_all(&data)?;
        restrict_directory(&data)?;
        fs::create_dir_all(&home)?;
        fs::create_dir_all(&state)?;
        fs::create_dir_all(definition.parent().ok_or("service definition parent")?)?;
        let output = fixture
            .command()
            .arg("--db")
            .arg(&fixture.books)
            .args([
                "setup",
                "--preset",
                "read-only",
                "--non-interactive",
                "--no-start",
                "--no-open",
                "--secret-store-file",
            ])
            .arg(fixture.books.with_extension("passphrase"))
            .output()?;
        assert!(output.status.success(), "cannot provision upgrade fixture");

        // rc.12 issued the same read-only Desktop bootstrap grant and used
        // this registration format. It had no per-credential provenance file.
        fs::remove_file(fixture.books.with_extension("agent-access.json"))?;
        let config = state.join("service.json");
        fs::write(
            &config,
            serde_json::to_vec(&json!({
                "books": fixture.books,
                "port": port,
                "executable": env!("CARGO_BIN_EXE_bk"),
            }))?,
        )?;
        restrict_file(&config)?;
        fs::write(&definition, "fixture managed registration")?;
        Ok(fixture)
    }

    fn command(&self) -> std::process::Command {
        let root = self.root.path();
        let mut command = without_session(root);
        command
            .env("HOME", root.join("home"))
            .env("CODEX_HOME", root.join("home/codex"))
            .env("XDG_CONFIG_HOME", root.join("config"));
        command
    }

    fn host(&self) -> TestResult<common::Server> {
        let mut command = self.command();
        command.args(["service", "run"]);
        common::Server::launch(command)
    }

    fn bridge(&self) -> std::process::Command {
        let mut command = self.command();
        command.args(["mcp", "bridge", "--service"]);
        command
    }

    fn secret(&self, extension: &str) -> TestResult<secrecy::SecretString> {
        Ok(std::fs::read_to_string(self.books.with_extension(extension))?.into())
    }

    fn db(&self) -> TestResult<beankeeper_cli::db::Db> {
        Ok(beankeeper_cli::db::Db::open_existing(
            &self.books,
            Some(&self.secret("passphrase")?),
        )?)
    }

    fn service_access(&self, arguments: &[&str]) -> TestResult<Value> {
        let output = self.command().arg("service").args(arguments).output()?;
        assert!(output.status.success(), "service access command failed");
        Ok(serde_json::from_slice(&output.stdout)?)
    }
}

#[cfg(target_os = "linux")]
#[test]
fn managed_open_pairs_in_the_default_browser_and_restores_a_detail_link() -> TestResult {
    use common::rpc;
    use secrecy::ExposeSecret;
    use std::net::TcpListener;

    // Hold the configured port throughout the test to force a fallback.
    let occupied = TcpListener::bind("127.0.0.1:0")?;
    let preferred = occupied.local_addr()?;
    let fixture = LegacyService::with_port(preferred.port())?;
    let mut host = fixture.host()?;
    assert_ne!(host.base, format!("http://{preferred}"));
    assert!(host.pairing_url.is_none());
    let reader = fixture.secret("agent-token")?;
    let bearer = reader.expose_secret().trim();
    host.initialize(Some(bearer))?;

    let status = fixture.service_access(&["status", "--json"])?;
    let desktop = status["desktop_url"].as_str().ok_or("Desktop URL")?;
    assert_eq!(desktop, format!("{}/", host.base));
    assert_eq!(host.client.get(desktop).send()?.status(), 401);
    let detail = rpc(host.request(
        "tools/call",
        json!({"name":"ui_get_link","arguments":{
            "view_id":"transactions","state":{"company":"personal","payee":"Café & Tea / 100%"}
        }}),
        Some(bearer),
    )?)?;
    let view = &detail["result"]["structuredContent"];
    let url = view["url"].as_str().ok_or("detail URL")?;
    let path = view["path"].as_str().ok_or("detail path")?;
    assert_eq!(url, format!("{}{path}", host.base));
    assert!(path.contains("Caf%C3%A9%20%26%20Tea%20%2F%20100%25"));
    let response = host.client.get(url).send()?;
    assert_eq!(response.status(), 401);
    let pending = response.headers()["set-cookie"]
        .to_str()?
        .split(';')
        .next()
        .ok_or("return cookie")?
        .to_owned();
    let html = response.text()?;
    assert!(html.contains("Ask your assistant to open Beankeeper Desktop"));
    assert!(!html.contains("terminal"));

    let (cli_link, extension_link) = check_browser_launcher(&fixture, &host.base)?;
    assert_eq!(host.client.get(&extension_link).send()?.status(), 200);
    let token = extension_link.split_once('=').ok_or("browser token")?.1;
    let pair = || {
        host.client
            .post(format!("{}/pair", host.base))
            .header("Origin", &host.base)
            .header("Cookie", &pending)
            .form(&[("token", token)])
            .send()
    };
    let paired = pair()?;
    assert_eq!(paired.status(), 303);
    assert_eq!(paired.headers()["location"], path);
    assert_eq!(pair()?.status(), 410);
    let cookie = paired.headers()["set-cookie"]
        .to_str()?
        .split(';')
        .next()
        .ok_or("session cookie")?;
    for target in [desktop, url] {
        assert_eq!(
            host.client
                .get(target)
                .header("Cookie", cookie)
                .send()?
                .status(),
            200
        );
    }
    let reopened = host.client.get(cli_link).header("Cookie", cookie).send()?;
    assert_eq!(reopened.status(), 303);
    assert_eq!(reopened.headers()["location"], "/");
    assert!(reopened.headers().get("set-cookie").is_none());
    Ok(())
}

#[cfg(target_os = "linux")]
fn check_browser_launcher(fixture: &LegacyService, base: &str) -> TestResult<(String, String)> {
    use std::{fs, os::unix::fs::PermissionsExt};

    let bin = fixture.root.path().join("bin");
    fs::create_dir(&bin)?;
    let launcher = bin.join("xdg-open");
    let handoff = fixture.root.path().join("browser-handoff");
    fs::write(
        &launcher,
        "#!/bin/sh\numask 077\nprintf '%s' \"$1\" > \"$BEANKEEPER_TEST_BROWSER_HANDOFF\"\n",
    )?;
    fs::set_permissions(&launcher, fs::Permissions::from_mode(0o700))?;
    let browser_command = || {
        let mut command = fixture.command();
        command
            .env("PATH", &bin)
            .env("BEANKEEPER_TEST_BROWSER_HANDOFF", &handoff);
        command
    };

    let output = browser_command().arg("open").output()?;
    assert!(output.status.success());
    assert!(output.stdout.is_empty() && output.stderr.is_empty());
    let cli_link = fs::read_to_string(&handoff)?;
    assert!(cli_link.starts_with(&format!("{base}/pair?token=")));

    let mut lifecycle = browser_command();
    lifecycle.args(["service", "mcp"]);
    let result = stdio_tool(lifecycle, "service_open", &json!({}))?;
    assert_eq!(result["opened"], true);
    assert!(result["url"].is_null());
    let lifecycle_link = fs::read_to_string(&handoff)?;
    assert!(lifecycle_link.starts_with(&format!("{base}/pair?token=")));
    assert!(
        !lifecycle_link.eq(&cli_link),
        "lifecycle must issue a fresh link"
    );

    let mut extension = browser_command();
    extension.args(["mcp", "extension"]);
    let result = stdio_tool(extension, "service_open", &json!({}))?;
    assert_eq!(result["opened"], true);
    assert!(result["url"].is_null());
    let extension_link = fs::read_to_string(&handoff)?;
    assert!(extension_link.starts_with(&format!("{base}/pair?token=")));
    assert!(
        !extension_link.eq(&lifecycle_link),
        "extension must issue a fresh link"
    );
    Ok((cli_link, extension_link))
}

#[cfg(target_os = "linux")]
#[test]
fn legacy_managed_access_migrates_and_updates_an_open_stdio_session() -> TestResult {
    use beankeeper_cli::{db::audit, identity};
    use secrecy::ExposeSecret;

    let fixture = LegacyService::new()?;
    let prior = fixture.secret("agent-token")?;
    let db = fixture.db()?;
    let prior_id = identity::verify(db.read(), prior.expose_secret())?
        .jti()
        .to_owned();
    let owner_id = identity::verify(db.read(), fixture.secret("owner-token")?.expose_secret())?
        .jti()
        .to_owned();
    let host = fixture.host()?;
    let access = fixture.service_access(&["status", "--json"])?;
    let access = &access["agent_access"];
    assert_eq!(access["preset"], "bookkeeper");
    assert_eq!(access["write_mode"], "direct");
    assert_eq!(access["migrated"], true);
    assert!(
        access["note"]
            .as_str()
            .is_some_and(|note| note.contains("make me read-only"))
    );
    assert_eq!(
        stdio_tool(fixture.bridge(), "setup_status", &json!({}))?["agent_access"],
        *access
    );
    let replacement = fixture.secret("agent-token")?;
    let replacement_id = identity::verify(db.read(), replacement.expose_secret())?
        .jti()
        .to_owned();
    assert!(identity::verify(db.read(), prior.expose_secret()).is_err());
    let audit = audit::list_audit(db.read(), &common::audit_query())?;
    for (action, id) in [
        (audit::AuditAction::TokenIssue, replacement_id),
        (audit::AuditAction::TokenRevoke, prior_id),
    ] {
        let row = audit
            .iter()
            .find(|row| row.action == action && row.entity_id == id)
            .ok_or("rotation is missing its audit row")?;
        assert_eq!(row.actor, "desktop-owner");
        assert_eq!(row.token_jti.as_deref(), Some(owner_id.as_str()));
    }
    assert_managed_access_pipe(&fixture)?;
    let status = fixture.service_access(&[
        "access-set",
        "--preset",
        "bookkeeper",
        "--write-mode",
        "propose-only",
        "--json",
    ])?;
    let mcp = stdio_tool(
        fixture.bridge(),
        "access_set",
        &json!({
            "preset": "bookkeeper", "write_mode": "propose-only"
        }),
    )?;
    assert_eq!(status["agent_access"], mcp["agent_access"]);
    assert_eq!(mcp["agent_access"]["write_mode"], "propose-only");
    assert!(mcp["agent_access"]["migrated"].is_null());

    let mut explicit_bridge = fixture.command();
    explicit_bridge
        .args([
            "mcp",
            "bridge",
            "--url",
            &format!("{}/mcp", host.base),
            "--token-file",
        ])
        .arg(fixture.books.with_extension("agent-token"));
    let rejected = stdio_tool_response(
        explicit_bridge,
        "access_set",
        &json!({"preset":"bookkeeper"}),
    )?;
    assert!(rejected.is_null() || rejected["isError"] == true);
    drop(host);
    let _host = fixture.host()?;
    let restarted = fixture.service_access(&["status", "--json"])?;
    assert_eq!(restarted["agent_access"], mcp["agent_access"]);
    Ok(())
}

#[cfg(target_os = "linux")]
fn assert_managed_access_pipe(fixture: &LegacyService) -> TestResult {
    let mut child = fixture
        .bridge()
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()?;
    let mut input = child.stdin.take().ok_or("bridge input")?;
    for request in [
        json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{
            "protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"access-test","version":"1"}
        }}),
        json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
        json!({"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}),
        json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{
            "name":"access_set","arguments":{"preset":"read-only","write_mode":"propose-only"}
        }}),
        json!({"jsonrpc":"2.0","id":4,"method":"tools/call","params":{
            "name":"access_set","arguments":{"preset":"read-only"}
        }}),
        json!({"jsonrpc":"2.0","id":5,"method":"tools/list","params":{}}),
        json!({"jsonrpc":"2.0","id":6,"method":"tools/call","params":{"name":"setup_status"}}),
    ] {
        writeln!(input, "{request}")?;
    }
    drop(input);
    let output = child.wait_with_output()?;
    assert!(output.status.success());
    let frames: Vec<Value> = std::str::from_utf8(&output.stdout)?
        .lines()
        .map(serde_json::from_str)
        .collect::<Result<_, _>>()?;
    let result = |id| -> TestResult<&Value> {
        frames
            .iter()
            .find(|frame| frame["id"] == id)
            .map(|frame| &frame["result"])
            .ok_or_else(|| format!("missing MCP result {id}").into())
    };
    let tools = |id| -> TestResult<&Vec<Value>> {
        result(id)?["tools"]
            .as_array()
            .ok_or_else(|| "missing MCP tool list".into())
    };
    assert_eq!(result(1)?["capabilities"]["tools"]["listChanged"], true);
    let access_description = tools(2)?
        .iter()
        .find(|tool| tool["name"] == "access_set")
        .and_then(|tool| tool["description"].as_str())
        .ok_or("missing managed access tool description")?;
    assert!(
        access_description.contains("audit_undo by receipt ID or txn_reverse by transaction ID")
    );
    assert!(access_description.contains("pending proposals for sign-off"));
    assert!(access_description.contains("Never make offsetting entries by hand"));
    assert!(tools(2)?.iter().any(|tool| tool["name"] == "txn_post"));
    assert_eq!(result(3)?["structuredContent"]["code"], "SERVICE_ARGUMENTS");
    assert_eq!(
        result(4)?["structuredContent"]["agent_access"]["preset"],
        "read-only"
    );
    assert!(
        frames
            .iter()
            .any(|frame| frame["method"] == "notifications/tools/list_changed")
    );
    assert!(tools(5)?.iter().all(|tool| tool["name"] != "txn_post"));
    assert_eq!(
        result(6)?["structuredContent"]["agent_access"]["preset"],
        "read-only"
    );
    Ok(())
}

#[cfg(target_os = "linux")]
#[test]
fn recorded_read_only_choice_survives_service_start_and_resume() -> TestResult {
    use beankeeper_cli::{db::audit, identity};
    use secrecy::ExposeSecret;

    let fixture = LegacyService::new()?;
    let original = fixture.secret("agent-token")?;
    let status = fixture.service_access(&["access-set", "--preset", "read-only", "--json"])?;
    assert_eq!(status["agent_access"]["preset"], "read-only");
    let resume = fixture
        .command()
        .args(["setup", "--resume", "--non-interactive"])
        .output()?;
    assert!(resume.status.success(), "cannot resume managed setup");
    let _host = fixture.host()?;
    let status = fixture.service_access(&["status", "--json"])?;
    let access = &status["agent_access"];
    assert_eq!(access["preset"], "read-only");
    assert_eq!(access["write_mode"], "direct");
    assert!(access.get("docs_url").is_none());
    let note = access["note"].as_str().ok_or("missing read-only note")?;
    assert!(note.contains("Restarting cannot grant posting access"));
    assert!(note.contains("“let my agent post” in chat"));
    assert!(note.contains("access_set"));
    assert!(note.contains("Respect the owner's choice to stay read-only"));

    let mut extension = fixture.command();
    extension.args(["mcp", "extension"]);
    assert_eq!(
        stdio_tool(extension, "setup_status", &json!({}))?["agent_access"],
        *access
    );
    let mut lifecycle = fixture.command();
    lifecycle.args(["service", "mcp"]);
    assert_eq!(
        stdio_tool(lifecycle, "service_status", &json!({}))?["agent_access"],
        *access
    );
    let db = fixture.db()?;
    assert!(identity::verify(db.read(), original.expose_secret()).is_ok());
    let audit = audit::list_audit(db.read(), &common::audit_query())?;
    assert!(
        audit
            .iter()
            .all(|row| row.action != audit::AuditAction::TokenRevoke)
    );
    Ok(())
}

#[cfg(target_os = "linux")]
#[test]
fn setup_resume_honors_books_selected_by_flag_or_environment() -> TestResult {
    use secrecy::ExposeSecret;

    let fixture = LegacyService::new()?;
    let managed_grant = fixture.secret("agent-token")?;
    for source in ["flag", "environment"] {
        let books = fixture.books.with_file_name(format!("{source}.db"));
        let mut command = fixture.command();
        if source == "flag" {
            command.arg("--db").arg(&books);
        } else {
            command.env("BEANKEEPER_DB", &books);
        }
        let output = command
            .args([
                "setup",
                "--force",
                "--non-interactive",
                "--no-start",
                "--no-open",
                "--config-home",
            ])
            .arg(fixture.root.path().join(source))
            .arg("--secret-store-file")
            .arg(books.with_extension("passphrase"))
            .output()?;
        assert!(
            output.status.success(),
            "setup with books from {source}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(books.is_file());
    }
    assert!(
        fixture.secret("agent-token")?.expose_secret() == managed_grant.expose_secret(),
        "resuming explicitly selected books rotated the managed credential"
    );
    Ok(())
}

#[cfg(target_os = "linux")]
#[test]
fn managed_resume_finishes_an_interrupted_audited_rotation() -> TestResult {
    use beankeeper_cli::{db::Actor, identity};
    use secrecy::ExposeSecret;
    use std::fs;

    let fixture = LegacyService::new()?;
    let db = fixture.db()?;
    let old = fixture.secret("agent-token")?;
    let old_id = identity::verify(db.read(), old.expose_secret())?
        .jti()
        .to_owned();
    let owner = identity::verify(db.read(), fixture.secret("owner-token")?.expose_secret())?;
    let actor = Actor::new("desktop-owner".parse()?).with_identity(owner);
    let replacement = issue_sign_off_grant(&db, &actor)?;
    let state = fixture.books.with_extension("agent-access.json");
    fs::write(
        &state,
        serde_json::to_vec(&json!({
            "Pending": {
                "before": null,
                "old": old_id,
                "after": {"token": replacement.token_jti, "origin": "owner-choice"}
            }
        }))?,
    )?;
    beankeeper_desktop::setup::keychain::restrict_file(&state)?;
    fs::write(
        fixture.books.with_extension("agent-token"),
        replacement.token().expose_secret(),
    )?;

    let result = fixture
        .command()
        .args(["setup", "--resume", "--non-interactive"])
        .output()?;
    assert!(
        result.status.success(),
        "could not finish interrupted rotation"
    );
    assert!(identity::verify(db.read(), old.expose_secret()).is_err());
    assert_eq!(
        identity::verify(db.read(), replacement.token().expose_secret())?.jti(),
        replacement.token_jti.as_str()
    );
    let rows = beankeeper_cli::db::audit::list_audit(db.read(), &common::audit_query())?;
    assert!(rows.iter().any(|row| row.action
        == beankeeper_cli::db::audit::AuditAction::TokenRevoke
        && row.entity_id == old_id
        && row.actor == "desktop-owner"));
    Ok(())
}

#[cfg(target_os = "linux")]
#[test]
fn managed_demo_http_uses_the_startup_bearer_and_session() -> TestResult {
    use beankeeper_cli::{identity, mcp::LEDGER_TARGET_META_KEY};
    use secrecy::ExposeSecret;

    let fixture = LegacyService::new()?;
    let mut host = fixture.host()?;
    let credential = fixture.secret("agent-token")?;
    let token = credential.expose_secret();
    let identity = identity::verify(fixture.db()?.read(), token)?;
    host.initialize(Some(token))?;
    let cli = fixture
        .command()
        .args(["demo", "start", "--json"])
        .output()?;
    assert!(
        cli.status.success(),
        "{}",
        String::from_utf8_lossy(&cli.stderr)
    );
    let created: Value = serde_json::from_slice(&cli.stdout)?;
    let start =
        common::rpc(host.request("tools/call", json!({"name":"demo_start"}), Some(token))?)?;
    let target = start["result"]["structuredContent"]["target"]
        .as_str()
        .ok_or("missing demo target")?;
    assert_eq!(start["result"]["_meta"][LEDGER_TARGET_META_KEY], target);
    assert_eq!(created["target"], target);
    let resume = host
        .client
        .get(format!("{}/mcp", host.base))
        .bearer_auth(token)
        .header("Accept", "text/event-stream")
        .header("Mcp-Session-Id", host.session.as_deref().ok_or("session")?)
        .header("MCP-Protocol-Version", "2025-11-25")
        .header("Last-Event-ID", "0")
        .send()?;
    assert_eq!(resume.status(), 405);

    let posting = demo_posting();
    let post = demo_http(&host, token, target, "txn_post", &posting)?;
    assert_eq!(post["result"]["_meta"][LEDGER_TARGET_META_KEY], target);
    let posted = demo_payload(&post["result"])?;
    assert_eq!(posted["created"], true);
    let undo = json!({"company":"personal", "id":posted["id"], "reference":"demo-undo"});
    let approval = json!({"company":"personal", "id":"proposal_00000000000000000000000000"});
    let denied = demo_http(&host, token, target, "txn_approve", &approval)?;
    assert_eq!(denied["result"]["_meta"][LEDGER_TARGET_META_KEY], target);
    assert_eq!(demo_payload(&denied["result"])?["code"], "FORBIDDEN");
    let owner = fixture.secret("owner-token")?;
    let reversal = demo_http(&host, owner.expose_secret(), target, "txn_reverse", &undo)?;
    assert_eq!(reversal["result"]["_meta"][LEDGER_TARGET_META_KEY], target);
    assert_ne!(reversal["result"]["isError"], true, "{reversal}");
    let audit = demo_http(&host, token, target, "audit_log", &json!({"limit":8}))?;
    let rows = demo_payload(&audit["result"])?;
    let rows = rows.as_array().ok_or("missing audit rows")?;
    assert!(rows.iter().any(|row| row["action"] == "policy_change"));
    assert!(rows.iter().any(|row| {
        row["agent_uri"].as_str() == Some(identity.uri())
            && row["token_jti"].as_str() == Some(identity.jti())
    }));

    let removed =
        common::rpc(host.request("tools/call", json!({"name":"demo_remove"}), Some(token))?)?;
    assert_eq!(removed["result"]["structuredContent"]["state"], "removed");
    assert_eq!(removed["result"]["_meta"][LEDGER_TARGET_META_KEY], target);
    let real = common::rpc(host.request(
        "tools/call",
        json!({"name":"txn_list","arguments":{"company":"personal"}}),
        Some(token),
    )?)?;
    assert_eq!(demo_payload(&real["result"])?, json!([]));
    assert!(
        real["result"]["_meta"]
            .get(LEDGER_TARGET_META_KEY)
            .is_none()
    );
    Ok(())
}

#[cfg(target_os = "linux")]
fn demo_http(
    host: &common::Server,
    token: &str,
    target: &str,
    tool: &str,
    arguments: &Value,
) -> TestResult<Value> {
    common::rpc(
        host.client
            .post(format!("{}/mcp", host.base))
            .bearer_auth(token)
            .header("Accept", "application/json, text/event-stream")
            .header("MCP-Protocol-Version", "2025-11-25")
            .header("Mcp-Session-Id", host.session.as_deref().ok_or("session")?)
            .header(beankeeper_cli::mcp::LEDGER_TARGET_HEADER, target)
            .json(
                &json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{
                    "name": tool, "arguments": arguments
                }}),
            )
            .send()?,
    )
}

#[cfg(target_os = "linux")]
fn demo_posting() -> Value {
    json!({
        "company":"personal", "description":"A fictional test entry", "reference":"demo-post",
        "date":"2026-11-20", "debits":[{"account":"1000","amount":"12.50"}],
        "credits":[{"account":"3000","amount":"12.50"}]
    })
}

#[cfg(target_os = "linux")]
fn demo_payload(result: &Value) -> TestResult<Value> {
    if let Some(payload) = result.get("structuredContent") {
        return Ok(payload.clone());
    }
    Ok(serde_json::from_str(
        result["content"][0]["text"]
            .as_str()
            .ok_or("missing payload")?,
    )?)
}

#[cfg(target_os = "linux")]
#[test]
fn managed_demo_tailors_books_over_cli_and_mcp() -> TestResult {
    use beankeeper_cli::mcp::LEDGER_TARGET_META_KEY;

    let fixture = LegacyService::new()?;
    let _host = fixture.host()?;
    let mut pipe = McpPipe::new(&fixture)?;
    let original = demo_payload(&pipe.tool("company_list", &json!({}))?)?;
    let tools = pipe.request("tools/list", &json!({}))?;
    let start = tools["tools"]
        .as_array()
        .ok_or("missing tools")?
        .iter()
        .find(|tool| tool["name"] == "demo_start")
        .ok_or("missing demo_start")?;
    assert!(start["inputSchema"]["properties"]["profile"].is_object());

    let invalid = json!({
        "currencies":{"primary":"CAD"},
        "books":[{"accounts":[{"kind":"checking","currency":"USD"}]}]
    });
    let rejected = pipe.tool("demo_start", &json!({"profile":invalid}))?;
    assert_eq!(rejected["structuredContent"]["code"], "DEMO_ARGUMENTS");
    let cli = fixture
        .command()
        .args(["demo", "start", "--json", "--profile", &invalid.to_string()])
        .output()?;
    assert!(!cli.status.success());
    assert_eq!(
        serde_json::from_slice::<Value>(&cli.stderr)?,
        rejected["structuredContent"]
    );

    let profile = json!({
        "work_shape":"business_owner",
        "country":"CA",
        "currencies":{"secondary":["USD"]},
        "books":[{"kind":"llc","industry":"trades","accounts":[
            {"kind":"checking"},{"kind":"payment_app","currency":"USD"}
        ]}],
        "features":{"include_investments":true}
    });
    let mut cli = fixture
        .command()
        .args(["demo", "start", "--json", "--profile", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    serde_json::to_writer(cli.stdin.take().ok_or("missing CLI stdin")?, &profile)?;
    let cli = cli.wait_with_output()?;
    assert!(
        cli.status.success(),
        "{}",
        String::from_utf8_lossy(&cli.stderr)
    );
    let cli_start: Value = serde_json::from_slice(&cli.stdout)?;
    let resumed = pipe.tool("demo_start", &json!({}))?;
    assert_eq!(resumed["structuredContent"], cli_start);
    assert_eq!(
        resumed["_meta"][LEDGER_TARGET_META_KEY],
        cli_start["target"]
    );

    let tailored = demo_payload(&pipe.tool("company_list", &json!({}))?)?;
    let companies = tailored.as_array().ok_or("missing tailored books")?;
    assert_eq!(companies.len(), 1);
    assert_eq!(companies[0]["slug"], "demo-book-1");
    assert_eq!(companies[0]["name"], "Fictional Moonfern Studio 1 LLC");
    assert_tailored_demo_accounts(&mut pipe)?;

    let other = json!({"profile":{"books":[{"kind":"personal"}]}});
    let rejected = pipe.tool("demo_start", &other)?;
    assert_eq!(rejected["structuredContent"]["code"], "DEMO_ALREADY_ACTIVE");
    let removed = pipe.tool("demo_remove", &json!({}))?;
    assert_eq!(removed["structuredContent"]["target"], cli_start["target"]);
    assert_eq!(
        demo_payload(&pipe.tool("company_list", &json!({}))?)?,
        original
    );

    let started = pipe.tool("demo_start", &other)?;
    assert_eq!(started["structuredContent"]["state"], "active");
    assert_ne!(started["structuredContent"]["target"], cli_start["target"]);
    let new_books = demo_payload(&pipe.tool("company_list", &json!({}))?)?;
    assert_eq!(new_books[0]["name"], "Fictional Household 1");
    let removed = pipe.tool("demo_remove", &json!({}))?;
    assert_eq!(removed["structuredContent"]["state"], "removed");
    assert_eq!(
        demo_payload(&pipe.tool("company_list", &json!({}))?)?,
        original
    );
    assert!(
        !std::fs::read_dir(fixture.books.parent().ok_or("books parent")?)?
            .filter_map(Result::ok)
            .any(|entry| entry
                .file_name()
                .to_string_lossy()
                .starts_with(".beankeeper-demo-"))
    );
    Ok(())
}

#[cfg(target_os = "linux")]
fn assert_tailored_demo_accounts(pipe: &mut McpPipe) -> TestResult {
    let book = json!({"company":"demo-book-1"});
    let accounts = demo_payload(&pipe.tool("account_list", &book)?)?;
    let accounts = accounts.as_array().ok_or("missing synthetic accounts")?;
    for name in [
        "Sample Checking 1 (CAD)",
        "Sample Payment App 2 (USD)",
        "Sample Brokerage (CAD)",
    ] {
        assert!(accounts.iter().any(|account| account["name"] == name));
    }
    let transactions = demo_payload(&pipe.tool("txn_list", &book)?)?;
    let transactions = transactions
        .as_array()
        .ok_or("missing synthetic transactions")?;
    for currency in ["CAD", "USD"] {
        assert!(transactions.iter().any(|txn| txn["currency"] == currency));
    }
    Ok(())
}

#[cfg(target_os = "linux")]
#[test]
fn managed_stdio_demo_does_not_fall_back_when_another_client_removes_it() -> TestResult {
    use beankeeper_cli::mcp::LEDGER_TARGET_META_KEY;
    use std::{fs, io::Read};

    let fixture = LegacyService::new()?;
    let _host = fixture.host()?;
    let mut pipe = McpPipe::new(&fixture)?;
    let malformed = pipe.tool("demo_start", &json!({"path":fixture.books}))?;
    assert_eq!(malformed["structuredContent"]["code"], "DEMO_ARGUMENTS");
    let unavailable = pipe.tool("txn_post", &demo_posting())?;
    assert_eq!(unavailable["structuredContent"]["code"], "DEMO_UNAVAILABLE");

    let started = pipe.tool("demo_start", &json!({}))?;
    let target = &started["structuredContent"]["target"];
    assert_eq!(started["structuredContent"]["state"], "active");
    assert_eq!(&started["_meta"][LEDGER_TARGET_META_KEY], target);
    let sibling = fs::read_dir(fixture.books.parent().ok_or("books parent")?)?
        .filter_map(Result::ok)
        .map(|entry| entry.path().join("demo.db"))
        .find(|path| path.is_file())
        .ok_or("missing demo file")?;
    let mut header = [0; 16];
    fs::File::open(&sibling)?.read_exact(&mut header)?;
    assert_ne!(&header, b"SQLite format 3\0");

    let list = pipe.request("tools/list", &json!({}))?;
    assert_eq!(&list["_meta"][LEDGER_TARGET_META_KEY], target);
    let tools = list["tools"].as_array().ok_or("missing list")?;
    assert!(tools.iter().any(|tool| tool["name"] == "demo_remove"));
    assert!(tools.iter().all(|tool| tool["name"] != "setup_status"));
    assert!(tools.iter().all(|tool| tool["name"] != "agents_extension"));
    assert!(tools.iter().all(|tool| tool["name"] != "changes_since"));
    assert!(tools.iter().all(|tool| tool["name"] != "changes_mark_seen"));
    let listed = pipe.tool("company_list", &json!({}))?;
    assert_eq!(&listed["_meta"][LEDGER_TARGET_META_KEY], target);
    assert_eq!(
        demo_payload(&listed)?.as_array().ok_or("demo books")?.len(),
        3
    );

    let cli = fixture
        .command()
        .args(["demo", "remove", "--json"])
        .output()?;
    assert!(
        cli.status.success(),
        "{}",
        String::from_utf8_lossy(&cli.stderr)
    );
    let removed: Value = serde_json::from_slice(&cli.stdout)?;
    assert_eq!(&removed["target"], target);
    assert!(!sibling.exists());
    assert!(fixture.books.exists());
    let unavailable = pipe.tool("txn_post", &demo_posting())?;
    assert_eq!(&unavailable["_meta"][LEDGER_TARGET_META_KEY], target);
    assert_eq!(unavailable["structuredContent"]["code"], "DEMO_UNAVAILABLE");

    let restarted = pipe.tool("demo_start", &json!({}))?;
    assert_eq!(restarted["structuredContent"]["state"], "active");
    assert_ne!(&restarted["structuredContent"]["target"], target);
    let removed = pipe.tool("demo_remove", &json!({}))?;
    assert_eq!(removed["structuredContent"]["state"], "removed");
    let real = pipe.tool("txn_list", &json!({"company":"personal"}))?;
    assert_eq!(demo_payload(&real)?, json!([]));
    assert!(real["_meta"].get(LEDGER_TARGET_META_KEY).is_none());
    let receipts = pipe.tool("changes_since", &json!({"company":"personal"}))?;
    assert_eq!(receipts["structuredContent"]["company"], "personal");
    assert!(receipts["_meta"].get(LEDGER_TARGET_META_KEY).is_none());
    Ok(())
}

#[test]
fn extension_publishes_the_bundled_skill_before_setup() -> TestResult {
    use rmcp::model::{SKILLS_EXTENSION_ID, Skill, SkillResources};
    const URI: &str = "skill://beankeeper-desktop/SKILL.md";

    let root = tempfile::tempdir()?;
    let mut command = isolated(root.path());
    command.args(["mcp", "extension"]);
    let mut pipe = McpPipe::spawn(&mut command)?;
    assert_eq!(
        pipe.frame("server/discover", &json!({}))?["error"]["code"],
        -32602
    );
    let discover = pipe.request("server/discover", &inline_params(json!({})))?;
    assert_eq!(discover["resultType"], "complete");
    assert_eq!(
        discover["capabilities"]["extensions"][SKILLS_EXTENSION_ID],
        json!({})
    );
    assert_eq!(discover["capabilities"]["resources"], json!({}));
    let instructions = discover["instructions"].as_str().ok_or("instructions")?;
    for text in [
        "call `demo_start` with a fictional `profile`",
        "txn_recategorize",
        "audit_undo",
        "txn_reverse",
        "changes_since",
        "minor units",
        "receipt number",
        "You could ask:",
    ] {
        assert!(instructions.contains(text), "missing {text}");
    }
    let listed = pipe.request("skills/list", &inline_params(json!({})))?;
    assert_eq!(listed["resultType"], "complete");
    assert!(listed["ttlMs"].is_number());
    assert!(listed["cacheScope"].is_string());
    assert!(listed.get("nextCursor").is_none());
    let [entry] = listed["skills"]
        .as_array()
        .ok_or("missing skill list")?
        .as_slice()
    else {
        return Err("expected exactly one skill".into());
    };
    let skill: Skill = serde_json::from_value(entry.clone())?;
    assert_eq!(skill.uri, URI);
    assert_eq!(skill.name(), "beankeeper-desktop");
    let params = inline_params(json!({"uri":URI}));
    assert_eq!(pipe.request("skills/get", &params)?["skill"], *entry);
    let read = pipe.request("resources/read", &params)?;
    let file = &read["contents"][0];
    assert_eq!(file["uri"], URI);
    assert_eq!(file["mimeType"], "text/markdown");
    let text = file["text"].as_str().ok_or("missing skill text")?;
    assert_eq!(text, include_str!("../../../SKILL.md"));
    let SkillResources::Files(files) = &skill.resources else {
        return Err("expected the embedded skill manifest".into());
    };
    let [manifest] = files.as_slice() else {
        return Err("expected only the embedded SKILL.md".into());
    };
    assert_eq!(manifest.uri, URI);
    manifest.verify(text.as_bytes())?;
    skill.validate()?;
    let resources = pipe.request("resources/list", &inline_params(json!({})))?;
    assert_eq!(
        resources["resources"][0]["description"],
        skill.frontmatter.description
    );
    for method in ["skills/get", "resources/read"] {
        for invalid in [
            json!({}),
            json!({"uri":"file:///unrelated/SKILL.md"}),
            json!({"uri":42}),
        ] {
            assert_eq!(
                pipe.frame(method, &inline_params(invalid))?["error"]["code"],
                -32602
            );
        }
    }
    let status = pipe.request(
        "tools/call",
        &inline_params(json!({"name":"setup_status","arguments":{}})),
    )?;
    assert_eq!(status["structuredContent"]["state"], "not_started");
    assert!(!root.path().join("state").exists());
    assert!(!root.path().join("data").exists());
    Ok(())
}

fn inline_params(mut params: Value) -> Value {
    params["_meta"] = json!({
        "io.modelcontextprotocol/protocolVersion":"2026-07-28",
        "io.modelcontextprotocol/clientInfo":{"name":"skill-test","version":"1"},
        "io.modelcontextprotocol/clientCapabilities":{}
    });
    params
}

struct McpPipe {
    child: std::process::Child,
    input: std::process::ChildStdin,
    frames: mpsc::Receiver<std::io::Result<String>>,
    id: u64,
}

impl McpPipe {
    #[cfg(target_os = "linux")]
    fn new(fixture: &LegacyService) -> TestResult<Self> {
        Self::start(&mut fixture.bridge())
    }

    fn start(command: &mut std::process::Command) -> TestResult<Self> {
        let mut pipe = Self::spawn(command)?;
        pipe.request(
            "initialize",
            &json!({"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{
                "name":"demo-test","version":"1.0"
            }}),
        )?;
        writeln!(
            pipe.input,
            "{}",
            json!({"jsonrpc":"2.0","method":"notifications/initialized"})
        )?;
        pipe.input.flush()?;
        Ok(pipe)
    }

    fn spawn(command: &mut std::process::Command) -> TestResult<Self> {
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()?;
        let input = child.stdin.take().ok_or("missing stdin")?;
        let output = child.stdout.take().ok_or("missing stdout")?;
        let (send, frames) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(output).lines() {
                if send.send(line).is_err() {
                    break;
                }
            }
        });
        Ok(Self {
            child,
            input,
            frames,
            id: 0,
        })
    }

    fn tool(&mut self, tool: &str, arguments: &Value) -> TestResult<Value> {
        self.request("tools/call", &json!({"name":tool,"arguments":arguments}))
    }

    fn request(&mut self, method: &str, params: &Value) -> TestResult<Value> {
        let frame = self.frame(method, params)?;
        assert!(frame.get("error").is_none(), "{frame}");
        Ok(frame["result"].clone())
    }

    fn frame(&mut self, method: &str, params: &Value) -> TestResult<Value> {
        self.id += 1;
        let id = self.id;
        writeln!(
            self.input,
            "{}",
            json!({"jsonrpc":"2.0","id":id,"method":method,"params":params})
        )?;
        self.input.flush()?;
        loop {
            let frame: Value =
                serde_json::from_str(&self.frames.recv_timeout(Duration::from_secs(20))??)?;
            if frame["id"] == id {
                return Ok(frame);
            }
        }
    }
}

impl Drop for McpPipe {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn license_remove_cli_and_mcp_preserve_other_activation_and_books() -> TestResult {
    use std::fs;
    let service = std::net::TcpListener::bind("127.0.0.1:0")?;
    service.set_nonblocking(true)?;
    let service_url = format!("http://{}", service.local_addr()?);
    let root = tempfile::tempdir()?;
    let keys = root.path().join("issuer.json");
    fs::write(&keys, include_bytes!("fixtures/license-public-keys.json"))?;
    let books = root.path().join("beankeeper.db");
    fs::write(&books, "untouched books")?;
    let command = || {
        let mut command = isolated(root.path());
        command
            .current_dir(root.path())
            .env("BEANKEEPER_LICENSE_KEYS_FILE", &keys)
            .env("BEANKEEPER_LICENSE_SERVICE_URL", &service_url);
        command
    };
    let mut pipe = McpPipe::start(command().args(["service", "mcp"]))?;
    let mut ids = Vec::new();
    for token in [
        include_str!("fixtures/license-v4.public.txt"),
        include_str!("fixtures/license-major-2-v4.public.txt"),
    ] {
        let activated = pipe.tool("license_activate", &json!({"license": token.trim_end()}))?;
        ids.push(
            activated["structuredContent"]["licenses"][0]["license_id"]
                .as_str()
                .ok_or("license ID")?
                .to_owned(),
        );
    }
    let result = pipe.tool("license_remove", &json!({"license_id": ids[0]}))?;
    assert_eq!(result["structuredContent"]["code"], "removed");
    assert_eq!(result["structuredContent"]["books_access"], "writable");
    assert!(
        !root
            .path()
            .join("state/license-bindings")
            .join(&ids[0])
            .exists()
    );
    let status = pipe.tool("license_status", &json!({}))?;
    assert_eq!(
        status["structuredContent"]["licenses"]
            .as_array()
            .ok_or("licenses")?
            .len(),
        1
    );
    assert_eq!(
        status["structuredContent"]["licenses"][0]["license_id"],
        ids[1]
    );
    assert_eq!(status["structuredContent"]["writable"], true);
    let repeated = command()
        .args(["--json", "license", "remove", &ids[0]])
        .output()?;
    assert!(repeated.status.success());
    assert_eq!(
        serde_json::from_slice::<Value>(&repeated.stdout)?["code"],
        "not_installed"
    );
    let last = command()
        .args(["--json", "license", "remove", &ids[1]])
        .output()?;
    assert!(last.status.success());
    assert_eq!(
        serde_json::from_slice::<Value>(&last.stdout)?["code"],
        "removed"
    );
    assert_eq!(fs::read_to_string(books)?, "untouched books");
    let status = pipe.tool("license_status", &json!({}))?;
    assert_eq!(status["structuredContent"]["status"], "not_activated");
    assert!(
        matches!(service.accept(), Err(error) if error.kind() == std::io::ErrorKind::WouldBlock)
    );
    Ok(())
}
