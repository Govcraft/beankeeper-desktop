use super::*;
use crate::release_policy::ReleaseMetadata;
use beankeeper_cli::{
    cli::Cli,
    commands,
    db::{Actor, audit::restore::RestoreRequest},
    identity::{self, Admin, Granted, IssueRequest},
    request::{LedgerRequest, LedgerResponse},
};
use clap::Parser;
use std::cell::Cell;

const KEYS: &[u8] = include_bytes!("../../tests/fixtures/license-public-keys.json");
const LICENSE: &[u8] = include_bytes!("../../tests/fixtures/license-v4.public.txt");
const CLEAN: &[u8] = include_bytes!("../../tests/fixtures/revocations-empty.json");
const REVOKED: &[u8] = include_bytes!("../../tests/fixtures/revocations.json");
const PASSPHRASE: &str = "desktop-entitlement-test";
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
            keys: KEYS.into(),
            revocations,
        }),
    })
}

#[test]
fn policy_only_enforces_official_stable_releases_from_major_one() -> TestResult {
    for (major, prerelease, official) in [
        ("0", "rc.30", true),
        ("0", "", true),
        ("1", "rc.1", true),
        ("2", "beta.1", true),
        ("1", "", false),
    ] {
        assert!(matches!(
            EnforcementPolicy::try_from(ReleaseMetadata {
                official,
                major,
                prerelease
            })?,
            EnforcementPolicy::Free
        ));
    }
    for (major, expected) in [("1", 1), ("2", 2)] {
        assert!(matches!(
            EnforcementPolicy::try_from(ReleaseMetadata { official: true, major, prerelease: "" })?,
            EnforcementPolicy::Enforced(major) if major == MajorVersion::try_from(expected)?
        ));
    }
    for major in ["invalid", "4294967296"] {
        assert!(
            EnforcementPolicy::try_from(ReleaseMetadata {
                official: true,
                major,
                prerelease: ""
            })
            .is_err()
        );
    }
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("books.db");
    let db = Db::open(&path, None)?;
    let mut access = enforced(&path, dir.path(), CLEAN)?;
    access.policy = EnforcementPolicy::try_from(ReleaseMetadata {
        official: true,
        major: "1",
        prerelease: "rc.1",
    })?;
    let mut cli = Cli::try_parse_from(["bk", "company", "create", "preview", "Preview"])?;
    commands::dispatch_with_db(&mut cli, access.protect(db, LedgerTarget::default()), None)?;
    Ok(())
}

fn active_fixture(
    keys: &license::TrustedKeys,
) -> Result<ActiveLicense, Box<dyn std::error::Error>> {
    let verified = license::verify_offline(LICENSE, keys)?;
    let unrevoked = license::VerifiedRevocations::verify(CLEAN, keys)?.admit(verified)?;
    Ok((unrevoked, MajorVersion::try_from(1)?).try_into()?)
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
    let active = active_fixture(&keys)?;
    let id = active.id();
    let path = license::activation_path(dir.path(), id);
    let bindings = Bindings(dir.path());
    let major = MajorVersion::try_from(1)?;

    assert!(matches!(
        first_access.current_entitlement(major),
        Entitlement::Missing
    ));
    active.persist(dir.path())?;
    assert!(matches!(
        first_access.current_entitlement(major),
        Entitlement::Missing
    ));
    bindings.bind(&active, &first_access.books)?;
    bindings.bind(&active, &first_access.books)?;
    assert!(matches!(
        bindings.bind(&active, &second_access.books),
        Err(ProductError::LicenseBoundElsewhere)
    ));
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
    let verified_revocations = license::VerifiedRevocations::verify(REVOKED, &keys)?;
    let status = revoked.status_from(dir.path(), |id| {
        load_activation(
            &license::activation_path(dir.path(), id),
            &keys,
            &verified_revocations,
        )
    });
    assert!(!status.writable());
    assert!(status.to_string().contains("was revoked"));
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
    let McpBooks { db, token } = McpBooks::open(&path)?;
    let passphrase = secrecy::SecretString::from(PASSPHRASE);
    let passphrase_file = crate::setup::temporary_passphrase(&passphrase)?;
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
            .protect(Db::open_existing(&path, Some(&passphrase))?, target.clone());
        commands::dispatch_with_db(&mut cli, db, None)?;
    }
    assert!(export.is_file());
    let database = access.protect(Db::open_existing(&path, Some(&passphrase))?, target);
    let actor = Actor::new("owner".parse()?)
        .with_identity(identity::verify(database.read(), token.expose_secret())?);
    let response = LedgerRequest::Restore(RestoreRequest::Backup(backup.clone().try_into()?))
        .execute(&database, &actor)?;
    let LedgerResponse::Restore(_, receipt) = response else {
        return Err("expected a restore receipt".into());
    };
    LedgerRequest::Restore(RestoreRequest::Undo(receipt.restore_id()))
        .execute(&database, &actor)?;
    let mut cli = Cli::try_parse_from([
        "bk",
        "--db",
        recovered.to_str().ok_or("recovery path")?,
        "recover",
        "--input",
        backup.to_str().ok_or("backup path")?,
    ])?;
    cli.passphrase.passphrase_file = Some(passphrase_file.path().into());
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
        let db = Db::open(path, Some(&secrecy::SecretString::from(PASSPHRASE)))?;
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
    next_id: Cell<u64>,
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
        response.text().await?;
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
            next_id: Cell::new(2),
        })
    }

    async fn call(
        &self,
        target: &str,
        token: &secrecy::SecretString,
        name: &str,
        arguments: serde_json::Value,
    ) -> Result<serde_json::Value, Box<dyn std::error::Error>> {
        let id = self.next_id.get();
        self.next_id.set(id + 1);
        let text = self.client.post(&self.url)
            .header("accept", "application/json, text/event-stream")
            .header("mcp-protocol-version", "2025-11-25")
            .header("mcp-session-id", &self.session)
            .header(beankeeper_cli::mcp::LEDGER_TARGET_HEADER, target)
            .bearer_auth(token.expose_secret())
            .json(&serde_json::json!({"jsonrpc":"2.0","id":id,"method":"tools/call","params":{"name":name,"arguments":arguments}}))
            .send().await?.error_for_status()?.text().await?;
        let result: serde_json::Value = serde_json::from_str(&text)
            .ok()
            .or_else(|| {
                text.lines()
                    .filter_map(|line| line.strip_prefix("data:"))
                    .filter_map(|data| serde_json::from_str::<serde_json::Value>(data.trim()).ok())
                    .find(|value| value["id"] == id)
            })
            .ok_or_else(|| format!("missing MCP response for {name} on {target}: {text}"))?;
        if result["result"].is_null() {
            return Err(format!("no tool response: {result}").into());
        }
        Ok(result["result"].clone())
    }
}

#[tokio::test]
async fn authenticated_mcp_checks_the_selected_ledger_on_every_write() -> TestResult {
    use beankeeper_cli::mcp::LedgerTargets;
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
    let (client, cancel, server) = mcp_client(targets, &first.token).await?;
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
    let valid = active_fixture(&keys)?;
    let path = license::activation_path(dir.path(), valid.id());
    let bindings = Bindings(dir.path());
    valid.persist(dir.path())?;
    bindings.bind(&valid, &first_access.books)?;
    let allowed = client
        .call("startup", &first.token, "company_create", create("active"))
        .await?;
    assert_ne!(allowed["isError"], true);
    let denied = client
        .call("other", &second.token, "company_create", create("unbound"))
        .await?;
    assert_eq!(denied["structuredContent"]["code"], "LEDGER_WRITE_DENIED");

    fs::remove_file(bindings.path(valid.id()))?;
    bindings.bind(&valid, &second_access.books)?;
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

async fn mcp_client(
    targets: beankeeper_cli::mcp::LedgerTargets,
    token: &secrecy::SecretString,
) -> Result<
    (
        McpClient,
        tokio_util::sync::CancellationToken,
        tokio::task::JoinHandle<io::Result<()>>,
    ),
    Box<dyn std::error::Error>,
> {
    use crate::backups::NoHostTools;
    use beankeeper_cli::mcp::{
        HttpLedgerTargets, StreamableHttpServerConfig, http_router_with_ledger_targets,
    };
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
    let client = McpClient::initialize(url, token).await?;
    Ok((client, cancel, server))
}

#[tokio::test]
async fn cli_and_mcp_undo_receipts_on_the_same_read_only_ledger() -> TestResult {
    use beankeeper_cli::mcp::LedgerTargets;
    use serde_json::json;

    let dir = tempfile::tempdir()?;
    let path = dir.path().join("books.db");
    let McpBooks { db, token } = McpBooks::open(&path)?;
    let passphrase = secrecy::SecretString::from(PASSPHRASE);
    let mut company = Cli::try_parse_from(["bk", "company", "create", "acme", "Acme"])?;
    company.db.clone_from(&path);
    commands::dispatch_with_db(&mut company, db, None)?;
    for code in ["1000", "1100"] {
        let mut account = Cli::try_parse_from([
            "bk",
            "--company",
            "acme",
            "account",
            "create",
            code,
            "Cash",
            "--type",
            "asset",
        ])?;
        account.db.clone_from(&path);
        commands::dispatch_with_db(
            &mut account,
            Db::open_existing(&path, Some(&passphrase))?,
            None,
        )?;
    }

    let access = enforced(&path, dir.path(), CLEAN)?;
    let open = || {
        Db::open_existing(&path, Some(&passphrase))
            .map(|db| access.clone().protect(db, LedgerTarget::default()))
    };
    let (client, cancel, server) = mcp_client(LedgerTargets::new(open()?), &token).await?;
    let page = client
        .call(
            "startup",
            &token,
            "audit_receipts",
            json!({"company":"acme"}),
        )
        .await?;
    let receipts = page["structuredContent"]["receipts"]
        .as_array()
        .ok_or_else(|| format!("missing audit receipts: {page}"))?;
    let receipt = |code| {
        receipts
            .iter()
            .find(|row| row["change"]["target"]["id"] == code)
            .and_then(|row| row["id"].as_i64())
            .ok_or("missing account receipt")
    };
    let cli_receipt = receipt("1000")?;
    let mcp_receipt = receipt("1100")?;
    let mut undo = Cli::try_parse_from([
        "bk",
        "--company",
        "acme",
        "audit",
        "undo",
        &cli_receipt.to_string(),
    ])?;
    undo.db.clone_from(&path);
    undo.agent_token = Some(token.clone());
    commands::dispatch_with_db(&mut undo, open()?, None)?;

    for (original, code) in [(cli_receipt, "1000"), (mcp_receipt, "1100")] {
        let args = json!({"company":"acme", "receipt": original});
        let undone = client
            .call("startup", &token, "audit_undo", args.clone())
            .await?;
        let result = &undone["structuredContent"];
        assert_eq!(result["kind"], "archived", "{undone}");
        assert_eq!(result["account"], code);
        assert_eq!(result["undo_of"], original);
        let retried = client.call("startup", &token, "audit_undo", args).await?;
        assert_eq!(&retried["structuredContent"], result);
    }
    let denied = client
        .call(
            "startup",
            &token,
            "company_create",
            json!({"slug":"blocked", "name":"Blocked"}),
        )
        .await?;
    assert_eq!(denied["structuredContent"]["code"], "LEDGER_WRITE_DENIED");
    cancel.cancel();
    server.abort();
    Ok(())
}

#[test]
fn removing_only_license_keeps_books_intact_and_enforces_read_only() -> TestResult {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("books.db");
    fs::write(&path, "unchanged books")?;
    let access = enforced(&path, dir.path(), CLEAN)?;
    let active = active_fixture(&license::TrustedKeys::from_json(KEYS)?)?;
    active.persist(dir.path())?;
    Bindings(dir.path()).bind(&active, &access.books)?;
    assert!(access.status().writable());
    let keys = license::TrustedKeys::from_json(KEYS)?;
    let other = license::verify_offline(
        include_bytes!("../../tests/fixtures/license-major-2-v4.public.txt"),
        &keys,
    )?;
    let other = license::VerifiedRevocations::verify(CLEAN, &keys)?.admit(other)?;
    let other = ActiveLicense::try_from((other, MajorVersion::try_from(2)?))?;
    other.persist(dir.path())?;
    Bindings(dir.path()).bind(&other, &access.books)?;
    crate::product::removal::remove_at(dir.path(), other.id())?;
    assert!(access.status().writable());
    assert!(license::activation_path(dir.path(), active.id()).exists());
    crate::product::removal::remove_at(dir.path(), active.id())?;
    assert!(!access.status().writable());
    assert_eq!(fs::read_to_string(path)?, "unchanged books");
    Ok(())
}

#[test]
fn trial_expiry_pauses_only_ordinary_writes_and_rechecks_each_time() -> TestResult {
    use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
    use ed25519_dalek::SigningKey;
    use rusty_paseto::core::{
        Footer, ImplicitAssertion, Key, Paseto, PasetoAsymmetricPrivateKey, Payload, Public, V4,
    };
    use serde_json::json;

    let dir = tempfile::tempdir()?;
    let path = dir.path().join("books.db");
    fs::write(&path, "books remain unchanged")?;
    let signing = SigningKey::from_bytes(&[7; 32]);
    let mut trust: serde_json::Value = serde_json::from_slice(KEYS)?;
    trust["trial-test"] = json!(URL_SAFE_NO_PAD.encode(signing.verifying_key().to_bytes()));
    let keys_bytes = serde_json::to_vec(&trust)?;
    let keys = license::TrustedKeys::from_json(&keys_bytes)?;
    let mut access = enforced(&path, dir.path(), CLEAN)?;
    access.test_files.as_mut().ok_or("test files")?.keys = keys_bytes.into();
    let active = active_fixture(&keys)?;
    Bindings(dir.path()).bind(&active, &access.books)?;
    let stored = license::activation_path(dir.path(), active.id());
    let sign =
        |expiry: chrono::DateTime<chrono::Utc>| -> Result<String, Box<dyn std::error::Error>> {
            let claims = json!({
            "jti": active.id().to_string(), "product": "beankeeper-desktop", "purchaser_ref": "trial-test",
            "storefront": "trial", "eligible_major": 1, "iat": "2020-01-01T00:00:00Z", "exp": expiry,
        }).to_string();
            let key = Key::from(signing.to_keypair_bytes());
            let mut paseto = Paseto::<V4, Public>::default();
            paseto
                .set_payload(Payload::from(claims.as_str()))
                .set_footer(Footer::from("{\"kid\":\"trial-test\"}"))
                .set_implicit_assertion(ImplicitAssertion::from("beankeeper-desktop"));
            Ok(paseto.try_sign(&PasetoAsymmetricPrivateKey::from(&key))?)
        };
    let now = chrono::Utc::now();
    let future = sign(now + chrono::TimeDelta::hours(1))?;
    license::write_private(future.as_bytes(), &stored, license::SaveMode::Replace)?;
    let target = LedgerTarget::default();
    assert!(
        access
            .authorize_write(&target, LedgerWriteKind::Ordinary)
            .is_ok()
    );
    let expired = sign(now - chrono::TimeDelta::minutes(6))?;
    license::write_private(expired.as_bytes(), &stored, license::SaveMode::Replace)?;
    assert!(matches!(
        access.current_entitlement(MajorVersion::try_from(1)?),
        Entitlement::Expired
    ));
    let refusal = access
        .authorize_write(&target, LedgerWriteKind::Ordinary)
        .err()
        .ok_or("must refuse")?;
    assert!(refusal.to_string().contains("Nothing was lost"));
    assert!(
        access
            .authorize_write(&target, LedgerWriteKind::Restore)
            .is_ok()
    );
    assert!(
        access
            .authorize_write(&target, LedgerWriteKind::Reversal)
            .is_ok()
    );
    assert_eq!(fs::read_to_string(path)?, "books remain unchanged");
    Ok(())
}

#[tokio::test]
async fn expired_owner_gate_does_not_block_demo_mcp_writes() -> TestResult {
    use crate::demo::{DemoBooks, DemoOperation};
    use beankeeper_cli::mcp::LedgerTargets;
    use serde_json::json;

    struct ExpiredOwner;
    impl LedgerWriteAuthorizer for ExpiredOwner {
        fn authorize_write(
            &self,
            target: &LedgerTarget,
            _: LedgerWriteKind,
        ) -> Result<(), WriteDenied> {
            assert_eq!(
                target,
                &LedgerTarget::default(),
                "demo consulted the owner gate"
            );
            Err("expired trial".into())
        }
    }

    let root = tempfile::tempdir()?;
    let owner = root.path().join("owner.db");
    let McpBooks { db, token } = McpBooks::open(&owner)?;
    let targets =
        LedgerTargets::new(db.with_write_authorizer(LedgerTarget::default(), ExpiredOwner));
    let demo = DemoBooks::new(
        targets.clone(),
        &owner,
        tokio_util::sync::CancellationToken::new(),
    );
    let started = demo
        .call(
            DemoOperation::Start,
            serde_json::from_value(json!({"name":"demo_start"}))?,
        )
        .await;
    let content = started.structured_content.ok_or("missing demo result")?;
    let target = content["target"].as_str().ok_or("missing demo target")?;
    let (client, cancel, server) = mcp_client(targets, &token).await?;
    let create = json!({"slug":"demo-check", "name":"Demo check"});
    let denied = client
        .call("startup", &token, "company_create", create.clone())
        .await?;
    assert_eq!(denied["structuredContent"]["code"], "LEDGER_WRITE_DENIED");
    assert!(denied.to_string().contains("expired trial"));
    let allowed = client
        .call(target, &token, "company_create", create)
        .await?;
    assert_ne!(allowed["isError"], true, "{allowed}");
    demo.remove().await?;
    let owner = Db::open_existing(&owner, Some(&PASSPHRASE.into()))?;
    assert!(beankeeper_cli::db::list_companies(owner.read())?.is_empty());
    cancel.cancel();
    server.abort();
    Ok(())
}
