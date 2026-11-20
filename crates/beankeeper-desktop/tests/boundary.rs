use std::{error::Error, net::SocketAddr};

use axum::http::{HeaderMap, HeaderValue, header};
use beankeeper_desktop::{boundary::LocalBoundary, cli, server};
use clap::{FromArgMatches, Parser};

type TestResult = Result<(), Box<dyn Error>>;

#[test]
fn host_and_origin_are_exact_port_specific_and_same_origin() -> TestResult {
    let boundary = LocalBoundary::new(8750);
    for host in ["127.0.0.1:8750", "localhost:8750", "[::1]:8750"] {
        let mut headers = HeaderMap::new();
        headers.insert(header::HOST, HeaderValue::from_str(host)?);
        assert!(boundary.accepts(&headers));
        headers.insert(
            header::ORIGIN,
            HeaderValue::from_str(&format!("http://{host}"))?,
        );
        assert!(boundary.accepts(&headers));
        for origin in [
            "null",
            "https://evil.example",
            "http://evil.example:8750",
            "http://localhost:8751",
            "http://localhost",
            "https://localhost:8750",
            "http://localhost:8750/",
            "http://localhost:8750 http://evil.example",
        ] {
            headers.insert(header::ORIGIN, HeaderValue::from_str(origin)?);
            assert!(!boundary.accepts(&headers), "accepted {host}, {origin}");
        }
        headers.insert(
            header::ORIGIN,
            HeaderValue::from_str(&format!("http://{host}"))?,
        );
        headers.append(
            header::ORIGIN,
            HeaderValue::from_static("http://evil.example"),
        );
        assert!(!boundary.accepts(&headers));
        headers.remove(header::ORIGIN);
        headers.append(header::HOST, HeaderValue::from_str(host)?);
        assert!(!boundary.accepts(&headers));
    }
    for host in [
        None,
        Some("rebinding.example:8750"),
        Some("127.0.0.1"),
        Some("127.0.0.1:8751"),
        Some("127.0.0.1:0"),
        Some("127.0.0.1.evil.example:8750"),
        Some("localhost:8750@evil.example"),
        Some("localhost.:8750"),
        Some("0.0.0.0:8750"),
    ] {
        let mut headers = HeaderMap::new();
        if let Some(host) = host {
            headers.insert(header::HOST, HeaderValue::from_str(host)?);
        }
        assert!(!boundary.accepts(&headers), "accepted {host:?}");
    }
    let mut headers = HeaderMap::new();
    headers.insert(header::HOST, HeaderValue::from_static("127.0.0.1:8750"));
    headers.insert(
        header::ORIGIN,
        HeaderValue::from_static("http://localhost:8750"),
    );
    assert!(!boundary.accepts(&headers));
    headers.remove(header::ORIGIN);
    assert!(!LocalBoundary::new(49152).accepts(&headers));
    Ok(())
}

#[test]
fn remote_bind_requires_explicit_consent() -> TestResult {
    for bind in ["127.0.0.1:0", "[::1]:8750"] {
        assert!(server::validate_bind(bind.parse()?, false).is_ok());
    }
    assert!(server::validate_bind("127.0.0.2:8750".parse()?, false).is_err());
    for bind in ["0.0.0.0:8750", "[::]:8750", "192.0.2.1:8750"] {
        let bind: SocketAddr = bind.parse()?;
        assert!(server::validate_bind(bind, false).is_err());
        assert!(server::validate_bind(bind, true).is_ok());
    }
    assert_eq!(
        server::local_url("[::1]:8751".parse()?),
        "http://[::1]:8751/"
    );
    assert_eq!(
        server::local_url("0.0.0.0:8751".parse()?),
        "http://127.0.0.1:8751/"
    );
    Ok(())
}

#[test]
fn upstream_globals_and_transports_remain_parseable() -> TestResult {
    let matches = cli::command().try_get_matches_from([
        "bk",
        "serve",
        "--read-only",
        "--db",
        "/tmp/books.db",
        "--passphrase-file",
        "/tmp/secret",
        "--agent-token",
        "test-token",
    ])?;
    let ledger = cli::ledger_options(&matches)?;
    assert_eq!(ledger.db.to_str(), Some("/tmp/books.db"));
    assert_eq!(
        ledger
            .passphrase
            .passphrase_file
            .as_deref()
            .and_then(|path| path.to_str()),
        Some("/tmp/secret")
    );
    assert!(ledger.agent_token.is_some());
    let args =
        cli::ServeOptions::from_arg_matches(matches.subcommand_matches("serve").ok_or("serve")?)?;
    assert!(args.read_only);
    assert_eq!(args.bind.to_string(), "127.0.0.1:8750");
    for args in [
        vec!["bk", "mcp", "serve"],
        vec!["bk", "mcp", "serve", "--http"],
    ] {
        let matches = cli::command().try_get_matches_from(args)?;
        assert!(beankeeper_cli::cli::Cli::from_arg_matches(&matches).is_ok());
    }
    assert!(server::validate_bind("127.0.0.2:8750".parse()?, false).is_err());
    for bind in ["0.0.0.0:8750", "192.0.2.1:8750", "127.0.0.1:0"] {
        assert!(
            cli::command()
                .try_get_matches_from(["bk", "open", "--bind", bind])
                .is_err()
        );
    }
    assert!(beankeeper_cli::cli::Cli::try_parse_from(["bk", "mcp", "serve"]).is_ok());
    Ok(())
}
