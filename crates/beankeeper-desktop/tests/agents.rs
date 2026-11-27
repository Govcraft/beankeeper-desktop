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
fn extension_help_explains_verified_path_only_mode() -> Result<(), Box<dyn std::error::Error>> {
    let home = tempfile::tempdir()?;
    let output = bk_command(home.path())
        .args(["agents", "extension", "--help"])
        .output()?;
    assert!(output.status.success());
    let help = String::from_utf8(output.stdout)?;
    assert!(help.contains("latest signed Claude Desktop extension"));
    assert!(help.contains("--path-only"));
    assert!(!home.path().join("state").exists());
    Ok(())
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
fn chatgpt_codex_uses_the_shared_profile_and_chat_does_not_register()
-> Result<(), Box<dyn std::error::Error>> {
    let home = tempfile::tempdir()?;
    let path = home.path();
    let config = path.join(".codex/config.toml");
    let (success, chat) = bk(path, &["agents", "connect", "chatgpt", "--json"])?;
    assert!(!success, "{chat}");
    let row = &chat["data"]["agents"][0];
    assert_eq!(row["error"]["code"], "AGENT_UNSUPPORTED");
    assert!(row["message"].as_str().is_some_and(|message| {
        message.contains("This computer") && message.contains("install.md")
    }));
    assert!(!config.exists());

    let (success, connect) = bk(path, &["agents", "connect", "chatgpt-codex", "--json"])?;
    assert!(success, "{connect}");
    let row = &connect["data"]["agents"][0];
    assert_eq!(row["agent"], "codex");
    assert_eq!(row["config"], config.to_string_lossy().as_ref());
    assert_eq!(row["state"], "configured");
    assert!(
        row["message"]
            .as_str()
            .is_some_and(|message| { message.contains("If this chat still lacks the tools") })
    );
    let registered = fs::read(&config)?;
    let (success, again) = bk(path, &["agents", "connect", "codex", "--json"])?;
    assert!(success, "{again}");
    assert_eq!(again["data"]["agents"][0], *row);
    assert_eq!(fs::read(&config)?, registered);
    assert!(!config.with_extension("toml.beankeeper-backup").exists());
    Ok(())
}

#[test]
fn chatgpt_codex_preserves_the_servers_enabled_setting() -> Result<(), Box<dyn std::error::Error>> {
    let home = tempfile::tempdir()?;
    let path = home.path();
    let config = path.join(".codex/config.toml");
    let connect = ["agents", "connect", "chatgpt-codex", "--json"];
    let doctor = ["agents", "doctor", "chatgpt-codex", "--json"];
    let (success, result) = bk(path, &connect)?;
    assert!(success, "{result}");
    let original = fs::read_to_string(&config)?;
    let with_setting = |setting: &str| {
        original.replace(
            "[mcp_servers.beankeeper-service]",
            &format!("[mcp_servers.beankeeper-service]\nenabled = {setting} # my choice"),
        )
    };
    let enabled = with_setting("true");
    fs::write(&config, &enabled)?;
    let (success, result) = bk(path, &connect)?;
    assert!(success, "{result}");
    let (success, result) = bk(path, &doctor)?;
    assert!(!success, "the local service has not been installed");
    let row = &result["data"]["agents"][0];
    assert_eq!(row["state"], "configured");
    assert!(
        row["message"]
            .as_str()
            .is_some_and(|message| message.contains("company_list"))
    );
    assert_eq!(fs::read_to_string(&config)?, enabled);

    for (setting, code) in [
        ("false", "AGENT_MCP_SERVER_DISABLED"),
        (r#""true""#, "AGENT_CONFIG_CONFLICT"),
    ] {
        let modified = with_setting(setting);
        fs::write(&config, &modified)?;
        for args in [&connect, &doctor] {
            let (success, result) = bk(path, args)?;
            assert!(!success, "{result}");
            let error = &result["data"]["agents"][0]["error"];
            assert_eq!(error["code"], code, "{result}");
            assert!(
                error["message"]
                    .as_str()
                    .is_some_and(|message| { message.contains("beankeeper-service") })
            );
            assert_eq!(fs::read_to_string(&config)?, modified);
        }
    }
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

#[test]
fn connect_plan_uses_real_adapter_destinations_without_writes()
-> Result<(), Box<dyn std::error::Error>> {
    let home = tempfile::tempdir()?;
    for client in [".claude", ".codex", ".gemini", ".cursor", "fixture-grok"] {
        fs::create_dir(home.path().join(client))?;
    }
    let before = fs::read_dir(home.path())?.count();
    let (ok, output) = bk(
        home.path(),
        &["agents", "connect", "--all", "--plan", "--json"],
    )?;
    assert!(ok, "{output}");
    let directories = output["data"]["plan"]["directories"]
        .as_array()
        .ok_or("missing folders")?;
    for expected in [
        ".codex",
        ".agents/skills/beankeeper-desktop",
        ".claude/skills/beankeeper-desktop",
        ".gemini",
        "fixture-grok",
    ] {
        assert!(directories.iter().any(|entry| entry["path"] == home.path().join(expected).to_string_lossy().as_ref()), "missing {expected}: {output}");
    }
    assert!(
        directories
            .iter()
            .any(|entry| entry["path"] == home.path().to_string_lossy().as_ref())
    );
    for entry in directories {
        assert!(Path::new(entry["parent"].as_str().ok_or("missing parent")?).is_dir());
        assert!(
            !entry["path"]
                .as_str()
                .ok_or("missing path")?
                .ends_with("SKILL.md")
        );
    }
    assert_eq!(before, fs::read_dir(home.path())?.count());
    assert!(!home.path().join("state").exists());
    assert!(!home.path().join(".codex/config.toml").exists());
    Ok(())
}

#[cfg(unix)]
#[test]
fn connect_denied_folder_reports_resume_then_retry_is_idempotent()
-> Result<(), Box<dyn std::error::Error>> {
    use std::os::unix::fs::PermissionsExt;
    if rustix::process::geteuid().as_raw() == 0 {
        return Ok(());
    }
    let home = tempfile::tempdir()?;
    let blocked = home.path().join("blocked");
    fs::create_dir(&blocked)?;
    fs::set_permissions(&blocked, fs::Permissions::from_mode(0o500))?;
    let command = || {
        let mut command = bk_command(home.path());
        command.env("CODEX_HOME", blocked.join("codex"));
        command.args(["agents", "connect", "codex", "--json"]);
        command
    };
    let first = command().output()?;
    fs::set_permissions(&blocked, fs::Permissions::from_mode(0o700))?;
    let output: Value = serde_json::from_slice(&first.stdout)?;
    assert!(!first.status.success());
    let error = &output["data"]["agents"][0]["error"];
    assert_eq!(error["code"], "AGENT_CONFIG_WRITE_DENIED");
    assert_eq!(
        error["path"],
        blocked.join("codex").to_string_lossy().as_ref()
    );
    assert_eq!(error["parent"], blocked.to_string_lossy().as_ref());
    assert_eq!(error["resume"], "bk agents connect codex --json");
    assert_eq!(output["error"], *error);
    assert!(command().output()?.status.success());
    let config = fs::read(blocked.join("codex/config.toml"))?;
    assert!(command().output()?.status.success());
    assert_eq!(config, fs::read(blocked.join("codex/config.toml"))?);
    Ok(())
}
