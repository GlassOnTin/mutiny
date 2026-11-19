//! Authenticated streamable-HTTP MCP carrier, so a peer such as Haven can drive
//! the desktop terminal from another device.
//!
//! Off by default: nothing listens unless an bind address is explicitly
//! configured. A wildcard bind is refused, because the listener is meant to sit
//! on a named WireGuard or SSH-tunneled interface, not on every interface.
//!
//! Every request must carry the pairing token. An unpaired client is refused
//! before its body is even read.
//!
//! Requests are forwarded to the **agent** Unix socket, never the operator
//! socket. The agent carrier's mode is pinned by the operator at GUI launch
//! (`--agent-consent`, one of ask/paired/observe-only), so a network client can
//! never be assigned `Operator`. That is structural, not a check that could be
//! forgotten: this module has no path to the operator socket at all.
//!
//! Bounded slice: POST /mcp with a JSON response. SSE streaming, session
//! management, and DELETE are not implemented. Each request is stateless, so
//! the name a client sends in `initialize` does not persist and every call is
//! audited under the carrier's own label, `http-peer`.
//!
//! Known trust gap in this slice: the agent socket sees this process as the
//! peer, so every HTTP client shares one `PeerIdentity`. Session grants key on
//! that identity, which means an Allow Session tap under `--agent-consent ask`
//! would grant every HTTP client, not just the one that prompted. Prefer
//! `paired` (one token, one trust level) or `observe-only` for a network
//! listener until per-peer identity exists for this carrier.

use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crate::ipc::ConsentMode;
use crate::mcp::McpBridge;

pub const MAX_BODY_BYTES: usize = 1024 * 1024;
const IO_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone)]
pub struct HttpMcpConfig {
    pub bind: SocketAddr,
    pub token: String,
    /// Must be the agent socket. The operator socket is deliberately not
    /// reachable from here.
    pub agent_socket: PathBuf,
}

/// Refuses a wildcard bind. The listener must name an explicit interface.
pub fn validate_bind(addr: &SocketAddr) -> Result<(), String> {
    if addr.ip().is_unspecified() {
        return Err(format!(
            "refusing to bind {addr}: name an explicit interface address (for example a WireGuard IP), not a wildcard"
        ));
    }
    Ok(())
}

/// A random pairing token, hex-encoded. Reads the kernel CSPRNG rather than
/// adding a dependency or using a non-cryptographic hasher.
pub fn generate_token() -> io::Result<String> {
    let mut bytes = [0u8; 24];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

/// Length-independent comparison over equal-length slices, so a wrong token
/// cannot be recovered by timing byte-by-byte guesses.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

pub struct HttpHandle {
    pub local_addr: SocketAddr,
    shutdown: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
}

impl HttpHandle {
    pub fn stop(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

impl Drop for HttpHandle {
    fn drop(&mut self) {
        self.stop();
    }
}

pub fn spawn(cfg: HttpMcpConfig) -> io::Result<HttpHandle> {
    validate_bind(&cfg.bind).map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
    if cfg.token.trim().is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "an HTTP MCP listener requires a pairing token",
        ));
    }
    let listener = TcpListener::bind(cfg.bind)?;
    listener.set_nonblocking(true)?;
    let local_addr = listener.local_addr()?;
    let shutdown = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&shutdown);
    let token = cfg.token;
    let agent = cfg.agent_socket;

    let join = thread::spawn(move || {
        while !flag.load(Ordering::Relaxed) {
            match listener.accept() {
                Ok((stream, _)) => {
                    let token = token.clone();
                    let agent = agent.clone();
                    // A thread per connection with a read timeout, so one slow
                    // client cannot stall the others.
                    thread::spawn(move || {
                        let _ = handle_conn(stream, &token, &agent);
                    });
                }
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(20));
                }
                Err(_) => break,
            }
        }
    });

    Ok(HttpHandle {
        local_addr,
        shutdown,
        join: Some(join),
    })
}

fn respond(stream: &mut TcpStream, status: u16, body: &str) -> io::Result<()> {
    let reason = match status {
        200 => "OK",
        202 => "Accepted",
        401 => "Unauthorized",
        404 => "Not Found",
        413 => "Payload Too Large",
        _ => "Error",
    };
    let head = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes())?;
    stream.write_all(body.as_bytes())?;
    stream.flush()
}

fn handle_conn(mut stream: TcpStream, token: &str, agent_socket: &Path) -> io::Result<()> {
    stream.set_read_timeout(Some(IO_TIMEOUT))?;
    stream.set_write_timeout(Some(IO_TIMEOUT))?;
    let mut reader = BufReader::new(stream.try_clone()?);

    let mut request_line = String::new();
    reader.read_line(&mut request_line)?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_owned();
    let path = parts.next().unwrap_or_default().to_owned();

    let mut content_length = 0usize;
    let mut authorization: Option<String> = None;
    loop {
        let mut line = String::new();
        reader.read_line(&mut line)?;
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        if let Some((key, value)) = line.split_once(':') {
            match key.trim().to_ascii_lowercase().as_str() {
                "content-length" => content_length = value.trim().parse().unwrap_or(0),
                "authorization" => authorization = Some(value.trim().to_owned()),
                _ => {}
            }
        }
    }

    if method != "POST" || path != "/mcp" {
        return respond(
            &mut stream,
            404,
            r#"{"error":"only POST /mcp is served by this carrier"}"#,
        );
    }

    // Authenticate before reading the body, so an unpaired client cannot make
    // us allocate or forward anything.
    let presented = authorization
        .as_deref()
        .and_then(|value| value.strip_prefix("Bearer "))
        .map(str::trim);
    let paired =
        presented.is_some_and(|presented| constant_time_eq(presented.as_bytes(), token.as_bytes()));
    if !paired {
        return respond(
            &mut stream,
            401,
            r#"{"jsonrpc":"2.0","id":null,"error":{"code":-32001,"message":"unpaired client: a valid pairing token is required"}}"#,
        );
    }

    if content_length > MAX_BODY_BYTES {
        return respond(&mut stream, 413, r#"{"error":"request body too large"}"#);
    }
    let mut body = vec![0u8; content_length];
    reader.read_exact(&mut body)?;
    let text = String::from_utf8_lossy(&body);

    // Paired here means "no local ceiling"; the effective mode is whatever the
    // agent carrier was pinned to at launch, which is never Operator.
    let mut bridge = McpBridge::new(
        agent_socket.to_path_buf(),
        ConsentMode::Paired,
        String::from("http-peer"),
    );
    match bridge.handle_line(&text) {
        Some(reply) => respond(&mut stream, 200, &reply),
        // A JSON-RPC notification has no reply.
        None => respond(&mut stream, 202, ""),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wildcard_binds_are_refused_and_named_addresses_allowed() {
        for wildcard in ["0.0.0.0:8730", "[::]:8730"] {
            let addr: SocketAddr = wildcard.parse().unwrap();
            assert!(validate_bind(&addr).is_err(), "{wildcard} must be refused");
        }
        for named in ["127.0.0.1:8730", "100.64.0.5:8730", "[fd00::5]:8730"] {
            let addr: SocketAddr = named.parse().unwrap();
            assert!(validate_bind(&addr).is_ok(), "{named} must be allowed");
        }
    }

    #[test]
    fn token_comparison_is_exact() {
        assert!(constant_time_eq(b"abc123", b"abc123"));
        assert!(!constant_time_eq(b"abc123", b"abc124"));
        assert!(!constant_time_eq(b"abc123", b"abc12"));
        assert!(!constant_time_eq(b"", b"x"));
        assert!(constant_time_eq(b"", b""));
    }

    #[test]
    fn generated_tokens_are_long_random_and_distinct() {
        let a = generate_token().expect("token");
        let b = generate_token().expect("token");
        assert_eq!(a.len(), 48, "24 bytes hex-encoded");
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b);
    }

    #[test]
    fn spawn_requires_a_token_and_refuses_wildcards() {
        let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let err = spawn(HttpMcpConfig {
            bind: addr,
            token: String::new(),
            agent_socket: PathBuf::from("/nonexistent.sock"),
        })
        .err()
        .expect("empty token must be refused");
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);

        let err = spawn(HttpMcpConfig {
            bind: "0.0.0.0:0".parse().unwrap(),
            token: String::from("t"),
            agent_socket: PathBuf::from("/nonexistent.sock"),
        })
        .err()
        .expect("wildcard must be refused");
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }
}
