//! Durable bilateral peer store at `$XDG_CONFIG_HOME/mutiny/peers.json` (mode
//! `0600`) plus the live in-memory `InboundPeerTable` shared with the HTTP MCP
//! listener.
//!
//! Security rules enforced here:
//! - Inbound peers store ONLY `token_sha256` (lowercase hex SHA-256) at rest,
//!   never the plaintext bearer token.
//! - Every remote peer (CLI `--http-peer` or `peers.json`) resolves strictly to
//!   `CarrierRole::Peer`, never `CarrierRole::Operator`.
//! - `peers.json` is written atomically via `.peers.json.tmp` created with mode
//!   `0600` + `sync_all()` + `rename()`, and `load` refuses files accessible to
//!   group or world (`mode & 0o077 != 0`).

use std::collections::HashMap;
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::crypto;
use crate::ipc::{CarrierRole, ConsentMode};

pub const PEERS_STORE_VERSION: u32 = 1;

fn default_store_version() -> u32 {
    PEERS_STORE_VERSION
}

/// Notification routing scope for inbound notifications arriving from an
/// outbound-paired Haven device (prepared for M3.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InboundNotifyScope {
    #[default]
    HavenOnly,
    All,
    None,
}

/// Persisted record for a remote client allowed to call Mutiny's inbound HTTP
/// MCP carrier. Only `token_sha256` is stored on disk.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InboundPeer {
    pub id: String,
    pub label: String,
    pub client_name: String,
    pub token_sha256: String,
    pub consent_mode: ConsentMode,
    pub paired_at: u64,
    #[serde(default)]
    pub last_seen_at: Option<u64>,
}

/// Persisted record for a paired Haven HTTP MCP endpoint that Mutiny dials
/// outbound.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutboundHavenPeer {
    pub id: String,
    pub label: String,
    pub url: String,
    pub bearer_token: String,
    #[serde(default)]
    pub inbound_notify_scope: InboundNotifyScope,
    pub paired_at: u64,
    #[serde(default)]
    pub last_connected_at: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signing_secret_b64: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signing_key_id: Option<String>,
}

/// Live connection state of an outbound Haven limb, surfaced on `list-peers`
/// and the HeaderBar Limb Status Pill.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "kebab-case")]
pub enum OutboundLimbStatus {
    Connected {
        id: String,
        label: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        remote_version: Option<String>,
        rtt_ms: u64,
    },
    Pairing {
        label: String,
    },
    Offline {
        id: String,
        label: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        last_error: Option<String>,
    },
}

/// Versioned durable peer store at `$XDG_CONFIG_HOME/mutiny/peers.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeerStore {
    #[serde(default = "default_store_version")]
    pub version: u32,
    #[serde(default)]
    pub inbound_peers: Vec<InboundPeer>,
    #[serde(default)]
    pub outbound_peers: Vec<OutboundHavenPeer>,
}

impl Default for PeerStore {
    fn default() -> Self {
        Self {
            version: PEERS_STORE_VERSION,
            inbound_peers: Vec::new(),
            outbound_peers: Vec::new(),
        }
    }
}

/// Redacted view of an inbound peer for IPC/CLI/MCP responses. Never carries
/// `token_sha256` or any token material.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InboundPeerSummary {
    pub id: String,
    pub label: String,
    pub client_name: String,
    pub consent_mode: ConsentMode,
    pub ephemeral: bool,
    pub paired_at: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_seen_at: Option<u64>,
}

/// Redacted view of an outbound Haven peer for IPC/CLI/MCP responses. Never
/// carries `bearer_token` or `signing_secret_b64`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutboundPeerSummary {
    pub id: String,
    pub label: String,
    pub url: String,
    pub inbound_notify_scope: InboundNotifyScope,
    pub paired_at: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_connected_at: Option<u64>,
    pub has_signing_key: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signing_key_id: Option<String>,
    #[serde(default)]
    pub rate_limited_count: u64,
    #[serde(default)]
    pub unread_alert_count: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shade_status: Option<String>,
    pub status: OutboundLimbStatus,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OutboundPeerTelemetry {
    pub rate_limited_count: u64,
    pub unread_alert_count: usize,
    pub shade_status: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PeerAlertKind {
    LimbOffline,
    LimbReconnected,
    AuthPrompt,
    PendingConsent,
    PeerNotice,
    AndroidShade,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeerAlert {
    pub id: u64,
    pub peer_id: String,
    pub peer_label: String,
    pub kind: PeerAlertKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub package_name: Option<String>,
    pub title: String,
    pub body: String,
    pub timestamp_ms: u64,
    pub unread: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeersListing {
    pub inbound: Vec<InboundPeerSummary>,
    pub outbound: Vec<OutboundPeerSummary>,
}

pub fn peers_path() -> PathBuf {
    peers_path_from(
        std::env::var("XDG_CONFIG_HOME").ok().as_deref(),
        std::env::var("HOME").ok().as_deref(),
    )
}

pub fn peers_path_from(xdg_config_home: Option<&str>, home: Option<&str>) -> PathBuf {
    let base = xdg_config_home
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            home.filter(|s| !s.is_empty())
                .map(|h| PathBuf::from(h).join(".config"))
        })
        .unwrap_or_else(std::env::temp_dir);
    base.join("mutiny").join("peers.json")
}

pub fn unix_now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Loads `peers.json`. A missing file returns an empty store. Files with group
/// or world permission bits (`mode & 0o077 != 0`), unknown versions, or an
/// inbound peer attempting to claim `Operator` are rejected with a diagnostic.
pub fn load(path: &Path) -> Result<PeerStore, String> {
    let meta = match std::fs::metadata(path) {
        Ok(m) => m,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(PeerStore::default()),
        Err(err) => return Err(format!("{}: {err}", path.display())),
    };
    let mode = meta.mode();
    if mode & 0o077 != 0 {
        return Err(format!(
            "{}: refusing to load peer credentials from mode {:04o}; expected 0600 (no group or world permissions)",
            path.display(),
            mode & 0o777
        ));
    }
    let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let store: PeerStore =
        serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?;
    if store.version != PEERS_STORE_VERSION {
        return Err(format!(
            "{}: unsupported peers store version {} (expected {PEERS_STORE_VERSION})",
            path.display(),
            store.version
        ));
    }
    for peer in &store.inbound_peers {
        if peer.consent_mode.carrier_role() != CarrierRole::Peer {
            return Err(format!(
                "{}: inbound peer {:?} has forbidden Operator consent mode",
                path.display(),
                peer.label
            ));
        }
        crypto::hex_decode_32(&peer.token_sha256).map_err(|e| {
            format!(
                "{}: inbound peer {:?} has invalid token_sha256: {e}",
                path.display(),
                peer.label
            )
        })?;
    }
    for peer in &store.outbound_peers {
        if peer.bearer_token.trim().is_empty() {
            return Err(format!(
                "{}: outbound peer {:?} has an empty bearer_token",
                path.display(),
                peer.label
            ));
        }
        if let Some(secret_b64) = peer.signing_secret_b64.as_deref() {
            crypto::decode_signing_secret_b64(secret_b64).map_err(|e| {
                format!(
                    "{}: outbound peer {:?} has invalid signing_secret_b64: {e}",
                    path.display(),
                    peer.label
                )
            })?;
        }
    }
    Ok(store)
}

/// Writes `peers.json` atomically via `.peers.json.tmp` created with mode
/// `0600`, `sync_all()`, and `rename()`.
pub fn save(path: &Path, store: &PeerStore) -> io::Result<()> {
    for peer in &store.inbound_peers {
        if peer.consent_mode.carrier_role() != CarrierRole::Peer {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "inbound peer can never store Operator consent mode",
            ));
        }
    }
    if let Some(parent) = path.parent()
        && !parent.exists()
    {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(parent)?;
    }
    let json = serde_json::to_vec_pretty(store)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    let file_name = path
        .file_name()
        .map(|n| format!(".{}.tmp", n.to_string_lossy()))
        .unwrap_or_else(|| String::from(".peers.json.tmp"));
    let tmp = match path.parent() {
        Some(parent) => parent.join(file_name),
        None => PathBuf::from(file_name),
    };

    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)?;
    // Enforce 0600 even if a stale tmp file already existed with wider mode.
    file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    file.write_all(&json)?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    drop(file);
    std::fs::rename(&tmp, path)
}

/// One live inbound peer entry in the HTTP MCP listener's lookup table. Both
/// ephemeral CLI `--http-peer` tokens and durable `peers.json` records compile
/// into this unified SHA-256 entry.
#[derive(Debug, Clone)]
pub struct LiveInboundPeer {
    pub id: String,
    pub label: String,
    pub client_name: String,
    pub token_sha256: [u8; 32],
    pub socket: PathBuf,
    pub consent_mode: ConsentMode,
    pub ephemeral: bool,
    pub paired_at: u64,
    pub last_seen_at: Option<u64>,
}

impl LiveInboundPeer {
    pub fn summary(&self) -> InboundPeerSummary {
        InboundPeerSummary {
            id: self.id.clone(),
            label: self.label.clone(),
            client_name: self.client_name.clone(),
            consent_mode: self.consent_mode,
            ephemeral: self.ephemeral,
            paired_at: self.paired_at,
            last_seen_at: self.last_seen_at,
        }
    }
}

/// Thread-safe inbound peer lookup table shared between `http_mcp` worker
/// threads and `AppController` so tap-to-pair approvals and `revoke-peer` take
/// effect immediately without restarting `--http-bind`.
#[derive(Debug, Default)]
pub struct InboundPeerTable {
    entries: Vec<LiveInboundPeer>,
}

impl InboundPeerTable {
    pub fn new() -> Self {
        Self {
            entries: Vec::new(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn entries(&self) -> &[LiveInboundPeer] {
        &self.entries
    }

    pub fn insert(&mut self, entry: LiveInboundPeer) -> Result<(), String> {
        if entry.consent_mode.carrier_role() != CarrierRole::Peer {
            return Err(String::from(
                "inbound HTTP peer can never hold CarrierRole::Operator",
            ));
        }
        self.entries
            .retain(|e| e.id != entry.id && e.label != entry.label);
        self.entries.push(entry);
        Ok(())
    }

    /// Removes any peer whose `id` or `label` matches `target`, returning the
    /// removed entries so the caller can unlink their Unix relay sockets.
    pub fn revoke(&mut self, target: &str) -> Vec<LiveInboundPeer> {
        let mut removed = Vec::new();
        let mut kept = Vec::with_capacity(self.entries.len());
        for entry in self.entries.drain(..) {
            if entry.id == target || entry.label == target {
                removed.push(entry);
            } else {
                kept.push(entry);
            }
        }
        self.entries = kept;
        removed
    }

    /// Authenticates a presented Bearer token by hashing it once with SHA-256
    /// and comparing against every entry in constant time with no early exit.
    pub fn authenticate(&mut self, presented: &str, now_secs: u64) -> Option<LiveInboundPeer> {
        let digest = crypto::sha256(presented.as_bytes());
        let mut matched_idx: Option<usize> = None;
        for (idx, entry) in self.entries.iter().enumerate() {
            if crypto::constant_time_eq(&digest, &entry.token_sha256) {
                matched_idx = Some(idx);
            }
        }
        let idx = matched_idx?;
        self.entries[idx].last_seen_at = Some(now_secs);
        Some(self.entries[idx].clone())
    }

    pub fn summaries(&self) -> Vec<InboundPeerSummary> {
        self.entries.iter().map(LiveInboundPeer::summary).collect()
    }
}

/// Builds the redacted `PeersListing` combining live inbound peers and durable
/// outbound Haven peers with their latest connection status.
pub fn build_listing(
    inbound: &[InboundPeerSummary],
    outbound_store: &[OutboundHavenPeer],
    outbound_statuses: &HashMap<String, OutboundLimbStatus>,
    telemetry: &HashMap<String, OutboundPeerTelemetry>,
) -> PeersListing {
    let outbound = outbound_store
        .iter()
        .map(|p| {
            let status = outbound_statuses.get(&p.id).cloned().unwrap_or_else(|| {
                OutboundLimbStatus::Offline {
                    id: p.id.clone(),
                    label: p.label.clone(),
                    last_error: None,
                }
            });
            let telem = telemetry.get(&p.id).cloned().unwrap_or_default();
            OutboundPeerSummary {
                id: p.id.clone(),
                label: p.label.clone(),
                url: p.url.clone(),
                inbound_notify_scope: p.inbound_notify_scope,
                paired_at: p.paired_at,
                last_connected_at: p.last_connected_at,
                has_signing_key: p.signing_secret_b64.is_some(),
                signing_key_id: p.signing_key_id.clone(),
                rate_limited_count: telem.rate_limited_count,
                unread_alert_count: telem.unread_alert_count,
                shade_status: telem.shade_status,
                status,
            }
        })
        .collect();
    PeersListing {
        inbound: inbound.to_vec(),
        outbound,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("mutiny-peers-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn atomic_save_writes_0600_never_stores_plaintext_inbound_token_and_rejects_0644() {
        let dir = temp_dir("perms");
        let path = dir.join("mutiny").join("peers.json");
        let secret_token = "mty_super_secret_plaintext_inbound_token_value";
        let hash = crypto::sha256_hex(secret_token.as_bytes());

        let store = PeerStore {
            version: PEERS_STORE_VERSION,
            inbound_peers: vec![InboundPeer {
                id: String::from("in-1"),
                label: String::from("haven-phone"),
                client_name: String::from("haven-agent"),
                token_sha256: hash.clone(),
                consent_mode: ConsentMode::Ask,
                paired_at: 1_700_000_000,
                last_seen_at: None,
            }],
            outbound_peers: vec![OutboundHavenPeer {
                id: String::from("out-1"),
                label: String::from("pixel"),
                url: String::from("http://127.0.0.1:8740/mcp"),
                bearer_token: String::from("hvn_outbound_token"),
                inbound_notify_scope: InboundNotifyScope::default(),
                paired_at: 1_700_000_001,
                last_connected_at: None,
                signing_secret_b64: None,
                signing_key_id: None,
            }],
        };

        save(&path, &store).expect("atomic save");
        let mode = std::fs::metadata(&path).unwrap().mode() & 0o777;
        assert_eq!(mode, 0o600, "peers.json must be mode 0600, got {mode:04o}");

        let on_disk = std::fs::read_to_string(&path).unwrap();
        assert!(
            !on_disk.contains(secret_token),
            "plaintext inbound token must NEVER appear on disk: {on_disk}"
        );
        assert!(on_disk.contains(&hash));
        assert!(on_disk.contains("\"haven_only\""));

        let loaded = load(&path).expect("load 0600");
        assert_eq!(loaded, store);

        // Widening permissions to 0644 must cause load() to refuse the file.
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        let err = load(&path).expect_err("0644 must be rejected");
        assert!(err.contains("0644") && err.contains("0600"), "got: {err}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn peer_store_and_table_refuse_operator_role() {
        let dir = temp_dir("no-op");
        let path = dir.join("peers.json");
        let bad_store = PeerStore {
            version: PEERS_STORE_VERSION,
            inbound_peers: vec![InboundPeer {
                id: String::from("in-bad"),
                label: String::from("evil"),
                client_name: String::from("evil"),
                token_sha256: crypto::sha256_hex(b"tok"),
                consent_mode: ConsentMode::Operator,
                paired_at: 1,
                last_seen_at: None,
            }],
            outbound_peers: Vec::new(),
        };
        assert!(save(&path, &bad_store).is_err());

        // Even a hand-edited 0600 file on disk with `"operator"` is rejected.
        let raw = serde_json::to_string(&bad_store).unwrap();
        std::fs::write(&path, raw).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(load(&path).is_err());

        let mut table = InboundPeerTable::new();
        let res = table.insert(LiveInboundPeer {
            id: String::from("in-bad"),
            label: String::from("evil"),
            client_name: String::from("evil"),
            token_sha256: crypto::sha256(b"tok"),
            socket: PathBuf::from("/tmp/x.sock"),
            consent_mode: ConsentMode::Operator,
            ephemeral: false,
            paired_at: 1,
            last_seen_at: None,
        });
        assert!(res.is_err());

        let _ = std::fs::remove_dir_all(&dir);
    }
}
