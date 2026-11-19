//! Desktop notifications for OSC 777.
//!
//! The engine parses the sequence and raises attention; this module forwards
//! it to `org.freedesktop.Notifications` on the session bus. That is the
//! interface desktop environments and the notification portal implement;
//! there is no separate notification portal method to call.
//!
//! Sending is best-effort. A missing session bus (a bare container, a TTY
//! login) must never take down a pane, so failures are logged and dropped.
//! The trait exists so tests can assert what would be sent without a bus.

use gtk4::gio;
use gtk4::glib;
use gtk4::prelude::*;

pub trait DesktopNotifier {
    /// Sends a notification. Implementations must not panic on a missing bus.
    fn notify(&self, app_name: &str, title: &str, body: &str);
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
    use std::rc::Rc;

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
}
