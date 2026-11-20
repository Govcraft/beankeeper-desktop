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
#[test]
fn license_cli_activates_fixture_shows_perpetual_entitlement_and_rejects_tampering() -> TestResult {
    let dir = tempfile::tempdir()?;
    let directory = dir.path();
    fs::write(
        directory.join("keys.json"),
        include_bytes!("fixtures/license-public-keys.json"),
    )?;
    fs::write(
        directory.join("input.json"),
        include_bytes!("fixtures/license.json"),
    )?;
    let initial = command(directory).arg("license").output()?;
    assert!(
        initial.status.success(),
        "{}",
        String::from_utf8_lossy(&initial.stderr)
    );
    assert!(String::from_utf8_lossy(&initial.stdout).contains("No Desktop license"));
    let activated = command(directory)
        .args(["license", "activate"])
        .arg(directory.join("input.json"))
        .output()?;
    assert!(
        activated.status.success(),
        "{}",
        String::from_utf8_lossy(&activated.stderr)
    );
    assert!(String::from_utf8_lossy(&activated.stdout).contains("perpetual Desktop use"));
    let status = command(directory).arg("license").output()?;
    assert!(status.status.success());
    assert!(String::from_utf8_lossy(&status.stdout).contains("2027-11-20T12:00:00+00:00"));
    let original = fs::read(directory.join("state/license.json"))?;
    let tampered = String::from_utf8(original.clone())?.replace("AHHH0LU", "BHHH0LU");
    fs::write(directory.join("input.json"), tampered)?;
    let rejected = command(directory)
        .args(["license", "activate"])
        .arg(directory.join("input.json"))
        .output()?;
    assert!(!rejected.status.success());
    assert!(String::from_utf8_lossy(&rejected.stderr).contains("signature"));
    assert_eq!(fs::read(directory.join("state/license.json"))?, original);
    assert!(!directory.join("state/updates.json").exists());
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
