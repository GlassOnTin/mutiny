//! Desktop notifications for OSC 777, long-command completions, and
//! focus-aware mirroring to paired Haven limbs (`CompositeNotifier` +
//! per-peer `TokenBucket` rate limiting).
//!
//! The engine parses OSC 777 and OSC 133;D markers without touching GTK; this
//! module forwards local alerts to `org.freedesktop.Notifications` on the
//! session bus and gates outbound Haven `raise_notification` relays by window
//! focus and a 1.0 token/s (burst 5) token bucket matching Haven's
//! `GuestNotifyBridge`.
//!
//! Sending is best-effort. A missing session bus (a bare container, a TTY
//! login) must never take down a pane, so failures are logged and dropped.
//! The trait exists so tests can assert what would be sent without a bus.

use std::rc::Rc;
use std::time::Instant;

use gtk4::gio;
use gtk4::glib;
use gtk4::prelude::*;

use crate::config::PeerRelayMode;

pub const DEFAULT_REFILL_PER_SEC: f64 = 1.0;
pub const DEFAULT_BURST_TOKENS: f64 = 5.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum NotificationPriority {
    Min,
    Low,
    #[default]
    Default,
    High,
    Max,
}

impl NotificationPriority {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Min => "min",
            Self::Low => "low",
            Self::Default => "default",
            Self::High => "high",
            Self::Max => "max",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutboundNotification {
    pub title: String,
    pub body: String,
    pub priority: NotificationPriority,
}

/// Per-peer token-bucket rate limiter (`1.0` token/sec refill, burst `5` by
/// default) so runaway `OSC 777` loops in a shell script cannot flood a paired
/// phone's notification shade.
#[derive(Debug, Clone)]
pub struct TokenBucket {
    refill_per_sec: f64,
    burst: f64,
    tokens: f64,
    last_refill: Instant,
    rate_limited_count: u64,
}

impl TokenBucket {
    pub fn new(refill_per_sec: f64, burst: f64, now: Instant) -> Self {
        let cap = burst.max(1.0);
        Self {
            refill_per_sec: refill_per_sec.max(0.0),
            burst: cap,
            tokens: cap,
            last_refill: now,
            rate_limited_count: 0,
        }
    }

    pub fn haven_default(now: Instant) -> Self {
        Self::new(DEFAULT_REFILL_PER_SEC, DEFAULT_BURST_TOKENS, now)
    }

    pub fn try_acquire(&mut self, now: Instant) -> bool {
        if now > self.last_refill {
            let elapsed = now.duration_since(self.last_refill).as_secs_f64();
            self.tokens = (self.tokens + elapsed * self.refill_per_sec).min(self.burst);
            self.last_refill = now;
        }
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            true
        } else {
            self.rate_limited_count = self.rate_limited_count.saturating_add(1);
            false
        }
    }

    pub fn rate_limited_count(&self) -> u64 {
        self.rate_limited_count
    }
}

pub trait DesktopNotifier {
    /// Sends a notification. Implementations must not panic on a missing bus.
    fn notify(&self, app_name: &str, title: &str, body: &str);
}

/// Focus-aware composite notifier that always delivers to the local
/// `DesktopNotifier` and conditionally mirrors to paired outbound Haven peers.
pub struct CompositeNotifier {
    desktop: Rc<dyn DesktopNotifier>,
}

impl CompositeNotifier {
    pub fn new(desktop: Rc<dyn DesktopNotifier>) -> Self {
        Self { desktop }
    }

    pub fn set_desktop(&mut self, desktop: Rc<dyn DesktopNotifier>) {
        self.desktop = desktop;
    }

    pub const fn should_relay_to_peer(mode: PeerRelayMode, any_window_focused: bool) -> bool {
        match mode {
            PeerRelayMode::Off => false,
            PeerRelayMode::Always => true,
            PeerRelayMode::Unfocused => !any_window_focused,
        }
    }

    pub fn dispatch(
        &self,
        app_name: &str,
        event: &OutboundNotification,
        relay_mode: PeerRelayMode,
        any_window_focused: bool,
        peer_sink: impl FnOnce(&OutboundNotification),
    ) {
        self.desktop.notify(app_name, &event.title, &event.body);
        if Self::should_relay_to_peer(relay_mode, any_window_focused) {
            peer_sink(event);
        }
    }
}

/// Sends via the session bus `org.freedesktop.Notifications.Notify`.
pub struct DBusNotifier;

impl DesktopNotifier for DBusNotifier {
    fn notify(&self, app_name: &str, title: &str, body: &str) {
        if let Err(err) = send(app_name, title, body) {
            eprintln!("warning: desktop notification not sent: {err}");
        }
    }
}

fn send(app_name: &str, title: &str, body: &str) -> Result<u32, String> {
    let connection = gio::bus_get_sync(gio::BusType::Session, gio::Cancellable::NONE)
        .map_err(|e| format!("no session bus: {e}"))?;

    // Notify(s app_name, u replaces_id, s app_icon, s summary, s body,
    //        as actions, a{sv} hints, i expire_timeout) -> u id
    let actions = glib::Variant::array_from_iter::<String>([]);
    let hints = glib::VariantDict::new(None).end();
    let params = glib::Variant::tuple_from_iter([
        app_name.to_variant(),
        0u32.to_variant(),
        String::new().to_variant(),
        title.to_variant(),
        body.to_variant(),
        actions,
        hints,
        (-1i32).to_variant(),
    ]);

    let reply = connection
        .call_sync(
            Some("org.freedesktop.Notifications"),
            "/org/freedesktop/Notifications",
            "org.freedesktop.Notifications",
            "Notify",
            Some(&params),
            Some(glib::VariantTy::new("(u)").expect("valid reply type")),
            gio::DBusCallFlags::NONE,
            2_000,
            gio::Cancellable::NONE,
        )
        .map_err(|e| format!("Notify call failed: {e}"))?;

    Ok(reply.child_value(0).get::<u32>().unwrap_or(0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::time::Duration;

    /// Records what would be sent, so tests can assert forwarding without a
    /// session bus.
    #[derive(Default)]
    pub struct RecordingNotifier {
        pub sent: RefCell<Vec<(String, String, String)>>,
    }

    impl DesktopNotifier for RecordingNotifier {
        fn notify(&self, app_name: &str, title: &str, body: &str) {
            self.sent
                .borrow_mut()
                .push((app_name.to_owned(), title.to_owned(), body.to_owned()));
        }
    }

    #[test]
    fn recording_notifier_captures_calls() {
        let n = Rc::new(RecordingNotifier::default());
        n.notify("mutiny", "Build done", "exit 0");
        assert_eq!(
            n.sent.borrow().as_slice(),
            &[("mutiny".into(), "Build done".into(), "exit 0".into())]
        );
    }

    #[test]
    fn dbus_send_without_a_bus_is_an_error_not_a_panic() {
        // In a bare container there is no session bus. The call must return
        // an error rather than panic, so a pane survives a missing desktop.
        let result = std::panic::catch_unwind(|| send("mutiny", "t", "b"));
        assert!(result.is_ok(), "send must not panic without a bus");
    }

    #[test]
    fn token_bucket_enforces_burst_five_and_one_per_sec_refill() {
        let t0 = Instant::now();
        let mut bucket = TokenBucket::haven_default(t0);
        for _ in 0..5 {
            assert!(bucket.try_acquire(t0));
        }
        assert!(!bucket.try_acquire(t0));
        assert!(!bucket.try_acquire(t0 + Duration::from_millis(500)));
        assert_eq!(bucket.rate_limited_count(), 2);

        // After 1.05 s total, 1 token has refilled.
        assert!(bucket.try_acquire(t0 + Duration::from_millis(1050)));
        assert!(!bucket.try_acquire(t0 + Duration::from_millis(1050)));
        assert_eq!(bucket.rate_limited_count(), 3);
    }

    #[test]
    fn composite_notifier_respects_focus_and_relay_modes() {
        let rec = Rc::new(RecordingNotifier::default());
        let comp = CompositeNotifier::new(rec.clone());
        let ev = OutboundNotification {
            title: String::from("Deploy"),
            body: String::from("ok"),
            priority: NotificationPriority::Default,
        };

        let mut peer_hits = 0usize;
        comp.dispatch("mutiny", &ev, PeerRelayMode::Unfocused, true, |_| {
            peer_hits += 1;
        });
        assert_eq!(
            peer_hits, 0,
            "focused window must not relay in unfocused mode"
        );

        comp.dispatch("mutiny", &ev, PeerRelayMode::Unfocused, false, |_| {
            peer_hits += 1;
        });
        assert_eq!(
            peer_hits, 1,
            "unfocused window must relay in unfocused mode"
        );

        comp.dispatch("mutiny", &ev, PeerRelayMode::Always, true, |_| {
            peer_hits += 1;
        });
        assert_eq!(peer_hits, 2, "always mode relays even when focused");

        comp.dispatch("mutiny", &ev, PeerRelayMode::Off, false, |_| {
            peer_hits += 1;
        });
        assert_eq!(peer_hits, 2, "off mode never relays to peer");
        assert_eq!(rec.sent.borrow().len(), 4, "local desktop receives all 4");
    }
}
