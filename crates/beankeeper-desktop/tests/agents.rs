//! Public agent command contract with an isolated home and no live service.

use std::{fs, path::Path, process::Command};

use serde_json::Value;

fn bk(home: &Path, args: &[&str]) -> Result<(bool, Value), Box<dyn std::error::Error>> {
    let output = bk_command(home).args(args).output()?;
    Ok((
        output.status.success(),
        serde_json::from_slice(&output.stdout)?,
    ))
}

fn bk_command(home: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_bk"));
    command
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
        .env("CLAUDE_USER_DATA_DIR", home.join("claude-desktop"))
        .env("COPILOT_HOME", home.join(".copilot"))
        .env("GROK_HOME", home.join("fixture-grok"))
        .env("PATH", "");
    command
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

#[test]
fn claude_code_connect_adopts_stdio_defaults_and_reports_custom_env_consistently()
-> Result<(), Box<dyn std::error::Error>> {
    let home = tempfile::tempdir()?;
    let path = home.path();
    let config = path.join(".claude.json");
    let skill = path.join(".claude/skills/beankeeper-desktop/SKILL.md");
    let binary = fs::canonicalize(env!("CARGO_BIN_EXE_bk"))?;
    let mut original = serde_json::json!({
        "mcpServers": {
            "beankeeper": {
                "command": binary,
                "args": ["mcp", "bridge", "--service"],
                "type": "stdio",
                "env": {}
            },
            "beankeeper-service": {
                "command": binary,
                "args": ["service", "mcp"],
                "type": "stdio",
                "env": {"BEANKEEPER_DB": "custom-value"}
            },
            "other": {"type": "stdio", "command": "other", "env": {"KEEP": "yes"}}
        }
    });
    let custom = serde_json::to_string(&original)?;
    fs::write(&config, &custom)?;
    let mut diagnostics = Vec::new();
    for args in [
        &["agents", "detect", "--json"][..],
        &["agents", "doctor", "claude-code", "--json"],
        &["agents", "connect", "claude-code", "--json"],
        &["agents", "disconnect", "claude-code", "--json"],
    ] {
        let (success, result) = bk(path, args)?;
        assert!(!success, "{result}");
        let row = result["data"]["agents"]
            .as_array()
            .and_then(|agents| agents.iter().find(|row| row["agent"] == "claude-code"))
            .ok_or("missing Claude Code status")?;
        assert_eq!(row["state"], "error");
        assert_eq!(row["error"]["code"], "AGENT_CONFIG_CONFLICT");
        let message = row["message"].as_str().ok_or("missing diagnostic")?;
        assert!(message.contains("`env`") && message.contains("`beankeeper-service`"));
        assert!(!message.contains("custom-value"));
        diagnostics.push(message.to_owned());
        assert_eq!(fs::read_to_string(&config)?, custom);
        assert!(!skill.exists());
    }
    assert!(
        diagnostics
            .windows(2)
            .all(|messages| messages[0] == messages[1])
    );

    original["mcpServers"]["beankeeper-service"]["env"] = serde_json::json!({});
    let original_text = serde_json::to_string(&original)?;
    fs::write(&config, &original_text)?;
    let (success, connected) = bk(path, &["agents", "connect", "claude-code", "--json"])?;
    assert!(success, "{connected}");
    assert_eq!(connected["data"]["agents"][0]["state"], "configured");
    assert!(skill.is_file());
    let canonical = fs::read_to_string(&config)?;
    let parsed: Value = serde_json::from_str(&canonical)?;
    for name in ["beankeeper", "beankeeper-service"] {
        let mut expected = original["mcpServers"][name].clone();
        let object = expected.as_object_mut().ok_or("missing managed entry")?;
        object.remove("env");
        object.remove("type");
        assert_eq!(parsed["mcpServers"][name], expected);
    }
    assert_eq!(
        parsed["mcpServers"]["other"],
        original["mcpServers"]["other"]
    );
    assert_eq!(
        fs::read_to_string(path.join(".claude.json.beankeeper-backup"))?,
        original_text
    );
    let (success, result) = bk(path, &["agents", "connect", "claude-code", "--json"])?;
    assert!(success, "{result}");
    assert_eq!(fs::read_to_string(&config)?, canonical);
    let (success, result) = bk(path, &["agents", "disconnect", "claude-code", "--json"])?;
    assert!(success, "{result}");
    assert!(!skill.exists());
    let remaining: Value = serde_json::from_slice(&fs::read(config)?)?;
    assert_eq!(
        remaining["mcpServers"],
        serde_json::json!({"other": original["mcpServers"]["other"]})
    );
    Ok(())
}

#[test]
fn enabled_claude_extension_and_managed_code_connections_report_overlap()
-> Result<(), Box<dyn std::error::Error>> {
    let home = tempfile::tempdir()?;
    let home = home.path();
    let profile = home.join("claude-desktop");
    let settings_dir = profile.join("Claude Extensions Settings");
    fs::create_dir_all(&settings_dir)?;
    let id = "local.mcpb.govcraft.beankeeper";
    let registry = serde_json::json!({"extensions": {id: {
        "id": id,
        "manifest": {"name": "beankeeper", "server": {"mcp_config": {
            "command": "${__dirname}/server/bk", "args": ["mcp", "extension"]
        }}}
    }}});
    let registry_file = profile.join("extensions-installations.json");
    fs::write(&registry_file, registry.to_string())?;
    let settings = settings_dir.join(format!("{id}.json"));
    let enabled = r#"{"isEnabled":true}"#;
    fs::write(&settings, enabled)?;

    let (_, before) = bk(home, &["agents", "detect", "--json"])?;
    assert!(before["data"].get("claude_overlap").is_none());

    let (success, connect) = bk(home, &["agents", "connect", "claude-code", "--json"])?;
    assert!(success, "{connect}");
    let notice = connect["data"]["claude_overlap"]
        .as_str()
        .ok_or("missing overlap on connect")?;
    assert!(notice.contains("Claude Code") && notice.contains("Local dev"));
    assert!(notice.contains("you only need the Beankeeper extension"));

    let config = home.join(".claude.json");
    let original = fs::read_to_string(&config)?;
    let mut code: Value = serde_json::from_str(&original)?;
    assert!(code["mcpServers"]["beankeeper"].is_object());
    assert!(code["mcpServers"]["beankeeper-service"].is_object());

    let (success, detect) = bk(home, &["agents", "detect", "--json"])?;
    assert!(success, "{detect}");
    assert_eq!(detect["data"]["claude_overlap"], notice);
    let text = bk_command(home).args(["agents", "detect"]).output()?;
    assert!(text.status.success());
    assert!(String::from_utf8(text.stdout)?.contains(notice));

    for (client, state) in [
        ("claude-desktop", "extension_managed"),
        ("claude-code", "configured"),
    ] {
        let (success, doctor) = bk(home, &["agents", "doctor", client, "--json"])?;
        assert!(!success);
        let agents = doctor["data"]["agents"]
            .as_array()
            .ok_or("missing agents")?;
        assert_eq!(agents.len(), 1);
        assert_eq!(agents[0]["state"], state);
        assert!(agents[0].get("error").is_none());
        assert_eq!(doctor["data"]["claude_overlap"], notice);
        assert_eq!(
            doctor["data"]["service"]["error"]["code"],
            "SERVICE_NOT_INSTALLED"
        );
    }
    assert_eq!(fs::read_to_string(&config)?, original);
    assert_eq!(fs::read_to_string(&settings)?, enabled);
    assert_eq!(fs::read_to_string(&registry_file)?, registry.to_string());
    assert!(!profile.join("claude_desktop_config.json").exists());

    code["mcpServers"]
        .as_object_mut()
        .ok_or("missing Claude Code servers")?
        .remove("beankeeper");
    fs::write(&config, code.to_string())?;
    let (success, partial) = bk(home, &["agents", "detect", "--json"])?;
    assert!(success, "{partial}");
    assert_eq!(partial["data"]["claude_overlap"], notice);

    for inactive in [
        r#"{"isEnabled":false}"#,
        r#"{"isEnabled":true,"orgBlockedReason":"policy"}"#,
    ] {
        fs::write(&settings, inactive)?;
        let (success, detect) = bk(home, &["agents", "detect", "--json"])?;
        assert!(success, "{detect}");
        assert!(detect["data"].get("claude_overlap").is_none());
    }
    Ok(())
}
