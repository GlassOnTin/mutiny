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
//! Requests are forwarded to a per-peer relay Unix socket, never the operator
//! socket. Each relay carrier pins its own consent mode (`ask`, `paired`, or
//! `observe-only`, defaulting to `--agent-consent` from GUI launch), so a
//! network client can never be assigned `Operator`. That is structural, not a
//! check that could be forgotten: this module has no path to the operator
//! socket at all.
//!
//! HTTP has no `SO_PEERCRED` equivalent, so the pairing token IS the peer
//! credential. Each configured peer has its own token and its own relay socket,
//! and the controller assigns that socket a distinct token-derived identity.
//! Two peers are therefore distinguishable in the audit trail and in the grant
//! set: an Allow Session tap for one peer does not cover another. The remote
//! client cannot choose its identity, only present a token; the token-to-carrier
//! mapping is fixed by operator configuration at launch.
//!
//! Bounded slice: POST /mcp with a JSON response. SSE streaming, session
//! management, and DELETE are not implemented. Each request is stateless, so
//! the name a client sends in `initialize` does not persist; calls are audited
//! under the peer's configured label, with per-peer identity, grants, and
//! consent mode.

use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crate::ipc::ConsentMode;
use crate::mcp::McpBridge;

pub const MAX_BODY_BYTES: usize = 1024 * 1024;
const IO_TIMEOUT: Duration = Duration::from_secs(5);

/// One paired peer. `socket` is the relay carrier the controller bound for this
/// token's identity; the operator socket is deliberately not reachable from
/// here.
#[derive(Debug, Clone)]
pub struct HttpPeer {
    /// Display and audit label the operator assigned. Not an identity: the
    /// identity is derived from the token by the carrier.
    pub label: String,
    pub token: String,
    pub socket: PathBuf,
}

#[derive(Debug, Clone)]
pub struct HttpMcpConfig {
    pub bind: SocketAddr,
    pub peers: Vec<HttpPeer>,
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

/// Parses `LABEL=TOKEN[:MODE]`. Without a MODE the peer inherits
/// `--agent-consent`, so existing invocations keep working.
///
/// The mode suffix is only stripped when it actually names a mode, so a token
/// containing a colon still works. A token whose final segment happens to be
/// `ask`, `paired`, or `observe-only` is ambiguous; use `--http-token` for that
/// case. Generated tokens are hex and cannot collide.
pub fn parse_http_peer(
    raw: &str,
    default_mode: ConsentMode,
) -> Result<(String, String, ConsentMode), String> {
    let (label, rest) = raw
        .split_once('=')
        .ok_or_else(|| String::from("expected LABEL=TOKEN[:MODE]"))?;
    let label = label.trim();
    if label.is_empty() {
        return Err(String::from("label must not be empty"));
    }
    let (token, mode) = match rest.rsplit_once(':') {
        Some((head, suffix)) if !head.is_empty() => match ConsentMode::parse_peer_mode(suffix) {
            Ok(mode) => (head, mode),
            // Not a mode, so the colon belongs to the token. An explicit
            // "operator" suffix is still refused rather than silently treated
            // as part of a token, because it signals a mistaken expectation.
            Err(err) if suffix.trim().eq_ignore_ascii_case("operator") => return Err(err),
            Err(_) => (rest, default_mode),
        },
        _ => (rest, default_mode),
    };
    if token.is_empty() {
        return Err(String::from("token must not be empty"));
    }
    Ok((label.to_owned(), token.to_owned(), mode))
}

pub fn mode_label(mode: ConsentMode) -> &'static str {
    match mode {
        ConsentMode::Operator => "operator",
        ConsentMode::Paired => "paired",
        ConsentMode::Ask => "ask",
        ConsentMode::ObserveOnly => "observe-only",
    }
}

/// Refuses an empty peer set, a blank token, or two peers sharing a label.
/// Duplicate labels would make the trail ambiguous even though the identities
/// differ, so they are rejected at launch rather than explained later.
pub fn validate_peers(peers: &[HttpPeer]) -> Result<(), String> {
    if peers.is_empty() {
        return Err(String::from(
            "an HTTP MCP listener requires at least one paired peer token",
        ));
    }
    let mut labels = std::collections::BTreeSet::new();
    for peer in peers {
        if peer.token.trim().is_empty() {
            return Err(format!("peer {:?} has an empty token", peer.label));
        }
        if peer.label.trim().is_empty() {
            return Err(String::from("every HTTP peer needs a label"));
        }
        if !labels.insert(peer.label.as_str()) {
            return Err(format!("duplicate HTTP peer label {:?}", peer.label));
        }
    }
    Ok(())
}

/// Finds the peer whose token was presented. Every token is compared, with no
/// early exit, so the work does not depend on which peer matched.
fn authenticate<'a>(peers: &'a [HttpPeer], presented: Option<&str>) -> Option<&'a HttpPeer> {
    let presented = presented?;
    let mut found = None;
    for peer in peers {
        if constant_time_eq(presented.as_bytes(), peer.token.as_bytes()) {
            found = Some(peer);
        }
    }
    found
}

pub fn spawn(cfg: HttpMcpConfig) -> io::Result<HttpHandle> {
    validate_bind(&cfg.bind).map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
    validate_peers(&cfg.peers).map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
    let listener = TcpListener::bind(cfg.bind)?;
    listener.set_nonblocking(true)?;
    let local_addr = listener.local_addr()?;
    let shutdown = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&shutdown);
    let peers = Arc::new(cfg.peers);

    let join = thread::spawn(move || {
        while !flag.load(Ordering::Relaxed) {
            match listener.accept() {
                Ok((stream, _)) => {
                    let peers = Arc::clone(&peers);
                    // A thread per connection with a read timeout, so one slow
                    // client cannot stall the others.
                    thread::spawn(move || {
                        let _ = handle_conn(stream, &peers);
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

fn handle_conn(mut stream: TcpStream, peers: &[HttpPeer]) -> io::Result<()> {
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
    let Some(peer) = authenticate(peers, presented) else {
        return respond(
            &mut stream,
            401,
            r#"{"jsonrpc":"2.0","id":null,"error":{"code":-32001,"message":"unpaired client: a valid pairing token is required"}}"#,
        );
    };

    if content_length > MAX_BODY_BYTES {
        return respond(&mut stream, 413, r#"{"error":"request body too large"}"#);
    }
    let mut body = vec![0u8; content_length];
    reader.read_exact(&mut body)?;
    let text = String::from_utf8_lossy(&body);

    // Paired here means "no local ceiling"; the effective mode is whatever the
    // agent carrier was pinned to at launch, which is never Operator. The socket
    // is this peer's own relay carrier, so the controller records the identity
    // that peer's token maps to rather than this process's pid.
    let mut bridge = McpBridge::new(peer.socket.clone(), ConsentMode::Paired, peer.label.clone());
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

    fn peer(label: &str, token: &str) -> HttpPeer {
        HttpPeer {
            label: label.to_owned(),
            token: token.to_owned(),
            socket: PathBuf::from(format!("/nonexistent-{label}.sock")),
        }
    }

    #[test]
    fn spawn_requires_a_peer_and_refuses_wildcards() {
        let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let err = spawn(HttpMcpConfig {
            bind: addr,
            peers: vec![],
        })
        .err()
        .expect("no peers must be refused");
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);

        let err = spawn(HttpMcpConfig {
            bind: addr,
            peers: vec![peer("phone", "")],
        })
        .err()
        .expect("empty token must be refused");
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);

        let err = spawn(HttpMcpConfig {
            bind: "0.0.0.0:0".parse().unwrap(),
            peers: vec![peer("phone", "t")],
        })
        .err()
        .expect("wildcard must be refused");
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn duplicate_peer_labels_are_refused() {
        assert!(validate_peers(&[peer("phone", "a"), peer("laptop", "b")]).is_ok());
        let err = validate_peers(&[peer("phone", "a"), peer("phone", "b")])
            .expect_err("duplicate labels must be refused");
        assert!(err.contains("duplicate"), "{err}");
        assert!(validate_peers(&[peer("", "a")]).is_err());
    }

    #[test]
    fn peer_mode_syntax_defaults_inherits_and_refuses_operator() {
        use crate::ipc::ConsentMode;
        // No suffix inherits the launch default, so old invocations still work.
        let (label, token, mode) = parse_http_peer("phone=abc123", ConsentMode::Ask).unwrap();
        assert_eq!((label.as_str(), token.as_str()), ("phone", "abc123"));
        assert_eq!(mode, ConsentMode::Ask);

        for (text, expected) in [
            ("ask", ConsentMode::Ask),
            ("paired", ConsentMode::Paired),
            ("observe-only", ConsentMode::ObserveOnly),
        ] {
            let (_, _, mode) =
                parse_http_peer(&format!("phone=abc123:{text}"), ConsentMode::Ask).unwrap();
            assert_eq!(mode, expected, "{text}");
        }

        // Operator is not assignable, and is refused rather than absorbed into
        // the token, because it signals a mistaken expectation.
        let err = parse_http_peer("phone=abc123:operator", ConsentMode::Ask).unwrap_err();
        assert!(err.contains("operator"), "{err}");
        assert!(ConsentMode::parse_peer_mode("operator").is_err());
        assert!(ConsentMode::parse_peer_mode("nonsense").is_err());

        // A colon that is not a mode belongs to the token.
        let (_, token, mode) = parse_http_peer("phone=abc:123", ConsentMode::ObserveOnly).unwrap();
        assert_eq!(token, "abc:123");
        assert_eq!(mode, ConsentMode::ObserveOnly);

        assert!(parse_http_peer("no-equals", ConsentMode::Ask).is_err());
        assert!(parse_http_peer("=abc", ConsentMode::Ask).is_err());
        assert!(parse_http_peer("phone=", ConsentMode::Ask).is_err());
    }

    #[test]
    fn each_token_selects_its_own_peer_and_nothing_else_does() {
        let peers = vec![peer("phone", "token-aaa"), peer("laptop", "token-bbb")];
        assert_eq!(
            authenticate(&peers, Some("token-aaa")).unwrap().label,
            "phone"
        );
        assert_eq!(
            authenticate(&peers, Some("token-bbb")).unwrap().label,
            "laptop"
        );
        assert!(authenticate(&peers, Some("token-ccc")).is_none());
        assert!(
            authenticate(&peers, Some("token-aa")).is_none(),
            "prefix is not a match"
        );
        assert!(authenticate(&peers, None).is_none());
        // Each peer routes to its own relay socket, which is what carries its
        // identity into the audit trail.
        assert_ne!(peers[0].socket, peers[1].socket);
    }
}
