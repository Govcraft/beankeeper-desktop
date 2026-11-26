use super::*;
use beankeeper_cli::{
    cli::Cli,
    commands,
    db::Actor,
    identity::{self, Admin, Granted, IssueRequest},
};
use clap::Parser;

const KEYS: &[u8] = include_bytes!("../../tests/fixtures/license-public-keys.json");
const LICENSE: &[u8] = include_bytes!("../../tests/fixtures/license-v4.public.txt");
const CLEAN: &[u8] = include_bytes!("../../tests/fixtures/revocations-empty.json");
const REVOKED: &[u8] = include_bytes!("../../tests/fixtures/revocations.json");
type TestResult = Result<(), Box<dyn std::error::Error>>;

fn enforced(
    path: &Path,
    state: &Path,
    revocations: &'static [u8],
) -> Result<LedgerAccess, io::Error> {
    Ok(LedgerAccess {
        books: path.try_into()?,
        policy: EnforcementPolicy::Enforced(MajorVersion::try_from(1).map_err(io::Error::other)?),
        test_files: Some(TestFiles {
            state: state.into(),
            keys: KEYS,
            revocations,
        }),
    })
}

#[test]
fn policy_only_enforces_official_stable_releases_from_major_one() -> TestResult {
    for (version, official) in [
        ("0.1.0-rc.30", true),
        ("0.9.0", true),
        ("1.0.0-rc.1", true),
        ("2.0.0-beta.1", true),
        ("1.0.0", false),
    ] {
        assert!(matches!(
            EnforcementPolicy::from_release(&version.parse()?, official),
            EnforcementPolicy::Free
        ));
    }
    for (version, expected) in [("1.0.0", 1), ("2.1.2", 2)] {
        assert!(matches!(
            EnforcementPolicy::from_release(&version.parse()?, true),
            EnforcementPolicy::Enforced(major) if major == MajorVersion::try_from(expected)?
        ));
    }
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("books.db");
    let db = Db::open(&path, None)?;
    let mut access = enforced(&path, dir.path(), CLEAN)?;
    access.policy = EnforcementPolicy::from_release(&"1.0.0-rc.1".parse()?, true);
    let mut cli = Cli::try_parse_from(["bk", "company", "create", "preview", "Preview"])?;
    commands::dispatch_with_db(&mut cli, access.protect(db, LedgerTarget::default()), None)?;
    Ok(())
}

#[test]
fn activation_is_per_ledger_and_signed_revocations_apply_offline() -> TestResult {
    let dir = tempfile::tempdir()?;
    let first = dir.path().join("first.db");
    let second = dir.path().join("second.db");
    fs::File::create(&first)?;
    fs::File::create(&second)?;
    let first_access = enforced(&first, dir.path(), CLEAN)?;
    let second_access = enforced(&second, dir.path(), CLEAN)?;
    let keys = license::TrustedKeys::from_json(KEYS)?;
    let license = license::verify_offline(LICENSE, &keys)?;
    let id = license.id();
    let path = license::activation_path(dir.path(), id);
    let bindings = Bindings(dir.path());
    let major = MajorVersion::try_from(1)?;

    assert!(matches!(
        first_access.current_entitlement(major),
        Entitlement::Missing
    ));
    license::write_private(LICENSE, &path, license::SaveMode::Replace)?;
    assert!(matches!(
        first_access.current_entitlement(major),
        Entitlement::Missing
    ));
    bindings.bind(id, &first_access.books)?;
    bindings.bind(id, &first_access.books)?;
    assert!(bindings.bind(id, &second_access.books).is_err());
    assert!(matches!(
        first_access.current_entitlement(major),
        Entitlement::Active(_)
    ));
    assert!(matches!(
        second_access.current_entitlement(major),
        Entitlement::Missing
    ));
    assert!(matches!(
        first_access.current_entitlement(MajorVersion::try_from(2)?),
        Entitlement::WrongMajor(_)
    ));

    let revoked = enforced(&first, dir.path(), REVOKED)?;
    assert!(
        matches!(revoked.current_entitlement(major), Entitlement::Revoked(revoked_id) if &revoked_id == id)
    );
    let tampered = String::from_utf8(REVOKED.to_vec())?.replace("KIS9Uh", "LIS9Uh");
    assert!(matches!(
        license::VerifiedRevocations::verify(tampered.as_bytes(), &keys),
        Err(LicenseError::Signature)
    ));

    license::write_private(b"invalid", &path, license::SaveMode::Replace)?;
    assert!(matches!(
        first_access.current_entitlement(major),
        Entitlement::Invalid(_)
    ));
    fs::remove_file(path)?;
    assert!(matches!(
        first_access.current_entitlement(major),
        Entitlement::Missing
    ));
    Ok(())
}

#[test]
fn stable_cli_keeps_reads_exports_and_backup_recovery_available() -> TestResult {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("books.db");
    use beankeeper_cli::{
        db::audit::restore::RestoreRequest,
        request::{LedgerRequest, LedgerResponse},
    };

    let McpBooks { db, token } = McpBooks::open(&path)?;
    let access = enforced(&path, dir.path(), CLEAN)?;
    let target = LedgerTarget::default();
    let mut cli = Cli::try_parse_from(["bk", "company", "create", "blocked", "Blocked"])?;
    cli.db.clone_from(&path);
    let result =
        commands::dispatch_with_db(&mut cli, access.clone().protect(db, target.clone()), None);
    let Err(error) = result else {
        panic!("unlicensed CLI write succeeded");
    };
    assert_eq!(error.error_code(), "LEDGER_WRITE_DENIED");
    assert_eq!(error.exit_code(), 7);
    assert!(error.to_string().contains("You can still read, export"));
    assert!(
        access
            .authorize_write(&target, LedgerWriteKind::Restore)
            .is_ok()
    );

    let export = dir.path().join("export.json");
    let backup = dir.path().join("backup.db");
    let recovered = dir.path().join("recovered.db");
    for args in [
        vec!["bk", "company", "list"],
        vec![
            "bk",
            "export",
            "--format",
            "json",
            "--output",
            export.to_str().ok_or("export path")?,
        ],
        vec![
            "bk",
            "backup",
            "--output",
            backup.to_str().ok_or("backup path")?,
        ],
    ] {
        let mut cli = Cli::try_parse_from(args)?;
        cli.db.clone_from(&path);
        let db = access
            .clone()
            .protect(Db::open_existing(&path, None)?, target.clone());
        commands::dispatch_with_db(&mut cli, db, None)?;
    }
    assert!(export.is_file());
    let database = access.protect(Db::open_existing(&path, None)?, target);
    let actor = Actor::new("owner".parse()?)
        .with_identity(identity::verify(database.read(), token.expose_secret())?);
    let response = LedgerRequest::Restore(RestoreRequest::Backup(backup.clone().try_into()?))
        .execute(&database, &actor)?;
    let LedgerResponse::Restore(_, receipt) = response else {
        return Err("expected a restore receipt".into());
    };
    LedgerRequest::Restore(RestoreRequest::Undo(receipt.restore_id()))
        .execute(&database, &actor)?;
    let cli = Cli::try_parse_from([
        "bk",
        "--db",
        recovered.to_str().ok_or("recovery path")?,
        "recover",
        "--input",
        backup.to_str().ok_or("backup path")?,
    ])?;
    commands::dispatch(&cli, None)?;
    assert!(recovered.is_file());
    Ok(())
}

struct McpBooks {
    db: Db,
    token: secrecy::SecretString,
}

impl McpBooks {
    fn open(path: &Path) -> Result<Self, Box<dyn std::error::Error>> {
        let db = Db::open(path, None)?;
        let owner = Actor::new("owner".parse()?);
        Granted::<Admin>::with_db(&db, &owner, identity::init_root)?;
        let caller = Granted::<Admin>::with_db(&db, &owner, |grant| {
            identity::issue_credential(
                grant,
                &IssueRequest {
                    name: "licensing_test",
                    capabilities: &["ledger/read".into(), "ledger/admin".into()],
                    ttl: std::time::Duration::from_secs(600),
                    public_key: None,
                    generate_key: true,
                },
            )
        })?;
        Ok(Self {
            db,
            token: caller.token().clone(),
        })
    }
}

struct McpClient {
    url: String,
    client: reqwest::Client,
    session: reqwest::header::HeaderValue,
}

impl McpClient {
    async fn initialize(
        url: String,
        token: &secrecy::SecretString,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let client = reqwest::Client::builder()
            .no_proxy()
            .timeout(std::time::Duration::from_secs(10))
            .build()?;
        let response = client.post(&url)
            .header("accept", "application/json, text/event-stream")
            .header("mcp-protocol-version", "2025-11-25")
            .bearer_auth(token.expose_secret())
            .json(&serde_json::json!({
                "jsonrpc":"2.0","id":1,"method":"initialize",
                "params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"test","version":"1"}}
            }))
            .send().await?.error_for_status()?;
        let session = response
            .headers()
            .get("mcp-session-id")
            .ok_or("session id")?
            .clone();
        client
            .post(&url)
            .header("accept", "application/json, text/event-stream")
            .header("mcp-protocol-version", "2025-11-25")
            .header("mcp-session-id", &session)
            .bearer_auth(token.expose_secret())
            .json(&serde_json::json!({"jsonrpc":"2.0","method":"notifications/initialized"}))
            .send()
            .await?
            .error_for_status()?;
        Ok(Self {
            url,
            client,
            session,
        })
    }

    async fn call(
        &self,
        target: &str,
        token: &secrecy::SecretString,
        name: &str,
        arguments: serde_json::Value,
    ) -> Result<serde_json::Value, Box<dyn std::error::Error>> {
        let text = self.client.post(&self.url)
            .header("accept", "application/json, text/event-stream")
            .header("mcp-protocol-version", "2025-11-25")
            .header("mcp-session-id", &self.session)
            .header(beankeeper_cli::mcp::LEDGER_TARGET_HEADER, target)
            .bearer_auth(token.expose_secret())
            .json(&serde_json::json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":name,"arguments":arguments}}))
            .send().await?.error_for_status()?.text().await?;
        let json = text
            .lines()
            .find_map(|line| line.strip_prefix("data: "))
            .unwrap_or(&text);
        let result: serde_json::Value = serde_json::from_str(json)?;
        if result["result"].is_null() {
            return Err(format!("no tool response: {result}").into());
        }
        Ok(result["result"].clone())
    }
}

#[tokio::test]
async fn authenticated_mcp_checks_the_selected_ledger_on_every_write() -> TestResult {
    use crate::backups::NoHostTools;
    use beankeeper_cli::mcp::{
        HttpLedgerTargets, LedgerTargets, StreamableHttpServerConfig,
        http_router_with_ledger_targets,
    };
    use serde_json::json;

    let dir = tempfile::tempdir()?;
    let primary = dir.path().join("primary.db");
    let other = dir.path().join("other.db");
    let first = McpBooks::open(&primary)?;
    let second = McpBooks::open(&other)?;
    let first_access = enforced(&primary, dir.path(), CLEAN)?;
    let second_access = enforced(&other, dir.path(), REVOKED)?;
    let other_target: LedgerTarget = "other".parse()?;
    let targets = LedgerTargets::new(
        first_access
            .clone()
            .protect(first.db, LedgerTarget::default()),
    );
    targets.register(
        other_target.clone(),
        second_access.clone().protect(second.db, other_target),
    )?;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let url = format!("http://{}/mcp", listener.local_addr()?);
    let cli = Cli::try_parse_from(["bk", "mcp", "serve", "--http"])?;
    let config = StreamableHttpServerConfig::default();
    let cancel = config.cancellation_token.clone();
    let router = http_router_with_ledger_targets(
        &cli,
        HttpLedgerTargets::from(targets),
        false,
        config,
        NoHostTools,
    )?;
    let server = tokio::spawn(async move { axum::serve(listener, router).await });
    let client = McpClient::initialize(url, &first.token).await?;
    let create = |slug| json!({ "slug": slug, "name": "Fixture" });
    let denied = client
        .call("startup", &first.token, "company_create", create("missing"))
        .await?;
    assert_eq!(denied["structuredContent"]["code"], "LEDGER_WRITE_DENIED");
    assert!(denied.to_string().contains("no active Desktop license"));
    let read = client
        .call("startup", &first.token, "company_list", json!({}))
        .await?;
    assert_ne!(read["isError"], true);

    let keys = license::TrustedKeys::from_json(KEYS)?;
    let valid = license::verify_offline(LICENSE, &keys)?;
    let path = license::activation_path(dir.path(), valid.id());
    let bindings = Bindings(dir.path());
    license::write_private(LICENSE, &path, license::SaveMode::Replace)?;
    bindings.bind(valid.id(), &first_access.books)?;
    let allowed = client
        .call("startup", &first.token, "company_create", create("active"))
        .await?;
    assert_ne!(allowed["isError"], true);
    let denied = client
        .call("other", &second.token, "company_create", create("unbound"))
        .await?;
    assert_eq!(denied["structuredContent"]["code"], "LEDGER_WRITE_DENIED");

    fs::remove_file(bindings.path(valid.id()))?;
    bindings.bind(valid.id(), &second_access.books)?;
    let denied = client
        .call("other", &second.token, "company_create", create("revoked"))
        .await?;
    assert_eq!(denied["structuredContent"]["code"], "LEDGER_WRITE_DENIED");
    assert!(denied.to_string().contains("was revoked"));

    fs::remove_file(path)?;
    let denied = client
        .call(
            "other",
            &second.token,
            "company_create",
            create("deactivated"),
        )
        .await?;
    assert_eq!(denied["structuredContent"]["code"], "LEDGER_WRITE_DENIED");
    assert!(denied.to_string().contains("no active Desktop license"));
    cancel.cancel();
    server.abort();
    Ok(())
}
