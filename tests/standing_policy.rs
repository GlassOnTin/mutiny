//! Tier 3 standing policies, end to end over real carriers.
//!
//! The agent carrier is pinned to `ask` and there is no GUI, so `ask` fails
//! closed. That gives a clean denied baseline: any allow must come from an
//! installed policy, not from a prompt someone answered.

use std::path::PathBuf;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use mutiny::app::AppController;
use mutiny::config::Overrides;
use mutiny::ipc::{ConsentMode, IpcRequest, IpcResponse, send_request};

struct Server {
    operator: PathBuf,
    agent: PathBuf,
    stop: mpsc::Sender<()>,
    handle: Option<thread::JoinHandle<()>>,
    _dir: PathBuf,
}

impl Server {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("mutiny-policy-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let operator = dir.join("mutiny.sock");
        let agent = dir.join("mutiny-agent.sock");
        let (stop_tx, stop_rx) = mpsc::channel();
        let op = operator.clone();
        let ag = agent.clone();
        let audit = dir.join("audit.jsonl");
        let cfg = dir.join("config.toml");
        let handle = thread::spawn(move || {
            let ctrl = AppController::with_config(
                Some((String::from("/bin/sh"), Vec::new())),
                audit,
                cfg,
                Overrides::default(),
            );
            ctrl.start_ipc_listener(&op).expect("operator");
            ctrl.start_agent_listener(&ag, ConsentMode::Ask)
                .expect("agent");
            ctrl.create_window(None).expect("pane");
            while stop_rx.try_recv().is_err() {
                ctrl.poll_once();
                thread::sleep(Duration::from_millis(4));
            }
            ctrl.shutdown_ipc();
        });
        thread::sleep(Duration::from_millis(300));
        Self {
            operator,
            agent,
            stop: stop_tx,
            handle: Some(handle),
            _dir: dir,
        }
    }

    fn call(&self, sock: &std::path::Path, req: &IpcRequest) -> IpcResponse {
        send_request(sock, req, Duration::from_secs(10)).expect("reply")
    }

    fn agent_act(&self) -> bool {
        matches!(
            self.call(
                &self.agent,
                &IpcRequest::SpawnTab {
                    window: None,
                    cwd: None
                }
            ),
            IpcResponse::Ok { .. }
        )
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.stop.send(());
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
        let _ = std::fs::remove_dir_all(&self._dir);
    }
}

#[test]
fn a_policy_replaces_the_prompt_within_its_scope_ceiling_and_lifetime() {
    let s = Server::new("flow");

    // Baseline: ask with no GUI fails closed.
    assert!(!s.agent_act(), "an ask carrier with no GUI must deny Act");

    // The agent proposes. This grants nothing.
    let proposal = IpcRequest::ProposePolicy {
        description: String::from("let the agent open tabs"),
        verbs: vec![String::from("spawn-tab")],
        arg_constraints: Default::default(),
        max_calls_per_minute: 2,
        expires_in_minutes: 5,
    };
    let id = match s.call(&s.agent, &proposal) {
        IpcResponse::Ok {
            policy_id: Some(id),
            ..
        } => id,
        other => panic!("proposal must return an id: {other:?}"),
    };
    assert!(!s.agent_act(), "proposing must not grant anything");

    // Only installed, not pending, is listed as in force.
    match s.call(&s.operator, &IpcRequest::ListPolicies) {
        IpcResponse::Ok {
            policies: Some(installed),
            pending_policies: Some(pending),
            ..
        } => {
            assert!(installed.is_empty(), "nothing installed yet");
            assert_eq!(pending.len(), 1, "the proposal is pending");
        }
        other => panic!("list: {other:?}"),
    }

    // The operator installs it.
    assert!(matches!(
        s.call(&s.operator, &IpcRequest::InstallPolicy { id }),
        IpcResponse::Ok { .. }
    ));

    // Now the covered verb is allowed, up to the ceiling of 2.
    assert!(s.agent_act(), "an installed policy must cover its verb");
    assert!(s.agent_act(), "second call is still within the ceiling");
    assert!(
        !s.agent_act(),
        "beyond the ceiling it must fall back to prompting, which fails closed here"
    );

    // A verb outside the scope is never covered.
    assert!(
        !matches!(
            s.call(&s.agent, &IpcRequest::SpawnWindow { cwd: None }),
            IpcResponse::Ok { .. }
        ),
        "a policy must not cover verbs it does not name"
    );

    // The audit records the policy decision, not a prompt approval.
    let entries = match s.call(&s.operator, &IpcRequest::AuditLog { limit: 100 }) {
        IpcResponse::Ok { audit: Some(a), .. } => a,
        other => panic!("audit: {other:?}"),
    };
    let covered: Vec<_> = entries
        .iter()
        .filter(|e| e.verb == "spawn-tab" && format!("{:?}", e.decision) == "ApprovedPolicy")
        .collect();
    assert_eq!(
        covered.len(),
        2,
        "exactly the two covered calls: {entries:?}"
    );

    // Revoking takes effect immediately.
    assert!(matches!(
        s.call(&s.operator, &IpcRequest::RevokePolicy { id }),
        IpcResponse::Ok { .. }
    ));
    assert!(!s.agent_act(), "a revoked policy must stop covering calls");
}

#[test]
fn an_agent_cannot_install_its_own_proposal() {
    let s = Server::new("self-install");
    let id = match s.call(
        &s.agent,
        &IpcRequest::ProposePolicy {
            description: String::from("self serving"),
            verbs: vec![String::from("spawn-tab")],
            arg_constraints: Default::default(),
            max_calls_per_minute: 10,
            expires_in_minutes: 5,
        },
    ) {
        IpcResponse::Ok {
            policy_id: Some(id),
            ..
        } => id,
        other => panic!("proposal: {other:?}"),
    };
    // install-policy is Act, so on an ask carrier with no GUI it fails closed.
    assert!(
        !matches!(
            s.call(&s.agent, &IpcRequest::InstallPolicy { id }),
            IpcResponse::Ok { .. }
        ),
        "an agent must not be able to install its own proposal"
    );
    assert!(!s.agent_act(), "and the policy must still not be in force");
}

#[test]
fn a_pinned_policy_only_covers_matching_arguments() {
    let s = Server::new("pins");
    let mut pins = std::collections::BTreeMap::new();
    pins.insert(String::from("kind"), String::from("none"));
    let id = match s.call(
        &s.agent,
        &IpcRequest::ProposePolicy {
            description: String::from("plain shells only"),
            verbs: vec![String::from("new-session")],
            arg_constraints: pins,
            max_calls_per_minute: 10,
            expires_in_minutes: 5,
        },
    ) {
        IpcResponse::Ok {
            policy_id: Some(id),
            ..
        } => id,
        other => panic!("proposal: {other:?}"),
    };
    s.call(&s.operator, &IpcRequest::InstallPolicy { id });

    // kind=none matches the pin.
    assert!(
        matches!(
            s.call(
                &s.agent,
                &IpcRequest::NewSession {
                    kind: None,
                    name: None,
                    mode: mutiny::mux::SessionMode::Tab,
                    window: None,
                }
            ),
            IpcResponse::Ok { .. }
        ),
        "the pinned value must be covered"
    );
    // kind=tmux does not.
    assert!(
        !matches!(
            s.call(
                &s.agent,
                &IpcRequest::NewSession {
                    kind: Some(mutiny::mux::MuxKind::Tmux),
                    name: Some(String::from("pin-probe")),
                    mode: mutiny::mux::SessionMode::Tab,
                    window: None,
                }
            ),
            IpcResponse::Ok { .. }
        ),
        "a different value must not be covered by the pin"
    );
}
