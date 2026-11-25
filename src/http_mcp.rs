//! Authenticated streamable-HTTP MCP carrier, so a peer such as Haven can drive
//! the desktop terminal from another device, with optional symmetric
//! `initialize` tap-to-pair.
//!
//! Off by default: nothing listens unless a bind address is explicitly
//! configured. A wildcard bind is refused, because the listener is meant to sit
//! on a named WireGuard, Yggdrasil, or SSH-tunneled interface, not on every
//! interface.
//!
//! Authenticated requests present `Authorization: Bearer <token>`, verified via
//! SHA-256 constant-time lookup against the live `InboundPeerTable`. When
//! tap-to-pair is enabled (the default on `--http-bind`, disabled by
//! `--no-http-pair`), an unauthenticated `POST /mcp` whose JSON-RPC method is
//! `initialize` raises at most one inline operator prompt at a time (returning
//! HTTP `429 Too Many Requests` if a prompt is already open) and, only if
//! approved within 60 seconds, mints an `mty_...` token, persists only its
//! SHA-256 hash to `peers.json`, and returns `_meta["sh.mutiny/pairingToken"]`
//! in the `initialize` result. Any other unauthenticated request fails
//! immediately with `401 Unauthorized`.
//!
//! Requests are forwarded to a per-peer relay Unix socket, never the operator
//! socket. Each relay carrier pins its own consent mode (`ask`, `paired`, or
//! `observe-only`), so a network client can never be assigned `Operator`.
//!
//! Bounded slice: `POST /mcp` with a JSON response over `40` IPC verbs and `36`
//! MCP tools. SSE streaming, session management, and `DELETE` are not
//! implemented.

use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::sync::{Arc, RwLock};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use serde_json::{Value, json};

use crate::crypto;
use crate::ipc::ConsentMode;
use crate::mcp::{McpBridge, negotiate_protocol_version};
use crate::mux;
use crate::peers::{InboundPeerTable, LiveInboundPeer, unix_now_secs};

pub const MAX_BODY_BYTES: usize = 1024 * 1024;
const MAX_PAIRING_BODY_BYTES: usize = 64 * 1024;
const MAX_HEADER_LINES: usize = 64;
const IO_TIMEOUT: Duration = Duration::from_secs(5);
pub const PAIRING_TIMEOUT: Duration = Duration::from_secs(60);

/// One statically configured HTTP peer (used by `HttpMcpConfig` / CLI launch).
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

#[derive(Debug)]
pub enum PairingOutcome {
    Approved {
        peer_id: String,
        plaintext_token: String,
    },
    Denied,
}

#[derive(Debug)]
pub struct PairingRequest {
    pub client_name: String,
    pub protocol_version: Option<String>,
    pub remote_addr: SocketAddr,
    pub reply_tx: SyncSender<PairingOutcome>,
}

/// Shared single-slot gate enforcing at most one pending tap-to-pair prompt at
/// a time across all HTTP connections.
#[derive(Clone)]
pub struct PairingGate {
    in_flight: Arc<AtomicBool>,
    tx: SyncSender<PairingRequest>,
}

pub fn new_pairing_gate() -> (PairingGate, Receiver<PairingRequest>) {
    let (tx, rx) = mpsc::sync_channel(1);
    (
        PairingGate {
            in_flight: Arc::new(AtomicBool::new(false)),
            tx,
        },
        rx,
    )
}

struct InFlightGuard(Arc<AtomicBool>);

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
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

/// A random pairing token (`mty_` + base64url-no-pad) generated via
/// `libc::getrandom`.
pub fn generate_token() -> io::Result<String> {
    crypto::generate_pairing_token()
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

/// Spawns an HTTP MCP listener from a fixed `HttpMcpConfig` with tap-to-pair
/// disabled (every token hashed into an `InboundPeerTable`).
pub fn spawn(cfg: HttpMcpConfig) -> io::Result<HttpHandle> {
    validate_bind(&cfg.bind).map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
    validate_peers(&cfg.peers).map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
    let mut table = InboundPeerTable::new();
    let now = unix_now_secs();
    for (idx, peer) in cfg.peers.into_iter().enumerate() {
        table
            .insert(LiveInboundPeer {
                id: format!("cli-{idx}"),
                label: peer.label.clone(),
                client_name: peer.label,
                token_sha256: crypto::sha256(peer.token.as_bytes()),
                socket: peer.socket,
                consent_mode: ConsentMode::Paired,
                ephemeral: true,
                paired_at: now,
                last_seen_at: None,
            })
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
    }
    spawn_pairable(cfg.bind, Arc::new(RwLock::new(table)), None)
}

/// Spawns an HTTP MCP listener backed by a hot-updatable `InboundPeerTable` and
/// an optional symmetric `initialize` tap-to-pair `PairingGate`.
pub fn spawn_pairable(
    bind: SocketAddr,
    peer_table: Arc<RwLock<InboundPeerTable>>,
    pairing: Option<PairingGate>,
) -> io::Result<HttpHandle> {
    validate_bind(&bind).map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
    if pairing.is_none() && peer_table.read().unwrap().is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "an HTTP MCP listener with tap-to-pair disabled requires at least one paired peer",
        ));
    }
    let listener = TcpListener::bind(bind)?;
    listener.set_nonblocking(true)?;
    let local_addr = listener.local_addr()?;
    let shutdown = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&shutdown);

    let join = thread::spawn(move || {
        while !flag.load(Ordering::Relaxed) {
            match listener.accept() {
                Ok((stream, remote_addr)) => {
                    let table = Arc::clone(&peer_table);
                    let gate = pairing.clone();
                    thread::spawn(move || {
                        let _ = handle_conn(stream, remote_addr, &table, gate.as_ref());
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
        429 => "Too Many Requests",
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

fn respond_unpaired(stream: &mut TcpStream) -> io::Result<()> {
    respond(
        stream,
        401,
        r#"{"jsonrpc":"2.0","id":null,"error":{"code":-32001,"message":"unpaired client: a valid pairing token is required"}}"#,
    )
}

fn handle_conn(
    mut stream: TcpStream,
    remote_addr: SocketAddr,
    peer_table: &RwLock<InboundPeerTable>,
    pairing: Option<&PairingGate>,
) -> io::Result<()> {
    stream.set_read_timeout(Some(IO_TIMEOUT))?;
    stream.set_write_timeout(Some(IO_TIMEOUT))?;
    let mut reader = BufReader::new(stream.try_clone()?);

    let mut request_line = String::new();
    if reader.read_line(&mut request_line)? == 0 {
        return Ok(());
    }
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_owned();
    let path = parts.next().unwrap_or_default().to_owned();

    let mut content_length = 0usize;
    let mut authorization: Option<String> = None;
    for _ in 0..MAX_HEADER_LINES {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 {
            break;
        }
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

    // If an Authorization header was sent, verify the Bearer token via SHA-256
    // constant-time lookup before reading any request body.
    if let Some(auth_val) = authorization.as_deref() {
        let presented = auth_val.strip_prefix("Bearer ").map(str::trim);
        let peer = presented.and_then(|tok| {
            peer_table
                .write()
                .ok()
                .and_then(|mut t| t.authenticate(tok, unix_now_secs()))
        });
        let Some(peer) = peer else {
            return respond_unpaired(&mut stream);
        };

        if content_length > MAX_BODY_BYTES {
            return respond(&mut stream, 413, r#"{"error":"request body too large"}"#);
        }
        let mut body = vec![0u8; content_length];
        reader.read_exact(&mut body)?;
        let text = String::from_utf8_lossy(&body);

        let mut bridge = McpBridge::new(peer.socket, ConsentMode::Paired, peer.label);
        return match bridge.handle_line(&text) {
            Some(reply) => respond(&mut stream, 200, &reply),
            None => respond(&mut stream, 202, ""),
        };
    }

    // Unauthenticated POST /mcp: only `initialize` can trigger tap-to-pair, and
    // only when `pairing` is enabled.
    let Some(gate) = pairing else {
        return respond_unpaired(&mut stream);
    };
    if content_length > MAX_BODY_BYTES {
        return respond(&mut stream, 413, r#"{"error":"request body too large"}"#);
    }
    if content_length == 0 || content_length > MAX_PAIRING_BODY_BYTES {
        return respond_unpaired(&mut stream);
    }

    let mut body = vec![0u8; content_length];
    reader.read_exact(&mut body)?;
    let parsed: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(_) => return respond_unpaired(&mut stream),
    };
    if parsed.get("method").and_then(Value::as_str) != Some("initialize") {
        return respond_unpaired(&mut stream);
    }
    let Some(req_id) = parsed.get("id").filter(|id| !id.is_null()).cloned() else {
        return respond_unpaired(&mut stream);
    };

    if gate
        .in_flight
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        let busy = json!({
            "jsonrpc": "2.0",
            "id": req_id,
            "error": {
                "code": -32002,
                "message": "pairing prompt already pending; wait for operator decision"
            }
        });
        return respond(&mut stream, 429, &busy.to_string());
    }
    let _guard = InFlightGuard(Arc::clone(&gate.in_flight));

    let params = parsed.get("params").cloned().unwrap_or_else(|| json!({}));
    let raw_client = params
        .get("clientInfo")
        .and_then(|ci| ci.get("name"))
        .and_then(Value::as_str)
        .unwrap_or("mcp-peer");
    let clean_client = {
        let s = mux::sanitize_text(raw_client.trim(), 64);
        if s.is_empty() {
            String::from("mcp-peer")
        } else {
            s
        }
    };
    let requested_proto = params
        .get("protocolVersion")
        .and_then(Value::as_str)
        .map(str::to_owned);

    let (reply_tx, reply_rx) = mpsc::sync_channel(1);
    if gate
        .tx
        .try_send(PairingRequest {
            client_name: clean_client,
            protocol_version: requested_proto.clone(),
            remote_addr,
            reply_tx,
        })
        .is_err()
    {
        return respond_unpaired(&mut stream);
    }

    match reply_rx.recv_timeout(PAIRING_TIMEOUT) {
        Ok(PairingOutcome::Approved {
            peer_id,
            plaintext_token,
        }) => {
            let negotiated = negotiate_protocol_version(requested_proto.as_deref());
            let ok = json!({
                "jsonrpc": "2.0",
                "id": req_id,
                "result": {
                    "protocolVersion": negotiated,
                    "capabilities": {
                        "tools": {},
                        "resources": {}
                    },
                    "serverInfo": {
                        "name": "mutiny",
                        "version": env!("CARGO_PKG_VERSION")
                    },
                    "_meta": {
                        "sh.mutiny/pairingToken": plaintext_token,
                        "sh.mutiny/peerId": peer_id
                    }
                }
            });
            respond(&mut stream, 200, &ok.to_string())
        }
        Ok(PairingOutcome::Denied) | Err(_) => respond_unpaired(&mut stream),
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
    fn generated_tokens_are_long_random_and_distinct() {
        let a = generate_token().expect("token");
        let b = generate_token().expect("token");
        assert!(a.starts_with("mty_"));
        assert_eq!(a.len(), 47, "mty_ + 43 chars base64url");
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

        let err = parse_http_peer("phone=abc123:operator", ConsentMode::Ask).unwrap_err();
        assert!(err.contains("operator"), "{err}");
        assert!(ConsentMode::parse_peer_mode("operator").is_err());
        assert!(ConsentMode::parse_peer_mode("nonsense").is_err());

        let (_, token, mode) = parse_http_peer("phone=abc:123", ConsentMode::ObserveOnly).unwrap();
        assert_eq!(token, "abc:123");
        assert_eq!(mode, ConsentMode::ObserveOnly);

        assert!(parse_http_peer("no-equals", ConsentMode::Ask).is_err());
        assert!(parse_http_peer("=abc", ConsentMode::Ask).is_err());
        assert!(parse_http_peer("phone=", ConsentMode::Ask).is_err());
    }

    #[test]
    fn each_token_selects_its_own_peer_and_nothing_else_does() {
        let mut table = InboundPeerTable::new();
        for (id, label, tok) in [("1", "phone", "token-aaa"), ("2", "laptop", "token-bbb")] {
            table
                .insert(LiveInboundPeer {
                    id: id.to_owned(),
                    label: label.to_owned(),
                    client_name: label.to_owned(),
                    token_sha256: crypto::sha256(tok.as_bytes()),
                    socket: PathBuf::from(format!("/nonexistent-{label}.sock")),
                    consent_mode: ConsentMode::Ask,
                    ephemeral: true,
                    paired_at: 1,
                    last_seen_at: None,
                })
                .unwrap();
        }
        assert_eq!(table.authenticate("token-aaa", 2).unwrap().label, "phone");
        assert_eq!(table.authenticate("token-bbb", 2).unwrap().label, "laptop");
        assert!(table.authenticate("token-ccc", 2).is_none());
        assert!(table.authenticate("token-aa", 2).is_none());
    }
}
