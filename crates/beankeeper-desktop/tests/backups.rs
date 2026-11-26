//! Exercise the Desktop backup catalog through both public agent interfaces.

pub mod common;

use std::{
    fs,
    io::{BufRead, BufReader},
    path::Path,
    process::Command,
    time::Duration,
};

use beankeeper::types::{AccountType, Currency, DebitOrCredit, DocumentType};
use beankeeper_cli::{
    capability::Capability,
    db::{
        self, ConflictStrategy, CreateAccountParams, PostEntryParams, PostTransactionParams,
        StoreAttachmentParams, ids::TransactionId, verification::VerifiedLedger,
    },
    identity::{self, Admin, Granted, IssueRequest, Post},
    mcp::LEDGER_TARGET_HEADER,
};
use serde_json::{Value, json};

use common::{Ledger, Server, TestResult, rpc, token};

fn cli(ledger: &Ledger, bearer: &str) -> Command {
    let mut command = ledger.command();
    command
        .env("BK_AGENT_TOKEN", bearer)
        .args(["--json", "backups"]);
    command
}

fn data(command: &mut Command) -> TestResult<Value> {
    let output = command.output()?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(value["ok"], true);
    Ok(value["data"].clone())
}

fn call(server: &Server, bearer: &str, name: &str, arguments: &Value) -> TestResult<Value> {
    let value = rpc(server.request(
        "tools/call",
        json!({"name":name,"arguments":arguments}),
        Some(bearer),
    )?)?;
    Ok(value["result"]["structuredContent"].clone())
}

fn admin(ledger: &Ledger) -> TestResult<String> {
    let db = ledger.db()?;
    let grant = Granted::<Admin>::with_db(&db, ledger.owner(), |grant| {
        identity::issue(
            grant,
            &IssueRequest {
                name: "desktop_backup_admin",
                capabilities: &[Capability::Read, Capability::Admin]
                    .map(|capability| capability.path().to_owned()),
                ttl: Duration::from_hours(1),
                public_key: None,
                generate_key: true,
            },
        )
    })?;
    Ok(token(&grant)?.to_owned())
}

fn local_backup(ledger: &Ledger, name: &str) -> std::path::PathBuf {
    ledger
        .path
        .with_file_name(".beankeeper-backups")
        .join("books.db")
        .join(name)
}

fn accounts(ledger: &Ledger) -> TestResult {
    let company = "acme".parse()?;
    for (code, name, account_type) in [
        ("1000", "Cash", AccountType::Asset),
        ("4000", "Sales", AccountType::Revenue),
    ] {
        Granted::<Admin>::with_db(&ledger.db()?, ledger.owner(), |grant| {
            db::create_account(
                grant,
                &CreateAccountParams {
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

fn post_with_attachment(ledger: &Ledger, source: &Path) -> TestResult<TransactionId> {
    let db = ledger.db()?;
    let company = "acme".parse()?;
    let date = "2026-11-20".parse()?;
    let entries = [
        PostEntryParams {
            account_code: "1000".into(),
            direction: DebitOrCredit::Debit,
            amount: 2500.try_into()?,
            memo: None,
            tax_category: None,
        },
        PostEntryParams {
            account_code: "4000".into(),
            direction: DebitOrCredit::Credit,
            amount: 2500.try_into()?,
            memo: None,
            tax_category: None,
        },
    ];
    let (hash, _) = db::hash_and_store_file(source, &ledger.path)?;
    let db::PostResult::Created(id) = Granted::<Post>::with_db(&db, ledger.owner(), |grant| {
        db::post_transaction(
            grant,
            &PostTransactionParams {
                company_slug: &company,
                description: "Backup fixture",
                metadata: None,
                currency: Currency::USD,
                date,
                entries: &entries,
                correlate: None,
                reference: None,
                on_conflict: ConflictStrategy::Error,
                actor: ledger.owner(),
            },
        )
    })?
    else {
        return Err("fixture posting was not created".into());
    };
    Granted::<Post>::with_db(&db, ledger.owner(), |grant| {
        db::store_attachment(
            grant,
            &StoreAttachmentParams {
                transaction_id: id,
                entry_id: None,
                company_slug: &company,
                uri: &format!("attachments/{hash}"),
                document_type: DocumentType::Other,
                hash: Some(&hash),
                original_filename: source.file_name().and_then(|name| name.to_str()),
            },
        )
    })?;
    Ok(id)
}

fn contains_txn(ledger: &Ledger, txn: TransactionId) -> TestResult<bool> {
    Ok(ledger.fixture_sql()?.query_row(
        "SELECT EXISTS (SELECT 1 FROM transactions WHERE id = ?1)",
        [txn],
        |row| row.get(0),
    )?)
}

#[test]
fn standalone_desktop_replays_missed_startup_ledger_response() -> TestResult {
    let ledger = Ledger::new()?;
    let bearer = token(&ledger.reader)?;
    let mut server = Server::start(&ledger, &["serve"], None)?;
    server.initialize(Some(bearer))?;
    let expected = data(cli(&ledger, bearer).arg("list"))?;

    // Hold the tool in flight while the first stream drops and its replacement attaches.
    let catalog_lock = fs::File::options()
        .read(true)
        .write(true)
        .open(local_backup(&ledger, "catalog.lock"))?;
    catalog_lock.lock()?;
    let original = server.request(
        "tools/call",
        json!({"name":"backup_list", "arguments":{}}),
        Some(bearer),
    )?;
    assert_eq!(original.status(), 200);
    let mut lines = BufReader::new(original).lines();
    let checkpoint = loop {
        let line = lines.next().ok_or("missing SSE checkpoint")??;
        if let Some(id) = line.strip_prefix("id:") {
            break id.trim().to_owned();
        }
    };
    drop(lines);

    let session = server.session.as_deref().ok_or("session")?;
    let resume = || {
        server
            .client
            .get(format!("{}/mcp", server.base))
            .header("Accept", "text/event-stream")
            .header("Mcp-Session-Id", session)
            .header("MCP-Protocol-Version", "2025-11-25")
            .header("Last-Event-ID", &checkpoint)
    };
    assert_eq!(resume().bearer_auth("invalid").send()?.status(), 401);
    let resumed = resume().bearer_auth(bearer).send()?;
    drop(catalog_lock);
    let replayed = rpc(resumed)?;
    assert_eq!(replayed["id"], 1);
    assert_eq!(replayed["result"]["structuredContent"], expected);
    Ok(())
}

#[test]
fn upstream_http_backs_up_startup_and_named_books_before_serving() -> TestResult {
    let startup = Ledger::new()?;
    let mut named = Ledger::new()?;
    let named_path = named.path.with_file_name("books=secondary.db");
    fs::rename(&named.path, &named_path)?;
    named.path = named_path;
    let binding = format!("secondary={}", named.path.display());
    let mut command = startup.command();
    command.args([
        "--ledger",
        &binding,
        "--ledger-target",
        "secondary",
        "mcp",
        "serve",
        "--http",
        "--bind",
        "127.0.0.1:0",
    ]);
    let mut server = Server::launch(command)?;
    for ledger in [&startup, &named] {
        let catalog = data(cli(ledger, token(&ledger.reader)?).arg("list"))?;
        assert_eq!(catalog["backups"][0]["reason"], "first_backup");
    }

    let bearer = token(&named.reader)?;
    let expected = data(startup.command().args([
        "--ledger",
        &binding,
        "--ledger-target",
        "secondary",
        "--agent-token",
        bearer,
        "--json",
        "company",
        "list",
    ]))?;
    server.initialize(Some(token(&startup.reader)?))?;
    let response = server
        .client
        .post(format!("{}/mcp", server.base))
        .bearer_auth(bearer)
        .header("Accept", "application/json, text/event-stream")
        .header("MCP-Protocol-Version", "2025-11-25")
        .header("Mcp-Session-Id", server.session.as_deref().ok_or("session")?)
        .header(LEDGER_TARGET_HEADER, "secondary")
        .json(&json!({"jsonrpc":"2.0", "id":2, "method":"tools/call", "params":{"name":"company_list", "arguments":{}}}))
        .send()?;
    let result = &rpc(response)?["result"];
    let content: Value =
        serde_json::from_str(result["content"][0]["text"].as_str().ok_or("content")?)?;
    assert_eq!(content, expected);
    Ok(())
}

#[test]
fn agent_restores_encrypted_backup_and_attachments_and_undoes_with_either_interface() -> TestResult
{
    let ledger = Ledger::new()?;
    accounts(&ledger)?;
    let dir = tempfile::tempdir()?;
    let statement = dir.path().join("receipt.txt");
    let archived_bytes = b"private document from the first backup";
    fs::write(&statement, archived_bytes)?;
    let earlier = post_with_attachment(&ledger, &statement)?;
    let (_, earlier_path) = db::hash_and_store_file(&statement, &ledger.path)?;

    let admin = admin(&ledger)?;
    let reader = token(&ledger.reader)?;
    let mut server = Server::start(&ledger, &["serve"], None)?;
    server.initialize(Some(&admin))?;
    let list = data(cli(&ledger, reader).arg("list"))?;
    assert_eq!(list, call(&server, &admin, "backup_list", &json!({}))?);
    let binding = format!("primary={}", ledger.path.display());
    let mut named = common::clean_command();
    named.args(["--ledger", &binding, "--ledger-target", "primary"]);
    named.arg("--passphrase-file").arg(&ledger.secret);
    named
        .env("BK_AGENT_TOKEN", reader)
        .args(["--json", "backups", "list"]);
    assert_eq!(list, data(&mut named)?);
    let backups = list["backups"].as_array().ok_or("backups")?;
    assert_eq!(backups.len(), 1);
    let backup = &backups[0];
    assert_eq!(backup["archive_format"], "age_tar");
    assert_eq!(backup["reason"], "first_backup");
    let id = backup["id"].as_str().ok_or("backup ID")?;
    let directory = local_backup(&ledger, id);
    assert!(db::Db::open_snapshot(&directory.join("books.db"), None).is_err());
    let passphrase = secrecy::SecretString::from(fs::read_to_string(&ledger.secret)?);
    db::Db::open_snapshot(&directory.join("books.db"), Some(&passphrase))?;
    let archived = fs::read(directory.join("attachments.tar.age"))?;
    assert!(archived.starts_with(b"age-encryption.org/v1"));
    assert!(
        !archived
            .windows(archived_bytes.len())
            .any(|w| w == archived_bytes)
    );

    fs::remove_file(&earlier_path)?;
    fs::write(&statement, b"document added after the backup")?;
    let later = post_with_attachment(&ledger, &statement)?;
    let (_, later_path) = db::hash_and_store_file(&statement, &ledger.path)?;
    let missing_id = "bkbackup_00000000000000000000000000";
    let error = call(
        &server,
        &admin,
        "backup_restore",
        &json!({"backup_id": missing_id}),
    )?;
    let failed = cli(&ledger, &admin)
        .args(["restore", missing_id])
        .output()?;
    assert!(!failed.status.success());
    let envelope: Value = serde_json::from_slice(&failed.stderr)?;
    assert_eq!(error["code"], "NOT_FOUND");
    assert_eq!(envelope["error"]["code"], error["code"]);

    let receipt = call(&server, &admin, "backup_restore", &json!({"backup_id": id}))?;
    let undo = receipt["undo_id"].as_i64().ok_or("restore undo receipt")?;
    assert!(contains_txn(&ledger, earlier)?);
    assert!(!contains_txn(&ledger, later)?);
    assert_eq!(fs::read(&earlier_path)?, archived_bytes);
    assert!(later_path.exists());
    let undone = data(cli(&ledger, &admin).args(["undo", &undo.to_string()]))?;
    assert_eq!(undone["undo_of"], undo);
    assert!(contains_txn(&ledger, later)?);

    fs::remove_file(&earlier_path)?;
    let cli_restore = data(cli(&ledger, &admin).args(["restore", id]))?;
    let second_undo = cli_restore["undo_id"].as_i64().ok_or("CLI undo receipt")?;
    assert!(!contains_txn(&ledger, later)?);
    assert_eq!(fs::read(&earlier_path)?, archived_bytes);
    let undone = call(
        &server,
        &admin,
        "backup_undo",
        &json!({"undo_id": second_undo}),
    )?;
    assert_eq!(undone["undo_of"], second_undo);
    assert!(contains_txn(&ledger, later)?);
    let db = ledger.db()?;
    assert!(db::policy::get(db.read())?.enforce);
    VerifiedLedger::read(&db)?;
    Ok(())
}

#[test]
fn backup_catalog_rotates_and_same_schema_upgrade_takes_a_new_backup() -> TestResult {
    let ledger = Ledger::new()?;
    let admin = admin(&ledger)?;
    let mut server = Server::start(&ledger, &["serve"], None)?;
    server.initialize(Some(&admin))?;
    let list = data(cli(&ledger, &admin).arg("list"))?;
    // Simulate a build with the same Desktop and schema versions but a different core revision.
    let id = list["backups"][0]["id"]
        .as_str()
        .ok_or("latest backup ID")?;
    let manifest = local_backup(&ledger, id).join("backup.json");
    let mut previous: Value = serde_json::from_slice(&fs::read(&manifest)?)?;
    let (desktop, core) = previous["release"]
        .as_str()
        .and_then(|release| release.split_once('/'))
        .ok_or("backup release")?;
    assert_eq!(core, env!("BK_CORE_REVISION"));
    let other = if core.starts_with('0') { '1' } else { '0' };
    previous["release"] = format!("{desktop}/{other}{}", &core[1..]).into();
    fs::write(&manifest, serde_json::to_vec(&previous)?)?;
    drop(server);
    let server = Server::start(&ledger, &["serve"], None)?;
    let list = data(cli(&ledger, &admin).arg("list"))?;
    assert_ne!(list["backups"][0]["id"], id);
    assert_eq!(list["backups"][0]["reason"], "upgrade");

    let upgrade = list["backups"][0]["id"]
        .as_str()
        .ok_or("upgrade backup ID")?;
    let limit = list["retention"]["max_backups"].as_u64().ok_or("limit")?;
    let mut server = server;
    server.initialize(Some(&admin))?;
    let on_demand = call(&server, &admin, "backup_create", &json!({}))?;
    let first = on_demand["backup"]["id"].as_str().ok_or("backup ID")?;
    for _ in 0..limit {
        data(cli(&ledger, &admin).arg("create"))?;
    }
    let rotated = data(cli(&ledger, &admin).arg("list"))?;
    assert_eq!(
        rotated["backups"].as_array().ok_or("backups")?.len(),
        usize::try_from(limit)?
    );
    assert!(!local_backup(&ledger, first).exists());
    assert!(local_backup(&ledger, upgrade).join("books.db").is_file());
    assert!(
        rotated["backups"]
            .as_array()
            .ok_or("backups")?
            .iter()
            .any(|backup| backup["id"] == upgrade && backup["reason"] == "upgrade")
    );

    let manifest = local_backup(&ledger, upgrade).join("backup.json");
    let mut yesterday: Value = serde_json::from_slice(&fs::read(&manifest)?)?;
    yesterday["created_at"] = (chrono::Utc::now() - chrono::TimeDelta::hours(25))
        .to_rfc3339()
        .into();
    fs::write(manifest, serde_json::to_vec(&yesterday)?)?;
    drop(server);
    let _server = Server::start(&ledger, &["serve"], None)?;
    let list = data(cli(&ledger, &admin).arg("list"))?;
    assert_ne!(list["backups"][0]["id"], upgrade);
    assert_eq!(list["backups"][0]["reason"], "daily");
    assert!(local_backup(&ledger, upgrade).is_dir());
    Ok(())
}

#[test]
fn unreadable_manifest_is_reported_and_other_backups_stay_available() -> TestResult {
    let ledger = Ledger::new()?;
    let admin = admin(&ledger)?;
    let bad = data(cli(&ledger, &admin).arg("create"))?;
    let bad = bad["backup"]["id"].as_str().ok_or("backup ID")?;
    let manifest = local_backup(&ledger, bad).join("backup.json");
    fs::write(&manifest, b"unreadable manifest")?;

    let created = data(cli(&ledger, &admin).arg("create"))?;
    let id = created["backup"]["id"].as_str().ok_or("backup ID")?;
    let output = cli(&ledger, &admin).arg("list").output()?;
    assert!(output.status.success());
    let stderr = String::from_utf8(output.stderr)?;
    assert!(stderr.contains(&format!("Skipping unreadable local backup {bad}")));
    let envelope: Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(
        envelope["data"]["backups"]
            .as_array()
            .ok_or("backups")?
            .len(),
        1
    );
    assert_eq!(envelope["data"]["backups"][0]["id"], id);
    assert!(manifest.is_file());
    Ok(())
}

#[cfg(unix)]
#[test]
fn committed_capture_succeeds_when_an_expired_backup_cannot_be_removed() -> TestResult {
    use std::os::unix::fs::PermissionsExt;

    let ledger = Ledger::new()?;
    let admin = admin(&ledger)?;
    let first = data(cli(&ledger, &admin).arg("create"))?;
    let first = first["backup"]["id"].as_str().ok_or("backup ID")?;
    let directory = local_backup(&ledger, first);
    let manifest = directory.join("backup.json");
    let mut expired: Value = serde_json::from_slice(&fs::read(&manifest)?)?;
    expired["bytes"] = data(cli(&ledger, &admin).arg("list"))?["retention"]["max_bytes"].clone();
    fs::write(&manifest, serde_json::to_vec(&expired)?)?;
    let original = fs::metadata(&directory)?.permissions();
    fs::set_permissions(&directory, fs::Permissions::from_mode(0o500))?;
    let output = cli(&ledger, &admin).arg("create").output();
    if directory.exists() {
        fs::set_permissions(&directory, original)?;
    }
    let output = output?;
    let stderr = String::from_utf8(output.stderr)?;
    assert!(output.status.success(), "{stderr}");
    // Root can remove a directory regardless of its permission bits.
    if directory.exists() {
        assert!(stderr.contains(&format!("Cannot rotate local backup {first}")));
    }
    let envelope: Value = serde_json::from_slice(&output.stdout)?;
    let id = envelope["data"]["backup"]["id"]
        .as_str()
        .ok_or("backup ID")?;
    assert!(local_backup(&ledger, id).join("backup.json").is_file());
    Ok(())
}

#[test]
fn reader_and_read_only_service_cannot_call_restore() -> TestResult {
    let ledger = Ledger::new()?;
    let reader = token(&ledger.reader)?;
    let mut server = Server::start(&ledger, &["serve"], None)?;
    server.initialize(Some(reader))?;
    let list = rpc(server.request("tools/list", json!({}), Some(reader))?)?;
    let tools = list["result"]["tools"].as_array().ok_or("tools")?;
    assert!(tools.iter().any(|tool| tool["name"] == "backup_list"));
    assert!(tools.iter().all(|tool| tool["name"] != "backup_restore"));
    let id = call(&server, reader, "backup_list", &json!({}))?["backups"][0]["id"].clone();
    let error = call(&server, reader, "backup_restore", &json!({"backup_id": id}))?;
    assert_eq!(error["code"], "FORBIDDEN");
    drop(server);

    let admin = admin(&ledger)?;
    let mut server = Server::start(&ledger, &["serve", "--read-only"], None)?;
    server.initialize(Some(&admin))?;
    let list = rpc(server.request("tools/list", json!({}), Some(&admin))?)?;
    let tools = list["result"]["tools"].as_array().ok_or("tools")?;
    assert!(tools.iter().any(|tool| tool["name"] == "backup_list"));
    assert!(tools.iter().all(|tool| tool["name"] != "backup_restore"));
    Ok(())
}
