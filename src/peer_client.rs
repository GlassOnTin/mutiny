//! Outbound stdlib HTTP/1.1 JSON-RPC 2.0 client (`HavenPeerClient`) for
//! dialing a paired Haven phone over SSH loopback forwarding or an encrypted
//! WireGuard/Yggdrasil/private tunnel.
//!
//! Runs on a bounded background worker loop and communicates with
//! `AppController` exclusively via bounded `std::sync::mpsc` channels polled
//! with `try_recv` on the 4 ms tick. Human-interactive 65 s RPCs (`pair-haven`,
//! `get_egress_signing_secret`, `request_egress_grant`, `generate_totp_code`)
//! run on bounded short-lived worker threads (max 4 concurrent) so liveness
//! polling and client teardown never block.

use std::collections::HashMap;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpStream, ToSocketAddrs};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::crypto;
use crate::peers::{InboundNotifyScope, OutboundHavenPeer, OutboundLimbStatus, unix_now_secs};
use crate::topology::{PaneId, WindowId};

pub const MAX_RESPONSE_BYTES: usize = 1024 * 1024;
pub const MAX_HEADER_LINES: usize = 64;
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);
pub const NORMAL_READ_TIMEOUT: Duration = Duration::from_secs(5);
pub const PAIRING_READ_TIMEOUT: Duration = Duration::from_secs(65);
const WRITE_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_CONCURRENT_INTERACTIVE_RPCS: usize = 4;
const DEFAULT_TOTP_ROLLOVER_SLEEP: Duration = Duration::from_millis(2500);

#[derive(Debug, Clone)]
pub struct PeerPollConfig {
    pub connected_interval: Duration,
    pub offline_initial_backoff: Duration,
    pub offline_max_backoff: Duration,
    pub allow_lan: bool,
}

impl Default for PeerPollConfig {
    fn default() -> Self {
        Self {
            connected_interval: Duration::from_secs(15),
            offline_initial_backoff: Duration::from_secs(5),
            offline_max_backoff: Duration::from_secs(30),
            allow_lan: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedPeerUrl {
    pub host: String,
    pub port: u16,
    pub path: String,
    pub host_header: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HavenAuthPrompt {
    pub profile_id: String,
    pub label: String,
    pub requires_passphrase: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TotpSecretItem {
    pub id: String,
    pub label: String,
    pub issuer: Option<String>,
    pub account_name: Option<String>,
}

/// Checks whether a plaintext HTTP target IP is permitted.
pub fn is_allowed_plaintext_ip(ip: IpAddr, allow_lan: bool) -> bool {
    match ip {
        IpAddr::V4(v4) => is_allowed_v4(v4, allow_lan),
        IpAddr::V6(v6) => {
            if let Some(mapped) = v6.to_ipv4_mapped() {
                return is_allowed_v4(mapped, allow_lan);
            }
            is_allowed_v6(v6, allow_lan)
        }
    }
}

fn is_allowed_v4(v4: Ipv4Addr, allow_lan: bool) -> bool {
    if v4.is_unspecified() || v4.is_broadcast() || v4.is_multicast() {
        return false;
    }
    if v4.is_loopback() || v4.is_private() {
        return true;
    }
    let oct = v4.octets();
    // 100.64.0.0/10 (RFC 6598 CGNAT / Tailscale / WireGuard overlays)
    if oct[0] == 100 && (oct[1] & 0xc0) == 64 {
        return true;
    }
    if allow_lan && v4.is_link_local() {
        return true;
    }
    false
}

fn is_allowed_v6(v6: Ipv6Addr, allow_lan: bool) -> bool {
    if v6.is_unspecified() || v6.is_multicast() {
        return false;
    }
    if v6.is_loopback() {
        return true;
    }
    let seg0 = v6.segments()[0];
    // fc00::/7 (RFC 4193 Unique Local) or 0200::/7 (Yggdrasil overlay)
    if (seg0 & 0xfe00) == 0xfc00 || (seg0 & 0xfe00) == 0x0200 {
        return true;
    }
    if allow_lan && v6.is_unicast_link_local() {
        return true;
    }
    false
}

/// Parses `http://host[:port][/path]` and validates that literal IPs obey the
/// plaintext tunnel/loopback policy before any socket is opened.
pub fn parse_and_validate_peer_url(raw: &str, allow_lan: bool) -> Result<ParsedPeerUrl, String> {
    let url = raw.trim();
    if url.is_empty() {
        return Err(String::from("peer URL must not be empty"));
    }
    if url.chars().any(|c| c.is_control() || c.is_whitespace()) {
        return Err(String::from(
            "peer URL must not contain whitespace or control characters",
        ));
    }
    if url.starts_with("https://") {
        return Err(String::from(
            "HTTPS is not supported by the stdlib peer client; use an SSH loopback forward or WireGuard/Yggdrasil http:// endpoint",
        ));
    }
    let rest = url
        .strip_prefix("http://")
        .ok_or_else(|| format!("peer URL {url:?} must start with http://"))?;
    if rest.contains('#') {
        return Err(String::from("peer URL must not contain a #fragment"));
    }

    let (authority, path) = match rest.find('/') {
        Some(idx) => (&rest[..idx], &rest[idx..]),
        None => (rest, "/"),
    };
    if authority.is_empty() {
        return Err(String::from("peer URL is missing a host"));
    }
    if authority.contains('@') {
        return Err(String::from(
            "peer URL must not embed credentials in user@host",
        ));
    }

    let (host, port, bracketed) = if let Some(after_open) = authority.strip_prefix('[') {
        let close_idx = after_open
            .find(']')
            .ok_or_else(|| String::from("unterminated '[' in IPv6 host literal"))?;
        let ipv6_lit = &after_open[..close_idx];
        let port_part = &after_open[close_idx + 1..];
        let port = if port_part.is_empty() {
            80u16
        } else if let Some(p_str) = port_part.strip_prefix(':') {
            parse_port(p_str)?
        } else {
            return Err(format!(
                "unexpected characters {port_part:?} after IPv6 literal"
            ));
        };
        let ip: Ipv6Addr = ipv6_lit
            .parse()
            .map_err(|_| format!("invalid IPv6 literal [{ipv6_lit}]"))?;
        if !is_allowed_plaintext_ip(IpAddr::V6(ip), allow_lan) {
            return Err(format!(
                "refusing plaintext HTTP to public or unspecified IP [{ip}]; use loopback or a WireGuard/Yggdrasil/private tunnel"
            ));
        }
        (ipv6_lit.to_owned(), port, true)
    } else {
        let mut parts = authority.split(':');
        let host_part = parts.next().unwrap_or_default();
        let port = match (parts.next(), parts.next()) {
            (None, None) => 80u16,
            (Some(p_str), None) => parse_port(p_str)?,
            (_, Some(_)) => {
                return Err(String::from(
                    "IPv6 literal in URL must be enclosed in brackets, e.g. http://[::1]:8740/mcp",
                ));
            }
        };
        if host_part.is_empty() {
            return Err(String::from("peer URL is missing a host"));
        }
        if let Ok(ip) = host_part.parse::<IpAddr>() {
            if !is_allowed_plaintext_ip(ip, allow_lan) {
                return Err(format!(
                    "refusing plaintext HTTP to public or unspecified IP {ip}; use loopback or a WireGuard/Yggdrasil/private tunnel"
                ));
            }
        } else if !host_part.eq_ignore_ascii_case("localhost") && !allow_lan {
            return Err(format!(
                "refusing non-loopback hostname {host_part:?} over plaintext HTTP; pass a tunnel IP literal or --allow-lan-peer"
            ));
        }
        (host_part.to_owned(), port, false)
    };

    let host_header = if bracketed {
        if port == 80 {
            format!("[{host}]")
        } else {
            format!("[{host}]:{port}")
        }
    } else if port == 80 {
        host.clone()
    } else {
        format!("{host}:{port}")
    };

    Ok(ParsedPeerUrl {
        host,
        port,
        path: path.to_owned(),
        host_header,
    })
}

fn parse_port(raw: &str) -> Result<u16, String> {
    let p: u16 = raw
        .parse()
        .map_err(|_| format!("invalid TCP port {raw:?}"))?;
    if p == 0 {
        return Err(String::from("TCP port must be between 1 and 65535"));
    }
    Ok(p)
}

fn resolve_checked_addrs(
    parsed: &ParsedPeerUrl,
    allow_lan: bool,
) -> Result<Vec<SocketAddr>, String> {
    if let Ok(ip) = parsed.host.parse::<IpAddr>() {
        if !is_allowed_plaintext_ip(ip, allow_lan) {
            return Err(format!("refusing plaintext HTTP to disallowed IP {ip}"));
        }
        return Ok(vec![SocketAddr::new(ip, parsed.port)]);
    }
    if parsed.host.eq_ignore_ascii_case("localhost") {
        return Ok(vec![
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), parsed.port),
            SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), parsed.port),
        ]);
    }
    let addrs: Vec<SocketAddr> = (parsed.host.as_str(), parsed.port)
        .to_socket_addrs()
        .map_err(|e| format!("failed to resolve {}: {e}", parsed.host))?
        .collect();
    if addrs.is_empty() {
        return Err(format!(
            "hostname {} resolved to zero addresses",
            parsed.host
        ));
    }
    for addr in &addrs {
        if !is_allowed_plaintext_ip(addr.ip(), allow_lan) {
            return Err(format!(
                "refusing plaintext HTTP to {} because it resolved to disallowed IP {}",
                parsed.host,
                addr.ip()
            ));
        }
    }
    Ok(addrs)
}

/// Sends one HTTP/1.1 POST carrying `payload` and returns `(http_status, body_text)`.
pub fn http_post_json(
    url: &str,
    bearer_token: Option<&str>,
    payload: &Value,
    read_timeout: Duration,
    allow_lan: bool,
) -> Result<(u16, String), String> {
    let parsed = parse_and_validate_peer_url(url, allow_lan)?;
    let addrs = resolve_checked_addrs(&parsed, allow_lan)?;
    let body = serde_json::to_vec(payload).map_err(|e| format!("JSON encode error: {e}"))?;

    let mut last_err = None;
    let mut stream_opt = None;
    for addr in addrs {
        match TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT) {
            Ok(s) => {
                stream_opt = Some(s);
                break;
            }
            Err(e) => last_err = Some(e),
        }
    }
    let mut stream = stream_opt.ok_or_else(|| {
        format!(
            "connect to {} failed: {}",
            parsed.host_header,
            last_err
                .map(|e| e.to_string())
                .unwrap_or_else(|| String::from("no addresses"))
        )
    })?;
    stream
        .set_write_timeout(Some(WRITE_TIMEOUT))
        .map_err(|e| format!("set_write_timeout: {e}"))?;
    stream
        .set_read_timeout(Some(read_timeout))
        .map_err(|e| format!("set_read_timeout: {e}"))?;

    if let Some(tok) = bearer_token
        && tok.chars().any(|c| c == '\r' || c == '\n')
    {
        return Err(String::from("bearer token contains invalid newline"));
    }

    let mut req_head = format!(
        "POST {} HTTP/1.1\r\nHost: {}\r\nContent-Type: application/json\r\nAccept: application/json\r\n",
        parsed.path, parsed.host_header
    );
    if let Some(tok) = bearer_token {
        req_head.push_str(&format!("Authorization: Bearer {tok}\r\n"));
    }
    req_head.push_str(&format!(
        "Content-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    ));

    stream
        .write_all(req_head.as_bytes())
        .and_then(|()| stream.write_all(&body))
        .and_then(|()| stream.flush())
        .map_err(|e| format!("HTTP write failed: {e}"))?;

    let mut reader = BufReader::new(stream);
    let mut status_line = String::new();
    let n = read_bounded_line(&mut reader, &mut status_line, 4096)?;
    if n == 0 {
        return Err(String::from(
            "peer closed connection before HTTP status line",
        ));
    }
    let status: u16 = status_line
        .split_whitespace()
        .nth(1)
        .ok_or_else(|| format!("malformed HTTP status line: {:?}", status_line.trim_end()))?
        .parse()
        .map_err(|_| format!("invalid HTTP status code in {:?}", status_line.trim_end()))?;

    let mut content_length: Option<usize> = None;
    let mut is_chunked = false;
    for _ in 0..MAX_HEADER_LINES {
        let mut line = String::new();
        let count = read_bounded_line(&mut reader, &mut line, 8192)?;
        if count == 0 {
            return Err(String::from("unexpected EOF inside HTTP response headers"));
        }
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            break;
        }
        if let Some((k, v)) = trimmed.split_once(':') {
            match k.trim().to_ascii_lowercase().as_str() {
                "content-length" => {
                    let len: usize = v
                        .trim()
                        .parse()
                        .map_err(|_| format!("invalid Content-Length {:?}", v.trim()))?;
                    if len > MAX_RESPONSE_BYTES {
                        return Err(format!(
                            "HTTP response Content-Length {len} exceeds {MAX_RESPONSE_BYTES} byte cap"
                        ));
                    }
                    content_length = Some(len);
                }
                "transfer-encoding" if v.to_ascii_lowercase().contains("chunked") => {
                    is_chunked = true;
                }
                _ => {}
            }
        }
    }

    let resp_bytes = if is_chunked {
        read_chunked_body(&mut reader)?
    } else if let Some(len) = content_length {
        let mut buf = vec![0u8; len];
        reader
            .read_exact(&mut buf)
            .map_err(|e| format!("HTTP body read failed: {e}"))?;
        buf
    } else {
        let mut buf = Vec::new();
        reader
            .take((MAX_RESPONSE_BYTES + 1) as u64)
            .read_to_end(&mut buf)
            .map_err(|e| format!("HTTP EOF read failed: {e}"))?;
        if buf.len() > MAX_RESPONSE_BYTES {
            return Err(format!(
                "HTTP response body exceeds {MAX_RESPONSE_BYTES} byte cap"
            ));
        }
        buf
    };

    let text =
        String::from_utf8(resp_bytes).map_err(|e| format!("response is not valid UTF-8: {e}"))?;
    Ok((status, text))
}

fn read_bounded_line<R: BufRead>(
    reader: &mut R,
    out: &mut String,
    cap: usize,
) -> Result<usize, String> {
    let mut raw = Vec::new();
    let n = reader
        .take((cap + 1) as u64)
        .read_until(b'\n', &mut raw)
        .map_err(|e| format!("HTTP header read failed: {e}"))?;
    if raw.len() > cap {
        return Err(format!("HTTP header line exceeds {cap} byte cap"));
    }
    let s = String::from_utf8(raw).map_err(|e| format!("non-UTF8 HTTP header: {e}"))?;
    out.push_str(&s);
    Ok(n)
}

fn read_chunked_body<R: BufRead>(reader: &mut R) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    loop {
        let mut size_line = String::new();
        let n = read_bounded_line(reader, &mut size_line, 256)?;
        if n == 0 {
            return Err(String::from("unexpected EOF reading chunk size"));
        }
        let hex_part = size_line
            .trim()
            .split(';')
            .next()
            .unwrap_or_default()
            .trim();
        let chunk_len = usize::from_str_radix(hex_part, 16)
            .map_err(|_| format!("invalid chunk size {hex_part:?}"))?;
        if chunk_len == 0 {
            let mut tail = String::new();
            let _ = read_bounded_line(reader, &mut tail, 1024);
            break;
        }
        if out.len().saturating_add(chunk_len) > MAX_RESPONSE_BYTES {
            return Err(format!(
                "chunked HTTP response exceeds {MAX_RESPONSE_BYTES} byte cap"
            ));
        }
        let start = out.len();
        out.resize(start + chunk_len, 0);
        reader
            .read_exact(&mut out[start..])
            .map_err(|e| format!("chunk read failed: {e}"))?;
        let mut crlf = [0u8; 2];
        reader
            .read_exact(&mut crlf)
            .map_err(|e| format!("chunk CRLF read failed: {e}"))?;
    }
    Ok(out)
}

/// Sends a JSON-RPC 2.0 request and extracts the `result` value, surfacing
/// HTTP status errors and JSON-RPC `error` objects with concise messages.
pub fn jsonrpc_call(
    url: &str,
    bearer_token: Option<&str>,
    id: u64,
    method: &str,
    params: Value,
    read_timeout: Duration,
    allow_lan: bool,
) -> Result<Value, (Option<i32>, String)> {
    let payload = json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": method,
        "params": params,
    });
    let (status, text) = http_post_json(url, bearer_token, &payload, read_timeout, allow_lan)
        .map_err(|e| (None, e))?;
    if status == 401 {
        return Err((
            Some(-32001),
            String::from(
                "HTTP 401 Unauthorized (peer rejected or revoked token, or denied pairing)",
            ),
        ));
    }
    if status == 429 {
        return Err((
            Some(-32002),
            String::from(
                "HTTP 429 Too Many Requests (a pairing prompt is already pending on peer)",
            ),
        ));
    }
    if !(200..300).contains(&status) {
        return Err((
            None,
            format!(
                "HTTP {status}: {}",
                text.chars().take(160).collect::<String>()
            ),
        ));
    }
    let val: Value = serde_json::from_str(&text)
        .map_err(|e| (None, format!("invalid JSON-RPC response: {e}")))?;
    if let Some(err_obj) = val.get("error").filter(|v| !v.is_null()) {
        let code = err_obj
            .get("code")
            .and_then(Value::as_i64)
            .map(|c| c as i32);
        let msg = err_obj
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("remote JSON-RPC error");
        return Err((code, msg.to_owned()));
    }
    val.get("result")
        .cloned()
        .ok_or_else(|| (None, String::from("JSON-RPC response missing `result`")))
}

/// Extracts the structured JSON object from an MCP `tools/call` result,
/// accepting either `structuredContent` or JSON-encoded `content[0].text`.
pub fn extract_mcp_tool_json(val: &Value) -> Result<Value, String> {
    let is_err = val.get("isError").and_then(Value::as_bool).unwrap_or(false);
    if is_err {
        let msg = val
            .get("content")
            .and_then(Value::as_array)
            .and_then(|a| a.first())
            .and_then(|c| c.get("text"))
            .and_then(Value::as_str)
            .unwrap_or("remote MCP tool returned isError=true");
        return Err(msg.to_owned());
    }
    if let Some(sc) = val.get("structuredContent").filter(|v| v.is_object()) {
        return Ok(sc.clone());
    }
    if let Some(text) = val
        .get("content")
        .and_then(Value::as_array)
        .and_then(|a| a.first())
        .and_then(|c| c.get("text"))
        .and_then(Value::as_str)
    {
        return serde_json::from_str::<Value>(text)
            .map_err(|e| format!("tool text is not valid JSON: {e}"));
    }
    if val.is_object() {
        return Ok(val.clone());
    }
    Err(String::from("MCP tool result contained no JSON payload"))
}

#[derive(Debug)]
pub enum PeerClientCommand {
    SetPeers(Vec<OutboundHavenPeer>),
    PairOutbound {
        req_id: u64,
        label: String,
        url: String,
        bearer_token: Option<String>,
        enroll_signer: bool,
    },
    EnrollSigner {
        req_id: u64,
        peer_id_or_label: String,
    },
    RequestConsent {
        consent_id: u64,
        peer_id: String,
        request_id: String,
        sandbox_id: String,
        host: String,
        port: u16,
        scheme: String,
        reason: String,
    },
    AnswerAuthPrompt {
        peer_id: String,
        password: String,
        remember_password: bool,
    },
    ListTotpSecrets {
        peer_id: String,
        target_pane: PaneId,
        target_window: Option<WindowId>,
    },
    GenerateTotpCode {
        peer_id: String,
        totp_secret_id: String,
        target_pane: PaneId,
    },
    PollNow,
    CallTool {
        req_id: u64,
        peer_id: String,
        tool_name: String,
        arguments: Value,
        reply_tx: Option<SyncSender<Result<Value, String>>>,
    },
    Shutdown,
}

#[derive(Debug, Clone)]
pub enum PeerClientEvent {
    StatusChanged {
        peer_id: Option<String>,
        status: OutboundLimbStatus,
        connected_at: Option<u64>,
    },
    PairCompleted {
        req_id: u64,
        result: Result<(OutboundHavenPeer, OutboundLimbStatus), String>,
    },
    SignerEnrolled {
        req_id: u64,
        result: Result<(String, String, String), String>,
    },
    ConsentCompleted {
        consent_id: u64,
        peer_id: String,
        result: Result<Value, String>,
    },
    AuthPromptChanged {
        peer_id: String,
        peer_label: String,
        prompt: Option<HavenAuthPrompt>,
    },
    TotpChoicesReady {
        peer_id: String,
        target_pane: PaneId,
        target_window: Option<WindowId>,
        secrets: Vec<TotpSecretItem>,
    },
    TotpCodeReady {
        target_pane: PaneId,
        code: String,
    },
    TotpError(String),
    ToolCallCompleted {
        req_id: u64,
        peer_id: String,
        tool_name: String,
        result: Result<Value, String>,
    },
}

pub struct HavenPeerClientHandle {
    cmd_tx: SyncSender<PeerClientCommand>,
    evt_rx: Receiver<PeerClientEvent>,
    join: Option<JoinHandle<()>>,
}

impl HavenPeerClientHandle {
    pub fn spawn(initial_peers: Vec<OutboundHavenPeer>, cfg: PeerPollConfig) -> Self {
        let (cmd_tx, cmd_rx) = mpsc::sync_channel::<PeerClientCommand>(64);
        let (evt_tx, evt_rx) = mpsc::sync_channel::<PeerClientEvent>(128);
        let cmd_tx_internal = cmd_tx.clone();
        let join =
            thread::spawn(move || worker_loop(initial_peers, cfg, cmd_rx, cmd_tx_internal, evt_tx));
        Self {
            cmd_tx,
            evt_rx,
            join: Some(join),
        }
    }

    pub fn try_recv(&self) -> Option<PeerClientEvent> {
        self.evt_rx.try_recv().ok()
    }

    pub fn set_peers(&self, peers: Vec<OutboundHavenPeer>) -> Result<(), String> {
        self.cmd_tx
            .try_send(PeerClientCommand::SetPeers(peers))
            .map_err(|e| format!("peer client queue unavailable: {e}"))
    }

    pub fn poll_now(&self) {
        let _ = self.cmd_tx.try_send(PeerClientCommand::PollNow);
    }

    pub fn pair_outbound(
        &self,
        req_id: u64,
        label: String,
        url: String,
        bearer_token: Option<String>,
        enroll_signer: bool,
    ) -> Result<(), String> {
        self.cmd_tx
            .try_send(PeerClientCommand::PairOutbound {
                req_id,
                label,
                url,
                bearer_token,
                enroll_signer,
            })
            .map_err(|e| format!("peer client busy or unavailable: {e}"))
    }

    pub fn enroll_signer(&self, req_id: u64, peer_id_or_label: String) -> Result<(), String> {
        self.cmd_tx
            .try_send(PeerClientCommand::EnrollSigner {
                req_id,
                peer_id_or_label,
            })
            .map_err(|e| format!("peer client busy or unavailable: {e}"))
    }

    #[allow(clippy::too_many_arguments)]
    pub fn request_consent(
        &self,
        consent_id: u64,
        peer_id: String,
        request_id: String,
        sandbox_id: String,
        host: String,
        port: u16,
        scheme: String,
        reason: String,
    ) -> Result<(), String> {
        self.cmd_tx
            .try_send(PeerClientCommand::RequestConsent {
                consent_id,
                peer_id,
                request_id,
                sandbox_id,
                host,
                port,
                scheme,
                reason,
            })
            .map_err(|e| format!("peer client busy or unavailable: {e}"))
    }

    pub fn answer_auth_prompt(
        &self,
        peer_id: String,
        password: String,
        remember_password: bool,
    ) -> Result<(), String> {
        self.cmd_tx
            .try_send(PeerClientCommand::AnswerAuthPrompt {
                peer_id,
                password,
                remember_password,
            })
            .map_err(|e| format!("peer client busy or unavailable: {e}"))
    }

    pub fn list_totp_secrets(
        &self,
        peer_id: String,
        target_pane: PaneId,
        target_window: Option<WindowId>,
    ) -> Result<(), String> {
        self.cmd_tx
            .try_send(PeerClientCommand::ListTotpSecrets {
                peer_id,
                target_pane,
                target_window,
            })
            .map_err(|e| format!("peer client busy or unavailable: {e}"))
    }

    pub fn generate_totp_code(
        &self,
        peer_id: String,
        totp_secret_id: String,
        target_pane: PaneId,
    ) -> Result<(), String> {
        self.cmd_tx
            .try_send(PeerClientCommand::GenerateTotpCode {
                peer_id,
                totp_secret_id,
                target_pane,
            })
            .map_err(|e| format!("peer client busy or unavailable: {e}"))
    }

    /// Asynchronous non-blocking `tools/call` dispatch whose result arrives via
    /// `PeerClientEvent::ToolCallCompleted` on the 4 ms tick.
    pub fn send_call_tool(
        &self,
        req_id: u64,
        peer_id: String,
        tool_name: String,
        arguments: Value,
    ) -> Result<(), String> {
        self.cmd_tx
            .try_send(PeerClientCommand::CallTool {
                req_id,
                peer_id,
                tool_name,
                arguments,
                reply_tx: None,
            })
            .map_err(|e| format!("peer client queue full: {e}"))
    }

    /// Synchronous helper for non-UI callers/tests to invoke `tools/call` on a
    /// paired outbound Haven peer.
    pub fn call_tool(
        &self,
        peer_id: &str,
        tool_name: &str,
        arguments: Value,
        timeout: Duration,
    ) -> Result<Value, String> {
        let (reply_tx, reply_rx) = mpsc::sync_channel(1);
        self.cmd_tx
            .send(PeerClientCommand::CallTool {
                req_id: 0,
                peer_id: peer_id.to_owned(),
                tool_name: tool_name.to_owned(),
                arguments,
                reply_tx: Some(reply_tx),
            })
            .map_err(|e| format!("peer client stopped: {e}"))?;
        reply_rx
            .recv_timeout(timeout)
            .map_err(|e| format!("peer tool call timed out: {e}"))?
    }

    pub fn stop(&mut self) {
        let _ = self.cmd_tx.try_send(PeerClientCommand::Shutdown);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

impl Drop for HavenPeerClientHandle {
    fn drop(&mut self) {
        self.stop();
    }
}

struct TrackedPeer {
    peer: OutboundHavenPeer,
    next_poll: Instant,
    failures: u32,
    remote_version: Option<String>,
    /// `Some(true)` if peer's `tools/list` advertises `request_peer_consent`,
    /// `Some(false)` if it uses `request_egress_grant`, `None` before discovery.
    has_request_peer_consent: Option<bool>,
}

struct InteractiveSlotGuard(Arc<AtomicUsize>);

impl Drop for InteractiveSlotGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

fn try_acquire_interactive_slot(counter: &Arc<AtomicUsize>) -> Option<InteractiveSlotGuard> {
    let mut cur = counter.load(Ordering::Acquire);
    loop {
        if cur >= MAX_CONCURRENT_INTERACTIVE_RPCS {
            return None;
        }
        match counter.compare_exchange_weak(cur, cur + 1, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => return Some(InteractiveSlotGuard(Arc::clone(counter))),
            Err(actual) => cur = actual,
        }
    }
}

fn generate_outbound_id() -> String {
    let mut raw = [0u8; 4];
    if crypto::fill_random(&mut raw).is_ok() {
        format!("hvn-{}", crypto::hex_encode(&raw))
    } else {
        format!("hvn-{}", unix_now_secs())
    }
}

fn find_tracked<'a>(
    tracked: &'a HashMap<String, TrackedPeer>,
    id_or_label: &str,
) -> Option<&'a TrackedPeer> {
    tracked
        .get(id_or_label)
        .or_else(|| tracked.values().find(|t| t.peer.label == id_or_label))
}

fn worker_loop(
    initial_peers: Vec<OutboundHavenPeer>,
    cfg: PeerPollConfig,
    cmd_rx: Receiver<PeerClientCommand>,
    cmd_tx_self: SyncSender<PeerClientCommand>,
    evt_tx: SyncSender<PeerClientEvent>,
) {
    let mut rpc_seq: u64 = 1;
    let interactive_inflight = Arc::new(AtomicUsize::new(0));
    let mut tracked: HashMap<String, TrackedPeer> = HashMap::new();
    let now = Instant::now();
    for p in initial_peers {
        tracked.insert(
            p.id.clone(),
            TrackedPeer {
                peer: p,
                next_poll: now,
                failures: 0,
                remote_version: None,
                has_request_peer_consent: None,
            },
        );
    }

    loop {
        let now = Instant::now();
        let next_wakeup = tracked
            .values()
            .map(|t| t.next_poll)
            .min()
            .map(|deadline| deadline.saturating_duration_since(now))
            .unwrap_or_else(|| Duration::from_secs(60));

        match cmd_rx.recv_timeout(next_wakeup) {
            Ok(PeerClientCommand::Shutdown) | Err(RecvTimeoutError::Disconnected) => break,
            Ok(PeerClientCommand::PollNow) => {
                let t_now = Instant::now();
                for entry in tracked.values_mut() {
                    entry.next_poll = t_now;
                }
            }
            Ok(PeerClientCommand::SetPeers(new_peers)) => {
                let mut next_map = HashMap::with_capacity(new_peers.len());
                let t_now = Instant::now();
                for p in new_peers {
                    if let Some(mut existing) = tracked.remove(&p.id) {
                        let url_or_tok_changed = existing.peer.url != p.url
                            || existing.peer.bearer_token != p.bearer_token;
                        existing.peer = p;
                        if url_or_tok_changed {
                            existing.next_poll = t_now;
                            existing.failures = 0;
                            existing.has_request_peer_consent = None;
                        }
                        next_map.insert(existing.peer.id.clone(), existing);
                    } else {
                        next_map.insert(
                            p.id.clone(),
                            TrackedPeer {
                                peer: p,
                                next_poll: t_now,
                                failures: 0,
                                remote_version: None,
                                has_request_peer_consent: None,
                            },
                        );
                    }
                }
                tracked = next_map;
            }
            Ok(PeerClientCommand::PairOutbound {
                req_id,
                label,
                url,
                bearer_token,
                enroll_signer,
            }) => {
                let Some(slot) = try_acquire_interactive_slot(&interactive_inflight) else {
                    let _ = evt_tx.send(PeerClientEvent::PairCompleted {
                        req_id,
                        result: Err(String::from("too many interactive peer requests in flight")),
                    });
                    continue;
                };
                let _ = evt_tx.try_send(PeerClientEvent::StatusChanged {
                    peer_id: None,
                    status: OutboundLimbStatus::Pairing {
                        label: label.clone(),
                    },
                    connected_at: None,
                });
                rpc_seq = rpc_seq.wrapping_add(1);
                let seq = rpc_seq;
                let allow_lan = cfg.allow_lan;
                let evt_cl = evt_tx.clone();
                let cmd_cl = cmd_tx_self.clone();
                let existing_peers: Vec<OutboundHavenPeer> =
                    tracked.values().map(|t| t.peer.clone()).collect();
                thread::spawn(move || {
                    let _guard = slot;
                    let pair_res = run_pair_outbound(
                        seq,
                        &label,
                        &url,
                        bearer_token.as_deref(),
                        enroll_signer,
                        allow_lan,
                    );
                    match pair_res {
                        Ok((peer, status)) => {
                            let mut updated = existing_peers;
                            updated.retain(|p| p.label != peer.label && p.id != peer.id);
                            updated.push(peer.clone());
                            let _ = cmd_cl.try_send(PeerClientCommand::SetPeers(updated));
                            let _ = evt_cl.send(PeerClientEvent::PairCompleted {
                                req_id,
                                result: Ok((peer, status)),
                            });
                        }
                        Err(err) => {
                            let _ = evt_cl.send(PeerClientEvent::PairCompleted {
                                req_id,
                                result: Err(err),
                            });
                        }
                    }
                });
            }
            Ok(PeerClientCommand::EnrollSigner {
                req_id,
                peer_id_or_label,
            }) => {
                let Some(entry) = find_tracked(&tracked, &peer_id_or_label) else {
                    let _ = evt_tx.send(PeerClientEvent::SignerEnrolled {
                        req_id,
                        result: Err(format!(
                            "no paired outbound Haven peer matching {peer_id_or_label:?}"
                        )),
                    });
                    continue;
                };
                let Some(slot) = try_acquire_interactive_slot(&interactive_inflight) else {
                    let _ = evt_tx.send(PeerClientEvent::SignerEnrolled {
                        req_id,
                        result: Err(String::from("too many interactive peer requests in flight")),
                    });
                    continue;
                };
                rpc_seq = rpc_seq.wrapping_add(1);
                let seq = rpc_seq;
                let peer_id = entry.peer.id.clone();
                let url = entry.peer.url.clone();
                let tok = entry.peer.bearer_token.clone();
                let allow_lan = cfg.allow_lan;
                let evt_cl = evt_tx.clone();
                thread::spawn(move || {
                    let _guard = slot;
                    let res = fetch_signing_secret(&url, &tok, seq, allow_lan)
                        .map(|(sec_b64, key_id)| (peer_id, sec_b64, key_id));
                    let _ = evt_cl.send(PeerClientEvent::SignerEnrolled {
                        req_id,
                        result: res,
                    });
                });
            }
            Ok(PeerClientCommand::RequestConsent {
                consent_id,
                peer_id,
                request_id,
                sandbox_id,
                host,
                port,
                scheme,
                reason,
            }) => {
                let Some(entry) = tracked.get_mut(&peer_id) else {
                    let _ = evt_tx.try_send(PeerClientEvent::ConsentCompleted {
                        consent_id,
                        peer_id,
                        result: Err(String::from("outbound Haven peer not found")),
                    });
                    continue;
                };
                let Some(slot) = try_acquire_interactive_slot(&interactive_inflight) else {
                    let _ = evt_tx.try_send(PeerClientEvent::ConsentCompleted {
                        consent_id,
                        peer_id,
                        result: Err(String::from("too many interactive peer requests in flight")),
                    });
                    continue;
                };
                if entry.has_request_peer_consent.is_none() {
                    rpc_seq = rpc_seq.wrapping_add(1);
                    entry.has_request_peer_consent = Some(detect_request_peer_consent(
                        &entry.peer.url,
                        &entry.peer.bearer_token,
                        rpc_seq,
                        cfg.allow_lan,
                    ));
                }
                let use_peer_consent = entry.has_request_peer_consent.unwrap_or(false);
                let tool_name = if use_peer_consent {
                    "request_peer_consent"
                } else {
                    "request_egress_grant"
                };
                rpc_seq = rpc_seq.wrapping_add(1);
                let seq = rpc_seq;
                let url = entry.peer.url.clone();
                let tok = entry.peer.bearer_token.clone();
                let allow_lan = cfg.allow_lan;
                let evt_cl = evt_tx.clone();
                thread::spawn(move || {
                    let _guard = slot;
                    let args = json!({
                        "request_id": request_id,
                        "sandbox_id": sandbox_id,
                        "host": host,
                        "port": port,
                        "scheme": scheme,
                        "reason": reason,
                    });
                    let res = jsonrpc_call(
                        &url,
                        Some(&tok),
                        seq,
                        "tools/call",
                        json!({
                            "name": tool_name,
                            "arguments": args,
                        }),
                        PAIRING_READ_TIMEOUT,
                        allow_lan,
                    )
                    .map_err(|(_, m)| m)
                    .and_then(|v| extract_mcp_tool_json(&v));
                    let _ = evt_cl.try_send(PeerClientEvent::ConsentCompleted {
                        consent_id,
                        peer_id,
                        result: res,
                    });
                });
            }
            Ok(PeerClientCommand::AnswerAuthPrompt {
                peer_id,
                password,
                remember_password,
            }) => {
                if let Some(entry) = tracked.get_mut(&peer_id) {
                    rpc_seq = rpc_seq.wrapping_add(1);
                    let _ = call_remote_tool(
                        &entry.peer.url,
                        &entry.peer.bearer_token,
                        rpc_seq,
                        "answer_auth_prompt",
                        json!({
                            "password": password,
                            "rememberPassword": remember_password,
                        }),
                        cfg.allow_lan,
                    );
                    entry.next_poll = Instant::now();
                }
            }
            Ok(PeerClientCommand::ListTotpSecrets {
                peer_id,
                target_pane,
                target_window,
            }) => {
                let Some(entry) = find_tracked(&tracked, &peer_id) else {
                    let _ = evt_tx.try_send(PeerClientEvent::TotpError(String::from(
                        "no paired outbound Haven peer available",
                    )));
                    continue;
                };
                let Some(slot) = try_acquire_interactive_slot(&interactive_inflight) else {
                    let _ = evt_tx.try_send(PeerClientEvent::TotpError(String::from(
                        "too many interactive peer requests in flight",
                    )));
                    continue;
                };
                rpc_seq = rpc_seq.wrapping_add(2);
                let seq = rpc_seq;
                let resolved_id = entry.peer.id.clone();
                let url = entry.peer.url.clone();
                let tok = entry.peer.bearer_token.clone();
                let allow_lan = cfg.allow_lan;
                let evt_cl = evt_tx.clone();
                thread::spawn(move || {
                    let _guard = slot;
                    match fetch_totp_list(&url, &tok, seq, allow_lan) {
                        Ok(mut secrets) => {
                            if secrets.is_empty() {
                                let _ = evt_cl.try_send(PeerClientEvent::TotpError(String::from(
                                    "no TOTP secrets saved on Haven",
                                )));
                            } else if secrets.len() == 1 {
                                let chosen = secrets.remove(0);
                                match fetch_fresh_totp_code(
                                    &url,
                                    &tok,
                                    seq.wrapping_add(1),
                                    &chosen.id,
                                    allow_lan,
                                ) {
                                    Ok(code) => {
                                        let _ = evt_cl.try_send(PeerClientEvent::TotpCodeReady {
                                            target_pane,
                                            code,
                                        });
                                    }
                                    Err(err) => {
                                        let _ = evt_cl.try_send(PeerClientEvent::TotpError(err));
                                    }
                                }
                            } else {
                                let _ = evt_cl.try_send(PeerClientEvent::TotpChoicesReady {
                                    peer_id: resolved_id,
                                    target_pane,
                                    target_window,
                                    secrets,
                                });
                            }
                        }
                        Err(err) => {
                            let _ = evt_cl.try_send(PeerClientEvent::TotpError(err));
                        }
                    }
                });
            }
            Ok(PeerClientCommand::GenerateTotpCode {
                peer_id,
                totp_secret_id,
                target_pane,
            }) => {
                let Some(entry) = find_tracked(&tracked, &peer_id) else {
                    let _ = evt_tx.try_send(PeerClientEvent::TotpError(String::from(
                        "no paired outbound Haven peer available",
                    )));
                    continue;
                };
                let Some(slot) = try_acquire_interactive_slot(&interactive_inflight) else {
                    let _ = evt_tx.try_send(PeerClientEvent::TotpError(String::from(
                        "too many interactive peer requests in flight",
                    )));
                    continue;
                };
                rpc_seq = rpc_seq.wrapping_add(2);
                let seq = rpc_seq;
                let url = entry.peer.url.clone();
                let tok = entry.peer.bearer_token.clone();
                let allow_lan = cfg.allow_lan;
                let evt_cl = evt_tx.clone();
                thread::spawn(move || {
                    let _guard = slot;
                    match fetch_fresh_totp_code(&url, &tok, seq, &totp_secret_id, allow_lan) {
                        Ok(code) => {
                            let _ = evt_cl
                                .try_send(PeerClientEvent::TotpCodeReady { target_pane, code });
                        }
                        Err(err) => {
                            let _ = evt_cl.try_send(PeerClientEvent::TotpError(err));
                        }
                    }
                });
            }
            Ok(PeerClientCommand::CallTool {
                req_id,
                peer_id,
                tool_name,
                arguments,
                reply_tx,
            }) => {
                let res = match find_tracked(&tracked, &peer_id) {
                    Some(entry) => {
                        rpc_seq = rpc_seq.wrapping_add(1);
                        call_remote_tool(
                            &entry.peer.url,
                            &entry.peer.bearer_token,
                            rpc_seq,
                            &tool_name,
                            arguments,
                            cfg.allow_lan,
                        )
                    }
                    None => Err(format!("unknown outbound Haven peer {peer_id:?}")),
                };
                if let Some(tx) = reply_tx {
                    let _ = tx.send(res);
                } else {
                    let _ = evt_tx.try_send(PeerClientEvent::ToolCallCompleted {
                        req_id,
                        peer_id,
                        tool_name,
                        result: res,
                    });
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
        }

        // Poll any due peers.
        let due_ids: Vec<String> = tracked
            .iter()
            .filter_map(|(id, t)| {
                if Instant::now() >= t.next_poll {
                    Some(id.clone())
                } else {
                    None
                }
            })
            .collect();

        for id in due_ids {
            let Some(entry) = tracked.get_mut(&id) else {
                continue;
            };
            rpc_seq = rpc_seq.wrapping_add(1);
            let started = Instant::now();
            match probe_liveness(
                &entry.peer.url,
                &entry.peer.bearer_token,
                rpc_seq,
                cfg.allow_lan,
            ) {
                Ok(version_opt) => {
                    let rtt_ms = started.elapsed().as_millis() as u64;
                    if version_opt.is_some() {
                        entry.remote_version = version_opt;
                    }
                    entry.failures = 0;
                    entry.next_poll = Instant::now() + cfg.connected_interval;
                    let now_secs = unix_now_secs();
                    entry.peer.last_connected_at = Some(now_secs);
                    let status = OutboundLimbStatus::Connected {
                        id: entry.peer.id.clone(),
                        label: entry.peer.label.clone(),
                        remote_version: entry.remote_version.clone(),
                        rtt_ms,
                    };
                    let _ = evt_tx.try_send(PeerClientEvent::StatusChanged {
                        peer_id: Some(entry.peer.id.clone()),
                        status,
                        connected_at: Some(now_secs),
                    });

                    // Also poll Haven's stalled SSH fallback prompt (`get_pending_auth_prompt`).
                    rpc_seq = rpc_seq.wrapping_add(1);
                    if let Ok(prompt_opt) = poll_pending_auth_prompt(
                        &entry.peer.url,
                        &entry.peer.bearer_token,
                        rpc_seq,
                        cfg.allow_lan,
                    ) {
                        let _ = evt_tx.try_send(PeerClientEvent::AuthPromptChanged {
                            peer_id: entry.peer.id.clone(),
                            peer_label: entry.peer.label.clone(),
                            prompt: prompt_opt,
                        });
                    }
                }
                Err(err) => {
                    entry.failures = entry.failures.saturating_add(1);
                    let shift = entry.failures.saturating_sub(1).min(6);
                    let backoff = cfg
                        .offline_initial_backoff
                        .saturating_mul(1u32 << shift)
                        .min(cfg.offline_max_backoff);
                    entry.next_poll = Instant::now() + backoff;
                    let status = OutboundLimbStatus::Offline {
                        id: entry.peer.id.clone(),
                        label: entry.peer.label.clone(),
                        last_error: Some(err),
                    };
                    let _ = evt_tx.try_send(PeerClientEvent::StatusChanged {
                        peer_id: Some(entry.peer.id.clone()),
                        status,
                        connected_at: None,
                    });
                    let _ = evt_tx.try_send(PeerClientEvent::AuthPromptChanged {
                        peer_id: entry.peer.id.clone(),
                        peer_label: entry.peer.label.clone(),
                        prompt: None,
                    });
                }
            }
        }
    }
}

fn run_pair_outbound(
    rpc_id: u64,
    label: &str,
    url: &str,
    explicit_token: Option<&str>,
    enroll_signer: bool,
    allow_lan: bool,
) -> Result<(OutboundHavenPeer, OutboundLimbStatus), String> {
    let clean_label = label.trim();
    if clean_label.is_empty() {
        return Err(String::from("peer label must not be empty"));
    }
    let clean_url = url.trim();
    parse_and_validate_peer_url(clean_url, allow_lan)?;

    let read_timeout = if explicit_token.is_some() {
        NORMAL_READ_TIMEOUT
    } else {
        PAIRING_READ_TIMEOUT
    };
    let params = json!({
        "protocolVersion": "2025-03-26",
        "capabilities": {},
        "clientInfo": {
            "name": "mutiny",
            "version": env!("CARGO_PKG_VERSION"),
        }
    });
    let started = Instant::now();
    let result = jsonrpc_call(
        clean_url,
        explicit_token,
        rpc_id,
        "initialize",
        params,
        read_timeout,
        allow_lan,
    )
    .map_err(|(_, msg)| msg)?;
    let rtt_ms = started.elapsed().as_millis() as u64;

    let token = match explicit_token {
        Some(t) if !t.trim().is_empty() => t.trim().to_owned(),
        _ => result
            .get("_meta")
            .and_then(|m| m.get("sh.haven/pairingToken"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .ok_or_else(|| {
                String::from(
                    "peer completed `initialize` without issuing `_meta[\"sh.haven/pairingToken\"]`; supply `--token` or enable tap-to-pair on Haven",
                )
            })?,
    };

    let remote_version = result
        .get("serverInfo")
        .and_then(|s| s.get("version"))
        .and_then(Value::as_str)
        .map(str::to_owned);

    let (signing_secret_b64, signing_key_id) = if enroll_signer {
        let (sec, kid) =
            fetch_signing_secret(clean_url, &token, rpc_id.wrapping_add(1), allow_lan)?;
        (Some(sec), Some(kid))
    } else {
        (None, None)
    };

    let now_secs = unix_now_secs();
    let id = generate_outbound_id();
    let peer = OutboundHavenPeer {
        id: id.clone(),
        label: clean_label.to_owned(),
        url: clean_url.to_owned(),
        bearer_token: token,
        inbound_notify_scope: InboundNotifyScope::default(),
        paired_at: now_secs,
        last_connected_at: Some(now_secs),
        signing_secret_b64,
        signing_key_id,
    };
    let status = OutboundLimbStatus::Connected {
        id,
        label: clean_label.to_owned(),
        remote_version,
        rtt_ms,
    };
    Ok((peer, status))
}

fn fetch_signing_secret(
    url: &str,
    bearer_token: &str,
    rpc_id: u64,
    allow_lan: bool,
) -> Result<(String, String), String> {
    let raw = jsonrpc_call(
        url,
        Some(bearer_token),
        rpc_id,
        "tools/call",
        json!({
            "name": "get_egress_signing_secret",
            "arguments": {},
        }),
        PAIRING_READ_TIMEOUT,
        allow_lan,
    )
    .map_err(|(_, msg)| msg)?;
    let obj = extract_mcp_tool_json(&raw)?;
    let alg = obj
        .get("algorithm")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if alg != "HMAC-SHA256" {
        return Err(format!(
            "unsupported signing algorithm {alg:?}; expected \"HMAC-SHA256\""
        ));
    }
    let secret_b64 = obj
        .get("secret_base64")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| String::from("Haven response missing `secret_base64`"))?;
    crypto::decode_signing_secret_b64(secret_b64)?;
    let key_id = obj
        .get("key_id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("haven-hmac")
        .to_owned();
    Ok((secret_b64.to_owned(), key_id))
}

fn detect_request_peer_consent(
    url: &str,
    bearer_token: &str,
    rpc_id: u64,
    allow_lan: bool,
) -> bool {
    let Ok(val) = jsonrpc_call(
        url,
        Some(bearer_token),
        rpc_id,
        "tools/list",
        json!({}),
        NORMAL_READ_TIMEOUT,
        allow_lan,
    ) else {
        return false;
    };
    val.get("tools")
        .and_then(Value::as_array)
        .is_some_and(|tools| {
            tools.iter().any(|t| {
                t.get("name")
                    .and_then(Value::as_str)
                    .is_some_and(|n| n == "request_peer_consent")
            })
        })
}

fn poll_pending_auth_prompt(
    url: &str,
    bearer_token: &str,
    rpc_id: u64,
    allow_lan: bool,
) -> Result<Option<HavenAuthPrompt>, ()> {
    let res = jsonrpc_call(
        url,
        Some(bearer_token),
        rpc_id,
        "tools/call",
        json!({
            "name": "get_pending_auth_prompt",
            "arguments": {},
        }),
        NORMAL_READ_TIMEOUT,
        allow_lan,
    );
    let Ok(val) = res else {
        return Err(());
    };
    let Ok(obj) = extract_mcp_tool_json(&val) else {
        return Err(());
    };
    let pending = obj.get("pending").and_then(Value::as_bool).unwrap_or(false);
    if !pending {
        return Ok(None);
    }
    let profile_id = obj
        .get("profileId")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let label = obj
        .get("label")
        .and_then(Value::as_str)
        .unwrap_or(&profile_id)
        .to_owned();
    let requires_passphrase = obj
        .get("requiresPassphrase")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if profile_id.is_empty() {
        return Ok(None);
    }
    Ok(Some(HavenAuthPrompt {
        profile_id,
        label,
        requires_passphrase,
    }))
}

fn fetch_totp_list(
    url: &str,
    bearer_token: &str,
    rpc_id: u64,
    allow_lan: bool,
) -> Result<Vec<TotpSecretItem>, String> {
    let raw = call_remote_tool(
        url,
        bearer_token,
        rpc_id,
        "list_totp_secrets",
        json!({}),
        allow_lan,
    )?;
    let obj = extract_mcp_tool_json(&raw)?;
    let arr = obj
        .get("secrets")
        .and_then(Value::as_array)
        .ok_or_else(|| String::from("list_totp_secrets response missing `secrets` array"))?;
    let mut items = Vec::with_capacity(arr.len());
    for entry in arr {
        let Some(id) = entry
            .get("id")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        else {
            continue;
        };
        let label = entry
            .get("label")
            .and_then(Value::as_str)
            .unwrap_or("Authenticator")
            .to_owned();
        let issuer = entry
            .get("issuer")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_owned);
        let account_name = entry
            .get("accountName")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_owned);
        items.push(TotpSecretItem {
            id: id.to_owned(),
            label,
            issuer,
            account_name,
        });
    }
    Ok(items)
}

fn fetch_fresh_totp_code(
    url: &str,
    bearer_token: &str,
    rpc_id: u64,
    totp_secret_id: &str,
    allow_lan: bool,
) -> Result<String, String> {
    let call_once = |seq: u64| -> Result<(String, u64), String> {
        let raw = jsonrpc_call(
            url,
            Some(bearer_token),
            seq,
            "tools/call",
            json!({
                "name": "generate_totp_code",
                "arguments": { "totpSecretId": totp_secret_id },
            }),
            PAIRING_READ_TIMEOUT,
            allow_lan,
        )
        .map_err(|(_, msg)| msg)?;
        let obj = extract_mcp_tool_json(&raw)?;
        let code = obj
            .get("code")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| String::from("generate_totp_code response missing `code`"))?
            .to_owned();
        let secs = obj
            .get("secondsRemaining")
            .and_then(Value::as_u64)
            .unwrap_or(30);
        Ok((code, secs))
    };

    let (code, secs_rem) = call_once(rpc_id)?;
    if secs_rem <= 2 {
        thread::sleep(DEFAULT_TOTP_ROLLOVER_SLEEP);
        let (fresh_code, _) = call_once(rpc_id.wrapping_add(1))?;
        return Ok(fresh_code);
    }
    Ok(code)
}

fn probe_liveness(
    url: &str,
    bearer_token: &str,
    rpc_id: u64,
    allow_lan: bool,
) -> Result<Option<String>, String> {
    let args = json!({
        "name": "get_app_info",
        "arguments": {}
    });
    match jsonrpc_call(
        url,
        Some(bearer_token),
        rpc_id,
        "tools/call",
        args,
        NORMAL_READ_TIMEOUT,
        allow_lan,
    ) {
        Ok(val) => {
            let is_err = val.get("isError").and_then(Value::as_bool).unwrap_or(false);
            if !is_err {
                let ver = extract_version_from_tool_result(&val);
                return Ok(ver);
            }
        }
        Err((Some(-32601 | -32602), _)) => {}
        Err((_, msg)) => return Err(msg),
    }

    jsonrpc_call(
        url,
        Some(bearer_token),
        rpc_id.wrapping_add(1),
        "ping",
        json!({}),
        NORMAL_READ_TIMEOUT,
        allow_lan,
    )
    .map(|_| None)
    .map_err(|(_, msg)| msg)
}

fn extract_version_from_tool_result(val: &Value) -> Option<String> {
    let parsed = extract_mcp_tool_json(val).ok()?;
    parsed
        .get("versionName")
        .or_else(|| parsed.get("version"))
        .and_then(Value::as_str)
        .map(str::to_owned)
}

fn call_remote_tool(
    url: &str,
    bearer_token: &str,
    rpc_id: u64,
    tool_name: &str,
    arguments: Value,
    allow_lan: bool,
) -> Result<Value, String> {
    jsonrpc_call(
        url,
        Some(bearer_token),
        rpc_id,
        "tools/call",
        json!({
            "name": tool_name,
            "arguments": arguments,
        }),
        NORMAL_READ_TIMEOUT,
        allow_lan,
    )
    .map_err(|(_, msg)| msg)
}

#[allow(dead_code)]
fn _assert_io_sync(_: io::Error) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_validation_allows_loopback_and_tunnels_and_refuses_public_internet() {
        for allowed in [
            "http://127.0.0.1:8740/mcp",
            "http://localhost:8740/mcp",
            "http://[::1]:8740/mcp",
            "http://100.64.0.12:8740/mcp",
            "http://10.8.0.2:8740/mcp",
            "http://172.16.4.9:8740/mcp",
            "http://192.168.1.50:8740/mcp",
            "http://[fd12:3456::1]:8740/mcp",
            "http://[200:abcd:1234::1]:8740/mcp",
            "http://[3ff:ffff::1]:8740/mcp",
        ] {
            assert!(
                parse_and_validate_peer_url(allowed, false).is_ok(),
                "{allowed} should be allowed"
            );
        }

        for refused in [
            "http://8.8.8.8:8740/mcp",
            "http://1.1.1.1:8740/mcp",
            "http://93.184.216.34:8740/mcp",
            "http://100.128.0.1:8740/mcp",
            "http://0.0.0.0:8740/mcp",
            "http://[::]:8740/mcp",
            "http://[2606:4700:4700::1111]:8740/mcp",
            "http://[400::1]:8740/mcp",
            "http://[::ffff:8.8.8.8]:8740/mcp",
            "http://example.com:8740/mcp",
            "https://127.0.0.1:8740/mcp",
            "http://fe80::1:8740/mcp",
        ] {
            assert!(
                parse_and_validate_peer_url(refused, false).is_err(),
                "{refused} must be refused"
            );
        }

        assert!(parse_and_validate_peer_url("http://169.254.1.2:8740/mcp", false).is_err());
        assert!(parse_and_validate_peer_url("http://169.254.1.2:8740/mcp", true).is_ok());
        assert!(parse_and_validate_peer_url("http://8.8.8.8:8740/mcp", true).is_err());
    }
}
