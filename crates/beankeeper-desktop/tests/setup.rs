//! Onboarding tests use an explicit private file store, never the host keychain.
#![cfg(unix)]

use std::{error::Error, fs, os::unix::fs::PermissionsExt, path::Path, process::Command};

use beankeeper_cli::{
    capability::Capability,
    db::{self, Db},
    identity::{self, WriteMode},
};
use serde_json::Value;
use tempfile::TempDir;

type TestResult = Result<(), Box<dyn Error>>;
const SECRET: &str = "setup-integration-only-passphrase";

struct Setup {
    dir: TempDir,
}
impl Setup {
    fn new() -> Result<Self, Box<dyn Error>> {
        let dir = tempfile::Builder::new()
            .permissions(fs::Permissions::from_mode(0o700))
            .tempdir()?;
        fs::write(dir.path().join("input-secret"), SECRET)?;
        Ok(Self { dir })
    }
    fn path(&self, name: &str) -> std::path::PathBuf {
        self.dir.path().join(name)
    }
    fn command(&self) -> Command {
        let mut cmd = Command::new(assert_cmd::cargo::cargo_bin!("bk"));
        cmd.env("BEANKEEPER_NO_UPDATE_CHECK", "1");
        for name in [
            "BK_AGENT_TOKEN",
            "BEANKEEPER_PASSPHRASE_CMD",
            "BEANKEEPER_DB",
            "BEANKEEPER_COMPANY",
        ] {
            cmd.env_remove(name);
        }
        cmd.arg("--db").arg(self.path("books.db"));
        cmd
    }
    fn configured(&self) -> Command {
        let mut cmd = self.command();
        cmd.arg("--passphrase-file")
            .arg(self.path("input-secret"))
            .args(["setup", "--non-interactive", "--no-open", "--config-home"])
            .arg(self.path("home"))
            .arg("--secret-store-file")
            .arg(self.path("stored-secret"));
        cmd
    }
    fn setup(&self) -> Command {
        let mut command = self.configured();
        command.arg("--no-start");
        command
    }
    fn db(&self) -> Result<Db, Box<dyn Error>> {
        Ok(Db::open_existing(
            &self.path("books.db"),
            Some(&SECRET.into()),
        )?)
    }
}

#[test]
fn encrypted_setup_preserves_configs_keeps_secrets_private_and_reads_keychain_on_commands()
-> TestResult {
    let setup = Setup::new()?;
    fs::create_dir_all(setup.path("home/.codex"))?;
    fs::write(
        setup.path("home/.claude.json"),
        r#"{"theme":"dark","mcpServers":{"other":{"command":"other"}}}"#,
    )?;
    fs::write(
        setup.path("home/.codex/config.toml"),
        "# preserve this comment\nmodel = 'test'\n[mcp_servers.other]\ncommand = 'other'\n",
    )?;
    let output = setup.setup().output()?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(Db::open_existing(&setup.path("books.db"), None).is_err());
    let db = setup.db()?;
    assert!(db::policy::get(db.read())?.enforce);
    let token = fs::read_to_string(setup.path("books.agent-token"))?;
    let agent = identity::verify(db.read(), &token)?;
    assert!(agent.allows(Capability::Read)?);
    assert!(!agent.allows(Capability::Post)?);
    assert!(identity::authorize(db.read(), Some(&token), Capability::Post).is_err());
    assert!(!agent.allows(Capability::Approve)?);
    assert!(!agent.allows(Capability::Admin)?);
    let kit = fs::read_to_string(setup.path("books.recovery.txt"))?;
    assert!(kit.contains(SECRET));
    assert!(kit.contains("restore --input"));
    assert!(kit.contains("no backdoor"));
    for bytes in [&output.stdout, &output.stderr] {
        let text = String::from_utf8_lossy(bytes);
        assert!(!text.contains(SECRET));
        assert!(!text.contains(&token));
    }
    for name in [
        "books.db",
        "stored-secret",
        "books.agent-token",
        "books.owner-token",
        "books.recovery.txt",
        "books.desktop.json",
        "home/.claude.json",
        "home/.codex/config.toml",
        "home/.config/Claude/claude_desktop_config.json",
    ] {
        assert_eq!(
            fs::metadata(setup.path(name))?.permissions().mode() & 0o077,
            0,
            "{name}"
        );
    }
    let config: Value = serde_json::from_slice(&fs::read(setup.path("home/.claude.json"))?)?;
    assert_eq!(config["theme"], "dark");
    assert_eq!(config["mcpServers"]["other"]["command"], "other");
    assert!(
        config["mcpServers"]["beankeeper"]["args"]
            .as_array()
            .ok_or("args")?
            .iter()
            .any(|arg| arg == "bridge")
    );
    assert!(!config.to_string().contains(&token));
    let toml = fs::read_to_string(setup.path("home/.codex/config.toml"))?;
    assert!(toml.contains("# preserve this comment"));
    assert!(toml.contains("[mcp_servers.other]"));
    assert!(toml.contains("[mcp_servers.beankeeper]"));
    // Upstream dispatch obtains the registered passphrase, preserving policy checks.
    let output = setup
        .command()
        .env("BK_AGENT_TOKEN", &token)
        .args(["company", "list", "--json"])
        .output()?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8(output.stdout)?.contains("personal"));
    Ok(())
}

#[test]
fn standalone_bookkeeper_uses_direct_mode_without_spend_limits() -> TestResult {
    let setup = Setup::new()?;
    let output = setup
        .setup()
        .args(["--preset", "bookkeeper", "--demo"])
        .output()?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let db = setup.db()?;
    let raw = fs::read_to_string(setup.path("books.agent-token"))?;
    let agent = identity::verify(db.read(), &raw)?;
    assert_eq!(agent.write_mode(), WriteMode::Direct);
    for capability in [Capability::Read, Capability::Post, Capability::Propose] {
        assert!(agent.allows(capability)?);
    }
    for capability in [Capability::Approve, Capability::Admin] {
        assert!(!agent.allows(capability)?);
        assert!(identity::authorize(db.read(), Some(&raw), capability).is_err());
    }
    let limits = db::spend_limits::list(db.read(), agent.uri(), None)?;
    assert_eq!(limits.len(), 0, "unexpected spend limits: {limits:?}");
    assert_eq!(db::list_companies(db.read())?.len(), 3);
    Ok(())
}

#[test]
fn rerun_refuses_without_force_and_force_preserves_root_tokens_and_recovery() -> TestResult {
    let setup = Setup::new()?;
    assert!(setup.setup().output()?.status.success());
    let root = root(&setup.db()?)?;
    let files = [
        "books.agent-token",
        "books.owner-token",
        "books.recovery.txt",
    ];
    let original = files
        .map(|name| fs::read(setup.path(name)))
        .into_iter()
        .collect::<Result<Vec<_>, _>>()?;
    let output = setup.setup().output()?;
    assert!(!output.status.success());
    assert!(String::from_utf8(output.stderr)?.contains("--force"));
    let output = setup.setup().arg("--force").output()?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(root, self::root(&setup.db()?)?);
    for (name, bytes) in files.into_iter().zip(original) {
        assert_eq!(fs::read(setup.path(name))?, bytes);
    }
    Ok(())
}

fn root(db: &Db) -> Result<String, Box<dyn Error>> {
    Ok(db
        .read()
        .query_row("SELECT trust_root FROM identity_root", [], |row| row.get(0))?)
}

#[test]
fn invalid_config_fails_before_ledger_or_credential_creation() -> TestResult {
    let setup = Setup::new()?;
    fs::create_dir_all(setup.path("home"))?;
    let config = setup.path("home/.claude.json");
    fs::write(&config, "not-json")?;
    let output = setup.setup().output()?;
    assert!(!output.status.success());
    for name in [
        "books.db",
        "stored-secret",
        "books.agent-token",
        "books.recovery.txt",
    ] {
        assert!(!setup.path(name).exists());
    }
    assert_eq!(fs::read_to_string(config)?, "not-json");
    Ok(())
}

#[test]
fn wrong_recovery_passphrase_cannot_replace_valid_store() -> TestResult {
    let setup = Setup::new()?;
    assert!(setup.setup().output()?.status.success());
    fs::write(setup.path("input-secret"), "wrong-secret")?;
    let output = setup.setup().arg("--force").output()?;
    assert!(!output.status.success());
    assert_eq!(fs::read_to_string(setup.path("stored-secret"))?, SECRET);
    assert!(setup.db().is_ok());
    Ok(())
}

#[test]
fn generated_noninteractive_secret_is_only_in_private_recovery_and_store() -> TestResult {
    let setup = Setup::new()?;
    let output = setup
        .command()
        .args([
            "setup",
            "--non-interactive",
            "--no-start",
            "--no-open",
            "--config-home",
        ])
        .arg(setup.path("home"))
        .arg("--secret-store-file")
        .arg(setup.path("stored-secret"))
        .output()?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let secret = fs::read_to_string(setup.path("stored-secret"))?;
    assert_eq!(secret.len(), 64);
    assert!(!String::from_utf8(output.stdout)?.contains(&secret));
    assert!(!String::from_utf8(output.stderr)?.contains(&secret));
    assert!(fs::read_to_string(setup.path("books.recovery.txt"))?.contains(&secret));
    assert!(Db::open_existing(&setup.path("books.db"), Some(&secret.into())).is_ok());
    Ok(())
}

#[test]
fn missing_keychain_entry_explains_recovery_and_explicit_source_still_works() -> TestResult {
    let setup = Setup::new()?;
    assert!(setup.setup().output()?.status.success());
    let token = fs::read_to_string(setup.path("books.agent-token"))?;
    fs::remove_file(setup.path("stored-secret"))?;
    let output = setup.command().args(["company", "list"]).output()?;
    assert!(!output.status.success());
    assert!(String::from_utf8(output.stderr)?.contains("recovery kit"));
    let output = setup
        .command()
        .arg("--passphrase-file")
        .arg(setup.path("input-secret"))
        .env("BK_AGENT_TOKEN", &token)
        .args(["company", "list"])
        .output()?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(())
}

#[test]
fn help_exposes_setup_and_bridge_without_breaking_upstream_mcp() -> TestResult {
    for args in [
        vec!["setup", "--help"],
        vec!["mcp", "bridge", "--help"],
        vec!["mcp", "serve", "--help"],
    ] {
        let output = Command::new(assert_cmd::cargo::cargo_bin!("bk"))
            .args(args)
            .output()?;
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(())
}

#[test]
fn output_collision_never_overwrites_unrelated_file() -> TestResult {
    let setup = Setup::new()?;
    let kit = setup.path("books.recovery.txt");
    fs::write(&kit, "keep")?;
    assert!(!setup.setup().output()?.status.success());
    assert_eq!(fs::read_to_string(&kit)?, "keep");
    assert!(!Path::new(&setup.path("books.db")).exists());
    Ok(())
}

#[test]
fn credential_and_ledger_paths_cannot_alias() -> TestResult {
    let setup = Setup::new()?;
    let output = setup
        .command()
        .args([
            "setup",
            "--non-interactive",
            "--no-start",
            "--no-open",
            "--config-home",
        ])
        .arg(setup.path("home"))
        .arg("--secret-store-file")
        .arg(setup.path("books.db"))
        .output()?;
    assert!(!output.status.success());
    assert!(String::from_utf8(output.stderr)?.contains("must be distinct"));
    assert!(!setup.path("books.db").exists());
    Ok(())
}

struct Background(u32);
impl Drop for Background {
    fn drop(&mut self) {
        let _ = Command::new("kill")
            .args(["-TERM", &self.0.to_string()])
            .status();
    }
}

#[test]
fn setup_starts_detached_server_and_reuses_only_the_matching_ledger() -> TestResult {
    let setup = Setup::new()?;
    let reservation = std::net::TcpListener::bind("127.0.0.1:0")?;
    let bind = reservation.local_addr()?;
    drop(reservation);
    let output = setup
        .configured()
        .args(["--bind", &bind.to_string()])
        .output()?;
    let stderr = String::from_utf8(output.stderr)?;
    let pid: u32 = stderr
        .split_once("Desktop started (PID ")
        .ok_or_else(|| format!("server failed to start: {stderr}"))?
        .1
        .split(')')
        .next()
        .ok_or("PID")?
        .parse()?;
    let _child = Background(pid);
    assert!(output.status.success(), "{stderr}");
    let client = reqwest::blocking::Client::builder()
        .no_proxy()
        .timeout(std::time::Duration::from_secs(3))
        .build()?;
    assert_eq!(
        client.get(format!("http://{bind}/health")).send()?.status(),
        200
    );
    // No explicit passphrase needed on a resume with registered storage.
    let output = setup
        .command()
        .args([
            "setup",
            "--force",
            "--non-interactive",
            "--no-open",
            "--config-home",
        ])
        .arg(setup.path("home"))
        .args(["--bind", &bind.to_string()])
        .output()?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!String::from_utf8(output.stderr)?.contains("Desktop started (PID"));
    let other = Setup::new()?;
    let output = other
        .configured()
        .args(["--bind", &bind.to_string()])
        .output()?;
    assert!(!output.status.success());
    assert!(String::from_utf8(output.stderr)?.contains("did not accept this ledger's token"));
    // --no-start must not open the wrong ledger's healthy UI either.
    let output = other
        .command()
        .args([
            "setup",
            "--force",
            "--non-interactive",
            "--no-start",
            "--config-home",
        ])
        .arg(other.path("home"))
        .args(["--bind", &bind.to_string()])
        .output()?;
    assert!(!output.status.success());
    assert!(String::from_utf8(output.stderr)?.contains("did not accept this ledger's token"));
    let log = fs::read_to_string(setup.path("books.desktop.log"))?;
    assert!(!log.contains(SECRET));
    assert!(!log.contains(&fs::read_to_string(setup.path("books.agent-token"))?));
    Ok(())
}
