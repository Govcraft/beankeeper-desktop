//! A stdio transport for clients that cannot connect directly to Desktop HTTP.
//!
//! Requests use the running Desktop process and its MCP sessions. The bridge
//! never opens a ledger, starts a second tool server, or writes credentials to
//! stdout. Current upstream tools use request/response traffic; a separate GET
//! notification stream is not needed for them.

use std::{
    error::Error,
    fmt,
    io::{self, BufRead, Write},
    path::PathBuf,
    time::Duration,
};

use clap::Args;
use reqwest::{
    StatusCode, Url,
    blocking::{Client, Response},
    header::{AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderValue},
    redirect::Policy,
};
use secrecy::{ExposeSecret, zeroize::Zeroizing};
use serde_json::Value;

use crate::private_file::{BEARER_FILE_LIMIT, PrivateFile};

const MAX_MESSAGE_BYTES: u64 = 8 * 1024 * 1024;

/// Connect an MCP stdio client to the already-running Desktop HTTP endpoint.
#[derive(Args, Debug, Clone)]
pub struct BridgeOptions {
    /// Local Streamable HTTP endpoint belonging to `bk serve`.
    #[arg(long, default_value = "http://127.0.0.1:8750/mcp")]
    pub url: String,
    /// Private file containing the agent bearer token; never put the token in argv.
    #[arg(long)]
    pub token_file: PathBuf,
}

/// Transport failures without request bodies or secret credentials in diagnostics.
#[derive(Debug)]
pub enum BridgeError {
    /// A local-only URL was not supplied.
    InvalidUrl,
    /// The token is missing, malformed, or readable by other Unix users.
    InvalidToken,
    /// A token file or stdio stream could not be read or written.
    Io(io::Error),
    /// A bounded, proxy-free HTTP operation failed.
    Http(reqwest::Error),
    /// Desktop rejected the HTTP request.
    Rejected(StatusCode),
    /// The client or server sent malformed or oversized JSON-RPC.
    Protocol,
}

impl fmt::Display for BridgeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidUrl => f.write_str("MCP bridge requires http://127.0.0.1:PORT/mcp, http://localhost:PORT/mcp or http://[::1]:PORT/mcp"),
            Self::InvalidToken => f.write_str("MCP bridge requires a nonempty private agent token file (Unix mode 0600); run `bk setup` to configure it"),
            Self::Io(error) => write!(f, "MCP bridge I/O failed: {error}"),
            Self::Http(error) => write!(f, "MCP bridge cannot reach Desktop; start `bk serve` first: {error}"),
            Self::Rejected(status) => write!(f, "Desktop rejected the MCP bridge request (HTTP {status}); check the running server and agent token"),
            Self::Protocol => f.write_str("MCP bridge received malformed or oversized JSON-RPC"),
        }
    }
}
impl Error for BridgeError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Http(error) => Some(error),
            _ => None,
        }
    }
}
impl From<io::Error> for BridgeError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}
impl From<reqwest::Error> for BridgeError {
    fn from(error: reqwest::Error) -> Self {
        Self::Http(error)
    }
}

/// Forward newline-delimited JSON-RPC until stdin closes, then close the session.
///
/// # Errors
/// Rejects remote URLs, invalid credentials, HTTP failures, and malformed traffic.
pub fn run(options: &BridgeOptions) -> Result<(), BridgeError> {
    let url = local_url(&options.url)?;
    let secret = PrivateFile::open(&options.token_file)
        .and_then(|file| file.read_secret(BEARER_FILE_LIMIT))
        .map_err(|_| BridgeError::InvalidToken)?;
    let token = secret.expose_secret().trim();
    if token.is_empty() || token.bytes().any(|byte| byte.is_ascii_whitespace()) {
        return Err(BridgeError::InvalidToken);
    }
    let value = Zeroizing::new(format!("Bearer {token}"));
    let mut authorization = HeaderValue::from_str(&value).map_err(|_| BridgeError::InvalidToken)?;
    authorization.set_sensitive(true);
    let mut headers = HeaderMap::new();
    headers.insert(AUTHORIZATION, authorization);
    let client = Client::builder()
        .no_proxy()
        .redirect(Policy::none())
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(120))
        .default_headers(headers)
        .build()?;
    let mut transport = Transport {
        client,
        url,
        session: None,
        protocol: None,
    };
    let result = transport.forward(io::stdin().lock(), &mut io::stdout().lock());
    transport.close();
    result
}

fn local_url(input: &str) -> Result<Url, BridgeError> {
    let url = Url::parse(input).map_err(|_| BridgeError::InvalidUrl)?;
    if url.scheme() != "http"
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || url.path() != "/mcp"
        || url.port_or_known_default() == Some(0)
        || !matches!(url.host_str(), Some("127.0.0.1" | "localhost" | "[::1]"))
    {
        return Err(BridgeError::InvalidUrl);
    }
    Ok(url)
}

struct Transport {
    client: Client,
    url: Url,
    session: Option<HeaderValue>,
    protocol: Option<String>,
}

impl Transport {
    fn forward(
        &mut self,
        mut input: impl BufRead,
        output: &mut impl Write,
    ) -> Result<(), BridgeError> {
        loop {
            let mut line = String::new();
            let read = io::Read::take(&mut input, MAX_MESSAGE_BYTES + 1).read_line(&mut line)?;
            if read == 0 {
                return Ok(());
            }
            if read as u64 > MAX_MESSAGE_BYTES {
                return Err(BridgeError::Protocol);
            }
            if line.trim().is_empty() {
                continue;
            }
            let message: Value = serde_json::from_str(&line).map_err(|_| BridgeError::Protocol)?;
            if message.get("jsonrpc").and_then(Value::as_str) != Some("2.0") || !message.is_object()
            {
                return Err(BridgeError::Protocol);
            }
            let mut request = self
                .client
                .post(self.url.clone())
                .header("Accept", "application/json, text/event-stream")
                .json(&message);
            if let Some(session) = &self.session {
                request = request.header("Mcp-Session-Id", session);
            }
            if let Some(protocol) = &self.protocol {
                request = request.header("MCP-Protocol-Version", protocol);
            }
            let response = request.send()?;
            if !response.status().is_success() {
                return Err(BridgeError::Rejected(response.status()));
            }
            if let Some(session) = response.headers().get("Mcp-Session-Id") {
                self.session = Some(session.clone());
            }
            let replies = relay_response(response, message.get("id"), output)?;
            if message.get("method").and_then(Value::as_str) == Some("initialize") {
                self.protocol = replies.iter().find_map(|reply| {
                    reply
                        .get("result")?
                        .get("protocolVersion")?
                        .as_str()
                        .map(str::to_owned)
                });
            }
        }
    }

    fn close(&self) {
        if let Some(session) = &self.session {
            let mut request = self
                .client
                .delete(self.url.clone())
                .header("Mcp-Session-Id", session)
                .timeout(Duration::from_secs(2));
            if let Some(protocol) = &self.protocol {
                request = request.header("MCP-Protocol-Version", protocol);
            }
            let _ = request.send();
        }
    }
}

fn relay_response(
    response: Response,
    request_id: Option<&Value>,
    output: &mut impl Write,
) -> Result<Vec<Value>, BridgeError> {
    if response.status() == StatusCode::ACCEPTED || response.status() == StatusCode::NO_CONTENT {
        return Ok(Vec::new());
    }
    let sse = response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.starts_with("text/event-stream"));
    let mut reader = io::BufReader::new(response);
    if sse {
        return relay_sse(&mut reader, request_id, output);
    }
    let mut contents = Vec::new();
    io::Read::read_to_end(
        &mut io::Read::take(reader, MAX_MESSAGE_BYTES + 1),
        &mut contents,
    )?;
    if contents.len() as u64 > MAX_MESSAGE_BYTES {
        return Err(BridgeError::Protocol);
    }
    let message = emit(&contents, output)?;
    Ok(vec![message])
}

fn emit(contents: &[u8], output: &mut impl Write) -> Result<Value, BridgeError> {
    let message: Value = serde_json::from_slice(contents).map_err(|_| BridgeError::Protocol)?;
    if message.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
        return Err(BridgeError::Protocol);
    }
    serde_json::to_writer(&mut *output, &message).map_err(|_| BridgeError::Protocol)?;
    output.write_all(b"\n")?;
    output.flush()?;
    Ok(message)
}

fn relay_sse(
    reader: &mut impl BufRead,
    request_id: Option<&Value>,
    output: &mut impl Write,
) -> Result<Vec<Value>, BridgeError> {
    let mut data = String::new();
    let mut replies = Vec::new();
    loop {
        let mut line = String::new();
        let read = io::Read::take(&mut *reader, MAX_MESSAGE_BYTES + 1).read_line(&mut line)?;
        if read as u64 > MAX_MESSAGE_BYTES {
            return Err(BridgeError::Protocol);
        }
        let line = line.trim_end_matches(['\r', '\n']);
        if line.is_empty() {
            if !data.is_empty() {
                let message = emit(data.as_bytes(), output)?;
                let completed = request_id.is_some_and(|id| {
                    message.get("id") == Some(id)
                        && (message.get("result").is_some() || message.get("error").is_some())
                });
                replies.push(message);
                data.clear();
                if completed {
                    return Ok(replies);
                }
            }
        } else if let Some(value) = line.strip_prefix("data:") {
            if !data.is_empty() {
                data.push('\n');
            }
            data.push_str(value.strip_prefix(' ').unwrap_or(value));
            if data.len() as u64 > MAX_MESSAGE_BYTES {
                return Err(BridgeError::Protocol);
            }
        }
        if read == 0 {
            if request_id.is_some() && !replies.iter().any(|reply| reply.get("id") == request_id) {
                return Err(BridgeError::Protocol);
            }
            return Ok(replies);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    type TestResult = Result<(), Box<dyn Error>>;

    #[test]
    fn credentials_never_go_to_nonlocal_urls() -> TestResult {
        for url in [
            "https://example.com/mcp",
            "http://127.0.0.2:8750/mcp",
            "http://user:password@127.0.0.1:8750/mcp",
            "http://localhost:8750/mcp?token=secret",
            "http://localhost:8750/other",
            "http://localhost:0/mcp",
            "http://evil.example/mcp",
        ] {
            assert!(
                matches!(local_url(url), Err(BridgeError::InvalidUrl)),
                "{url}"
            );
        }
        for url in [
            "http://127.0.0.1:8750/mcp",
            "http://localhost:8750/mcp",
            "http://[::1]:8750/mcp",
        ] {
            local_url(url)?;
        }
        Ok(())
    }

    #[test]
    fn sse_handles_comments_multiline_events_and_protocol_errors() -> TestResult {
        let data = b": keepalive\nevent: message\ndata: {\ndata: \"jsonrpc\":\"2.0\",\"method\":\"notifications/message\"}\n\ndata: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{}}\n\n";
        let mut output = Vec::new();
        let replies = relay_sse(&mut &data[..], Some(&Value::from(1)), &mut output)?;
        assert_eq!(replies.len(), 2);
        assert_eq!(String::from_utf8(output)?.lines().count(), 2);
        assert!(matches!(
            relay_sse(
                &mut &b"data: not-json\n\n"[..],
                Some(&Value::from(1)),
                &mut Vec::new()
            ),
            Err(BridgeError::Protocol)
        ));
        assert!(matches!(
            relay_sse(&mut &b""[..], Some(&Value::from(1)), &mut Vec::new()),
            Err(BridgeError::Protocol)
        ));
        Ok(())
    }

    #[test]
    fn forwards_session_and_protocol_and_closes_session_on_eof() -> TestResult {
        use std::io::Read;
        let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
        let url = Url::parse(&format!("http://{}/mcp", listener.local_addr()?))?;
        let server = std::thread::spawn(move || -> io::Result<()> {
            for index in 0..4 {
                let (mut stream, _) = listener.accept()?;
                stream.set_read_timeout(Some(Duration::from_secs(5)))?;
                let mut reader = io::BufReader::new(stream.try_clone()?);
                let mut first = String::new();
                reader.read_line(&mut first)?;
                let mut headers = String::new();
                let mut length = 0;
                loop {
                    let mut line = String::new();
                    reader.read_line(&mut line)?;
                    if line == "\r\n" {
                        break;
                    }
                    if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                        length = value.trim().parse::<usize>().map_err(io::Error::other)?;
                    }
                    headers.push_str(&line.to_ascii_lowercase());
                }
                let mut body = vec![0; length];
                reader.read_exact(&mut body)?;
                assert!(headers.contains("authorization: bearer test-secret"));
                if index > 0 {
                    assert!(headers.contains("mcp-session-id: test-session"));
                    assert!(headers.contains("mcp-protocol-version: 2025-11-25"));
                }
                let (status, content_type, extra, body) = match index {
                    0 => (
                        "200 OK",
                        "application/json",
                        "Mcp-Session-Id: test-session\r\n",
                        "{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"protocolVersion\":\"2025-11-25\"}}",
                    ),
                    1 => ("202 Accepted", "application/json", "", ""),
                    2 => (
                        "200 OK",
                        "text/event-stream",
                        "",
                        "data: {\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{\"tools\":[]}}\n\n",
                    ),
                    _ => {
                        assert!(first.starts_with("DELETE "));
                        ("200 OK", "application/json", "", "")
                    }
                };
                write!(
                    stream,
                    "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\n{extra}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )?;
            }
            Ok(())
        });
        let mut authorization = HeaderValue::from_static("Bearer test-secret");
        authorization.set_sensitive(true);
        let mut headers = HeaderMap::new();
        headers.insert(AUTHORIZATION, authorization);
        let mut transport = Transport {
            client: Client::builder()
                .no_proxy()
                .timeout(Duration::from_secs(5))
                .default_headers(headers)
                .build()?,
            url,
            session: None,
            protocol: None,
        };
        let input = b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{}}\n{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"tools/list\"}\n";
        let mut output = Vec::new();
        transport.forward(&input[..], &mut output)?;
        transport.close();
        server.join().map_err(|_| "HTTP server panicked")??;
        let output = String::from_utf8(output)?;
        let replies: Vec<Value> = output
            .lines()
            .map(serde_json::from_str)
            .collect::<Result<_, _>>()?;
        assert_eq!(replies.len(), 2);
        assert_eq!(replies[1]["result"]["tools"], serde_json::json!([]));
        assert!(!output.contains("test-secret"));
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn token_with_public_permissions_is_rejected_before_network_access() -> TestResult {
        use std::{fs, os::unix::fs::PermissionsExt};
        let dir = tempfile::tempdir()?;
        let token_file = dir.path().join("token");
        fs::write(&token_file, "agent://private-secret")?;
        fs::set_permissions(&token_file, fs::Permissions::from_mode(0o644))?;
        let error = run(&BridgeOptions {
            url: "http://127.0.0.1:1/mcp".into(),
            token_file,
        })
        .err()
        .ok_or("expected error")?;
        assert!(matches!(error, BridgeError::InvalidToken));
        assert!(!error.to_string().contains("private-secret"));
        Ok(())
    }
}
