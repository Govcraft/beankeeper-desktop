pub mod common;

use std::time::Duration;

use base64::{Engine as _, engine::general_purpose::STANDARD};
use beankeeper::types::Sha256Hex;
use beankeeper_cli::{
    capability::Capability,
    db::audit::setup::Generation,
    identity::{self, Admin, Granted, IssueRequest, IssuedCredential},
};
use common::{Ledger, Server, TestResult, rpc, token};
use ed25519_dalek::SigningKey;
use secrecy::{ExposeSecret, SecretString};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

const BOOK: &str = "household";
const BANK: &str = "1010";
const SUSPENSE: &str = "3150";
const OPENING_EQUITY: &str = "3000";
const PREVIEW: &str = "ledger_setup_statement_preview";
const COMMIT: &str = "ledger_setup_statement_commit";
const RECEIPT: &str = "ledger_setup_statement_receipt";
const STATEMENT: &str = "date,description,amount,reference\n\
2026-05-01,Refund,12.34,may-refund\n\
2026-05-02,Existing,1.25,already-imported\n";
const QFX_STATEMENT: &str = "OFXHEADER:100\nDATA:OFXSGML\nVERSION:102\nSECURITY:NONE\n\
ENCODING:USASCII\nCHARSET:1252\nCOMPRESSION:NONE\nOLDFILEUID:NONE\nNEWFILEUID:NONE\n\n\
<OFX><SIGNONMSGSRSV1><SONRS><STATUS><CODE>0<SEVERITY>INFO</STATUS>\
<DTSERVER>20260531<LANGUAGE>ENG</SONRS></SIGNONMSGSRSV1>\
<BANKMSGSRSV1><STMTTRNRS><TRNUID>1<STATUS><CODE>0<SEVERITY>INFO</STATUS>\
<STMTRS><CURDEF>CAD<BANKACCTFROM><BANKID>123<ACCTID>456\
<ACCTTYPE>CHECKING</BANKACCTFROM><BANKTRANLIST><DTSTART>20260501<DTEND>20260531\
<STMTTRN><TRNTYPE>CREDIT<DTPOSTED>20260510<TRNAMT>42.01<FITID>TEST-DEPOSIT\
<NAME>Deposit</STMTTRN></BANKTRANLIST><LEDGERBAL><BALAMT>42.01\
<DTASOF>20260531</LEDGERBAL></STMTRS></STMTTRNRS></BANKMSGSRSV1></OFX>";

#[test]
fn desktop_imports_a_reviewed_statement_once_and_preserves_provenance() -> TestResult {
    let ledger = Ledger::new()?;
    let owner = issue(&ledger, "statement_owner", &[Capability::Admin])?;
    let approver = issue(
        &ledger,
        "statement_approver",
        &[Capability::Approve, Capability::Reverse],
    )?;
    let owner_token = owner.token().expose_secret();
    let approval_token = approver.token().expose_secret();
    let reader = token(&ledger.reader)?;
    let mut server = Server::start(&ledger, &["serve"], None)?;
    server.initialize(Some(owner_token))?;
    let (generation, scoped) = begin_setup(&server, owner_token)?;
    let scoped = scoped.expose_secret();
    prepare_book(&server, scoped, generation)?;
    approve_opening(&server, scoped, generation, approval_token)?;
    let existing = call(
        &server,
        token(&ledger.writer)?,
        "txn_post",
        json!({
            "company":BOOK,"reference":"already-imported","date":"2026-05-02",
            "description":"Existing","currency":"CAD",
            "debits":[{"account":BANK,"amount":"1.25"}],
            "credits":[{"account":SUSPENSE,"amount":"1.25"}]
        }),
    )?;
    let existing_id = existing["id"]
        .as_i64()
        .ok_or("missing matching transaction ID")?;
    let preview = preview_statement(&server, scoped, generation, existing_id)?;
    let approval = json!({
        "company":BOOK,"batch_id":preview["batch_id"],"preview_sha256":preview["preview_sha256"],
    });
    let read_transactions = || call(&server, reader, "txn_list", json!({"company":BOOK}));
    let before_approval = read_transactions()?;
    assert_eq!(before_approval.as_array().map(Vec::len), Some(2));
    let db = ledger.db()?;
    let company = BOOK.parse()?;
    let summary = || beankeeper_cli::commands::setup::statement_import_summary(&db, &company);
    assert_eq!(summary()?.committed_batches, 0);
    assert_tool_error(&server, scoped, COMMIT, approval.clone(), "FORBIDDEN")?;
    let mut wrong_digest = approval.clone();
    wrong_digest["preview_sha256"] = json!("0".repeat(64));
    assert_tool_error(&server, approval_token, COMMIT, wrong_digest, "VALIDATION")?;
    assert_eq!(read_transactions()?, before_approval);

    let receipt = call(&server, approval_token, COMMIT, approval.clone())?;
    let created = verify_receipt(&receipt, &preview, existing_id, approver.token_jti.as_str())?;
    let lookup = json!({"company":BOOK,"batch_id":preview["batch_id"]});
    assert_eq!(call(&server, reader, RECEIPT, lookup.clone())?, receipt);
    let posted = read_transactions()?;
    assert_eq!(posted.as_array().map(Vec::len), Some(3));
    let imported = summary()?;
    assert_eq!(imported.committed_batches, 1);
    assert_eq!(imported.created_transactions, 1);
    assert_eq!(imported.preexisting_matches, 1);

    call(
        &server,
        owner_token,
        "ledger_setup_finish",
        json!({"generation":generation}),
    )?;
    let audit = || call(&server, reader, "audit_log", json!({"company":BOOK}));
    let recorded = audit()?;
    assert_eq!(call(&server, approval_token, COMMIT, approval)?, receipt);
    assert_eq!(audit()?, recorded);
    assert_eq!(read_transactions()?, posted);

    let reversed = call(
        &server,
        approval_token,
        "txn_reverse",
        json!({"company":BOOK,"id":created,"date":"2026-05-03","reference":"correct-may-refund"}),
    )?;
    assert_eq!(reversed["created"], true);
    assert_eq!(read_transactions()?.as_array().map(Vec::len), Some(4));
    assert_eq!(call(&server, reader, RECEIPT, lookup)?, receipt);
    let after_reversal = summary()?;
    assert_eq!(after_reversal.committed_batches, imported.committed_batches);
    assert_eq!(
        after_reversal.created_transactions,
        imported.created_transactions
    );
    assert_eq!(
        after_reversal.preexisting_matches,
        imported.preexisting_matches
    );
    assert_eq!(
        call(&server, reader, "verify", json!({}))?["status"],
        "healthy"
    );
    Ok(())
}

#[test]
fn desktop_imports_qfx_through_the_ofx_statement_path() -> TestResult {
    let ledger = Ledger::new()?;
    let owner = issue(&ledger, "ofx_owner", &[Capability::Admin])?;
    let reviewer = issue(&ledger, "ofx_reviewer", &[Capability::Approve])?;
    let owner_token = owner.token().expose_secret();
    let mut server = Server::start(&ledger, &["serve"], None)?;
    server.initialize(Some(owner_token))?;
    let (generation, scoped) = begin_setup(&server, owner_token)?;
    let scoped = scoped.expose_secret();
    prepare_book(&server, scoped, generation)?;
    let preview = call(
        &server,
        scoped,
        PREVIEW,
        json!({
            "company":BOOK,"generation":generation,"reference":"qfx-may",
            "bank":BANK,"suspense":SUSPENSE,"currency":"CAD","format":"ofx","content":QFX_STATEMENT,
        }),
    )?;
    assert_eq!(preview["format"], "ofx");
    assert_eq!(preview["rows"][0]["entries"][0]["amount_minor"], "4201");
    let receipt = call(
        &server,
        reviewer.token().expose_secret(),
        COMMIT,
        json!({
            "company":BOOK,"batch_id":preview["batch_id"],"preview_sha256":preview["preview_sha256"],
        }),
    )?;
    assert_eq!(receipt["format"], "ofx");
    assert_eq!(receipt["created"].as_array().map(Vec::len), Some(1));
    assert_eq!(receipt["source_sha256"], preview["source_sha256"]);
    Ok(())
}

fn approve_opening(
    server: &Server,
    scoped: &str,
    generation: Generation,
    reviewer: &str,
) -> TestResult {
    let opening = json!({
        "company":BOOK,"generation":generation,"date":"2026-01-01","currency":"CAD",
        "debits":[{"account":BANK,"amount":"100.00"}],
        "credits":[{"account":OPENING_EQUITY,"amount":"100.00"}],
    });
    let preview = call(
        server,
        scoped,
        "ledger_setup_opening_preview",
        opening.clone(),
    )?;
    let reservation = call(
        server,
        scoped,
        "ledger_setup_opening_reserve",
        json!({
            "opening":opening,"fingerprint":preview["fingerprint"],"reference":"opening-household",
        }),
    )?;
    let posted = call(
        server,
        reviewer,
        "txn_approve",
        json!({
            "company":BOOK,"id":reservation["proposal_id"],
        }),
    )?;
    assert_eq!(posted["status"], "approved");
    Ok(())
}

fn issue(ledger: &Ledger, name: &str, capabilities: &[Capability]) -> TestResult<IssuedCredential> {
    let capabilities = capabilities
        .iter()
        .map(|capability| capability.path().to_owned())
        .collect::<Vec<_>>();
    Ok(Granted::<Admin>::with_db(
        &ledger.db()?,
        ledger.owner(),
        |grant| {
            identity::issue_credential(
                grant,
                &IssueRequest {
                    name,
                    capabilities: &capabilities,
                    ttl: Duration::from_secs(600),
                    public_key: None,
                    generate_key: true,
                },
            )
        },
    )?)
}

fn begin_setup(server: &Server, owner: &str) -> TestResult<(Generation, SecretString)> {
    let public_key = STANDARD.encode(SigningKey::from_bytes(&[9; 32]).verifying_key().to_bytes());
    let mut started = call(
        server,
        owner,
        "ledger_setup_begin",
        json!({"name":"statement_setup","public_key":public_key}),
    )?;
    assert_eq!(
        started["credential"]["capabilities"],
        json!([Capability::Setup.path()])
    );
    let generation = serde_json::from_value(started["session"]["generation"].take())?;
    let Value::String(bearer) = started["credential"]["token"].take() else {
        return Err("missing setup credential".into());
    };
    Ok((generation, SecretString::from(bearer)))
}

fn prepare_book(server: &Server, scoped: &str, generation: Generation) -> TestResult {
    let target = |reference| json!({"company":BOOK,"generation":generation,"reference":reference});
    call(
        server,
        scoped,
        "ledger_setup_book_create",
        json!({
            "target":target("create-household"),"name":"Household",
            "settings":{"purpose":"personal","base_currency":"CAD","fiscal_year_start":"01-01"},
        }),
    )?;
    call(
        server,
        scoped,
        "ledger_setup_chart_apply",
        json!({
            "target":target("household-chart"),"template":"personal-v1",
        }),
    )?;
    for (reference, code, name, kind) in [
        ("statement-bank", BANK, "Checking", "asset"),
        ("statement-suspense", SUSPENSE, "Suspense", "equity"),
    ] {
        call(
            server,
            scoped,
            "ledger_setup_account_add",
            json!({"target":target(reference),"code":code,"name":name,"account_type":kind}),
        )?;
    }
    Ok(())
}

fn preview_statement(
    server: &Server,
    scoped: &str,
    generation: Generation,
    existing_id: i64,
) -> TestResult<Value> {
    let mut source = json!({
        "company":BOOK,"generation":generation,"reference":"may-statement",
        "bank":BANK,"suspense":SUSPENSE,"currency":"CAD","format":"csv","content":STATEMENT,
    });
    let preview = call(server, scoped, PREVIEW, source.clone())?;
    assert_eq!(call(server, scoped, PREVIEW, source.clone())?, preview);
    let source_digest = Sha256Hex::from_digest(Sha256::digest(STATEMENT.as_bytes()).into());
    assert_eq!(preview["source_sha256"], source_digest.as_str());
    assert_eq!(preview["rows"][0]["entries"][0]["amount_minor"], "1234");
    assert_eq!(preview["rows"][1]["existing_transaction"], existing_id);
    source["content"] = json!(STATEMENT.replace("Refund", "Different refund"));
    assert_tool_error(server, scoped, PREVIEW, source, "VALIDATION")?;
    Ok(preview)
}

fn verify_receipt(
    receipt: &Value,
    preview: &Value,
    existing_id: i64,
    approver_jti: &str,
) -> TestResult<i64> {
    for field in ["batch_id", "source_sha256", "preview_sha256"] {
        assert_eq!(receipt[field], preview[field]);
    }
    assert_eq!(receipt["reference"], "may-statement");
    assert_eq!(receipt["format"], "csv");
    assert_eq!(receipt["approved_by_jti"], approver_jti);
    assert_eq!(
        receipt["preexisting"],
        json!([{"reference":"already-imported","transaction_id":existing_id}])
    );
    assert_eq!(receipt["created"].as_array().map(Vec::len), Some(1));
    assert_eq!(receipt["created"][0]["reference"], "may-refund");
    let created = receipt["created"][0]["transaction_id"]
        .as_i64()
        .ok_or("missing created transaction ID")?;
    assert_ne!(created, existing_id);
    Ok(created)
}

fn call(server: &Server, credential: &str, name: &str, arguments: Value) -> TestResult<Value> {
    let mut result = tool_result(server, credential, name, arguments)?;
    assert_eq!(result["isError"], false, "{name} did not succeed");
    payload(&mut result)
}

fn assert_tool_error(
    server: &Server,
    credential: &str,
    name: &str,
    arguments: Value,
    code: &str,
) -> TestResult {
    let mut result = tool_result(server, credential, name, arguments)?;
    assert_eq!(
        result["isError"], true,
        "{name} did not return a tool error"
    );
    assert_eq!(payload(&mut result)?["code"], code);
    Ok(())
}

fn tool_result(
    server: &Server,
    credential: &str,
    name: &str,
    arguments: Value,
) -> TestResult<Value> {
    let mut params = json!({"name":name});
    params["arguments"] = arguments;
    let mut response = rpc(server.request("tools/call", params, Some(credential))?)?;
    Ok(response["result"].take())
}

fn payload(result: &mut Value) -> TestResult<Value> {
    if let Some(data) = result.get_mut("structuredContent") {
        return Ok(data.take());
    }
    Ok(serde_json::from_str(
        result["content"][0]["text"]
            .as_str()
            .ok_or("missing tool data")?,
    )?)
}
