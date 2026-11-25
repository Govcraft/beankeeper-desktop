//! Exercise the stateless Files and Tasks extensions against an encrypted book.

pub mod common;

use std::{
    thread,
    time::{Duration, Instant},
};

use base64::{Engine as _, prelude::BASE64_URL_SAFE_NO_PAD};
use beankeeper::types::AccountType;
use beankeeper_cli::{
    db::{self, CreateAccountParams},
    identity::{self, Admin, Granted, IssueRequest, IssuedCredential},
};
use common::{Ledger, Server, TestResult, rpc, token};
use reqwest::blocking::Response;
use secrecy::ExposeSecret;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

const TOOL: &str = "ledger_setup_statement_import";
const FILES: &str = "io.modelcontextprotocol/files";
const TASKS: &str = "io.modelcontextprotocol/tasks";
const VERSION: &str = "2026-07-28";
const MEMO: &str = "this statement description should stay out of the result";

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

fn credential(
    ledger: &Ledger,
    name: &str,
    capabilities: &[String],
) -> TestResult<IssuedCredential> {
    Ok(Granted::<Admin>::with_db(
        &ledger.db()?,
        ledger.owner(),
        |grant| {
            identity::issue_credential(
                grant,
                &IssueRequest {
                    name,
                    capabilities,
                    ttl: Duration::from_mins(5),
                    public_key: None,
                    generate_key: true,
                },
            )
        },
    )?)
}

fn writer(ledger: &Ledger, name: &str) -> TestResult<IssuedCredential> {
    credential(
        ledger,
        name,
        &[
            "ledger/read".into(),
            "ledger/post".into(),
            "ledger/chart".into(),
        ],
    )
}

fn prepared_ledger() -> TestResult<Ledger> {
    let ledger = Ledger::new()?;
    let db = ledger.db()?;
    let company = "acme".parse()?;
    for (code, name, account_type) in [
        ("1010", "Checking", AccountType::Asset),
        ("3150", "Suspense", AccountType::Equity),
    ] {
        Granted::<Admin>::with_db(&db, ledger.owner(), |grant| {
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
    Ok(ledger)
}

fn request(
    server: &Server,
    bearer: &str,
    method: &str,
    mut params: Value,
    extensions: Value,
) -> TestResult<Response> {
    params["_meta"] = json!({
        "io.modelcontextprotocol/protocolVersion": VERSION,
        "io.modelcontextprotocol/clientInfo": {"name":"imports.test","version":"1"},
        "io.modelcontextprotocol/clientCapabilities": {}
    });
    params["_meta"]["io.modelcontextprotocol/clientCapabilities"]["extensions"] = extensions;
    let mut request = server
        .client
        .post(format!("{}/mcp", server.base))
        .bearer_auth(bearer)
        .header("accept", "application/json, text/event-stream")
        .header("mcp-protocol-version", VERSION)
        .header("mcp-method", method);
    if method == "tools/call" {
        request = request.header("mcp-name", params["name"].as_str().ok_or("tool name")?);
    }
    if let Some(task) = params["taskId"].as_str() {
        request = request.header("mcp-name", task);
    }
    Ok(request
        .json(&json!({"jsonrpc":"2.0","method":method,"id":1,"params":params}))
        .send()?)
}

fn capabilities() -> Value {
    json!({FILES:{"upload":{}},TASKS:{}})
}

fn arguments(reference: &str) -> Value {
    json!({"reference":reference,"company":"acme","bank":"1010","suspense":"3150","currency":"USD","format":"csv"})
}

fn file_args(reference: &str, content: &str) -> Value {
    let mut args = arguments(reference);
    args["file"] = json!({
        "id":"statement",
        "name":"private.csv",
        "mimeType":"text/csv",
        "size":content.len(),
        "digest":{"algorithm":"sha-256","value":BASE64_URL_SAFE_NO_PAD.encode(Sha256::digest(content))}
    });
    args
}

fn call_file(
    server: &Server,
    bearer: &str,
    args: &Value,
    state: Option<&Value>,
) -> TestResult<Value> {
    let mut params = json!({"name":TOOL,"arguments":args});
    if let Some(state) = state {
        params["requestState"] = state.clone();
    }
    rpc(request(
        server,
        bearer,
        "tools/call",
        params,
        capabilities(),
    )?)
}

fn poll(server: &Server, bearer: &str, task: &Value) -> TestResult<Value> {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let reply = rpc(request(
            server,
            bearer,
            "tasks/get",
            json!({"taskId":task["taskId"]}),
            capabilities(),
        )?)?;
        match reply["result"]["status"].as_str() {
            Some("working") if Instant::now() < deadline => {
                thread::sleep(Duration::from_millis(50));
            }
            Some("completed") => return Ok(reply["result"]["result"].clone()),
            _ => return Err(format!("Unexpected task state: {reply}").into()),
        }
    }
}

fn long_statement() -> String {
    use std::fmt::Write as _;
    let mut csv = "date,description,amount,reference\n".to_string();
    for row in 0..730 {
        let _ = writeln!(
            csv,
            "2026-05-01,{MEMO} and a longer reference line,1.25,row-{row}"
        );
    }
    csv
}

fn assert_extension_discovery(server: &Server, bearer: &str) -> TestResult {
    let discovery = rpc(request(
        server,
        bearer,
        "server/discover",
        json!({}),
        capabilities(),
    )?)?;
    assert!(
        discovery["result"]["capabilities"]["extensions"][FILES]["upload"].is_object(),
        "{discovery}"
    );
    assert!(discovery["result"]["capabilities"]["extensions"][TASKS].is_object());
    let list = |extensions| -> TestResult<Value> {
        let reply = rpc(request(
            server,
            bearer,
            "tools/list",
            json!({}),
            extensions,
        )?)?;
        Ok(reply["result"]["tools"]
            .as_array()
            .ok_or_else(|| format!("missing tools {reply}"))?
            .iter()
            .find(|tool| tool["name"] == TOOL)
            .ok_or_else(|| format!("missing importer {reply}"))?["inputSchema"]
            .clone())
    };
    assert!(list(capabilities())?["properties"]["file"]["x-mcp-file"].is_object());
    assert!(list(json!({}))?["properties"].get("file").is_none());

    Ok(())
}

fn assert_upload_binding(
    server: &Server,
    bearer: &str,
    args: &Value,
    state: &Value,
    size: f64,
) -> TestResult {
    for (field, value) in [("reference", json!("unbound")), ("suspense", json!("3151"))] {
        let mut changed = args.clone();
        changed[field] = value;
        assert_eq!(
            call_file(server, bearer, &changed, Some(state))?["error"]["code"],
            -32602
        );
    }
    let mut changed = args.clone();
    changed["file"]["mimeType"] = json!("application/octet-stream");
    assert_eq!(
        call_file(server, bearer, &changed, Some(state))?["error"]["code"],
        -32602
    );
    changed["file"]["size"] = json!(size + 0.5);
    assert_eq!(
        call_file(server, bearer, &changed, Some(state))?["result"]["structuredContent"]["code"],
        "VALIDATION"
    );

    Ok(())
}

#[test]
fn extension_uploads_before_task_and_uses_existing_audited_import() -> TestResult {
    let ledger = prepared_ledger()?;
    let writer = writer(&ledger, "import_writer")?;
    let bearer = writer.token().expose_secret();
    let server = Server::start(&ledger, &["serve"], None)?;
    assert_extension_discovery(&server, bearer)?;

    let mut existing = arguments("first");
    existing["content"] = json!(format!(
        "date,description,amount,reference\n2026-05-01,{MEMO} and a longer reference line,1.25,row-0\n"
    ));
    let existing = rpc(request(
        &server,
        bearer,
        "tools/call",
        json!({"name":TOOL,"arguments":existing}),
        json!({}),
    )?)?;
    assert_eq!(
        existing["result"]["structuredContent"]["counts"]["recorded"], 1,
        "{existing}"
    );

    let csv = long_statement();
    let mut args = file_args("large", &csv);
    args["csv"] = json!(r#"{"date_col":"date"}"#);
    args["hold_posting_window_days"] = json!(7);
    let upload = call_file(&server, bearer, &args, None)?;
    assert_eq!(
        upload["result"]["resultType"], "upload_required",
        "{upload}"
    );
    let state = &upload["result"]["requestState"];
    let target = &upload["result"]["uploads"][0];
    assert_eq!(target["fileId"], "statement");
    assert_eq!(target["credentials"], "include");
    let path = target["path"].as_str().ok_or("missing upload path")?;
    let url = format!("{}/mcp{path}", server.base);
    let retry_before_put = call_file(&server, bearer, &args, Some(state))?;
    assert_eq!(retry_before_put["result"]["isError"], true);
    let invalid = server
        .client
        .put(&url)
        .bearer_auth(bearer)
        .body("wrong size")
        .send()?;
    assert_eq!(invalid.status(), 400);
    let invalid = server
        .client
        .put(&url)
        .bearer_auth(bearer)
        .body("X".repeat(csv.len()))
        .send()?;
    assert_eq!(invalid.status(), 400);
    let uploaded = server
        .client
        .put(&url)
        .bearer_auth(bearer)
        .body(csv)
        .send()?;
    assert_eq!(uploaded.status(), 204);
    let size = f64::from(u32::try_from(
        args["file"]["size"].as_u64().ok_or("file size")?,
    )?);
    assert_upload_binding(&server, bearer, &args, state, size)?;

    let mut retry = args.clone();
    retry["file"]["size"] = json!(size);
    retry["csv"] = json!({"date_col":"date"});
    let imported = call_file(&server, bearer, &retry, Some(state))?;
    let task = &imported["result"];
    assert_eq!(task["resultType"], "task", "{imported}");
    assert_eq!(task["status"], "working");
    assert!(task["statusMessage"].is_string());
    let result = poll(&server, bearer, task)?;
    assert!(!result.to_string().contains(MEMO));
    let data = &result["structuredContent"];
    assert_eq!(
        data["counts"],
        json!({"recorded":729,"skipped_duplicates":1,"uncertain":729,"pending":0}),
        "{result}"
    );
    assert!(data["receipt_id"].is_number());
    assert!(data["desktop_path"].is_string());
    let again = call_file(&server, bearer, &args, Some(state))?;
    assert_eq!(
        poll(&server, bearer, &again["result"])?["structuredContent"],
        *data
    );
    Ok(())
}

#[test]
fn upload_targets_and_task_results_are_credential_bound() -> TestResult {
    let ledger = prepared_ledger()?;
    let first = writer(&ledger, "first_importer")?;
    let second = writer(&ledger, "second_importer")?;
    let first = first.token().expose_secret();
    let second = second.token().expose_secret();
    let server = Server::start(&ledger, &["serve"], None)?;
    let csv = long_statement();
    let args = file_args("bound", &csv);
    let call = |bearer, extensions| {
        request(
            &server,
            bearer,
            "tools/call",
            json!({"name":TOOL,"arguments":args}),
            extensions,
        )
    };
    assert_eq!(call("invalid", capabilities())?.status(), 401);
    let reader = token(&ledger.reader)?;
    let forbidden = rpc(call(reader, capabilities())?)?;
    assert_eq!(
        forbidden["result"]["structuredContent"]["code"], "FORBIDDEN",
        "{forbidden}"
    );
    assert_eq!(rpc(call(first, json!({}))?)?["error"]["code"], -32021);
    let upload = rpc(call(first, capabilities())?)?;
    let upload = &upload["result"];
    let state = &upload["requestState"];
    let target = upload["uploads"][0]["path"]
        .as_str()
        .ok_or("transfer target")?;
    let put = |bearer, content: &str| {
        server
            .client
            .put(format!("{}/mcp{target}", server.base))
            .bearer_auth(bearer)
            .body(content.to_owned())
            .send()
    };
    assert_eq!(put(second, "wrong credential")?.status(), 404);
    assert_eq!(put(first, &csv)?.status(), 204);
    assert_eq!(
        call_file(&server, second, &args, Some(state))?["error"]["code"],
        -32602
    );
    let task = call_file(&server, first, &args, Some(state))?;
    let task = &task["result"];
    let private = rpc(request(
        &server,
        second,
        "tasks/get",
        json!({"taskId":task["taskId"]}),
        capabilities(),
    )?)?;
    assert_eq!(private["error"]["code"], -32602);
    assert_eq!(poll(&server, first, task)?["isError"], false);
    Ok(())
}

#[test]
fn cli_uses_the_same_import_receipt_and_counts() -> TestResult {
    let ledger = prepared_ledger()?;
    let state = tempfile::tempdir()?;
    let writer = writer(&ledger, "cli_importer")?;
    let token = writer.token().expose_secret();
    let mut launch = ledger.command();
    launch
        .env("BEANKEEPER_DESKTOP_STATE_DIR", state.path())
        .args(["serve", "--bind", "127.0.0.1:0"]);
    let server = Server::launch(launch)?;
    let csv = long_statement();
    let file = state.path().join("statement.csv");
    std::fs::write(&file, &csv)?;
    let mut args = arguments("cli");
    args["csv"] = json!({"date_col":"date"});
    args["hold_amount_tolerance"] = json!(1);
    args["content"] = json!(csv);
    let task = rpc(request(
        &server,
        token,
        "tools/call",
        json!({"name":TOOL,"arguments":args}),
        capabilities(),
    )?)?;
    let mcp = poll(&server, token, &task["result"])?;
    let output = ledger
        .command()
        .env("BEANKEEPER_DESKTOP_STATE_DIR", state.path())
        .env("BK_AGENT_TOKEN", token)
        .args([
            "--json",
            "import",
            "--book",
            "acme",
            "--bank",
            "1010",
            "--suspense",
            "3150",
            "--currency",
            "USD",
            "--statement-format",
            "csv",
            "--reference",
            "cli",
            "--csv",
            r#"{"date_col":"date"}"#,
            "--hold-amount-tolerance",
            "1",
        ])
        .arg(file)
        .output()?;
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let response: Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(response["data"], mcp["structuredContent"]);
    Ok(())
}

#[test]
fn read_only_desktop_does_not_advertise_or_accept_uploads() -> TestResult {
    let ledger = prepared_ledger()?;
    let credential = writer(&ledger, "read_only_importer")?;
    let bearer = credential.token().expose_secret();
    let server = Server::start(&ledger, &["serve", "--read-only"], None)?;
    let discovery = rpc(request(
        &server,
        bearer,
        "server/discover",
        json!({}),
        capabilities(),
    )?)?;
    assert!(
        discovery["result"]["capabilities"]["extensions"]
            .get(FILES)
            .is_none()
    );
    assert!(
        discovery["result"]["capabilities"]["extensions"]
            .get(TASKS)
            .is_none()
    );
    let listing = rpc(request(
        &server,
        bearer,
        "tools/list",
        json!({}),
        capabilities(),
    )?)?;
    let tools = listing["result"]["tools"]
        .as_array()
        .ok_or("missing tools")?;
    assert!(tools.iter().all(|tool| tool["name"] != TOOL));
    let response = call_file(
        &server,
        bearer,
        &file_args("blocked", &long_statement()),
        None,
    )?;
    assert!(response["error"].is_object() || response["result"]["isError"] == true);
    assert_ne!(response["result"]["resultType"], "upload_required");
    Ok(())
}

#[test]
fn malformed_statement_finishes_as_tool_error_without_returning_input() -> TestResult {
    let ledger = prepared_ledger()?;
    let credential = writer(&ledger, "invalid_importer")?;
    let bearer = credential.token().expose_secret();
    let server = Server::start(&ledger, &["serve"], None)?;
    let mut arguments = arguments("bad-amount");
    arguments["content"] = json!(long_statement().replace("1.25", "PRIVATE INVALID AMOUNT"));
    let initial = rpc(request(
        &server,
        bearer,
        "tools/call",
        json!({"name":TOOL,"arguments":arguments}),
        capabilities(),
    )?)?;
    let result = poll(&server, bearer, &initial["result"])?;
    assert_eq!(result["isError"], true);
    assert_eq!(result["structuredContent"]["code"], "VALIDATION");
    assert!(!result.to_string().contains("PRIVATE INVALID AMOUNT"));
    Ok(())
}

#[test]
fn a_small_qfx_attachment_completes_without_a_task() -> TestResult {
    let ledger = prepared_ledger()?;
    let credential = writer(&ledger, "qfx_importer")?;
    let bearer = credential.token().expose_secret();
    let server = Server::start(&ledger, &["serve"], None)?;
    let mut args = file_args("qfx", QFX_STATEMENT);
    args["file"]["name"] = json!("bank.qfx");
    args["file"]["mimeType"] = json!("application/octet-stream");
    args["currency"] = json!("CAD");
    args["format"] = json!("ofx");
    let upload = call_file(&server, bearer, &args, None)?;
    let target = &upload["result"]["uploads"][0];
    let path = target["path"].as_str().ok_or("missing target")?;
    let response = server
        .client
        .put(format!("{}/mcp{path}", server.base))
        .bearer_auth(bearer)
        .body(QFX_STATEMENT)
        .send()?;
    assert_eq!(response.status(), 204);
    let imported = call_file(
        &server,
        bearer,
        &args,
        Some(&upload["result"]["requestState"]),
    )?;
    let result = &imported["result"];
    assert_eq!(result["isError"], false, "{result}");
    let data = &result["structuredContent"];
    assert_eq!(data["format"], "ofx");
    assert_eq!(data["counts"]["recorded"], 1);
    assert!(data["receipt_id"].is_number());
    assert!(!result.to_string().contains("<BANKACCTFROM>"));
    Ok(())
}
