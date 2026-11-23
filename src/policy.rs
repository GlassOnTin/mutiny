//! Tier 3 standing policies: a scoped, rate-capped, expiring grant the operator
//! installs once, so a trusted agent stops prompting for every action.
//!
//! Pure data and decisions, no GTK, so the whole policy surface is testable
//! headlessly. The vocabulary mirrors Haven's `create_standing_policy` so a peer
//! moving between the two projects carries one model.
//!
//! A policy never escalates the carrier: it only substitutes for the per-action
//! prompt an `ask` carrier would otherwise raise. An `observe-only` carrier still
//! denies Act, and a policy cannot cover a verb the carrier forbids.

use std::collections::{BTreeMap, VecDeque};

use serde::{Deserialize, Serialize};

use crate::action::ConsentTier;
use crate::ipc::{AuditDecision, ConsentMode, ConsentOutcome, PeerIdentity};

/// Rolling window for the rate ceiling.
pub const RATE_WINDOW_MS: u64 = 60_000;
pub const MAX_POLICIES: usize = 32;
pub const MAX_POLICY_VERBS: usize = 32;
pub const MAX_CALLS_PER_MINUTE: u32 = 600;
pub const MAX_LIFETIME_MINUTES: u64 = 1440;

/// A grant the operator installed. Bound to the kernel-verified peer that
/// proposed it, never to a client label.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StandingPolicy {
    pub id: u64,
    pub peer: PeerIdentity,
    /// Human label shown on the install prompt and the revoke list.
    pub description: String,
    /// Verb slugs in scope. Empty means nothing, never everything: a policy
    /// must name what it covers.
    pub verbs: Vec<String>,
    /// Exact-match pins on the request's scalar fields. Every key given must
    /// equal the call's value.
    #[serde(default)]
    pub arg_constraints: BTreeMap<String, String>,
    pub max_calls_per_minute: u32,
    pub expires_at_ms: u64,
}

impl StandingPolicy {
    /// Trims and bounds a proposal. Refuses an empty scope rather than
    /// silently granting everything.
    pub fn sanitised(mut self) -> Result<Self, String> {
        self.description = self.description.trim().to_owned();
        if self.description.is_empty() {
            return Err(String::from("a policy needs a description"));
        }
        self.verbs.retain(|v| !v.trim().is_empty());
        self.verbs.dedup();
        if self.verbs.is_empty() {
            return Err(String::from(
                "a policy must name at least one verb; an empty scope grants nothing",
            ));
        }
        if self.verbs.len() > MAX_POLICY_VERBS {
            return Err(format!("at most {MAX_POLICY_VERBS} verbs per policy"));
        }
        if self.max_calls_per_minute == 0 {
            return Err(String::from("the rate ceiling must be at least 1"));
        }
        self.max_calls_per_minute = self.max_calls_per_minute.min(MAX_CALLS_PER_MINUTE);
        Ok(self)
    }

    pub fn expired(&self, now_ms: u64) -> bool {
        now_ms >= self.expires_at_ms
    }
}

/// Why a policy did or did not cover a call. The distinctions matter because
/// the fallback differs: out of scope means prompt, expired means prompt, and
/// rate-limited means prompt too, but the operator sees different reasons.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PolicyMatch {
    Covered,
    NotInScope,
    ArgsMismatch,
    Expired,
    RateLimited,
}

/// Whether one policy covers one call. Pure: the caller supplies the clock and
/// the recent call timestamps, so tests need no sleeping.
pub fn policy_match(
    policy: &StandingPolicy,
    verb: &str,
    pins: &BTreeMap<String, String>,
    now_ms: u64,
    recent_calls: &VecDeque<u64>,
) -> PolicyMatch {
    if policy.expired(now_ms) {
        return PolicyMatch::Expired;
    }
    if !policy.verbs.iter().any(|v| v == verb) {
        return PolicyMatch::NotInScope;
    }
    for (key, want) in &policy.arg_constraints {
        match pins.get(key) {
            Some(got) if got == want => {}
            _ => return PolicyMatch::ArgsMismatch,
        }
    }
    let used = recent_calls
        .iter()
        .filter(|t| now_ms.saturating_sub(**t) < RATE_WINDOW_MS)
        .count();
    if used >= policy.max_calls_per_minute as usize {
        return PolicyMatch::RateLimited;
    }
    PolicyMatch::Covered
}

/// One call being tested against the installed policies.
pub struct PolicyCall<'a> {
    pub tier: ConsentTier,
    pub carrier: ConsentMode,
    pub identity: PeerIdentity,
    pub verb: &'a str,
    pub pins: &'a BTreeMap<String, String>,
    pub now_ms: u64,
}

/// The Tier-3 decision, tried before the per-action prompt. Returns `None` when
/// no policy applies, so the caller falls through to the base consent matrix.
///
/// A policy only substitutes for a prompt: it is consulted for an `ask` carrier
/// and an Act verb. It cannot make an `observe-only` carrier allow anything, and
/// it is irrelevant to a carrier that already allows Act.
pub fn policy_decision(
    call: PolicyCall<'_>,
    policies: &[StandingPolicy],
    uses: &BTreeMap<u64, VecDeque<u64>>,
) -> Option<ConsentOutcome> {
    let PolicyCall {
        tier,
        carrier,
        identity,
        verb,
        pins,
        now_ms,
    } = call;
    if tier != ConsentTier::Act || carrier != ConsentMode::Ask {
        return None;
    }
    let empty = VecDeque::new();
    let mut applicable = false;
    for policy in policies {
        if policy.peer != identity {
            continue;
        }
        let recent = uses.get(&policy.id).unwrap_or(&empty);
        match policy_match(policy, verb, pins, now_ms, recent) {
            PolicyMatch::Covered => {
                return Some(ConsentOutcome::Allow(AuditDecision::ApprovedPolicy));
            }
            // In scope but blocked: remember it so an exhausted ceiling is
            // distinguishable from a policy that never applied.
            PolicyMatch::RateLimited | PolicyMatch::ArgsMismatch => applicable = true,
            PolicyMatch::NotInScope | PolicyMatch::Expired => {}
        }
    }
    let _ = applicable;
    None
}

/// Trims a use-history to the rolling window. Bounded so a long-lived policy
/// cannot grow without limit.
pub fn prune_uses(uses: &mut VecDeque<u64>, now_ms: u64) {
    while let Some(front) = uses.front() {
        if now_ms.saturating_sub(*front) >= RATE_WINDOW_MS {
            uses.pop_front();
        } else {
            break;
        }
    }
    while uses.len() > MAX_CALLS_PER_MINUTE as usize {
        uses.pop_front();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn peer(pid: i32) -> PeerIdentity {
        PeerIdentity {
            uid: 1000,
            pid,
            start_time: 5,
            token_peer: None,
        }
    }

    fn policy(verbs: &[&str], ceiling: u32, expires: u64) -> StandingPolicy {
        StandingPolicy {
            id: 1,
            peer: peer(11),
            description: String::from("test grant"),
            verbs: verbs.iter().map(|s| s.to_string()).collect(),
            arg_constraints: BTreeMap::new(),
            max_calls_per_minute: ceiling,
            expires_at_ms: expires,
        }
    }

    fn pins(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn an_empty_scope_is_refused_rather_than_granting_everything() {
        let p = policy(&[], 10, 10_000);
        assert!(
            p.sanitised().is_err(),
            "an empty scope must not be accepted"
        );
        let p = policy(&["send-input"], 0, 10_000);
        assert!(p.sanitised().is_err(), "a zero ceiling is meaningless");
        let mut p = policy(&["send-input"], 10, 10_000);
        p.description = String::from("   ");
        assert!(p.sanitised().is_err());
    }

    #[test]
    fn scope_pins_ceiling_and_expiry_each_block_independently() {
        let p = policy(&["send-input"], 2, 10_000);
        let none = VecDeque::new();
        assert_eq!(
            policy_match(&p, "send-input", &pins(&[]), 1_000, &none),
            PolicyMatch::Covered
        );
        assert_eq!(
            policy_match(&p, "spawn-tab", &pins(&[]), 1_000, &none),
            PolicyMatch::NotInScope
        );
        assert_eq!(
            policy_match(&p, "send-input", &pins(&[]), 10_000, &none),
            PolicyMatch::Expired
        );
        let used: VecDeque<u64> = [900, 950].into_iter().collect();
        assert_eq!(
            policy_match(&p, "send-input", &pins(&[]), 1_000, &used),
            PolicyMatch::RateLimited
        );
        // Old calls fall out of the rolling window. The policy must still be
        // live at that clock, so use a later expiry for this case.
        let long = policy(&["send-input"], 2, 200_000);
        let stale: VecDeque<u64> = [10, 20].into_iter().collect();
        assert_eq!(
            policy_match(&long, "send-input", &pins(&[]), 70_000, &stale),
            PolicyMatch::Covered
        );

        let mut pinned = policy(&["open-session"], 5, 10_000);
        pinned
            .arg_constraints
            .insert(String::from("kind"), String::from("tmux"));
        assert_eq!(
            policy_match(
                &pinned,
                "open-session",
                &pins(&[("kind", "tmux")]),
                1_000,
                &none
            ),
            PolicyMatch::Covered
        );
        assert_eq!(
            policy_match(
                &pinned,
                "open-session",
                &pins(&[("kind", "zellij")]),
                1_000,
                &none
            ),
            PolicyMatch::ArgsMismatch
        );
        assert_eq!(
            policy_match(&pinned, "open-session", &pins(&[]), 1_000, &none),
            PolicyMatch::ArgsMismatch,
            "a pinned key the call does not carry must not match"
        );
    }

    #[test]
    fn a_policy_substitutes_for_a_prompt_and_never_escalates_a_carrier() {
        let p = policy(&["send-input"], 10, 10_000);
        let policies = vec![p.clone()];
        let uses = BTreeMap::new();
        let args = pins(&[]);

        // ask + Act + covered -> allowed as a policy decision.
        assert_eq!(
            policy_decision(
                PolicyCall {
                    tier: ConsentTier::Act,
                    carrier: ConsentMode::Ask,
                    identity: peer(11),
                    verb: "send-input",
                    pins: &args,
                    now_ms: 1_000,
                },
                &policies,
                &uses,
            ),
            Some(ConsentOutcome::Allow(AuditDecision::ApprovedPolicy))
        );
        // Observe is already free; a policy has nothing to add.
        assert_eq!(
            policy_decision(
                PolicyCall {
                    tier: ConsentTier::Observe,
                    carrier: ConsentMode::Ask,
                    identity: peer(11),
                    verb: "send-input",
                    pins: &args,
                    now_ms: 1_000,
                },
                &policies,
                &uses,
            ),
            None
        );
        // observe-only must stay denied: the base matrix decides, not a policy.
        assert_eq!(
            policy_decision(
                PolicyCall {
                    tier: ConsentTier::Act,
                    carrier: ConsentMode::ObserveOnly,
                    identity: peer(11),
                    verb: "send-input",
                    pins: &args,
                    now_ms: 1_000,
                },
                &policies,
                &uses,
            ),
            None
        );
        // A different peer cannot use this policy.
        assert_eq!(
            policy_decision(
                PolicyCall {
                    tier: ConsentTier::Act,
                    carrier: ConsentMode::Ask,
                    identity: peer(99),
                    verb: "send-input",
                    pins: &args,
                    now_ms: 1_000,
                },
                &policies,
                &uses,
            ),
            None
        );
        // Out of scope falls through to the prompt.
        assert_eq!(
            policy_decision(
                PolicyCall {
                    tier: ConsentTier::Act,
                    carrier: ConsentMode::Ask,
                    identity: peer(11),
                    verb: "spawn-tab",
                    pins: &args,
                    now_ms: 1_000,
                },
                &policies,
                &uses,
            ),
            None
        );
    }

    #[test]
    fn an_exhausted_ceiling_falls_back_to_prompting() {
        let p = policy(&["send-input"], 1, 10_000);
        let mut uses = BTreeMap::new();
        uses.insert(1u64, [900u64].into_iter().collect::<VecDeque<u64>>());
        assert_eq!(
            policy_decision(
                PolicyCall {
                    tier: ConsentTier::Act,
                    carrier: ConsentMode::Ask,
                    identity: peer(11),
                    verb: "send-input",
                    pins: &pins(&[]),
                    now_ms: 1_000,
                },
                &[p],
                &uses,
            ),
            None,
            "beyond the ceiling the caller must prompt, not allow"
        );
    }

    #[test]
    fn use_history_is_bounded_and_pruned() {
        let mut uses: VecDeque<u64> = (0..1000).collect();
        prune_uses(&mut uses, 1_000_000);
        assert!(uses.is_empty(), "everything old must be dropped");
        let mut uses: VecDeque<u64> = (0..2000).map(|i| 500_000 + i).collect();
        prune_uses(&mut uses, 500_100);
        assert!(
            uses.len() <= MAX_CALLS_PER_MINUTE as usize,
            "history must stay bounded, got {}",
            uses.len()
        );
    }
}
