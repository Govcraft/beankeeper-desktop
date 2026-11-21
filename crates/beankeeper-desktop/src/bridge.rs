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
    path::{Path, PathBuf},
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
use serde::Deserialize;
use serde_json::Value;

use crate::private_file::{BEARER_FILE_LIMIT, PrivateFile};

const MAX_MESSAGE_BYTES: u64 = 8 * 1024 * 1024;
const EXTENSION_INSTRUCTIONS: &str = "When asked to set up the books, call setup_status first if it is listed. Explain the next step in plain words and ask the owner for decisions before changing the books. Recovery material and owner credentials stay under the owner's control. Desktop views are optional.";

fn exceeds_message_limit(bytes: usize) -> bool {
    u64::try_from(bytes).map_or(true, |length| length > MAX_MESSAGE_BYTES)
}

/// Connect an MCP stdio client to the already-running Desktop HTTP endpoint.
#[derive(Args, Debug, Clone)]
pub struct BridgeOptions {
    /// Local Streamable HTTP endpoint belonging to `bk serve`.
    #[arg(long, default_value = "http://127.0.0.1:8750/mcp")]
    pub url: String,
    /// Private file containing the agent bearer token; never put the token in argv.
    #[arg(long, required_unless_present = "service", conflicts_with = "service")]
    pub token_file: Option<PathBuf>,
    /// Discover the per-user background service endpoint and its private agent token.
    #[arg(long, conflicts_with = "url")]
    pub service: bool,
}

/// Transport failures without request bodies or secret credentials in diagnostics.
#[derive(Debug)]
pub enum BridgeError {
    /// The managed Desktop service is unavailable.
    Service(Box<crate::service::ServiceError>),
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
    /// The managed service rejected the read-only diagnostic call.
    ToolFailed,
}

impl fmt::Display for BridgeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Service(error) => write!(f, "{}: {error}", error.code()),
            Self::InvalidUrl => f.write_str("MCP bridge requires http://127.0.0.1:PORT/mcp, http://localhost:PORT/mcp or http://[::1]:PORT/mcp"),
            Self::InvalidToken => f.write_str("MCP bridge requires a nonempty private agent token file (Unix mode 0600); run `bk setup` to configure it"),
            Self::Io(error) => write!(f, "MCP bridge I/O failed: {error}"),
            Self::Http(error) => write!(f, "MCP bridge cannot reach Desktop; start `bk serve` first: {error}"),
            Self::Rejected(status) => write!(f, "Desktop rejected the MCP bridge request (HTTP {status}); check the running server and agent token"),
            Self::Protocol => f.write_str("MCP bridge received malformed or oversized JSON-RPC"),
            Self::ToolFailed => f.write_str("Desktop rejected the read-only MCP diagnostic. Check the agent's ledger read access."),
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
    let (url, token_file) = if options.service {
        crate::service::connection().map_err(|error| BridgeError::Service(Box::new(error)))?
    } else {
        (
            options.url.clone(),
            options
                .token_file
                .clone()
                .ok_or(BridgeError::InvalidToken)?,
        )
    };
    run_connected(&url, &token_file, None)
}

/// Start the service when launched from an accepted extension, then bridge MCP.
///
/// # Errors
/// Returns actionable setup, service discovery, credential and transport errors.
pub fn run_extension() -> Result<(), BridgeError> {
    let (url, token_file) = crate::service::extension_connection()
        .map_err(|error| BridgeError::Service(Box::new(error)))?;
    run_connected(&url, &token_file, Some(EXTENSION_INSTRUCTIONS))
}

fn run_connected(
    url: &str,
    token_file: &Path,
    instructions: Option<&'static str>,
) -> Result<(), BridgeError> {
    let mut transport =
        connected_transport(url, token_file, instructions, Duration::from_secs(120))?;
    let result = transport.forward(io::stdin().lock(), &mut io::stdout().lock());
    transport.close();
    result
}

/// Check a fixed read-only tool through the same protocol transport as clients.
/// No tool result or credential is printed.
pub(crate) fn probe_service_tool(url: &str, token_file: &Path) -> Result<(), BridgeError> {
    use rmcp::model::CallToolResult;
    use serde_json::json;

    let mut transport = connected_transport(url, token_file, None, Duration::from_secs(5))?;
    let mut requests = Vec::new();
    for request in [
        json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{
            "protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{
                "name":"bk agents doctor", "version":env!("CARGO_PKG_VERSION")
            }
        }}),
        json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
        json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{
            "name":"company_list", "arguments":{}
        }}),
    ] {
        serde_json::to_writer(&mut requests, &request).map_err(|_| BridgeError::Protocol)?;
        requests.push(b'\n');
    }
    let mut replies = Vec::new();
    let result = transport.forward(requests.as_slice(), &mut replies);
    transport.close();
    result?;
    if transport.session.is_none() || transport.protocol.is_none() {
        return Err(BridgeError::Protocol);
    }
    for line in replies.lines() {
        let reply: Value = serde_json::from_str(&line?).map_err(|_| BridgeError::Protocol)?;
        if reply.get("id").and_then(Value::as_u64) != Some(2) {
            continue;
        }
        let result = reply.get("result").ok_or(BridgeError::ToolFailed)?;
        let result = CallToolResult::deserialize(result).map_err(|_| BridgeError::Protocol)?;
        // A list can travel as text content when the negotiated MCP version
        // does not support array-shaped structured content.
        return if result.is_error != Some(true)
            && (result.structured_content.is_some() || !result.content.is_empty())
        {
            Ok(())
        } else {
            Err(BridgeError::ToolFailed)
        };
    }
    Err(BridgeError::Protocol)
}

fn connected_transport(
    url: &str,
    token_file: &Path,
    instructions: Option<&'static str>,
    timeout: Duration,
) -> Result<Transport, BridgeError> {
    let url = local_url(url)?;
    let secret = PrivateFile::open(token_file)
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
        .timeout(timeout)
        .default_headers(headers)
        .build()?;
    Ok(Transport {
        client,
        url,
        session: None,
        protocol: None,
        instructions,
    })
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
    instructions: Option<&'static str>,
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
            if exceeds_message_limit(read) {
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
            let initial = message.get("method").and_then(Value::as_str) == Some("initialize");
            let relay = ReplyContext {
                request_id: message.get("id"),
                instructions: if initial { self.instructions } else { None },
            };
            let replies = relay_response(response, relay, output)?;
            if initial {
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

#[derive(Clone, Copy)]
struct ReplyContext<'a> {
    request_id: Option<&'a Value>,
    instructions: Option<&'static str>,
}

fn relay_response(
    response: Response,
    context: ReplyContext<'_>,
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
        return relay_sse(&mut reader, context, output);
    }
    let mut contents = Vec::new();
    io::Read::read_to_end(
        &mut io::Read::take(reader, MAX_MESSAGE_BYTES + 1),
        &mut contents,
    )?;
    if exceeds_message_limit(contents.len()) {
        return Err(BridgeError::Protocol);
    }
    let message = emit(&contents, context, output)?;
    Ok(vec![message])
}

fn emit(
    contents: &[u8],
    context: ReplyContext<'_>,
    output: &mut impl Write,
) -> Result<Value, BridgeError> {
    let mut message: Value = serde_json::from_slice(contents).map_err(|_| BridgeError::Protocol)?;
    if message.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
        return Err(BridgeError::Protocol);
    }
    if let Some(instructions) = context.instructions
        && context
            .request_id
            .is_some_and(|id| message.get("id") == Some(id))
        && let Some(result) = message.get_mut("result").and_then(Value::as_object_mut)
    {
        let value = result
            .entry("instructions")
            .or_insert_with(|| Value::String(String::new()));
        let Value::String(existing) = value else {
            return Err(BridgeError::Protocol);
        };
        if !existing.is_empty() {
            existing.push(' ');
        }
        existing.push_str(instructions);
    }
    serde_json::to_writer(&mut *output, &message).map_err(|_| BridgeError::Protocol)?;
    output.write_all(b"\n")?;
    output.flush()?;
    Ok(message)
}

fn relay_sse(
    reader: &mut impl BufRead,
    context: ReplyContext<'_>,
    output: &mut impl Write,
) -> Result<Vec<Value>, BridgeError> {
    let mut data = String::new();
    let mut replies = Vec::new();
    loop {
        let mut line = String::new();
        let read = io::Read::take(&mut *reader, MAX_MESSAGE_BYTES + 1).read_line(&mut line)?;
        if exceeds_message_limit(read) {
            return Err(BridgeError::Protocol);
        }
        let line = line.trim_end_matches(['\r', '\n']);
        if line.is_empty() {
            if !data.is_empty() {
                let message = emit(data.as_bytes(), context, output)?;
                let completed = context.request_id.is_some_and(|id| {
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
            if exceeds_message_limit(data.len()) {
                return Err(BridgeError::Protocol);
            }
        }
        if read == 0 {
            if context.request_id.is_some()
                && !replies
                    .iter()
                    .any(|reply| reply.get("id") == context.request_id)
            {
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

    fn reply(request_id: Option<&Value>) -> ReplyContext<'_> {
        ReplyContext {
            request_id,
            instructions: None,
        }
    }

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
        let replies = relay_sse(&mut &data[..], reply(Some(&Value::from(1))), &mut output)?;
        assert_eq!(replies.len(), 2);
        assert_eq!(String::from_utf8(output)?.lines().count(), 2);
        assert!(matches!(
            relay_sse(
                &mut &b"data: not-json\n\n"[..],
                reply(Some(&Value::from(1))),
                &mut Vec::new()
            ),
            Err(BridgeError::Protocol)
        ));
        assert!(matches!(
            relay_sse(&mut &b""[..], reply(Some(&Value::from(1))), &mut Vec::new()),
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
            instructions: None,
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
            service: false,
            url: "http://127.0.0.1:1/mcp".into(),
            token_file: Some(token_file),
        })
        .err()
        .ok_or("expected error")?;
        assert!(matches!(error, BridgeError::InvalidToken));
        assert!(!error.to_string().contains("private-secret"));
        Ok(())
    }
}
