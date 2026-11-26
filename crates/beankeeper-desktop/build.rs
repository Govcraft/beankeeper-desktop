//! Forwards license trust into rustc so it is actually compiled into the binary.
//!
//! `option_env!` in crate source only sees variables Cargo forwards as
//! `cargo:rustc-env`. A job-level environment variable is visible to this
//! build script, but rustc on some runners (observed: x86_64-apple-darwin on
//! macos-15-intel) will not embed `option_env!` from the process environment
//! alone. `rerun-if-env-changed` is still required so a later build with
//! different values is not satisfied by a stale `target/` artifact.
//!
//! `cargo:rustc-env` values cannot contain newlines, so JSON from
//! packaging/license-embed.json is compacted before it is forwarded.

use std::{error::Error, io, path::Path};

#[path = "src/backups/git_oid.rs"]
mod git_oid;
use git_oid::GitOid;

fn main() -> Result<(), Box<dyn Error>> {
    // The unoptimized upstream clap command tree exceeds Windows' 1 MiB
    // default main stack. Reserve the usual Unix main-stack budget; pages
    // are committed on demand. This applies only to the Desktop executable.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows")
        && std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc")
    {
        println!("cargo:rustc-link-arg-bin=bk=/STACK:8388608");
    }
    println!("cargo:rerun-if-env-changed=BEANKEEPER_LICENSE_PUBLIC_KEYS_JSON");
    println!("cargo:rerun-if-env-changed=BEANKEEPER_LICENSE_SERVICE_URL");

    let manifest_dir =
        std::env::var_os("CARGO_MANIFEST_DIR").ok_or(std::env::VarError::NotPresent)?;
    let embed_path =
        std::path::PathBuf::from(&manifest_dir).join("../../packaging/license-embed.json");
    let lock_path = std::path::PathBuf::from(&manifest_dir).join("../../Cargo.lock");
    println!("cargo:rerun-if-changed={}", lock_path.display());
    rustc_env("BK_CORE_REVISION", core_revision(&lock_path)?.as_ref());
    println!("cargo:rerun-if-changed={}", embed_path.display());

    let release = std::env::var("PROFILE").is_ok_and(|p| p == "release");
    let official = release && std::env::var("BEANKEEPER_OFFICIAL_BUILD").as_deref() == Ok("1");
    embed_revocations(Path::new(&manifest_dir), official)?;
    rustc_env(
        "BEANKEEPER_OFFICIAL_BUILD",
        if official { "1" } else { "0" },
    );
    let file_defaults = if release {
        std::fs::read_to_string(&embed_path)
            .ok()
            .map(|text| parse_license_embed(&text))
    } else {
        None
    };

    let keys = nonempty_env("BEANKEEPER_LICENSE_PUBLIC_KEYS_JSON")
        .or_else(|| file_defaults.as_ref().map(|d| d.keys.clone()))
        .map(|s| compact_json_ws(&s));
    let url = nonempty_env("BEANKEEPER_LICENSE_SERVICE_URL")
        .or_else(|| file_defaults.as_ref().map(|d| d.url.clone()));

    if release
        && (keys.as_ref().is_none_or(String::is_empty) || url.as_ref().is_none_or(String::is_empty))
    {
        panic!(
            "release builds must embed license trust: set BEANKEEPER_LICENSE_PUBLIC_KEYS_JSON and \
             BEANKEEPER_LICENSE_SERVICE_URL, or provide packaging/license-embed.json"
        );
    }

    // Debug/test builds stay unbaked so cargo test is hermetic even if the
    // developer shell exported these variables.
    if release {
        if let Some(keys) = keys.filter(|s| !s.is_empty()) {
            rustc_env("BEANKEEPER_LICENSE_PUBLIC_KEYS_JSON", &keys);
        }
        if let Some(url) = url.filter(|s| !s.is_empty()) {
            rustc_env("BEANKEEPER_LICENSE_SERVICE_URL", &url);
        }
    }
    Ok(())
}

fn embed_revocations(manifest: &Path, official: bool) -> io::Result<()> {
    let packaging = manifest.join("../../packaging");
    let path = packaging.join("revocations.json");
    let watched = if path.exists() { &path } else { &packaging };
    println!("cargo:rerun-if-changed={}", watched.display());
    println!("cargo:rerun-if-env-changed=BEANKEEPER_OFFICIAL_BUILD");
    let enforced = official
        && std::env::var("CARGO_PKG_VERSION_MAJOR").as_deref() != Ok("0")
        && std::env::var("CARGO_PKG_VERSION_PRE").as_deref() == Ok("");
    let bytes = match std::fs::read(path) {
        Ok(bytes) if bytes.len() <= 4 * 1024 * 1024 && (!bytes.is_empty() || !enforced) => bytes,
        Ok(_) => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid size for packaging/revocations.json",
            ));
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound && !enforced => Vec::new(),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Err(io::Error::new(
                error.kind(),
                "official stable builds require packaging/revocations.json signed by the license issuer",
            ));
        }
        Err(error) => return Err(error),
    };
    let out = std::env::var_os("OUT_DIR").ok_or_else(|| io::Error::other("OUT_DIR is unset"))?;
    std::fs::write(Path::new(&out).join("revocations.json"), bytes)
}

fn core_revision(lock_path: &Path) -> Result<GitOid, Box<dyn Error>> {
    let lock = toml_edit::Document::parse(std::fs::read_to_string(lock_path)?)?;
    let revision = lock
        .get("package")
        .and_then(toml_edit::Item::as_array_of_tables)
        .into_iter()
        .flatten()
        .find(|package| package.get("name").and_then(toml_edit::Item::as_str) == Some("beankeeper"))
        .and_then(|package| package.get("source"))
        .and_then(toml_edit::Item::as_str)
        .and_then(|source| source.strip_prefix("git+"))
        .and_then(|source| source.rsplit_once('#'))
        .map(|(_, revision)| revision)
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "{} needs a Git revision for the beankeeper package",
                    lock_path.display()
                ),
            )
        })?;
    Ok(revision.parse()?)
}

struct LicenseEmbed {
    url: String,
    keys: String,
}

fn nonempty_env(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|value| !value.is_empty())
}

fn rustc_env(key: &str, value: &str) {
    assert!(
        !value.contains(['\n', '\r']),
        "cargo:rustc-env={key} cannot contain newlines"
    );
    println!("cargo:rustc-env={key}={value}");
}

/// Minimal parse of packaging/license-embed.json. Avoids a build-dependency.
fn parse_license_embed(text: &str) -> LicenseEmbed {
    let url = json_string_field(text, "service_url")
        .unwrap_or_else(|| panic!("packaging/license-embed.json: missing service_url"));
    let keys = json_object_field(text, "public_keys")
        .unwrap_or_else(|| panic!("packaging/license-embed.json: missing public_keys"));
    LicenseEmbed {
        url,
        keys: compact_json_ws(&keys),
    }
}

fn compact_json_ws(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut in_str = false;
    let mut escape = false;
    for ch in text.chars() {
        if in_str {
            out.push(ch);
            if escape {
                escape = false;
            } else if ch == '\\' {
                escape = true;
            } else if ch == '"' {
                in_str = false;
            }
            continue;
        }
        if ch == '"' {
            in_str = true;
            out.push(ch);
            continue;
        }
        if !ch.is_whitespace() {
            out.push(ch);
        }
    }
    out
}

fn json_string_field(text: &str, key: &str) -> Option<String> {
    let needle = format!("\"{key}\"");
    let rest = text.split_once(needle.as_str())?.1.trim_start();
    let rest = rest.strip_prefix(':')?.trim_start();
    let rest = rest.strip_prefix('"')?;
    let mut out = String::new();
    let mut chars = rest.chars();
    while let Some(ch) = chars.next() {
        if ch == '\\' {
            out.push(chars.next()?);
        } else if ch == '"' {
            return Some(out);
        } else {
            out.push(ch);
        }
    }
    None
}

fn json_object_field(text: &str, key: &str) -> Option<String> {
    let needle = format!("\"{key}\"");
    let rest = text.split_once(needle.as_str())?.1.trim_start();
    let rest = rest.strip_prefix(':')?.trim_start();
    if !rest.starts_with('{') {
        return None;
    }
    let mut depth = 0i32;
    let mut in_str = false;
    let mut escape = false;
    for (i, ch) in rest.char_indices() {
        if in_str {
            if escape {
                escape = false;
            } else if ch == '\\' {
                escape = true;
            } else if ch == '"' {
                in_str = false;
            }
            continue;
        }
        match ch {
            '"' => in_str = true,
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(rest[..=i].to_string());
                }
            }
            _ => {}
        }
    }
    None
}
