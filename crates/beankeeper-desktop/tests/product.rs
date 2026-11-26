//! Real CLI boundaries use isolated user state and never contact the network.
use std::{error::Error, fs, process::Command};
type TestResult = Result<(), Box<dyn Error>>;
fn command(directory: &std::path::Path) -> Command {
    let mut command = Command::new(assert_cmd::cargo::cargo_bin!("bk"));
    command
        .env("BEANKEEPER_DESKTOP_STATE_DIR", directory.join("state"))
        .env("BEANKEEPER_LICENSE_KEYS_FILE", directory.join("keys.json"))
        .env("BEANKEEPER_NO_UPDATE_CHECK", "1")
        .env_remove("BEANKEEPER_LICENSE_SERVICE_URL");
    command
}

const ACTIVATION_PATH: &str = "state/licenses/lic_01h455vb4pex5vsknk084sn02q.license";

#[test]
fn license_cli_activates_fixture_shows_perpetual_entitlement_and_rejects_tampering() -> TestResult {
    let dir = tempfile::tempdir()?;
    let directory = dir.path();
    fs::write(
        directory.join("keys.json"),
        include_bytes!("fixtures/license-public-keys.json"),
    )?;
    fs::write(
        directory.join("input.license"),
        include_bytes!("fixtures/license-v4.public.txt"),
    )?;
    let initial = command(directory).arg("license").output()?;
    assert!(
        initial.status.success(),
        "{}",
        String::from_utf8_lossy(&initial.stderr)
    );
    assert!(String::from_utf8_lossy(&initial.stdout).contains("No Desktop license"));
    assert!(String::from_utf8_lossy(&initial.stdout).contains("License status: not activated."));
    let activated = command(directory)
        .args(["license", "activate"])
        .arg(directory.join("input.license"))
        .output()?;
    assert!(
        activated.status.success(),
        "{}",
        String::from_utf8_lossy(&activated.stderr)
    );
    assert!(
        String::from_utf8_lossy(&activated.stdout).contains("covers all 1.x releases forever.")
    );
    assert!(
        String::from_utf8_lossy(&activated.stdout).contains("License status: active and valid.")
    );
    let status = command(directory).arg("license").output()?;
    assert!(status.status.success());
    assert!(String::from_utf8_lossy(&status.stdout).contains("covers all 1.x releases"));
    assert!(String::from_utf8_lossy(&status.stdout).contains("License status: active and valid."));
    let original = fs::read(directory.join(ACTIVATION_PATH))?;
    let tampered =
        String::from_utf8(original.clone())?.replacen("v4.public.eyJ", "v4.public.fyJ", 1);
    fs::write(directory.join("input.license"), tampered)?;
    let rejected = command(directory)
        .args(["license", "activate"])
        .arg(directory.join("input.license"))
        .output()?;
    assert!(!rejected.status.success());
    assert!(String::from_utf8_lossy(&rejected.stderr).contains("signature"));
    assert_eq!(fs::read(directory.join(ACTIVATION_PATH))?, original);
    assert!(!directory.join("state/updates.json").exists());
    Ok(())
}

#[test]
fn pasted_token_activates_offline_and_status_reads_saved_token() -> TestResult {
    let dir = tempfile::tempdir()?;
    let directory = dir.path();
    fs::write(
        directory.join("keys.json"),
        include_bytes!("fixtures/license-public-keys.json"),
    )?;
    let token = include_str!("fixtures/license-v4.public.txt").trim_end();
    let output = command(directory)
        .args(["license", "activate", "--token", token])
        .output()?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("covers all 1.x releases forever."));
    assert_eq!(fs::read_to_string(directory.join(ACTIVATION_PATH))?, token);
    let status = command(directory).arg("license").output()?;
    assert!(status.status.success());
    assert_eq!(status.stdout, output.stdout);
    Ok(())
}

#[test]
fn stored_license_verification_failures_never_report_active_status() -> TestResult {
    let dir = tempfile::tempdir()?;
    let directory = dir.path();
    fs::write(
        directory.join("keys.json"),
        include_bytes!("fixtures/license-public-keys.json"),
    )?;
    fs::write(
        directory.join("input.license"),
        include_bytes!("fixtures/license-v4.public.txt"),
    )?;
    assert!(
        command(directory)
            .args(["license", "activate"])
            .arg(directory.join("input.license"))
            .status()?
            .success()
    );
    let stored_path = directory.join(ACTIVATION_PATH);
    let original = fs::read_to_string(&stored_path)?;
    for (stored, keys, reason) in [
        (
            original.replacen("v4.public.eyJ", "v4.public.fyJ", 1),
            include_str!("fixtures/license-public-keys.json"),
            "invalid license signature",
        ),
        (
            original.clone(),
            r#"{"unknown":"6kpsY-KcUgq-9VB7Ey7F-ZVHdq6-vnuSQh7qaRRG0iw"}"#,
            "license issuer is not in the trusted public-key store",
        ),
        (
            original,
            "{}",
            "license issuer public keys are not configured",
        ),
    ] {
        fs::write(&stored_path, stored)?;
        fs::write(directory.join("keys.json"), keys)?;
        let output = command(directory).arg("license").output()?;
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains(reason));
        assert!(!String::from_utf8_lossy(&output.stdout).contains("active and valid"));
    }
    assert!(!directory.join("state/updates.json").exists());
    Ok(())
}

#[test]
fn json_envelope_is_refused_at_the_legacy_location() -> TestResult {
    let dir = tempfile::tempdir()?;
    let directory = dir.path();
    let legacy = directory.join("state/license.json");
    fs::write(
        directory.join("keys.json"),
        include_bytes!("fixtures/license-public-keys.json"),
    )?;
    fs::create_dir(directory.join("state"))?;
    let mut file = tempfile::NamedTempFile::new_in(directory.join("state"))?;
    std::io::Write::write_all(&mut file, br#"{"format":"beankeeper-license-v1","key_id":"test-2026","payload":"e30","signature":"old"}"#)?;
    beankeeper_desktop::setup::keychain::restrict_file(file.path())?;
    file.persist(&legacy)?;
    let status = command(directory).arg("license").output()?;
    assert!(!status.status.success());
    assert!(
        String::from_utf8_lossy(&status.stderr).contains("Ask the issuer for a current license")
    );
    assert!(!String::from_utf8_lossy(&status.stdout).contains("active and valid"));
    Ok(())
}

#[test]
fn opt_out_and_self_update_refuse_network_and_leave_no_cache() -> TestResult {
    let dir = tempfile::tempdir()?;
    let output = command(dir.path()).arg("update-check").output()?;
    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains("disabled"));
    assert!(!dir.path().join("state").exists());
    let output = command(dir.path()).arg("self-update").output()?;
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("disabled"));
    assert!(!dir.path().join("state").exists());
    Ok(())
}

#[test]
fn update_check_cli_displays_cached_outcome_and_readable_utc_attempt() -> TestResult {
    let dir = tempfile::tempdir()?;
    let state = dir.path().join("state");
    fs::create_dir(&state)?;
    let now = beankeeper_desktop::updates::now();
    let cache = serde_json::to_vec(&serde_json::json!({
        "desktop_version": env!("CARGO_PKG_VERSION"),
        "last_attempt": now,
        "manifest": null,
        "message": "update download failed; check connectivity and retry after the daily interval"
    }))?;
    fs::write(state.join("updates.json"), &cache)?;
    let output = command(dir.path())
        .env("BEANKEEPER_NO_UPDATE_CHECK", "0")
        .arg("update-check")
        .output()?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = String::from_utf8(output.stdout)?;
    let attempted_at = chrono::DateTime::<chrono::Utc>::from_timestamp(now.try_into()?, 0)
        .ok_or("fixture timestamp")?;
    assert!(text.contains("Could not check for updates:"));
    assert!(text.contains(&format!(
        "Last attempt: {} UTC.",
        attempted_at.format("%Y-%m-%d %H:%M:%S")
    )));
    assert!(!text.contains("Unix seconds"));
    assert!(!text.contains(&now.to_string()));
    assert_eq!(fs::read(state.join("updates.json"))?, cache);
    assert!(!state.join("updates.lock").exists());
    Ok(())
}
#[test]
fn redemption_requires_configured_service_and_trust_before_claim_consumption() -> TestResult {
    let dir = tempfile::tempdir()?;
    fs::write(
        dir.path().join("keys.json"),
        include_bytes!("fixtures/license-public-keys.json"),
    )?;
    let output = command(dir.path())
        .args(["license", "activate", "--code-file"])
        .arg(dir.path().join("code"))
        .output()?;
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("BEANKEEPER_LICENSE_SERVICE_URL"));
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn redemption_activates_the_issuers_plain_text_response() -> TestResult {
    use axum::{Json, Router, http::StatusCode, response::IntoResponse, routing::post};
    use serde_json::{Value, json};

    let dir = tempfile::tempdir()?;
    let directory = dir.path();
    fs::write(
        directory.join("keys.json"),
        include_bytes!("fixtures/license-public-keys.json"),
    )?;
    let code_file = tempfile::NamedTempFile::new_in(directory)?;
    beankeeper_desktop::setup::keychain::restrict_file(code_file.path())?;
    fs::write(code_file.path(), "test-polar-code\n")?;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let origin = format!("http://{}", listener.local_addr()?);
    let router = Router::new().route(
        "/api/v1/licenses/redeem",
        post(|Json(request): Json<Value>| async move {
            if request
                != json!({"storefront":"polar", "key":"test-polar-code", "recovery_token":null})
            {
                return StatusCode::BAD_REQUEST.into_response();
            }
            include_str!("fixtures/license-v4.public.txt").into_response()
        }),
    );
    let server = tokio::spawn(async { axum::serve(listener, router).await });
    let output = command(directory)
        .env("BEANKEEPER_LICENSE_SERVICE_URL", origin)
        .args(["license", "activate", "--code-file"])
        .arg(code_file.path())
        .output();
    server.abort();
    let output = output?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        fs::read(directory.join(ACTIVATION_PATH))?,
        include_bytes!("fixtures/license-v4.public.txt")
    );
    Ok(())
}

#[test]
fn license_trust_json_reports_compile_time_embed_not_runtime_overrides() -> TestResult {
    let dir = tempfile::tempdir()?;
    fs::write(
        dir.path().join("keys.json"),
        br#"{"runtime":"not-embedded"}"#,
    )?;
    let output = command(dir.path())
        .env("BEANKEEPER_LICENSE_SERVICE_URL", "https://example.invalid")
        .args(["license", "trust", "--json"])
        .output()?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let trust: serde_json::Value = serde_json::from_slice(&output.stdout)?;
    let expected_url = option_env!("BEANKEEPER_LICENSE_SERVICE_URL").filter(|url| !url.is_empty());
    let expected_keys: serde_json::Value =
        serde_json::from_str(option_env!("BEANKEEPER_LICENSE_PUBLIC_KEYS_JSON").unwrap_or("{}"))?;
    assert_eq!(
        trust.get("service_url"),
        Some(&expected_url.map_or(serde_json::Value::Null, std::convert::Into::into))
    );
    assert_eq!(trust.get("public_keys"), Some(&expected_keys));
    assert_ne!(
        trust.get("service_url").and_then(serde_json::Value::as_str),
        Some("https://example.invalid")
    );
    assert!(trust["public_keys"].get("runtime").is_none());
    Ok(())
}

#[test]
fn activation_cannot_reuse_a_license_for_a_second_set_of_books() -> TestResult {
    let dir = tempfile::tempdir()?;
    let directory = dir.path();
    fs::write(
        directory.join("keys.json"),
        include_bytes!("fixtures/license-public-keys.json"),
    )?;
    let license = directory.join("input.json");
    fs::write(&license, include_bytes!("fixtures/license-v4.public.txt"))?;
    let first = directory.join("first.db");
    let second = directory.join("second.db");
    for books in [&first, &second] {
        beankeeper_cli::db::Db::open(books, None)?;
    }
    let activate = |books: &std::path::Path| {
        command(directory)
            .arg("--db")
            .arg(books)
            .args(["license", "activate"])
            .arg(&license)
            .output()
    };
    let activated = activate(&first)?;
    assert!(
        activated.status.success(),
        "{}",
        String::from_utf8_lossy(&activated.stderr)
    );
    assert!(activate(&first)?.status.success());

    let activation = directory.join(ACTIVATION_PATH);
    fs::remove_file(&activation)?;
    let rejected = activate(&second)?;
    assert!(!rejected.status.success());
    assert!(String::from_utf8_lossy(&rejected.stderr).contains("already bound"));
    assert!(!activation.exists());
    Ok(())
}
