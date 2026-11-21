//! Public agent command contract with an isolated home and no live service.

use std::{fs, path::Path, process::Command};

use serde_json::Value;

fn bk(home: &Path, args: &[&str]) -> Result<(bool, Value), Box<dyn std::error::Error>> {
    let output = Command::new(env!("CARGO_BIN_EXE_bk"))
        .env("HOME", home)
        .env("USERPROFILE", home)
        .env("WINDOWS_USERPROFILE", home)
        .env("BEANKEEPER_DESKTOP_STATE_DIR", home.join("state"))
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env("XDG_DATA_HOME", home.join(".local/share"))
        .env("APPDATA", home.join("AppData/Roaming"))
        .env("LOCALAPPDATA", home.join("AppData/Local"))
        .env("CODEX_HOME", home.join(".codex"))
        .env("CLAUDE_CONFIG_DIR", home.join(".claude"))
        .env("COPILOT_HOME", home.join(".copilot"))
        .env("GROK_HOME", home.join("fixture-grok"))
        .env("PATH", "")
        .args(args)
        .output()?;
    Ok((
        output.status.success(),
        serde_json::from_slice(&output.stdout)?,
    ))
}

#[test]
fn detection_and_doctor_distinguish_unsupported_unconfigured_and_stopped_service()
-> Result<(), Box<dyn std::error::Error>> {
    let home = tempfile::tempdir()?;
    let path = home.path();
    fs::create_dir(path.join(".codex"))?;
    fs::create_dir(path.join("fixture-grok"))?;
    let (success, result) = bk(path, &["agents", "detect", "--json"])?;
    assert!(success, "{result}");
    assert_eq!(result["meta"]["command"], "agents.detect");
    let rows = result["data"]["agents"]
        .as_array()
        .ok_or("missing agents")?;
    assert!(
        rows.iter()
            .any(|row| row["agent"] == "codex" && row["state"] == "available")
    );
    assert!(
        rows.iter()
            .any(|row| row["agent"] == "chatgpt" && row["state"] == "unsupported")
    );
    assert!(
        rows.iter()
            .any(|row| row["agent"] == "grok-build" && row["state"] == "available")
    );
    assert!(
        rows.iter()
            .any(|row| row["agent"] == "grok" && row["state"] == "unsupported")
    );
    assert!(!path.join("fixture-grok/config.toml").exists());
    assert!(!path.join(".codex/config.toml").exists());
    let (success, doctor) = bk(path, &["agents", "doctor", "codex", "--json"])?;
    assert!(!success);
    assert_eq!(
        doctor["data"]["agents"][0]["error"]["code"],
        "AGENT_NOT_CONFIGURED"
    );
    assert_eq!(
        doctor["data"]["service"]["error"]["code"],
        "SERVICE_NOT_INSTALLED"
    );
    let (success, connected) = bk(path, &["agents", "connect", "codex", "--json"])?;
    assert!(success, "{connected}");
    let (success, doctor) = bk(path, &["--format", "json", "agents", "doctor", "codex"])?;
    assert!(!success);
    assert_eq!(doctor["data"]["agents"][0]["state"], "configured");
    assert_eq!(
        doctor["data"]["service"]["error"]["code"],
        "SERVICE_NOT_INSTALLED"
    );
    let (success, removed) = bk(path, &["agents", "disconnect", "codex", "--json"])?;
    assert!(success, "{removed}");
    assert!(!fs::read_to_string(path.join(".codex/config.toml"))?.contains("beankeeper"));
    Ok(())
}
