pub mod common;

use std::{
    io::{BufRead, BufReader, Write},
    process::{Child, Stdio},
    sync::mpsc,
    time::{Duration, Instant},
};

use common::{Ledger, Server, TestResult, clean_command, token};
use serde_json::{Value, json};

struct Bridge(Child);
impl Drop for Bridge {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn stdio_bridge_uses_the_running_desktop_session_and_cleans_up_on_eof() -> TestResult {
    let ledger = Ledger::new()?;
    let server = Server::start(&ledger, &["serve"], None)?;
    let directory = tempfile::tempdir()?;
    let token_file = directory.path().join("agent-token");
    std::fs::write(&token_file, token(&ledger.reader)?)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&token_file, std::fs::Permissions::from_mode(0o600))?;
    }
    let mut child = Bridge(
        clean_command()
            .args([
                "mcp",
                "bridge",
                "--url",
                &format!("{}/mcp", server.base),
                "--token-file",
            ])
            .arg(&token_file)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?,
    );
    let mut stdin = child.0.stdin.take().ok_or("missing stdin")?;
    let stdout = child.0.stdout.take().ok_or("missing stdout")?;
    let (send, receive) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            if send.send(line).is_err() {
                break;
            }
        }
    });
    writeln!(
        stdin,
        "{}",
        json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"bridge-integration","version":"1.0"}}})
    )?;
    stdin.flush()?;
    let initialized: Value =
        serde_json::from_str(&receive.recv_timeout(Duration::from_secs(10))??)?;
    assert_eq!(initialized["result"]["serverInfo"]["name"], "beankeeper");
    writeln!(
        stdin,
        "{}",
        json!({"jsonrpc":"2.0","method":"notifications/initialized"})
    )?;
    writeln!(
        stdin,
        "{}",
        json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"company_list","arguments":{}}})
    )?;
    stdin.flush()?;
    let response_text = receive.recv_timeout(Duration::from_secs(10))??;
    let response: Value = serde_json::from_str(&response_text)?;
    assert_eq!(response["id"], 2);
    assert_ne!(response["result"]["isError"], true);
    assert!(response_text.contains("acme"));
    assert!(!response_text.contains(token(&ledger.reader)?));
    drop(stdin);
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(status) = child.0.try_wait()? {
            assert!(status.success());
            break;
        }
        assert!(
            Instant::now() < deadline,
            "bridge did not exit after stdin closed"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(
        server
            .client
            .get(format!("{}/health", server.base))
            .send()?
            .status(),
        200
    );
    Ok(())
}

#[test]
fn invalid_bridge_token_fails_without_disclosing_credentials() -> TestResult {
    let ledger = Ledger::new()?;
    let server = Server::start(&ledger, &["serve"], None)?;
    let directory = tempfile::tempdir()?;
    let token_file = directory.path().join("agent-token");
    std::fs::write(&token_file, "private-invalid-bearer-token")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&token_file, std::fs::Permissions::from_mode(0o600))?;
    }
    let mut child = Bridge(
        clean_command()
            .args([
                "mcp",
                "bridge",
                "--url",
                &format!("{}/mcp", server.base),
                "--token-file",
            ])
            .arg(&token_file)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?,
    );
    let mut stdin = child.0.stdin.take().ok_or("missing stdin")?;
    writeln!(
        stdin,
        "{}",
        json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{}})
    )?;
    drop(stdin);
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(status) = child.0.try_wait()? {
            assert!(!status.success());
            break;
        }
        assert!(
            Instant::now() < deadline,
            "bridge did not reject invalid token"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    let mut diagnostics = String::new();
    std::io::Read::read_to_string(
        &mut child.0.stderr.take().ok_or("missing stderr")?,
        &mut diagnostics,
    )?;
    assert!(diagnostics.contains("401"));
    assert!(!diagnostics.contains("private-invalid-bearer-token"));
    let mut protocol_output = String::new();
    std::io::Read::read_to_string(
        &mut child.0.stdout.take().ok_or("missing stdout")?,
        &mut protocol_output,
    )?;
    assert_eq!(protocol_output, "");
    Ok(())
}
