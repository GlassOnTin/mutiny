use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::{Rc, Weak};

use gtk4::gdk;
use gtk4::gio;
use gtk4::glib;
use gtk4::prelude::*;

use crate::action::{
    ACTIONS, ActionId, build_window_menubar_model, filter_palette, sync_close_menu_sections,
};
use crate::app::{AppController, ConsentResolution};
use crate::engine::AttentionReason;
use crate::mux::{HostState, MuxInventory, MuxKind, SessionMode};
use crate::topology::{PaneId, SplitAxis, SplitNode, TabId, WindowId};
use crate::ui::terminal_view::TerminalView;

#[derive(Clone)]
pub struct MutinyWindow {
    inner: Rc<MutinyWindowInner>,
}

struct MutinyWindowInner {
    window_id: WindowId,
    gtk_window: gtk4::ApplicationWindow,
    menubar: gtk4::PopoverMenuBar,
    session_close_menu: gio::Menu,
    pane_close_menu: gio::Menu,
    last_close_menu_state: Cell<Option<(bool, bool, bool)>>,
    /// A FlowBox, not a horizontal Box: a Box's minimum width is the sum of
    /// its children, so enough tabs stretched the whole window. A FlowBox's
    /// minimum is its widest child, so it wraps within the window instead.
    tab_buttons_box: gtk4::FlowBox,
    /// Caps the wrapped strip's height so many tabs scroll rather than
    /// squeezing the terminal. Horizontal scrolling is off: we wrap instead.
    tab_scroller: gtk4::ScrolledWindow,
    toolbar_box: gtk4::Box,
    /// One tab context menu per window, parented to the WINDOW and reused.
    /// Parenting it to a tab button and unparenting on `closed` broke every
    /// item: GTK emits `closed` before activating the action, so the popover
    /// lost its widget path and the action could not resolve. The terminal
    /// view's menu has always used this parent-once pattern.
    tab_menu: gtk4::PopoverMenu,
    limb_pill_btn: gtk4::MenuButton,
    agent_badge_btn: gtk4::Button,
    pairing_bar: gtk4::Box,
    pairing_label: gtk4::Label,
    pairing_mode_dropdown: gtk4::DropDown,
    active_pairing_id: Cell<Option<u64>>,
    consent_bar: gtk4::Box,
    consent_label: gtk4::Label,
    send_haven_btn: gtk4::Button,
    allow_sess_btn: gtk4::Button,
    active_consent_id: Cell<Option<u64>>,
    haven_auth_bar: gtk4::Box,
    haven_auth_label: gtk4::Label,
    haven_auth_entry: gtk4::PasswordEntry,
    haven_auth_remember: gtk4::CheckButton,
    haven_auth_unlock_btn: gtk4::Button,
    haven_auth_dismiss_btn: gtk4::Button,
    active_haven_auth_target: RefCell<Option<(String, String)>>,
    notice_bar: gtk4::Box,
    notice_label: gtk4::Label,
    search_bar: gtk4::Box,
    search_entry: gtk4::SearchEntry,
    search_status: gtk4::Label,
    content_slot: gtk4::Box,
    pane_views: RefCell<HashMap<PaneId, TerminalView>>,
    /// Button plus its two labels, so a refresh updates the labels rather than
    /// calling `set_label`, which would replace the two-line child.
    tab_buttons: RefCell<Vec<(TabId, gtk4::Button, gtk4::Label, gtk4::Label)>>,
    controller: Weak<AppController>,
    closing_programmatically: Cell<bool>,
    session_picker: RefCell<Option<Rc<SessionPicker>>>,
}

impl MutinyWindow {
    pub fn new(
        app: &gtk4::Application,
        window_id: WindowId,
        controller: Rc<AppController>,
    ) -> Self {
        // Leave titlebar unset so Wayland xdg-decoration uses KWin native Breeze SSD on KDE.
        let (win_w, win_h) = {
            let cfg = controller.config_ref();
            (cfg.window_width, cfg.window_height)
        };
        let gtk_window = gtk4::ApplicationWindow::builder()
            .application(app)
            .title("Mutiny")
            // Set per window as well as via the global default: on X11 this is
            // what the taskbar reads, and it keeps the intent explicit.
            .icon_name(crate::desktop::APP_ID)
            .default_width(win_w)
            .default_height(win_h)
            .show_menubar(false)
            .handle_menubar_accel(false)
            .build();

        let (menu_model, sessions_menu, session_close_menu, pane_close_menu) =
            build_window_menubar_model();
        let menubar = gtk4::PopoverMenuBar::from_model(Some(&menu_model));
        controller.register_window_sessions_menu(window_id, sessions_menu);

        let chrome_bar = gtk4::Box::new(gtk4::Orientation::Horizontal, 4);
        chrome_bar.set_margin_start(6);
        chrome_bar.set_margin_end(6);
        chrome_bar.set_margin_top(3);
        chrome_bar.set_margin_bottom(3);

        // Wrapping tab strip. Selection is ours (the buttons carry the active
        // class), so FlowBox's own selection is off. Wrapping is driven by the
        // available width, not a per-row count, hence the high cap.
        let tab_buttons_box = gtk4::FlowBox::new();
        tab_buttons_box.set_selection_mode(gtk4::SelectionMode::None);
        tab_buttons_box.set_homogeneous(false);
        tab_buttons_box.set_min_children_per_line(1);
        tab_buttons_box.set_max_children_per_line(64);
        tab_buttons_box.set_row_spacing(4);
        tab_buttons_box.set_column_spacing(4);
        tab_buttons_box.set_hexpand(true);
        tab_buttons_box.set_valign(gtk4::Align::Start);
        let tab_scroller = gtk4::ScrolledWindow::builder()
            .hscrollbar_policy(gtk4::PolicyType::Never)
            .vscrollbar_policy(gtk4::PolicyType::Automatic)
            .propagate_natural_width(false)
            .propagate_natural_height(true)
            .hexpand(true)
            .child(&tab_buttons_box)
            .build();
        chrome_bar.append(&tab_scroller);

        let limb_popover = gtk4::Popover::new();
        let limb_popover_box = gtk4::Box::new(gtk4::Orientation::Vertical, 6);
        limb_popover_box.set_margin_top(8);
        limb_popover_box.set_margin_bottom(8);
        limb_popover_box.set_margin_start(10);
        limb_popover_box.set_margin_end(10);
        limb_popover.set_child(Some(&limb_popover_box));

        let limb_pill_btn = gtk4::MenuButton::new();
        limb_pill_btn.set_label("● haven");
        limb_pill_btn.set_popover(Some(&limb_popover));
        limb_pill_btn.set_can_focus(false);
        limb_pill_btn.set_visible(false);
        chrome_bar.append(&limb_pill_btn);

        let agent_badge_btn = make_action_button(
            "Agent: idle",
            "win.agent-audit",
            "Agent Activity & Audit Log (Ctrl+Shift+J)",
        );
        agent_badge_btn.set_visible(false);
        chrome_bar.append(&agent_badge_btn);

        // Persistent SSD-safe controls: never hidden when CSD is disabled.
        // Glyph-only with tooltips: the words cost more horizontal space than
        // the tab strip could afford once sessions wrap. Meaning lives in the
        // tooltip and in the Session/Pane menus, which name each verb in full.
        let tab_menu = gtk4::PopoverMenu::from_model(None::<&gtk4::gio::Menu>);
        tab_menu.set_parent(&gtk_window);
        tab_menu.set_has_arrow(false);
        let toolbar_box = gtk4::Box::new(gtk4::Orientation::Horizontal, 2);
        for (glyph, action, tip) in [
            ("+", "win.new-tab", "New Tab (Ctrl+Shift+T)"),
            ("→", "win.split-right", "Split Right (Ctrl+Shift+R)"),
            ("↓", "win.split-down", "Split Down (Ctrl+Shift+E)"),
            (
                "↗",
                "win.detach-split",
                "Detach Split to Window (Ctrl+Shift+D)",
            ),
            ("⌘", "win.command-palette", "Command Palette (Ctrl+Shift+P)"),
            (
                "?",
                "win.keybind-cheatsheet",
                "Keybind Cheatsheet (Ctrl+Shift+?)",
            ),
        ] {
            ensure_compact_toolbar_css();
            let btn = make_action_button(glyph, action, tip);
            btn.add_css_class("compact-toolbar");
            toolbar_box.append(&btn);
        }
        chrome_bar.append(&toolbar_box);
        {
            let ctrl_chrome_drop = Rc::downgrade(&controller);
            let win_for_chrome_drop = window_id;
            attach_tab_drop_target(&chrome_bar, move |dragged_tab| {
                let Some(c) = ctrl_chrome_drop.upgrade() else {
                    return false;
                };
                c.move_tab_to_window(dragged_tab, win_for_chrome_drop, None)
            });
        }
        {
            // Allow `Ctrl+Scroll` directly over the session tab strip too, while
            // leaving plain vertical wheel events alone so wrapped tabs scroll.
            let tab_zoom_scroll =
                gtk4::EventControllerScroll::new(gtk4::EventControllerScrollFlags::VERTICAL);
            tab_zoom_scroll.set_propagation_phase(gtk4::PropagationPhase::Capture);
            let ctrl_zoom = Rc::downgrade(&controller);
            let win_for_zoom = window_id;
            let acc = Rc::new(Cell::new(0.0f64));
            tab_zoom_scroll.connect_scroll(move |sc, _dx, dy| {
                let ctrl_held = sc
                    .current_event()
                    .is_some_and(|e| e.modifier_state().contains(gdk::ModifierType::CONTROL_MASK));
                if !ctrl_held {
                    acc.set(0.0);
                    return glib::Propagation::Proceed;
                }
                if let Some(c) = ctrl_zoom.upgrade() {
                    let raw = -dy;
                    if raw != 0.0 {
                        let prev = acc.get();
                        let base = if (prev > 0.0 && raw < 0.0) || (prev < 0.0 && raw > 0.0) {
                            0.0
                        } else {
                            prev
                        };
                        let total = base + raw;
                        let steps = total.trunc() as i32;
                        acc.set(total - f64::from(steps));
                        if steps != 0 {
                            c.zoom_window(win_for_zoom, steps);
                        }
                    }
                }
                glib::Propagation::Stop
            });
            chrome_bar.add_controller(tab_zoom_scroll);
        }

        // Symmetric inbound `initialize` tap-to-pair bar
        let pairing_bar = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
        pairing_bar.set_margin_start(8);
        pairing_bar.set_margin_end(8);
        pairing_bar.set_margin_top(4);
        pairing_bar.set_margin_bottom(4);
        pairing_bar.set_visible(false);

        let pairing_label = gtk4::Label::new(None);
        pairing_label.set_hexpand(true);
        pairing_label.set_xalign(0.0);
        pairing_bar.append(&pairing_label);

        let pairing_mode_dropdown = gtk4::DropDown::from_strings(&[
            "Prompt / Standing Policy",
            "Session Trusted (Paired)",
            "Observe Only",
        ]);
        pairing_mode_dropdown.set_selected(0);
        pairing_mode_dropdown.set_can_focus(false);
        pairing_bar.append(&pairing_mode_dropdown);

        let pair_approve_btn = gtk4::Button::with_label("Pair");
        pair_approve_btn.add_css_class("suggested-action");
        pair_approve_btn.set_can_focus(false);
        let pair_deny_btn = gtk4::Button::with_label("Deny");
        pair_deny_btn.set_can_focus(false);
        pairing_bar.append(&pair_approve_btn);
        pairing_bar.append(&pair_deny_btn);

        // Non-injectable Consent Bar for Tier-2 agent actions in `ask` mode
        let consent_bar = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
        consent_bar.set_margin_start(8);
        consent_bar.set_margin_end(8);
        consent_bar.set_margin_top(4);
        consent_bar.set_margin_bottom(4);
        consent_bar.set_visible(false);

        let consent_label = gtk4::Label::new(None);
        consent_label.set_hexpand(true);
        consent_label.set_xalign(0.0);
        consent_bar.append(&consent_label);

        let send_haven_btn = gtk4::Button::with_label("Send to Haven");
        send_haven_btn.set_can_focus(false);
        send_haven_btn.set_visible(false);
        let allow_once_btn = gtk4::Button::with_label("Allow Once");
        allow_once_btn.set_can_focus(false);
        let allow_sess_btn = gtk4::Button::with_label("Allow Session");
        allow_sess_btn.set_can_focus(false);
        let deny_btn = gtk4::Button::with_label("Deny");
        deny_btn.set_can_focus(false);
        consent_bar.append(&send_haven_btn);
        consent_bar.append(&allow_once_btn);
        consent_bar.append(&allow_sess_btn);
        consent_bar.append(&deny_btn);

        // Inline Haven stalled SSH authentication bar (physical GUI interaction only)
        let haven_auth_bar = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
        haven_auth_bar.set_margin_start(8);
        haven_auth_bar.set_margin_end(8);
        haven_auth_bar.set_margin_top(4);
        haven_auth_bar.set_margin_bottom(4);
        haven_auth_bar.set_visible(false);

        let haven_auth_label = gtk4::Label::new(None);
        haven_auth_label.set_hexpand(true);
        haven_auth_label.set_xalign(0.0);
        haven_auth_label.set_ellipsize(gtk4::pango::EllipsizeMode::End);
        haven_auth_bar.append(&haven_auth_label);

        let haven_auth_entry = gtk4::PasswordEntry::new();
        haven_auth_entry.set_show_peek_icon(true);
        haven_auth_entry.set_width_chars(20);
        haven_auth_bar.append(&haven_auth_entry);

        let haven_auth_remember = gtk4::CheckButton::with_label("Remember on Haven");
        haven_auth_remember.set_active(false);
        haven_auth_remember.set_can_focus(false);
        haven_auth_bar.append(&haven_auth_remember);

        let haven_auth_unlock_btn = gtk4::Button::with_label("Unlock");
        haven_auth_unlock_btn.add_css_class("suggested-action");
        haven_auth_unlock_btn.set_can_focus(false);
        let haven_auth_dismiss_btn = gtk4::Button::with_label("Dismiss");
        haven_auth_dismiss_btn.set_can_focus(false);
        haven_auth_bar.append(&haven_auth_unlock_btn);
        haven_auth_bar.append(&haven_auth_dismiss_btn);

        // Bidirectional agent -> operator notice banner
        let notice_bar = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
        notice_bar.set_margin_start(8);
        notice_bar.set_margin_end(8);
        notice_bar.set_margin_top(4);
        notice_bar.set_margin_bottom(4);
        notice_bar.set_visible(false);

        let notice_label = gtk4::Label::new(None);
        notice_label.set_hexpand(true);
        notice_label.set_xalign(0.0);
        let ack_btn = gtk4::Button::with_label("Acknowledge");
        ack_btn.set_can_focus(false);
        notice_bar.append(&notice_label);
        notice_bar.append(&ack_btn);

        // Viewport search over the chunked scan. The query lives here; the
        // scan state lives in the PaneEngine, so it survives a reparent.
        let search_bar = gtk4::Box::new(gtk4::Orientation::Horizontal, 6);
        search_bar.set_margin_start(8);
        search_bar.set_margin_end(8);
        search_bar.set_margin_top(4);
        search_bar.set_margin_bottom(4);
        search_bar.set_visible(false);

        let search_entry = gtk4::SearchEntry::new();
        search_entry.set_hexpand(true);
        search_entry.set_placeholder_text(Some("Search scrollback…"));
        search_bar.append(&search_entry);

        let search_status = gtk4::Label::new(None);
        search_status.set_xalign(0.0);
        search_bar.append(&search_status);

        let search_prev = gtk4::Button::with_label("↑");
        search_prev.set_tooltip_text(Some("Previous match (Shift+Enter)"));
        search_prev.set_can_focus(false);
        let search_next = gtk4::Button::with_label("↓");
        search_next.set_tooltip_text(Some("Next match (Enter)"));
        search_next.set_can_focus(false);
        let search_close = gtk4::Button::with_label("✕");
        search_close.set_tooltip_text(Some("Close search (Escape)"));
        search_close.set_can_focus(false);
        search_bar.append(&search_prev);
        search_bar.append(&search_next);
        search_bar.append(&search_close);

        let content_slot = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
        content_slot.set_hexpand(true);
        content_slot.set_vexpand(true);

        // Drag-to-redock / drag-to-detach: dropping a tab from ANOTHER Mutiny
        // window onto this terminal area redocks that tab into this window;
        // dropping a tab from THIS window onto its own terminal area detaches
        // its active pane into a fresh window.
        {
            let ctrl_detach = Rc::downgrade(&controller);
            let win_for_detach = window_id;
            attach_tab_drop_target(&content_slot, move |dragged_tab| {
                let Some(c) = ctrl_detach.upgrade() else {
                    return false;
                };
                let (src_win, active_pane) = {
                    let ws = c.workspace.borrow();
                    let found = ws.windows().iter().find_map(|w| {
                        w.tabs
                            .iter()
                            .find(|t| t.id == dragged_tab)
                            .map(|t| (w.id, t.active_pane))
                    });
                    match found {
                        Some((w, p)) => (Some(w), Some(p)),
                        None => (None, None),
                    }
                };
                match src_win {
                    Some(w) if w != win_for_detach => {
                        c.move_tab_to_window(dragged_tab, win_for_detach, None)
                    }
                    Some(_) => {
                        if c.detach_tab(dragged_tab).is_ok() {
                            true
                        } else {
                            match active_pane {
                                Some(p) => c.detach_split(Some(p)).is_ok(),
                                None => false,
                            }
                        }
                    }
                    None => false,
                }
            });
        }

        let root_vbox = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
        root_vbox.append(&menubar);
        root_vbox.append(&chrome_bar);
        root_vbox.append(&pairing_bar);
        root_vbox.append(&consent_bar);
        root_vbox.append(&haven_auth_bar);
        root_vbox.append(&notice_bar);
        root_vbox.append(&search_bar);
        root_vbox.append(&gtk4::Separator::new(gtk4::Orientation::Horizontal));
        root_vbox.append(&content_slot);
        gtk_window.set_child(Some(&root_vbox));

        let this = Self {
            inner: Rc::new(MutinyWindowInner {
                window_id,
                gtk_window: gtk_window.clone(),
                menubar,
                session_close_menu,
                pane_close_menu,
                last_close_menu_state: Cell::new(Some((false, false, false))),
                tab_buttons_box,
                tab_scroller,
                toolbar_box,
                tab_menu,
                limb_pill_btn,
                agent_badge_btn,
                pairing_bar,
                pairing_label,
                pairing_mode_dropdown,
                active_pairing_id: Cell::new(None),
                consent_bar,
                consent_label,
                send_haven_btn: send_haven_btn.clone(),
                allow_sess_btn: allow_sess_btn.clone(),
                active_consent_id: Cell::new(None),
                haven_auth_bar,
                haven_auth_label,
                haven_auth_entry: haven_auth_entry.clone(),
                haven_auth_remember: haven_auth_remember.clone(),
                haven_auth_unlock_btn: haven_auth_unlock_btn.clone(),
                haven_auth_dismiss_btn: haven_auth_dismiss_btn.clone(),
                active_haven_auth_target: RefCell::new(None),
                notice_bar,
                notice_label,
                search_bar,
                search_entry,
                search_status,
                content_slot,
                pane_views: RefCell::new(HashMap::new()),
                tab_buttons: RefCell::new(Vec::new()),
                controller: Rc::downgrade(&controller),
                closing_programmatically: Cell::new(false),
                session_picker: RefCell::new(None),
            }),
        };

        this.wire_banner_buttons(
            &send_haven_btn,
            &allow_once_btn,
            &allow_sess_btn,
            &deny_btn,
            &ack_btn,
        );
        this.wire_haven_auth_bar();
        this.wire_pairing_and_limb_ui(
            &pair_approve_btn,
            &pair_deny_btn,
            &limb_popover,
            &limb_popover_box,
        );
        this.wire_search_bar(&search_prev, &search_next, &search_close);
        this.register_window_actions(&controller);

        let ctrl_weak = Rc::downgrade(&controller);
        gtk_window.connect_is_active_notify(move |win| {
            if let Some(ctrl) = ctrl_weak.upgrade() {
                if win.is_active() {
                    ctrl.workspace.borrow_mut().set_active_window(window_id);
                    ctrl.poll_haven_peers_now();
                }
                ctrl.sync_shade_poll_state();
            }
        });

        let inner_weak = Rc::downgrade(&this.inner);
        gtk_window.connect_close_request(move |_| {
            if let Some(inner) = inner_weak.upgrade() {
                // Any close path takes the picker with it, including a WM close.
                let picker = inner.session_picker.borrow_mut().take();
                if let Some(picker) = picker {
                    picker.dialog.close();
                }
                if !inner.closing_programmatically.get()
                    && let Some(ctrl) = inner.controller.upgrade()
                {
                    // A WM close on the quick terminal dismisses it. GTK's
                    // hide-on-close does the hiding; ending the window here
                    // would kill the scratch shell the toggle promises to keep.
                    let quick = ctrl
                        .workspace
                        .borrow()
                        .window(inner.window_id)
                        .is_some_and(|w| w.quick);
                    if quick {
                        ctrl.mark_quick_dismissed();
                    } else {
                        ctrl.handle_window_closed(inner.window_id);
                    }
                }
            }
            glib::Propagation::Proceed
        });

        this
    }

    fn wire_banner_buttons(
        &self,
        send_haven: &gtk4::Button,
        allow_once: &gtk4::Button,
        allow_sess: &gtk4::Button,
        deny: &gtk4::Button,
        ack: &gtk4::Button,
    ) {
        {
            let inner_weak = Rc::downgrade(&self.inner);
            send_haven.connect_clicked(move |_| {
                if let Some(inner) = inner_weak.upgrade()
                    && let (Some(cid), Some(ctrl)) =
                        (inner.active_consent_id.get(), inner.controller.upgrade())
                    && let Err(err) = ctrl.relay_consent_to_haven(cid)
                {
                    eprintln!("warning: could not relay consent to Haven: {err}");
                }
            });
        }

        for (btn, res) in [
            (allow_once, ConsentResolution::AllowOnce),
            (allow_sess, ConsentResolution::AllowSession),
            (deny, ConsentResolution::Deny),
        ] {
            let inner_weak = Rc::downgrade(&self.inner);
            btn.connect_clicked(move |_| {
                if let Some(inner) = inner_weak.upgrade()
                    && let (Some(cid), Some(ctrl)) =
                        (inner.active_consent_id.get(), inner.controller.upgrade())
                {
                    ctrl.resolve_consent(cid, res);
                }
            });
        }

        let inner_weak = Rc::downgrade(&self.inner);
        ack.connect_clicked(move |_| {
            if let Some(inner) = inner_weak.upgrade()
                && let Some(ctrl) = inner.controller.upgrade()
            {
                let ap = ctrl
                    .workspace
                    .borrow()
                    .window(inner.window_id)
                    .and_then(|w| w.active_pane());
                if let Some(pid) = ap {
                    ctrl.clear_pane_notice(pid);
                }
            }
        });
    }

    fn wire_haven_auth_bar(&self) {
        {
            let this = self.clone();
            self.inner.haven_auth_unlock_btn.connect_clicked(move |_| {
                this.submit_active_haven_auth();
            });
        }
        {
            let this = self.clone();
            self.inner.haven_auth_entry.connect_activate(move |_| {
                this.submit_active_haven_auth();
            });
        }
        {
            let this = self.clone();
            self.inner.haven_auth_dismiss_btn.connect_clicked(move |_| {
                this.dismiss_active_haven_auth();
            });
        }
        {
            let this = self.clone();
            let keys = gtk4::EventControllerKey::new();
            keys.connect_key_pressed(move |_, key, _, _| {
                if key == gdk::Key::Escape {
                    this.dismiss_active_haven_auth();
                    return glib::Propagation::Stop;
                }
                glib::Propagation::Proceed
            });
            self.inner.haven_auth_entry.add_controller(keys);
        }
    }

    fn submit_active_haven_auth(&self) {
        let secret = self.inner.haven_auth_entry.text().to_string();
        let remember = self.inner.haven_auth_remember.is_active();
        // Clear immediately so credentials do not linger in GTK widget buffers.
        self.inner.haven_auth_entry.set_text("");
        self.inner.haven_auth_remember.set_active(false);
        if secret.is_empty() {
            return;
        }
        let target = self.inner.active_haven_auth_target.borrow().clone();
        if let (Some((peer_id, profile_id)), Some(ctrl)) = (target, self.inner.controller.upgrade())
            && let Err(err) = ctrl.submit_haven_auth_prompt(&peer_id, &profile_id, secret, remember)
        {
            eprintln!("warning: could not submit Haven auth prompt: {err}");
        }
    }

    fn dismiss_active_haven_auth(&self) {
        self.inner.haven_auth_entry.set_text("");
        self.inner.haven_auth_remember.set_active(false);
        let target = self.inner.active_haven_auth_target.borrow().clone();
        if let (Some((peer_id, profile_id)), Some(ctrl)) = (target, self.inner.controller.upgrade())
        {
            ctrl.dismiss_haven_auth_prompt(&peer_id, &profile_id);
        }
    }

    fn wire_search_bar(&self, prev: &gtk4::Button, next: &gtk4::Button, close: &gtk4::Button) {
        let inner_weak = Rc::downgrade(&self.inner);
        self.inner
            .search_entry
            .connect_search_changed(move |entry| {
                if let Some(inner) = inner_weak.upgrade()
                    && let Some(ctrl) = inner.controller.upgrade()
                {
                    ctrl.begin_pane_search(None, &entry.text());
                }
            });

        for (btn, forward) in [(prev, false), (next, true)] {
            let inner_weak = Rc::downgrade(&self.inner);
            btn.connect_clicked(move |_| {
                if let Some(inner) = inner_weak.upgrade()
                    && let Some(ctrl) = inner.controller.upgrade()
                {
                    ctrl.navigate_pane_search(None, forward);
                }
            });
        }

        let inner_weak = Rc::downgrade(&self.inner);
        close.connect_clicked(move |_| {
            if let Some(inner) = inner_weak.upgrade() {
                inner.search_bar.set_visible(false);
                if let Some(ctrl) = inner.controller.upgrade() {
                    ctrl.cancel_pane_search(None);
                }
            }
        });

        let inner_weak = Rc::downgrade(&self.inner);
        let keys = gtk4::EventControllerKey::new();
        keys.connect_key_pressed(move |_, keyval, _, state| {
            let Some(inner) = inner_weak.upgrade() else {
                return glib::Propagation::Proceed;
            };
            let Some(ctrl) = inner.controller.upgrade() else {
                return glib::Propagation::Proceed;
            };
            match keyval {
                gdk::Key::Escape => {
                    inner.search_bar.set_visible(false);
                    ctrl.cancel_pane_search(None);
                    glib::Propagation::Stop
                }
                gdk::Key::Return | gdk::Key::KP_Enter => {
                    let forward = !state.contains(gdk::ModifierType::SHIFT_MASK);
                    ctrl.navigate_pane_search(None, forward);
                    glib::Propagation::Stop
                }
                _ => glib::Propagation::Proceed,
            }
        });
        self.inner.search_entry.add_controller(keys);
    }

    pub fn show_search_bar(&self) {
        self.inner.search_bar.set_visible(true);
        self.inner.search_entry.grab_focus();
        if let Some(ctrl) = self.inner.controller.upgrade() {
            ctrl.begin_pane_search(None, &self.inner.search_entry.text());
        }
    }

    /// Reflects the active pane's scan state. The scan itself is advanced by
    /// the controller's tick, never here, so painting cannot stall on a search.
    pub fn refresh_search_bar(&self) {
        let Some(ctrl) = self.inner.controller.upgrade() else {
            return;
        };
        let state = {
            let ws = ctrl.workspace.borrow();
            ws.window(self.inner.window_id)
                .and_then(|w| w.active_pane())
                .and_then(|pid| ws.pane(pid))
                .and_then(|e| e.search())
                .map(|s| {
                    (
                        s.matches().len(),
                        s.lines_scanned(),
                        s.is_done(),
                        s.cursor(),
                    )
                })
        };
        match state {
            None => self.inner.search_bar.set_visible(false),
            Some((count, scanned, done, cursor)) => {
                self.inner.search_bar.set_visible(true);
                let text = if !done {
                    format!("{count} matches, scanning {scanned} lines…")
                } else if count == 0 {
                    format!("no matches ({scanned} lines scanned)")
                } else {
                    match cursor {
                        Some(i) => format!("{} of {count}", i + 1),
                        None => format!("{count} matches"),
                    }
                };
                self.inner.search_status.set_text(&text);
            }
        }
    }

    fn register_window_actions(&self, controller: &Rc<AppController>) {
        let win_id = self.inner.window_id;
        for spec in ACTIONS {
            let action = gio::SimpleAction::new(spec.slug, None);
            let ctrl_weak = Rc::downgrade(controller);
            let action_id = spec.id;
            action.connect_activate(move |_, _| {
                if let Some(ctrl) = ctrl_weak.upgrade() {
                    ctrl.execute_action(action_id, Some(win_id));
                }
            });
            self.inner.gtk_window.add_action(&action);
        }

        // Keyboard-accessible actions for the inline tap-to-pair bar.
        {
            let approve = gio::SimpleAction::new("approve-pairing", None);
            let this_approve = self.clone();
            approve.connect_activate(move |_, _| {
                this_approve.finish_active_pairing(true);
            });
            self.inner.gtk_window.add_action(&approve);

            let deny = gio::SimpleAction::new("deny-pairing", None);
            let this_deny = self.clone();
            deny.connect_activate(move |_, _| {
                this_deny.finish_active_pairing(false);
            });
            self.inner.gtk_window.add_action(&deny);
        }

        // Parameterised action for the Sessions submenu: the target names a
        // discovered session, so it is not a keybind-able verb in ACTIONS.
        let open = gio::SimpleAction::new("open-session-target", Some(glib::VariantTy::STRING));
        let ctrl_weak = Rc::downgrade(controller);
        open.connect_activate(move |_, param| {
            let Some(target) = param.and_then(|p| p.get::<String>()) else {
                return;
            };
            let Some((kind, host, name)) = crate::action::parse_session_target(&target) else {
                eprintln!("malformed session target: {target:?}");
                return;
            };
            if let Some(ctrl) = ctrl_weak.upgrade()
                && let Err(err) = ctrl.open_session(
                    kind,
                    &name,
                    host.as_deref(),
                    SessionMode::Tab,
                    Some(win_id),
                    None,
                )
            {
                eprintln!("failed to open session: {err}");
            }
        });
        self.inner.gtk_window.add_action(&open);

        // Tab-targeted context-menu actions. The target is a TabId, so these
        // are parameterised rather than entries in the keybind-able table.
        for name in [
            crate::action::CLOSE_TAB_TARGET,
            crate::action::DETACH_TAB_TARGET,
            crate::action::HANDOFF_TAB_TARGET,
            crate::action::RENAME_TAB_TARGET,
            crate::action::COPY_NAME_TAB_TARGET,
            crate::action::END_TAB_TARGET,
        ] {
            let slug = name.trim_start_matches("win.");
            let action = gio::SimpleAction::new(slug, Some(glib::VariantTy::UINT64));
            let ctrl_weak = Rc::downgrade(controller);
            let this = self.clone();
            let which = slug.to_owned();
            action.connect_activate(move |_, param| {
                let Some(id) = param.and_then(|p| p.get::<u64>()) else {
                    return;
                };
                let tab = TabId(id);
                let Some(ctrl) = ctrl_weak.upgrade() else {
                    return;
                };
                match which.as_str() {
                    "close-tab-target" => {
                        ctrl.close_tab(win_id, tab);
                    }
                    "detach-tab-target" => {
                        if ctrl.detach_tab(tab).is_err() {
                            let pane = ctrl
                                .workspace
                                .borrow()
                                .window(win_id)
                                .and_then(|w| w.tabs.iter().find(|t| t.id == tab))
                                .map(|t| t.active_pane);
                            if let Some(pane) = pane {
                                let _ = ctrl.detach_split(Some(pane));
                            }
                        }
                    }
                    "handoff-session-tab" => {
                        let pane = ctrl
                            .workspace
                            .borrow()
                            .window(win_id)
                            .and_then(|w| w.tabs.iter().find(|t| t.id == tab))
                            .map(|t| t.active_pane);
                        if let Some(pane) = pane
                            && let Err(err) =
                                ctrl.begin_handoff_to_peer(Some(pane), None, None, None)
                        {
                            eprintln!("warning: handoff-to-peer: {err}");
                        }
                    }
                    "rename-session-tab" => this.show_rename_dialog(tab),
                    "end-session-tab" => this.show_end_session_dialog(tab),
                    "copy-session-name-tab" => {
                        let name = ctrl
                            .workspace
                            .borrow()
                            .window(win_id)
                            .and_then(|w| w.tabs.iter().find(|t| t.id == tab))
                            .and_then(|t| {
                                ctrl.workspace
                                    .borrow()
                                    .pane(t.active_pane)
                                    .and_then(|e| e.mux_session())
                                    .map(|s| s.name.clone())
                            });
                        if let (Some(name), Some(display)) = (name, gdk::Display::default()) {
                            display.clipboard().set_text(&name);
                        }
                    }
                    _ => {}
                }
            });
            self.inner.gtk_window.add_action(&action);
        }
    }

    pub fn present(&self) {
        self.inner.gtk_window.present();
    }

    /// Drops the session picker and closes its dialog. The window owns the
    /// picker slot, so it clears it on every close path rather than relying on
    /// GTK's transient-destroy propagation, which does not fire for a
    /// programmatic close.
    fn dismiss_session_picker(&self) {
        self.inner.tab_menu.popdown();
        let picker = self.inner.session_picker.borrow_mut().take();
        if let Some(picker) = picker {
            picker.dialog.close();
        }
    }

    pub fn close_programmatically(&self) {
        self.inner.closing_programmatically.set(true);
        self.dismiss_session_picker();
        self.inner.gtk_window.close();
    }

    /// Keeps the active tab visible when the wrapped strip scrolls. Allocations
    /// are not ready during the rebuild, so this waits on a tick like the split
    /// ratio restore does, then runs once.
    fn scroll_active_tab_into_view(&self) {
        let Some(ctrl) = self.inner.controller.upgrade() else {
            return;
        };
        let active = ctrl
            .workspace
            .borrow()
            .window(self.inner.window_id)
            .map(|w| w.active_tab);
        let Some(active) = active else { return };
        let button = self
            .inner
            .tab_buttons
            .borrow()
            .iter()
            .find(|(tid, _, _, _)| *tid == active)
            .map(|(_, btn, _, _)| btn.clone());
        let Some(button) = button else { return };
        let scroller = self.inner.tab_scroller.clone();
        let flowbox = self.inner.tab_buttons_box.clone();
        scroller.add_tick_callback(move |scroller, _| {
            let Some(child) = button.parent() else {
                return glib::ControlFlow::Break;
            };
            let Some(bounds) = child.compute_bounds(&flowbox) else {
                return glib::ControlFlow::Continue;
            };
            if bounds.height() <= 0.0 {
                return glib::ControlFlow::Continue;
            }
            let adj = scroller.vadjustment();
            let top = adj.value();
            let visible = adj.page_size();
            let y = f64::from(bounds.y());
            let bottom = y + f64::from(bounds.height());
            if y < top {
                adj.set_value(y);
            } else if visible > 0.0 && bottom > top + visible {
                adj.set_value((bottom - visible).max(0.0));
            }
            glib::ControlFlow::Break
        });
    }

    /// Width of the toolbar control cluster, for measuring the compaction.
    pub fn toolbar_width(&self) -> i32 {
        self.inner.toolbar_box.width()
    }

    /// Switches the window/taskbar icon between idle and agent-active. Only
    /// called when the state actually changes, so a busy agent does not churn
    /// GTK properties every tick.
    pub fn set_agent_active(&self, active: bool) {
        let name = if active {
            crate::desktop::AGENT_ICON_NAME
        } else {
            crate::desktop::APP_ID
        };
        self.inner.gtk_window.set_icon_name(Some(name));
    }

    /// Makes a WM close hide this window instead of destroying it, so the
    /// quick terminal's shell keeps running while it is dismissed.
    pub fn set_hide_on_close(&self, hide: bool) {
        self.inner.gtk_window.set_hide_on_close(hide);
    }

    pub fn is_visible(&self) -> bool {
        self.inner.gtk_window.is_visible()
    }

    pub fn hide(&self) {
        self.inner.gtk_window.set_visible(false);
    }

    /// The tab context menu, for tests that check its lifetime.
    pub fn tab_menu(&self) -> gtk4::PopoverMenu {
        self.inner.tab_menu.clone()
    }

    /// The underlying GTK window, for tests that drive real gio actions.
    pub fn gtk_window(&self) -> gtk4::ApplicationWindow {
        self.inner.gtk_window.clone()
    }

    /// The icon name this window advertises to the shell and taskbar.
    pub fn icon_name(&self) -> Option<String> {
        self.inner.gtk_window.icon_name().map(|s| s.to_string())
    }

    /// The toplevel's minimum horizontal size. This is what forces a window to
    /// grow: GTK will not allocate below it, so a tab strip whose minimum is the
    /// sum of its tabs stretches the window instead of adapting.
    pub fn window_minimum_width(&self) -> i32 {
        self.inner
            .gtk_window
            .measure(gtk4::Orientation::Horizontal, -1)
            .0
    }

    /// The toplevel's minimum vertical size. Wrapping trades width for height,
    /// so this must not grow without bound either.
    pub fn window_minimum_height(&self) -> i32 {
        self.inner
            .gtk_window
            .measure(gtk4::Orientation::Vertical, -1)
            .0
    }

    /// Visible height of the strip, i.e. the scrolled viewport, not the full
    /// wrapped content height inside it.
    pub fn tab_strip_height(&self) -> i32 {
        self.inner.tab_scroller.height()
    }

    /// Full wrapped content height, which may exceed the visible viewport.
    pub fn tab_strip_content_height(&self) -> i32 {
        self.inner.tab_buttons_box.height()
    }

    /// Distinct rows the tab strip occupies. One row means the tabs are laid
    /// out in a single line; more than one means they wrapped.
    pub fn tab_strip_row_count(&self) -> usize {
        let mut rows: Vec<i32> = Vec::new();
        let mut child = self.inner.tab_buttons_box.first_child();
        while let Some(widget) = child {
            let y = widget
                .compute_bounds(&self.inner.tab_buttons_box)
                .map(|b| b.y().round() as i32)
                .unwrap_or(0);
            if !rows.contains(&y) {
                rows.push(y);
            }
            child = widget.next_sibling();
        }
        rows.len()
    }

    /// The button for a tab, so a test can drive the real clicked signal rather
    /// than the model behind it.
    pub fn tab_button(&self, tid: TabId) -> Option<gtk4::Button> {
        self.inner
            .tab_buttons
            .borrow()
            .iter()
            .find(|(id, _, _, _)| *id == tid)
            .map(|(_, btn, _, _)| btn.clone())
    }

    /// Minimum width of the widest single tab. The strip's own minimum should
    /// be about this, not this times the tab count.
    pub fn widest_tab_minimum_width(&self) -> i32 {
        self.inner
            .tab_buttons
            .borrow()
            .iter()
            .map(|(_, btn, _, _)| btn.measure(gtk4::Orientation::Horizontal, -1).0)
            .max()
            .unwrap_or(0)
    }

    pub fn tab_strip_width(&self) -> i32 {
        self.inner.tab_buttons_box.width()
    }

    /// Test/inspection accessors for the open session picker: row count and the
    /// selected session name. `None` when no picker is open.
    pub fn session_picker_rows(&self) -> Option<usize> {
        let picker = self.inner.session_picker.borrow().clone()?;
        Some(picker.rows.borrow().len())
    }

    pub fn session_picker_selected(&self) -> Option<String> {
        let picker = self.inner.session_picker.borrow().clone()?;
        picker.selected_row_item().map(|item| item.name)
    }

    pub fn session_picker_group_headers(&self) -> Vec<String> {
        let Some(picker) = self.inner.session_picker.borrow().clone() else {
            return Vec::new();
        };
        picker.group_headers.borrow().clone()
    }

    pub fn session_picker_item_labels(&self) -> Vec<String> {
        let Some(picker) = self.inner.session_picker.borrow().clone() else {
            return Vec::new();
        };
        picker.item_labels.borrow().clone()
    }

    /// Current tab button labels, including status badges. Exposed so tests
    /// can assert a notification badges its tab.
    pub fn tab_labels(&self) -> Vec<String> {
        self.inner
            .tab_buttons
            .borrow()
            .iter()
            .map(|(_, _, title, _)| title.text().to_string())
            .collect()
    }

    pub fn queue_draw_pane(&self, pane_id: PaneId) {
        if let Some(view) = self.inner.pane_views.borrow().get(&pane_id) {
            view.queue_draw();
        }
    }

    /// Applies a live config reload: rebuild each view's font and cell metrics
    /// (which resizes its grid and PTY), then repaint. Window geometry is not
    /// resized here; it applies to newly opened windows.
    pub fn refresh_from_config(&self) {
        let views: Vec<TerminalView> = self.inner.pane_views.borrow().values().cloned().collect();
        for view in views {
            view.refresh_font();
        }
        self.refresh_tab_strip_scale();
        self.apply_tab_strip_height_cap();
    }

    /// Scales session tab title/subtitle Pango font sizes and button padding in
    /// proportion to this window's effective terminal font size (`config.font.size`
    /// plus any `Ctrl+Scroll` zoom offset), so zooming out packs more tabs per row
    /// and zooming in keeps tab labels legible.
    fn refresh_tab_strip_scale(&self) {
        let Some(ctrl) = self.inner.controller.upgrade() else {
            return;
        };
        ensure_compact_toolbar_css();
        let term_pt = ctrl.effective_font_size_for_window(Some(self.inner.window_id));
        let gap = if term_pt <= 9 {
            2
        } else if term_pt >= 15 {
            6
        } else {
            4
        };
        self.inner.tab_buttons_box.set_row_spacing(gap);
        self.inner.tab_buttons_box.set_column_spacing(gap);
        for (_, btn, title_label, sub_label) in self.inner.tab_buttons.borrow().iter() {
            apply_session_tab_scale(btn, title_label, sub_label, term_pt);
        }
        self.inner.tab_buttons_box.queue_resize();
        self.inner.tab_scroller.queue_resize();
    }

    /// Applies the configured height cap to the wrapping strip.
    fn apply_tab_strip_height_cap(&self) {
        let Some(ctrl) = self.inner.controller.upgrade() else {
            return;
        };
        let cap = ctrl.config_ref().tab_strip_max_height_px;
        self.inner.tab_scroller.set_max_content_height(cap);
        self.inner.tab_scroller.set_min_content_height(0);
    }

    pub fn rebuild_from_model(&self) {
        let Some(ctrl) = self.inner.controller.upgrade() else {
            return;
        };

        let (menubar_vis, active_tab_id, active_pane_id, active_tree, win_title) = {
            let ws = ctrl.workspace.borrow();
            let Some(wm) = ws.window(self.inner.window_id) else {
                return;
            };
            let Some(tab) = wm.active_tab_ref() else {
                return;
            };
            let title = ws
                .pane(tab.active_pane)
                .map(|e| match e.mux_session() {
                    Some(s) => format!("Mutiny: {} ({})", s.name, s.subtitle()),
                    None => format!("Mutiny: {}", e.title()),
                })
                .unwrap_or_else(|| String::from("Mutiny"));
            (
                wm.menubar_visible,
                wm.active_tab,
                tab.active_pane,
                tab.root.clone(),
                title,
            )
        };

        self.inner.gtk_window.set_title(Some(&win_title));
        self.inner.menubar.set_visible(menubar_vis);

        while let Some(child) = self.inner.tab_buttons_box.first_child() {
            self.inner.tab_buttons_box.remove(&child);
        }
        self.inner.tab_buttons.borrow_mut().clear();

        let tab_ids_and_labels = self.compute_tab_labels(&ctrl, active_tab_id);
        for (idx, (tid, label, subtitle, first_pane)) in tab_ids_and_labels.into_iter().enumerate()
        {
            let btn = gtk4::Button::new();
            let lines = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
            let title_label = gtk4::Label::new(Some(&label));
            title_label.set_xalign(0.0);
            title_label.set_ellipsize(gtk4::pango::EllipsizeMode::End);
            title_label.set_max_width_chars(24);
            let sub_label = gtk4::Label::new(Some(&subtitle));
            sub_label.set_xalign(0.0);
            sub_label.set_ellipsize(gtk4::pango::EllipsizeMode::End);
            sub_label.set_max_width_chars(24);
            sub_label.add_css_class("dim-label");
            sub_label.add_css_class("tab-subtitle");
            lines.append(&title_label);
            lines.append(&sub_label);
            btn.set_child(Some(&lines));
            // No tooltip: the two-line label already shows name and host, and a
            // tooltip window stacks over the right-click menu and hides items.
            btn.set_can_focus(false);
            if tid == active_tab_id {
                btn.add_css_class("suggested-action");
            }
            let ctrl_weak = Rc::downgrade(&ctrl);
            btn.connect_clicked(move |_| {
                if let Some(c) = ctrl_weak.upgrade() {
                    let res = c.workspace.borrow_mut().focus_pane(first_pane);
                    if let Ok((w, _)) = res {
                        c.sync_ui_window(w);
                    }
                }
            });
            // Drag-to-reorder, cross-window drag-to-redock, and drag-out tear-off:
            // releasing outside any Mutiny drop target (`DragCancelReason::NoTarget`)
            // tears the tab out into a new Mutiny window.
            attach_tab_drag_source(&btn, tid, &ctrl);
            {
                let ctrl_drop = Rc::downgrade(&ctrl);
                let win_for_drop = self.inner.window_id;
                attach_tab_drop_target(&btn, move |dragged_tab| {
                    let Some(c) = ctrl_drop.upgrade() else {
                        return false;
                    };
                    c.move_tab_to_window(dragged_tab, win_for_drop, Some(idx))
                });
            }

            // Right-click opens a per-tab menu. The tab DragSource is
            // primary-button only, so a secondary GestureClick does not fight
            // it, and connect_clicked fires on primary release only.
            let click = gtk4::GestureClick::new();
            click.set_button(gdk::BUTTON_SECONDARY);
            let ctrl_menu = Rc::downgrade(&ctrl);
            let menu = self.inner.tab_menu.clone();
            let win_for_menu = self.inner.gtk_window.clone();
            let win_id_for_menu = self.inner.window_id;
            let btn_for_menu = btn.clone();
            click.connect_pressed(move |_, _, x, y| {
                let Some(c) = ctrl_menu.upgrade() else {
                    return;
                };
                // Focus the tab first, so the parameterless New Session action
                // and any active-pane verb target the tab that was clicked.
                let _ = c.workspace.borrow_mut().focus_pane(first_pane);
                let tab_leaves = c
                    .workspace
                    .borrow()
                    .window(win_id_for_menu)
                    .and_then(|w| w.tabs.iter().find(|t| t.id == tid))
                    .map(|t| t.root.leaves())
                    .unwrap_or_else(|| vec![first_pane]);
                let attached = tab_leaves
                    .into_iter()
                    .any(|pid| c.detect_pane_mux_backing(pid).ok().flatten().is_some());
                let model = crate::action::build_tab_menu_model(attached, tid.0);
                menu.set_menu_model(Some(&model));
                // The gesture gives button-relative coordinates; the popover is
                // parented to the window, so translate them.
                let point = gtk4::graphene::Point::new(x as f32, y as f32);
                let translated = btn_for_menu
                    .compute_point(&win_for_menu, &point)
                    .unwrap_or(point);
                let (wx, wy) = (f64::from(translated.x()), f64::from(translated.y()));
                menu.set_pointing_to(Some(&gdk::Rectangle::new(
                    wx.round() as i32,
                    wy.round() as i32,
                    1,
                    1,
                )));
                menu.popup();
            });
            btn.add_controller(click);

            self.inner.tab_buttons_box.append(&btn);
            self.inner
                .tab_buttons
                .borrow_mut()
                .push((tid, btn, title_label, sub_label));
        }

        self.refresh_tab_strip_scale();
        self.apply_tab_strip_height_cap();
        self.scroll_active_tab_into_view();
        self.sync_banners(&ctrl);

        while let Some(child) = self.inner.content_slot.first_child() {
            self.inner.content_slot.remove(&child);
        }
        self.inner.pane_views.borrow_mut().clear();

        let mut views_map = HashMap::new();
        let tree_widget =
            build_split_widget(&active_tree, &ctrl, &mut views_map, active_tab_id, &[]);
        self.inner.content_slot.append(&tree_widget);
        let focus_view = views_map.get(&active_pane_id).cloned();
        *self.inner.pane_views.borrow_mut() = views_map;

        if let Some(v) = focus_view {
            v.grab_focus();
        }
    }

    pub fn refresh_tab_strip_labels(&self) {
        let Some(ctrl) = self.inner.controller.upgrade() else {
            return;
        };
        let active_tab_id = {
            let ws = ctrl.workspace.borrow();
            let Some(wm) = ws.window(self.inner.window_id) else {
                return;
            };
            if let Some(ap) = wm.active_pane().and_then(|p| ws.pane(p)) {
                self.inner
                    .gtk_window
                    .set_title(Some(&match ap.mux_session() {
                        Some(s) => format!("Mutiny: {} ({})", s.name, s.subtitle()),
                        None => format!("Mutiny: {}", ap.title()),
                    }));
            }
            wm.active_tab
        };

        let labels: HashMap<TabId, (String, String)> = self
            .compute_tab_labels(&ctrl, active_tab_id)
            .into_iter()
            .map(|(tid, lbl, sub, _)| (tid, (lbl, sub)))
            .collect();

        for (tid, _btn, title_label, sub_label) in self.inner.tab_buttons.borrow().iter() {
            if let Some((lbl, sub)) = labels.get(tid) {
                title_label.set_text(lbl);
                sub_label.set_text(sub);
            }
        }

        self.sync_banners(&ctrl);
    }

    fn sync_banners(&self, ctrl: &Rc<AppController>) {
        let close_state = {
            let ws = ctrl.workspace.borrow();
            if let Some(wm) = ws.window(self.inner.window_id) {
                let pane_attached = wm
                    .active_pane()
                    .and_then(|pid| ws.pane(pid))
                    .is_some_and(|e| e.mux_session().is_some());
                let tab_attached = wm.active_tab_ref().is_some_and(|t| {
                    t.root
                        .leaves()
                        .into_iter()
                        .any(|pid| ws.pane(pid).is_some_and(|e| e.mux_session().is_some()))
                });
                let window_has_mux = wm.tabs.iter().any(|t| {
                    t.root
                        .leaves()
                        .into_iter()
                        .any(|pid| ws.pane(pid).is_some_and(|e| e.mux_session().is_some()))
                });
                (tab_attached, window_has_mux, pane_attached)
            } else {
                (false, false, false)
            }
        };
        if self.inner.last_close_menu_state.get() != Some(close_state) {
            self.inner.last_close_menu_state.set(Some(close_state));
            sync_close_menu_sections(
                &self.inner.session_close_menu,
                &self.inner.pane_close_menu,
                close_state.0,
                close_state.1,
                close_state.2,
            );
        }

        if let Some((pill_label, pill_tip)) = ctrl.limb_pill_summary() {
            self.inner.limb_pill_btn.set_label(&pill_label);
            self.inner.limb_pill_btn.set_tooltip_text(Some(&pill_tip));
            self.inner.limb_pill_btn.set_visible(true);
        } else {
            self.inner.limb_pill_btn.set_visible(false);
        }

        if let Some((pid, client_name, remote_addr)) = ctrl.pending_pairing_prompt() {
            if self.inner.active_pairing_id.get() != Some(pid) {
                self.inner.active_pairing_id.set(Some(pid));
                self.inner.pairing_mode_dropdown.set_selected(0);
            }
            self.inner.pairing_label.set_text(&format!(
                "Pair inbound MCP client '{client_name}' from {remote_addr}?"
            ));
            self.inner.pairing_bar.set_visible(true);
        } else {
            self.inner.active_pairing_id.set(None);
            self.inner.pairing_bar.set_visible(false);
        }

        if let Some(agent_name) = ctrl.active_agent_badge() {
            self.inner
                .agent_badge_btn
                .set_label(&format!("Agent: {agent_name}"));
            self.inner.agent_badge_btn.set_visible(true);
        } else {
            self.inner.agent_badge_btn.set_visible(false);
        }

        if let Some((cid, client, verb, detail, tier)) = ctrl.pending_consent_prompt() {
            self.inner.active_consent_id.set(Some(cid));
            let no_session_grant = matches!(
                tier,
                crate::action::ConsentTier::Irreversible | crate::action::ConsentTier::PolicyChange
            );
            self.inner.allow_sess_btn.set_visible(!no_session_grant);
            let (can_relay, already_relayed) = ctrl.consent_haven_relay_state(cid);
            self.inner
                .send_haven_btn
                .set_visible(can_relay || already_relayed);
            self.inner.send_haven_btn.set_sensitive(!already_relayed);
            self.inner.send_haven_btn.set_label(if already_relayed {
                "Sent to Haven…"
            } else {
                "Send to Haven"
            });
            let tier_label = match tier {
                crate::action::ConsentTier::Irreversible => "Tier-4 irreversible",
                crate::action::ConsentTier::PolicyChange => "PolicyChange",
                _ => "Tier-2",
            };
            self.inner.consent_label.set_text(&format!(
                "Agent '{client}' requests {tier_label} '{verb}' {detail}"
            ));
            self.inner.consent_bar.set_visible(true);
        } else {
            self.inner.active_consent_id.set(None);
            self.inner.send_haven_btn.set_visible(false);
            self.inner.consent_bar.set_visible(false);
        }

        if let Some((peer_id, peer_label, prompt)) = ctrl.active_haven_auth_prompt() {
            let target = (peer_id, prompt.profile_id.clone());
            let changed = *self.inner.active_haven_auth_target.borrow() != Some(target.clone());
            if changed {
                *self.inner.active_haven_auth_target.borrow_mut() = Some(target);
                self.inner.haven_auth_entry.set_text("");
                self.inner.haven_auth_remember.set_active(false);
            }
            let secret_kind = if prompt.requires_passphrase {
                "SSH key passphrase"
            } else {
                "host password"
            };
            self.inner
                .haven_auth_entry
                .set_placeholder_text(Some(secret_kind));
            self.inner.haven_auth_label.set_text(&format!(
                "Haven '{peer_label}' needs {secret_kind} for '{}'",
                prompt.label
            ));
            self.inner.haven_auth_bar.set_visible(true);
        } else {
            if self
                .inner
                .active_haven_auth_target
                .borrow_mut()
                .take()
                .is_some()
            {
                self.inner.haven_auth_entry.set_text("");
                self.inner.haven_auth_remember.set_active(false);
            }
            self.inner.haven_auth_bar.set_visible(false);
        }

        let active_notice = {
            let ws = ctrl.workspace.borrow();
            ws.window(self.inner.window_id)
                .and_then(|w| w.active_pane())
                .and_then(|pid| {
                    ws.pane(pid)
                        .and_then(|e| e.agent_notice().map(|m| (pid, m.to_owned())))
                })
        };
        if let Some((pid, msg)) = active_notice {
            self.inner
                .notice_label
                .set_text(&format!("Pane {pid} notice: {msg}"));
            self.inner.notice_bar.set_visible(true);
        } else {
            self.inner.notice_bar.set_visible(false);
        }
    }

    fn compute_tab_labels(
        &self,
        ctrl: &Rc<AppController>,
        _active_tab: TabId,
    ) -> Vec<(TabId, String, String, PaneId)> {
        let ws = ctrl.workspace.borrow();
        let Some(wm) = ws.window(self.inner.window_id) else {
            return Vec::new();
        };

        wm.tabs
            .iter()
            .map(|tab| {
                let leaves = tab.root.leaves();
                let pane_count = leaves.len();
                let active = ws.pane(tab.active_pane);
                let title = active
                    .map(|e| e.title().to_owned())
                    .unwrap_or_else(|| String::from("shell"));
                // Haven sibling convention: title is the session name,
                // subtitle is the host. Local sessions show this hostname so
                // both shapes read the same.
                let subtitle = active
                    .and_then(|e| e.mux_session())
                    .map(|s| s.subtitle())
                    .unwrap_or_else(crate::mux::local_hostname);

                let mut status_badges = Vec::new();
                let mut any_running = false;
                for &pid in &leaves {
                    if let Some(eng) = ws.pane(pid) {
                        if eng.is_command_running() {
                            any_running = true;
                        }
                        match eng.attention() {
                            Some(AttentionReason::CommandFailed(code)) => {
                                status_badges.push(format!("!exit {code}"));
                            }
                            Some(AttentionReason::Bell) => {
                                status_badges.push(String::from("!bell"));
                            }
                            Some(AttentionReason::AgentNotice) => {
                                status_badges.push(String::from("!agent"));
                            }
                            Some(AttentionReason::OscNotification) => {
                                status_badges.push(String::from("!notify"));
                            }
                            None => {}
                        }
                    }
                }
                if any_running {
                    status_badges.push(String::from("run"));
                }
                if pane_count > 1 {
                    status_badges.push(format!("{pane_count}p"));
                }

                // No leading index: once sessions are named the number is
                // redundant noise. Attention badges stay on the title line,
                // where they are glanceable.
                let label = if status_badges.is_empty() {
                    truncate_label(&title, 24)
                } else {
                    format!(
                        "{} [{}]",
                        truncate_label(&title, 20),
                        status_badges.join(" ")
                    )
                };
                (tab.id, label, subtitle, tab.active_pane)
            })
            .collect()
    }

    /// A small modal that asks for one line of text. Shared by Rename and New
    /// Session so both get the same Escape/Enter/close-before-dispatch behaviour
    /// as the other dialogs.
    fn name_prompt(
        &self,
        title: &str,
        label: &str,
        initial: &str,
        extra: Option<gtk4::Box>,
        on_ok: impl Fn(&str) + 'static,
    ) {
        let on_ok = Rc::new(on_ok);
        let dialog = gtk4::Window::builder()
            .transient_for(&self.inner.gtk_window)
            .modal(true)
            .destroy_with_parent(true)
            .title(title)
            .default_width(420)
            .build();
        let vbox = gtk4::Box::new(gtk4::Orientation::Vertical, 8);
        for m in ["top", "bottom", "start", "end"] {
            match m {
                "top" => vbox.set_margin_top(12),
                "bottom" => vbox.set_margin_bottom(12),
                "start" => vbox.set_margin_start(12),
                _ => vbox.set_margin_end(12),
            }
        }
        let prompt = gtk4::Label::new(Some(label));
        prompt.set_xalign(0.0);
        vbox.append(&prompt);
        let entry = gtk4::Entry::new();
        entry.set_text(initial);
        entry.set_activates_default(true);
        vbox.append(&entry);
        if let Some(extra) = extra {
            vbox.append(&extra);
        }
        let buttons = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
        buttons.set_halign(gtk4::Align::End);
        let cancel = gtk4::Button::with_label("Cancel");
        let ok = gtk4::Button::with_label("OK");
        ok.add_css_class("suggested-action");
        buttons.append(&cancel);
        buttons.append(&ok);
        vbox.append(&buttons);
        dialog.set_child(Some(&vbox));
        dialog.set_default_widget(Some(&ok));

        {
            let dialog = dialog.clone();
            cancel.connect_clicked(move |_| dialog.close());
        }
        {
            let dialog = dialog.clone();
            let entry_for_ok = entry.clone();
            let on_ok = Rc::clone(&on_ok);
            ok.connect_clicked(move |_| {
                let text = entry_for_ok.text().to_string();
                // Close before dispatch, like every other dialog here, so a
                // topology rebuild finds no live modal.
                dialog.close();
                if !text.trim().is_empty() {
                    on_ok(&text);
                }
            });
        }
        {
            let dialog = dialog.clone();
            let entry_for_enter = entry.clone();
            entry.connect_activate(move |_| {
                let text = entry_for_enter.text().to_string();
                dialog.close();
                if !text.trim().is_empty() {
                    on_ok(&text);
                }
            });
        }
        attach_escape_to_close(&dialog);
        dialog.present();
        entry.grab_focus();
        entry.select_region(0, -1);
    }

    /// Rename the multiplexer session this tab is attached to, using the
    /// multiplexer's own command. Not a mutiny-side label override.
    pub fn show_rename_dialog(&self, tab: TabId) {
        let Some(ctrl) = self.inner.controller.upgrade() else {
            return;
        };
        let session = {
            let ws = ctrl.workspace.borrow();
            ws.window(self.inner.window_id)
                .and_then(|w| w.tabs.iter().find(|t| t.id == tab))
                .and_then(|t| ws.pane(t.active_pane))
                .and_then(|e| e.mux_session())
                .cloned()
        };
        let Some(session) = session else {
            return;
        };
        let old = session.name.clone();
        let prompt = format!("New name for {} session:", session.kind.label());
        let ctrl_ok = Rc::downgrade(&ctrl);
        let win_id = self.inner.window_id;
        let initial = old.clone();
        self.name_prompt("Rename Session", &prompt, &initial, None, move |new| {
            if let Some(c) = ctrl_ok.upgrade()
                && let Err(err) = c.rename_session(session.kind, &old, new, session.host.as_deref())
            {
                let msg = format!("failed to rename session: {err}");
                eprintln!("{msg}");
                c.surface_window_notice(Some(win_id), msg);
            }
        });
    }

    /// Confirm before ENDING a session. Closing a tab only ends this client;
    /// this stops everything running inside the session, so it names the
    /// session and says plainly what is lost.
    pub fn show_end_session_dialog(&self, tab: TabId) {
        let Some(ctrl) = self.inner.controller.upgrade() else {
            return;
        };
        let session = {
            let ws = ctrl.workspace.borrow();
            ws.window(self.inner.window_id)
                .and_then(|w| w.tabs.iter().find(|t| t.id == tab))
                .and_then(|t| ws.pane(t.active_pane))
                .and_then(|e| e.mux_session())
                .cloned()
        };
        let Some(session) = session else {
            return;
        };
        let where_ = session.subtitle();
        let dialog = gtk4::Window::builder()
            .transient_for(&self.inner.gtk_window)
            .modal(true)
            .destroy_with_parent(true)
            .title("End Session")
            .default_width(460)
            .build();
        let vbox = gtk4::Box::new(gtk4::Orientation::Vertical, 10);
        vbox.set_margin_top(14);
        vbox.set_margin_bottom(14);
        vbox.set_margin_start(14);
        vbox.set_margin_end(14);
        let heading = gtk4::Label::new(None);
        heading.set_markup(&format!(
            "<b>End {} session \"{}\"?</b>",
            session.kind.label(),
            glib::markup_escape_text(&session.name)
        ));
        heading.set_xalign(0.0);
        vbox.append(&heading);
        let body = gtk4::Label::new(Some(&format!(
            "On {where_}. Everything running inside this session stops, including \
             anything in other windows or on other devices.\n\n\
             Closing the tab instead would leave it running."
        )));
        body.set_xalign(0.0);
        body.set_wrap(true);
        vbox.append(&body);
        let buttons = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
        buttons.set_halign(gtk4::Align::End);
        let cancel = gtk4::Button::with_label("Keep It Running");
        let end = gtk4::Button::with_label("End Session");
        end.add_css_class("destructive-action");
        buttons.append(&cancel);
        buttons.append(&end);
        vbox.append(&buttons);
        dialog.set_child(Some(&vbox));
        // Default to the safe choice, so Enter cannot destroy anything.
        dialog.set_default_widget(Some(&cancel));
        {
            let dialog = dialog.clone();
            cancel.connect_clicked(move |_| dialog.close());
        }
        {
            let dialog = dialog.clone();
            let ctrl = Rc::downgrade(&ctrl);
            end.connect_clicked(move |_| {
                dialog.close();
                if let Some(c) = ctrl.upgrade()
                    && let Err(err) =
                        c.end_session(session.kind, &session.name, session.host.as_deref())
                {
                    eprintln!("failed to end session: {err}");
                }
            });
        }
        attach_escape_to_close(&dialog);
        dialog.present();
    }

    /// Create a new session, with the multiplexer preselected from config and
    /// overridable here, including "none" for a plain shell.
    pub fn show_new_session_dialog(&self) {
        let Some(ctrl) = self.inner.controller.upgrade() else {
            return;
        };
        let preferred = ctrl.config_ref().preferred_mux;
        let suggested = ctrl.suggested_session_name();

        let extra = gtk4::Box::new(gtk4::Orientation::Horizontal, 6);
        let tmux_btn = gtk4::ToggleButton::with_label("tmux");
        let zellij_btn = gtk4::ToggleButton::with_label("zellij");
        let none_btn = gtk4::ToggleButton::with_label("none");
        zellij_btn.set_group(Some(&tmux_btn));
        none_btn.set_group(Some(&tmux_btn));
        match preferred {
            Some(MuxKind::Tmux) => tmux_btn.set_active(true),
            Some(MuxKind::Zellij) => zellij_btn.set_active(true),
            None => none_btn.set_active(true),
        }
        for b in [&tmux_btn, &zellij_btn, &none_btn] {
            extra.append(b);
        }
        let in_window = gtk4::CheckButton::with_label("open in a new window");
        extra.append(&in_window);

        let ctrl_ok = Rc::downgrade(&ctrl);
        let win_self = self.clone();
        let win_id = self.inner.window_id;
        self.name_prompt(
            "New Session",
            "Session name (ignored for none):",
            &suggested,
            Some(extra),
            move |name| {
                let kind = if tmux_btn.is_active() {
                    Some(MuxKind::Tmux)
                } else if zellij_btn.is_active() {
                    Some(MuxKind::Zellij)
                } else {
                    None
                };
                let mode = if in_window.is_active() {
                    SessionMode::Window
                } else {
                    SessionMode::Tab
                };
                let Some(c) = ctrl_ok.upgrade() else {
                    return;
                };
                let trimmed = name.trim();
                if let Some(mux_kind) = kind
                    && !trimmed.is_empty()
                    && c.local_session_exists(mux_kind, trimmed)
                {
                    win_self.show_existing_session_prompt(mux_kind, trimmed.to_owned(), mode);
                    return;
                }
                if let Err(err) = c.new_session(kind, Some(name.to_owned()), mode, Some(win_id)) {
                    let msg = format!("failed to create session: {err}");
                    eprintln!("{msg}");
                    c.surface_window_notice(Some(win_id), msg);
                }
            },
        );
    }

    /// Offered when `New Session` names a local multiplexer session that is
    /// already running: never attaches behind the operator's back, and avoids
    /// spawning a `tmux new-session` pane that exits in milliseconds with a
    /// duplicate-session error.
    fn show_existing_session_prompt(&self, kind: MuxKind, name: String, mode: SessionMode) {
        let Some(ctrl) = self.inner.controller.upgrade() else {
            return;
        };
        let dialog = gtk4::Window::builder()
            .transient_for(&self.inner.gtk_window)
            .modal(true)
            .destroy_with_parent(true)
            .title("Session Already Exists")
            .default_width(440)
            .build();
        let vbox = gtk4::Box::new(gtk4::Orientation::Vertical, 10);
        vbox.set_margin_top(14);
        vbox.set_margin_bottom(14);
        vbox.set_margin_start(14);
        vbox.set_margin_end(14);
        let heading = gtk4::Label::new(None);
        heading.set_markup(&format!(
            "<b>{} session \"{}\" already exists</b>",
            kind.label(),
            glib::markup_escape_text(&name)
        ));
        heading.set_xalign(0.0);
        vbox.append(&heading);
        let body = gtk4::Label::new(Some(&format!(
            "A {} session named \"{name}\" is already running on this machine. \
             Open the existing session instead?",
            kind.label()
        )));
        body.set_xalign(0.0);
        body.set_wrap(true);
        vbox.append(&body);
        let buttons = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
        buttons.set_halign(gtk4::Align::End);
        let cancel = gtk4::Button::with_label("Cancel");
        let open_existing = gtk4::Button::with_label("Open Existing Session");
        open_existing.add_css_class("suggested-action");
        buttons.append(&cancel);
        buttons.append(&open_existing);
        vbox.append(&buttons);
        dialog.set_child(Some(&vbox));
        dialog.set_default_widget(Some(&open_existing));
        {
            let dialog = dialog.clone();
            cancel.connect_clicked(move |_| dialog.close());
        }
        {
            let dialog = dialog.clone();
            let ctrl = Rc::downgrade(&ctrl);
            let win_id = self.inner.window_id;
            open_existing.connect_clicked(move |_| {
                dialog.close();
                if let Some(c) = ctrl.upgrade()
                    && let Err(err) = c.open_session(kind, &name, None, mode, Some(win_id), None)
                {
                    let msg = format!("failed to open session: {err}");
                    eprintln!("{msg}");
                    c.surface_window_notice(Some(win_id), msg);
                }
            });
        }
        attach_escape_to_close(&dialog);
        dialog.present();
    }

    /// Settings: every key in the config store, grouped by its TOML table.
    /// Each field contributes a setter closure, so adding a config key means
    /// adding one row here rather than threading a new struct through.
    pub fn show_settings_dialog(&self) {
        let Some(ctrl) = self.inner.controller.upgrade() else {
            return;
        };
        let path = ctrl.config_path().to_path_buf();
        let current = crate::config::load_or_default(&path);

        let dialog = gtk4::Window::builder()
            .transient_for(&self.inner.gtk_window)
            .modal(true)
            .destroy_with_parent(true)
            .title("Settings")
            .default_width(560)
            .default_height(620)
            .build();
        let outer = gtk4::Box::new(gtk4::Orientation::Vertical, 8);
        outer.set_margin_top(12);
        outer.set_margin_bottom(12);
        outer.set_margin_start(12);
        outer.set_margin_end(12);

        let grid = gtk4::Grid::new();
        grid.set_column_spacing(12);
        grid.set_row_spacing(6);
        let scroller = gtk4::ScrolledWindow::builder()
            .vexpand(true)
            .hexpand(true)
            .child(&grid)
            .build();
        outer.append(&scroller);

        let row = std::cell::Cell::new(0i32);
        let setters: Rc<RefCell<Vec<ConfigSetter>>> = Rc::new(RefCell::new(Vec::new()));

        let heading = |text: &str| {
            let label = gtk4::Label::new(None);
            label.set_markup(&format!("<b>{text}</b>"));
            label.set_xalign(0.0);
            label.set_margin_top(8);
            grid.attach(&label, 0, row.get(), 2, 1);
            row.set(row.get() + 1);
        };
        let entry = |label: &str, value: &str, set: TextSetter| {
            let l = gtk4::Label::new(Some(label));
            l.set_xalign(0.0);
            let e = gtk4::Entry::new();
            e.set_text(value);
            e.set_hexpand(true);
            grid.attach(&l, 0, row.get(), 1, 1);
            grid.attach(&e, 1, row.get(), 1, 1);
            row.set(row.get() + 1);
            let e = e.clone();
            setters
                .borrow_mut()
                .push(Box::new(move |c| set(c, e.text().to_string())));
        };
        let spin = |label: &str, value: f64, min: f64, max: f64, step: f64, set: NumSetter| {
            let l = gtk4::Label::new(Some(label));
            l.set_xalign(0.0);
            let s = gtk4::SpinButton::with_range(min, max, step);
            s.set_value(value);
            s.set_hexpand(true);
            grid.attach(&l, 0, row.get(), 1, 1);
            grid.attach(&s, 1, row.get(), 1, 1);
            row.set(row.get() + 1);
            let s = s.clone();
            setters
                .borrow_mut()
                .push(Box::new(move |c| set(c, s.value())));
        };
        let check = |label: &str, value: bool, set: BoolSetter| {
            let c = gtk4::CheckButton::with_label(label);
            c.set_active(value);
            grid.attach(&c, 0, row.get(), 2, 1);
            row.set(row.get() + 1);
            let c = c.clone();
            setters
                .borrow_mut()
                .push(Box::new(move |cfg| set(cfg, c.is_active())));
        };

        heading("Font");
        entry(
            "Family",
            &current.font.family,
            Box::new(|c, v| c.font.family = v),
        );
        spin(
            "Size",
            current.font.size as f64,
            6.0,
            72.0,
            1.0,
            Box::new(|c, v| c.font.size = v as u32),
        );

        heading("Colors (#rrggbb)");
        entry(
            "Background",
            &current.colors.background,
            Box::new(|c, v| c.colors.background = v),
        );
        entry(
            "Foreground",
            &current.colors.foreground,
            Box::new(|c, v| c.colors.foreground = v),
        );
        entry(
            "Cursor",
            &current.colors.cursor,
            Box::new(|c, v| c.colors.cursor = v),
        );
        entry(
            "Selection",
            &current.colors.selection,
            Box::new(|c, v| c.colors.selection = v),
        );
        entry(
            "Accent",
            &current.colors.accent,
            Box::new(|c, v| c.colors.accent = v),
        );
        for i in 0..16 {
            let value = current.colors.palette.get(i).cloned().unwrap_or_default();
            entry(
                &format!("ANSI {i}"),
                &value,
                Box::new(move |c, v| {
                    while c.colors.palette.len() < 16 {
                        c.colors.palette.push(String::from("#000000"));
                    }
                    c.colors.palette[i] = v;
                }),
            );
        }

        heading("Geometry");
        spin(
            "Initial columns",
            current.geometry.initial_cols as f64,
            20.0,
            500.0,
            1.0,
            Box::new(|c, v| c.geometry.initial_cols = v as usize),
        );
        spin(
            "Initial rows",
            current.geometry.initial_rows as f64,
            5.0,
            200.0,
            1.0,
            Box::new(|c, v| c.geometry.initial_rows = v as usize),
        );
        spin(
            "Window width",
            current.geometry.window_width as f64,
            320.0,
            7680.0,
            10.0,
            Box::new(|c, v| c.geometry.window_width = v as i32),
        );
        spin(
            "Window height",
            current.geometry.window_height as f64,
            200.0,
            4320.0,
            10.0,
            Box::new(|c, v| c.geometry.window_height = v as i32),
        );
        spin(
            "Padding px",
            current.geometry.padding_px as f64,
            0.0,
            64.0,
            1.0,
            Box::new(|c, v| c.geometry.padding_px = v as f32),
        );
        spin(
            "Tab strip max height px",
            current.geometry.tab_strip_max_height_px as f64,
            32.0,
            600.0,
            4.0,
            Box::new(|c, v| c.geometry.tab_strip_max_height_px = v as i32),
        );

        heading("Terminal");
        spin(
            "Scrollback lines",
            current.terminal.scrollback_lines as f64,
            0.0,
            1_000_000.0,
            1000.0,
            Box::new(|c, v| c.terminal.scrollback_lines = v as usize),
        );

        heading("Sessions");
        let preferred = gtk4::DropDown::from_strings(&["tmux", "zellij", "none"]);
        preferred.set_selected(match current.sessions.preferred.as_str() {
            "zellij" => 1,
            "none" => 2,
            _ => 0,
        });
        let l = gtk4::Label::new(Some("Preferred multiplexer"));
        l.set_xalign(0.0);
        grid.attach(&l, 0, row.get(), 1, 1);
        grid.attach(&preferred, 1, row.get(), 1, 1);
        row.set(row.get() + 1);
        {
            let preferred = preferred.clone();
            setters.borrow_mut().push(Box::new(move |c| {
                c.sessions.preferred = match preferred.selected() {
                    1 => String::from("zellij"),
                    2 => String::from("none"),
                    _ => String::from("tmux"),
                };
            }));
        }
        entry(
            "Hosts (comma separated)",
            &current.sessions.hosts.join(", "),
            Box::new(|c, v| {
                c.sessions.hosts = v
                    .split(',')
                    .map(|s| s.trim().to_owned())
                    .filter(|s| !s.is_empty())
                    .collect();
            }),
        );
        entry(
            "tmux program",
            &current.sessions.tmux_program,
            Box::new(|c, v| c.sessions.tmux_program = v),
        );
        entry(
            "zellij program",
            &current.sessions.zellij_program,
            Box::new(|c, v| c.sessions.zellij_program = v),
        );
        entry(
            "ssh program",
            &current.sessions.ssh_program,
            Box::new(|c, v| c.sessions.ssh_program = v),
        );
        entry(
            "tmux socket (blank = default)",
            current.sessions.tmux_socket.as_deref().unwrap_or(""),
            Box::new(|c, v| {
                c.sessions.tmux_socket = if v.trim().is_empty() { None } else { Some(v) }
            }),
        );
        entry(
            "Remote TERM",
            &current.sessions.remote_term,
            Box::new(|c, v| c.sessions.remote_term = v),
        );
        spin(
            "Connect timeout s",
            current.sessions.connect_timeout_s as f64,
            1.0,
            60.0,
            1.0,
            Box::new(|c, v| c.sessions.connect_timeout_s = v as u32),
        );
        spin(
            "Discovery timeout ms",
            current.sessions.discovery_timeout_ms as f64,
            200.0,
            60_000.0,
            100.0,
            Box::new(|c, v| c.sessions.discovery_timeout_ms = v as u64),
        );
        spin(
            "Refresh interval s (0 = off)",
            current.sessions.refresh_interval_s as f64,
            0.0,
            3600.0,
            1.0,
            Box::new(|c, v| c.sessions.refresh_interval_s = v as u32),
        );
        check(
            "Attach or create when opening a session",
            current.sessions.create_if_missing,
            Box::new(|c, v| c.sessions.create_if_missing = v),
        );
        check(
            "Ignore this client's size (tmux)",
            current.sessions.ignore_size,
            Box::new(|c, v| c.sessions.ignore_size = v),
        );
        check(
            "Show exited zellij sessions",
            current.sessions.show_exited,
            Box::new(|c, v| c.sessions.show_exited = v),
        );
        entry(
            "Constellation root session (blank = none)",
            &current.sessions.root_session,
            Box::new(|c, v| c.sessions.root_session = v.trim().to_owned()),
        );

        heading("Peer");
        check(
            "Relay unfocused consent prompts to Haven signer",
            current.peer.relay_unfocused_consent,
            Box::new(|c, v| c.peer.relay_unfocused_consent = v),
        );
        spin(
            "Unfocused consent relay grace ms",
            current.peer.unfocused_grace_ms as f64,
            0.0,
            60_000.0,
            100.0,
            Box::new(|c, v| c.peer.unfocused_grace_ms = v as u64),
        );
        entry(
            "Default Haven SSH profile ID (blank = auto)",
            current
                .peer
                .default_haven_ssh_profile
                .as_deref()
                .unwrap_or(""),
            Box::new(|c, v| {
                c.peer.default_haven_ssh_profile = if v.trim().is_empty() {
                    None
                } else {
                    Some(v.trim().to_owned())
                };
            }),
        );

        heading("Notifications");
        spin(
            "Long command threshold s (0 = off)",
            current.notifications.long_command_secs as f64,
            0.0,
            86_400.0,
            1.0,
            Box::new(|c, v| c.notifications.long_command_secs = v as u64),
        );
        let relay_mode = gtk4::DropDown::from_strings(&["unfocused", "always", "off"]);
        relay_mode.set_selected(
            match current
                .notifications
                .peer_relay_mode
                .trim()
                .to_ascii_lowercase()
                .as_str()
            {
                "always" => 1,
                "off" => 2,
                _ => 0,
            },
        );
        let relay_lbl = gtk4::Label::new(Some("Peer notification relay mode"));
        relay_lbl.set_xalign(0.0);
        grid.attach(&relay_lbl, 0, row.get(), 1, 1);
        grid.attach(&relay_mode, 1, row.get(), 1, 1);
        row.set(row.get() + 1);
        {
            let relay_mode = relay_mode.clone();
            setters.borrow_mut().push(Box::new(move |c| {
                c.notifications.peer_relay_mode = match relay_mode.selected() {
                    1 => String::from("always"),
                    2 => String::from("off"),
                    _ => String::from("unfocused"),
                };
            }));
        }
        entry(
            "Inbound Android shade allowlist (comma separated)",
            &current.notifications.inbound_shade_allowlist.join(", "),
            Box::new(|c, v| {
                c.notifications.inbound_shade_allowlist = v
                    .split(',')
                    .map(|s| s.trim().to_owned())
                    .filter(|s| !s.is_empty())
                    .collect();
            }),
        );

        let buttons = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
        buttons.set_halign(gtk4::Align::End);
        let defaults = gtk4::Button::with_label("Reset to Defaults");
        let cancel = gtk4::Button::with_label("Cancel");
        let save = gtk4::Button::with_label("Save");
        save.add_css_class("suggested-action");
        for b in [&defaults, &cancel, &save] {
            buttons.append(b);
        }
        outer.append(&buttons);
        let note = gtk4::Label::new(Some(&format!("Writes {}", path.display())));
        note.set_xalign(0.0);
        note.add_css_class("dim-label");
        note.set_wrap(true);
        outer.append(&note);
        dialog.set_child(Some(&outer));
        dialog.set_default_widget(Some(&save));

        {
            let dialog = dialog.clone();
            cancel.connect_clicked(move |_| dialog.close());
        }
        {
            let dialog = dialog.clone();
            let path = path.clone();
            let setters = Rc::clone(&setters);
            let note = note.clone();
            save.connect_clicked(move |_| {
                let mut config = crate::config::load_or_default(&path);
                for set in setters.borrow().iter() {
                    set(&mut config);
                }
                match crate::config::to_toml(&config) {
                    Ok(text) => match std::fs::write(&path, text) {
                        Ok(()) => dialog.close(),
                        Err(err) => note.set_text(&format!("Could not write: {err}")),
                    },
                    Err(err) => note.set_text(&format!("Could not serialise: {err}")),
                }
            });
        }
        {
            let path = path.clone();
            let note = note.clone();
            defaults.connect_clicked(move |_| {
                let config = crate::config::Config::default();
                match crate::config::to_toml(&config) {
                    Ok(text) => match std::fs::write(&path, text) {
                        Ok(()) => note.set_text("Defaults written. Reopen to see them."),
                        Err(err) => note.set_text(&format!("Could not write: {err}")),
                    },
                    Err(err) => note.set_text(&format!("Could not serialise: {err}")),
                }
            });
        }
        attach_escape_to_close(&dialog);
        dialog.present();
    }

    pub fn show_command_palette(&self) {
        let Some(ctrl) = self.inner.controller.upgrade() else {
            return;
        };
        let win_id = self.inner.window_id;
        let dialog = gtk4::Window::builder()
            .transient_for(&self.inner.gtk_window)
            .modal(true)
            .title("Command Palette")
            .default_width(540)
            .default_height(380)
            .build();

        let vbox = gtk4::Box::new(gtk4::Orientation::Vertical, 6);
        vbox.set_margin_top(10);
        vbox.set_margin_bottom(10);
        vbox.set_margin_start(10);
        vbox.set_margin_end(10);

        let search = gtk4::SearchEntry::new();
        search.set_placeholder_text(Some("Type an action, slug, or shortcut…"));
        vbox.append(&search);

        let list = gtk4::ListBox::new();
        list.set_selection_mode(gtk4::SelectionMode::Single);
        let scroller = gtk4::ScrolledWindow::builder()
            .vexpand(true)
            .hexpand(true)
            .child(&list)
            .build();
        vbox.append(&scroller);
        dialog.set_child(Some(&vbox));

        let matched_ids: Rc<RefCell<Vec<ActionId>>> = Rc::new(RefCell::new(Vec::new()));

        let populate = {
            let list = list.clone();
            let matched_ids = Rc::clone(&matched_ids);
            move |q: &str| {
                while let Some(child) = list.first_child() {
                    list.remove(&child);
                }
                let results = filter_palette(q);
                let mut ids = matched_ids.borrow_mut();
                ids.clear();
                for item in results {
                    ids.push(item.id);
                    let row_box = gtk4::Box::new(gtk4::Orientation::Horizontal, 12);
                    row_box.set_margin_top(6);
                    row_box.set_margin_bottom(6);
                    row_box.set_margin_start(8);
                    row_box.set_margin_end(8);

                    let left = gtk4::Box::new(gtk4::Orientation::Vertical, 2);
                    left.set_hexpand(true);
                    let title = gtk4::Label::new(Some(item.title));
                    title.set_xalign(0.0);
                    let sub = gtk4::Label::new(Some(item.summary));
                    sub.set_xalign(0.0);
                    sub.add_css_class("dim-label");
                    left.append(&title);
                    left.append(&sub);

                    let chord = gtk4::Label::new(Some(item.human_chord));
                    row_box.append(&left);
                    row_box.append(&chord);
                    list.append(&row_box);
                }
                if let Some(first) = list.row_at_index(0) {
                    list.select_row(Some(&first));
                }
            }
        };
        populate("");

        let populate_on_change = populate.clone();
        search.connect_search_changed(move |entry| {
            populate_on_change(&entry.text());
        });

        let ctrl_for_row = Rc::downgrade(&ctrl);
        let dialog_for_row = dialog.clone();
        let ids_for_row = Rc::clone(&matched_ids);
        list.connect_row_activated(move |_, row| {
            let idx = row.index();
            let chosen = if idx >= 0 {
                ids_for_row.borrow().get(idx as usize).copied()
            } else {
                None
            };
            dialog_for_row.close();
            if let (Some(action_id), Some(c)) = (chosen, ctrl_for_row.upgrade()) {
                c.execute_action(action_id, Some(win_id));
            }
        });

        let list_for_activate = list.clone();
        search.connect_activate(move |_| {
            if let Some(row) = list_for_activate.selected_row() {
                row.activate();
            }
        });

        attach_escape_to_close(&dialog);
        dialog.present();
        search.grab_focus();
    }

    /// Lists local sessions (with constellation root nesting), SSH hosts, and
    /// paired Haven limbs (`list_sessions` + `list_bridges`), and opens or
    /// focuses the chosen item.
    pub fn show_session_picker(&self) {
        let Some(ctrl) = self.inner.controller.upgrade() else {
            return;
        };
        let win_id = self.inner.window_id;
        let dialog = gtk4::Window::builder()
            .transient_for(&self.inner.gtk_window)
            .modal(true)
            .destroy_with_parent(true)
            .title("Sessions")
            .default_width(660)
            .default_height(460)
            .build();

        let vbox = gtk4::Box::new(gtk4::Orientation::Vertical, 6);
        vbox.set_margin_top(10);
        vbox.set_margin_bottom(10);
        vbox.set_margin_start(10);
        vbox.set_margin_end(10);

        let search = gtk4::SearchEntry::new();
        search.set_placeholder_text(Some("Filter by session, host, limb, or multiplexer…"));
        vbox.append(&search);

        let list = gtk4::ListBox::new();
        list.set_selection_mode(gtk4::SelectionMode::Single);
        let scroller = gtk4::ScrolledWindow::builder()
            .vexpand(true)
            .hexpand(true)
            .child(&list)
            .build();
        vbox.append(&scroller);

        let footer = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
        let hint = gtk4::Label::new(Some("Return: new tab   Shift+Return: new window"));
        hint.set_xalign(0.0);
        hint.set_hexpand(true);
        hint.add_css_class("dim-label");
        let open_grid = gtk4::Button::with_label("Open Grid");
        open_grid.set_can_focus(false);
        let refresh = gtk4::Button::with_label("Refresh");
        refresh.set_can_focus(false);
        let status = gtk4::Label::new(None);
        status.add_css_class("dim-label");
        footer.append(&hint);
        footer.append(&status);
        footer.append(&open_grid);
        footer.append(&refresh);
        vbox.append(&footer);
        dialog.set_child(Some(&vbox));

        let picker = Rc::new(SessionPicker {
            dialog: dialog.clone(),
            list: list.clone(),
            search: search.clone(),
            status,
            rows: RefCell::new(Vec::new()),
            group_headers: RefCell::new(Vec::new()),
            item_labels: RefCell::new(Vec::new()),
            selected: RefCell::new(None),
            last_gen: Cell::new(u64::MAX),
        });
        *self.inner.session_picker.borrow_mut() = Some(Rc::clone(&picker));

        {
            let picker = Rc::clone(&picker);
            let ctrl = Rc::clone(&ctrl);
            search.connect_search_changed(move |entry| {
                let tree = ctrl.session_tree();
                let inventory = ctrl.session_inventory();
                rebuild_session_rows(&picker, &tree, &inventory, &entry.text());
            });
        }

        // Activate the selected item. Closes BEFORE dispatch so a topology
        // rebuild triggered by opening a tab finds no live picker.
        let open_selected = {
            let picker = Rc::clone(&picker);
            let ctrl = Rc::downgrade(&ctrl);
            move |mode: SessionMode| {
                let chosen = picker.selected_row_item();
                picker.dialog.close();
                let (Some(item), Some(c)) = (chosen, ctrl.upgrade()) else {
                    return;
                };
                match item.node_kind {
                    crate::session::SessionNodeKind::LocalMux
                    | crate::session::SessionNodeKind::RemoteMux => {
                        if let Some(kind) = item.mux_kind
                            && let Err(err) = c.open_session(
                                kind,
                                &item.name,
                                item.host.as_deref(),
                                mode,
                                Some(win_id),
                                None,
                            )
                        {
                            eprintln!("failed to open session: {err}");
                        }
                    }
                    crate::session::SessionNodeKind::HavenSession => {
                        if let Some(ref shared_name) = item.chosen_session_name {
                            let local_match =
                                c.session_inventory().sessions.into_iter().find(|s| {
                                    s.host.is_none() && !s.exited && &s.name == shared_name
                                });
                            if let Some(local) = local_match {
                                if let Err(err) = c.open_session(
                                    local.kind,
                                    &local.name,
                                    None,
                                    mode,
                                    Some(win_id),
                                    None,
                                ) {
                                    eprintln!("failed to open shared local session: {err}");
                                }
                                return;
                            }
                        }
                        if let (Some(peer_id), Some(sid)) =
                            (item.peer_id.as_deref(), item.haven_session_id.as_deref())
                            && let Err(err) = c.focus_haven_terminal_session(peer_id, sid)
                        {
                            eprintln!("failed to focus Haven session: {err}");
                        }
                    }
                    crate::session::SessionNodeKind::HavenBridge
                    | crate::session::SessionNodeKind::HostStatus => {}
                }
            }
        };

        {
            let open = open_selected.clone();
            list.connect_row_activated(move |_, row| {
                if row.is_activatable() {
                    open(SessionMode::Tab);
                }
            });
        }
        {
            let open = open_selected.clone();
            let list_for_key = list.clone();
            search.connect_activate(move |_| {
                if let Some(row) = list_for_key.selected_row()
                    && row.is_activatable()
                {
                    open(SessionMode::Tab);
                }
            });
        }
        {
            let open = open_selected.clone();
            let keys = gtk4::EventControllerKey::new();
            keys.connect_key_pressed(move |_, key, _, mods| {
                if (key == gdk::Key::Return || key == gdk::Key::KP_Enter)
                    && mods.contains(gdk::ModifierType::SHIFT_MASK)
                {
                    open(SessionMode::Window);
                    return glib::Propagation::Stop;
                }
                glib::Propagation::Proceed
            });
            dialog.add_controller(keys);
        }
        {
            let picker = Rc::clone(&picker);
            list.connect_row_selected(move |_, row| {
                let identity = row
                    .and_then(|r| {
                        picker
                            .rows
                            .borrow()
                            .get(r.index() as usize)
                            .and_then(|s| s.clone())
                    })
                    .map(|item| item.id);
                *picker.selected.borrow_mut() = identity;
            });
        }
        {
            let dialog_grid = dialog.clone();
            let ctrl_grid = Rc::downgrade(&ctrl);
            open_grid.connect_clicked(move |_| {
                dialog_grid.close();
                if let Some(c) = ctrl_grid.upgrade() {
                    c.execute_action(ActionId::OpenSessionGrid, Some(win_id));
                }
            });
        }
        {
            let ctrl = Rc::clone(&ctrl);
            refresh.connect_clicked(move |_| {
                let _ = ctrl.refresh_haven_limb_inventories();
                let targets = ctrl.discovery_targets_all();
                ctrl.begin_discovery(targets, true, None);
            });
        }
        {
            let inner_slot = Rc::downgrade(&self.inner);
            dialog.connect_close_request(move |_| {
                if let Some(inner) = inner_slot.upgrade() {
                    *inner.session_picker.borrow_mut() = None;
                }
                glib::Propagation::Proceed
            });
        }
        {
            let inner_slot = Rc::downgrade(&self.inner);
            dialog.connect_destroy(move |_| {
                if let Some(inner) = inner_slot.upgrade() {
                    *inner.session_picker.borrow_mut() = None;
                }
            });
        }

        attach_escape_to_close(&dialog);
        dialog.present();
        search.grab_focus();

        let tree = ctrl.session_tree();
        let inventory = ctrl.session_inventory();
        rebuild_session_rows(&picker, &tree, &inventory, "");
        let _ = ctrl.refresh_haven_limb_inventories();
        let targets = ctrl.discovery_targets_all();
        ctrl.begin_discovery(targets, true, None);
    }

    /// Called from the controller when the inventory generation moves. Reads a
    /// cloned snapshot in a short borrow, then only touches widgets, following
    /// `refresh_search_bar`. Rebuilding every tick would fight focus.
    pub fn refresh_session_picker(&self, generation: u64) {
        let picker = self.inner.session_picker.borrow().clone();
        let Some(picker) = picker else { return };
        if picker.last_gen.get() == generation {
            return;
        }
        picker.last_gen.set(generation);
        let Some(ctrl) = self.inner.controller.upgrade() else {
            return;
        };
        let tree = ctrl.session_tree();
        let inventory = ctrl.session_inventory();
        let query = picker.search.text().to_string();
        rebuild_session_rows(&picker, &tree, &inventory, &query);
    }

    pub fn show_agent_audit(&self) {
        let Some(ctrl) = self.inner.controller.upgrade() else {
            return;
        };
        let dialog = gtk4::Window::builder()
            .transient_for(&self.inner.gtk_window)
            .modal(true)
            .title("Agent Activity & Audit Trail")
            .default_width(700)
            .default_height(460)
            .build();

        let vbox = gtk4::Box::new(gtk4::Orientation::Vertical, 8);
        vbox.set_margin_top(12);
        vbox.set_margin_bottom(12);
        vbox.set_margin_start(12);
        vbox.set_margin_end(12);

        let header = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
        let grants = ctrl.session_grants();
        let grants_lbl = gtk4::Label::new(Some(&if grants.is_empty() {
            String::from("Standing session grants: none")
        } else {
            format!("Standing session grants: {}", grants.join(", "))
        }));
        grants_lbl.set_hexpand(true);
        grants_lbl.set_xalign(0.0);

        let revoke_btn = gtk4::Button::with_label("Revoke Session Grants");
        revoke_btn.set_sensitive(!grants.is_empty());
        let ctrl_weak = Rc::downgrade(&ctrl);
        let grants_lbl_clone = grants_lbl.clone();
        let revoke_btn_clone = revoke_btn.clone();
        revoke_btn.connect_clicked(move |_| {
            if let Some(c) = ctrl_weak.upgrade() {
                c.revoke_session_grants();
                grants_lbl_clone.set_text("Standing session grants: none");
                revoke_btn_clone.set_sensitive(false);
            }
        });
        header.append(&grants_lbl);
        header.append(&revoke_btn);
        vbox.append(&header);

        // Tier-3 standing policies. Proposals are inert until installed here,
        // and an installed policy is revocable here, so the operator holds the
        // wheel without leaving this dialog.
        let policy_box = gtk4::Box::new(gtk4::Orientation::Vertical, 4);
        let policy_heading = gtk4::Label::new(None);
        policy_heading.set_markup("<b>Standing policies</b>");
        policy_heading.set_xalign(0.0);
        policy_box.append(&policy_heading);
        let rows = gtk4::Box::new(gtk4::Orientation::Vertical, 2);
        policy_box.append(&rows);
        vbox.append(&policy_box);
        {
            let rows = rows.clone();
            let ctrl = Rc::downgrade(&ctrl);
            let rebuild = Rc::new(std::cell::RefCell::new(None::<Box<dyn Fn()>>));
            let rebuild_slot = Rc::clone(&rebuild);
            let f = move || {
                while let Some(child) = rows.first_child() {
                    rows.remove(&child);
                }
                let Some(c) = ctrl.upgrade() else { return };
                let pending = c.pending_policies();
                let installed = c.installed_policies();
                if pending.is_empty() && installed.is_empty() {
                    let none = gtk4::Label::new(Some("None proposed or installed."));
                    none.set_xalign(0.0);
                    none.add_css_class("dim-label");
                    rows.append(&none);
                    return;
                }
                for p in pending.iter().chain(installed.iter()) {
                    let is_pending = pending.iter().any(|q| q.id == p.id);
                    let row = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
                    let text = gtk4::Label::new(Some(&format!(
                        "{}: {} call(s)/min, verbs: {}",
                        p.description,
                        p.max_calls_per_minute,
                        p.verbs.join(", ")
                    )));
                    text.set_xalign(0.0);
                    text.set_hexpand(true);
                    text.set_ellipsize(gtk4::pango::EllipsizeMode::End);
                    row.append(&text);
                    let id = p.id;
                    if is_pending {
                        let install = gtk4::Button::with_label("Install");
                        install.add_css_class("suggested-action");
                        let weak = Rc::downgrade(&c);
                        let again = Rc::clone(&rebuild_slot);
                        install.connect_clicked(move |_| {
                            if let Some(cc) = weak.upgrade() {
                                let _ = cc.install_policy(id);
                                if let Some(f) = again.borrow().as_ref() {
                                    f();
                                }
                            }
                        });
                        row.append(&install);
                        let reject = gtk4::Button::with_label("Reject");
                        let weak = Rc::downgrade(&c);
                        let again = Rc::clone(&rebuild_slot);
                        reject.connect_clicked(move |_| {
                            if let Some(cc) = weak.upgrade() {
                                cc.revoke_policy(id);
                                if let Some(f) = again.borrow().as_ref() {
                                    f();
                                }
                            }
                        });
                        row.append(&reject);
                    } else {
                        let revoke = gtk4::Button::with_label("Revoke");
                        let weak = Rc::downgrade(&c);
                        let again = Rc::clone(&rebuild_slot);
                        revoke.connect_clicked(move |_| {
                            if let Some(cc) = weak.upgrade() {
                                cc.revoke_policy(id);
                                if let Some(f) = again.borrow().as_ref() {
                                    f();
                                }
                            }
                        });
                        row.append(&revoke);
                    }
                    rows.append(&row);
                }
            };
            *rebuild.borrow_mut() = Some(Box::new(f));
            if let Some(f) = rebuild.borrow().as_ref() {
                f();
            }
        }

        let grid = gtk4::Grid::builder()
            .column_spacing(14)
            .row_spacing(6)
            .build();

        for (col, title) in ["#", "Client", "Tier", "Verb", "Decision", "Redacted Args"]
            .iter()
            .enumerate()
        {
            let lbl = gtk4::Label::new(None);
            lbl.set_markup(&format!("<b>{title}</b>"));
            lbl.set_xalign(0.0);
            grid.attach(&lbl, col as i32, 0, 1, 1);
        }

        for (idx, entry) in ctrl.audit_entries(100).into_iter().enumerate() {
            let row = (idx + 1) as i32;
            let cols = [
                entry.seq.to_string(),
                entry.client,
                format!("{:?}", entry.tier).to_lowercase(),
                entry.verb,
                format!("{:?}", entry.decision).to_lowercase(),
                entry.detail,
            ];
            for (c_idx, text) in cols.into_iter().enumerate() {
                let lbl = gtk4::Label::new(Some(&text));
                lbl.set_xalign(0.0);
                lbl.set_selectable(true);
                grid.attach(&lbl, c_idx as i32, row, 1, 1);
            }
        }

        let scroller = gtk4::ScrolledWindow::builder()
            .child(&grid)
            .hexpand(true)
            .vexpand(true)
            .build();
        vbox.append(&scroller);
        dialog.set_child(Some(&vbox));

        attach_escape_to_close(&dialog);
        dialog.present();
    }

    pub fn show_cheatsheet(&self) {
        let dialog = gtk4::Window::builder()
            .transient_for(&self.inner.gtk_window)
            .modal(true)
            .title("Mutiny Keybind & IPC Cheatsheet")
            .default_width(660)
            .default_height(460)
            .build();

        let grid = gtk4::Grid::builder()
            .column_spacing(18)
            .row_spacing(8)
            .margin_top(14)
            .margin_bottom(14)
            .margin_start(14)
            .margin_end(14)
            .build();

        for (col, heading) in ["Action", "Shortcut", "Tier", "IPC/MCP Slug"]
            .iter()
            .enumerate()
        {
            let lbl = gtk4::Label::new(None);
            lbl.set_markup(&format!("<b>{heading}</b>"));
            lbl.set_xalign(0.0);
            grid.attach(&lbl, col as i32, 0, 1, 1);
        }

        for (i, spec) in ACTIONS.iter().enumerate() {
            let row = (i + 1) as i32;
            let cells = [
                spec.title.to_owned(),
                spec.human_chord.to_owned(),
                format!("{:?}", spec.consent_tier).to_lowercase(),
                spec.slug.to_owned(),
            ];
            for (c_idx, text) in cells.into_iter().enumerate() {
                let lbl = gtk4::Label::new(Some(&text));
                lbl.set_xalign(0.0);
                if c_idx == 3 {
                    lbl.set_selectable(true);
                }
                grid.attach(&lbl, c_idx as i32, row, 1, 1);
            }
        }

        let scroller = gtk4::ScrolledWindow::builder()
            .child(&grid)
            .hexpand(true)
            .vexpand(true)
            .build();
        dialog.set_child(Some(&scroller));

        attach_escape_to_close(&dialog);
        dialog.present();
    }

    fn wire_pairing_and_limb_ui(
        &self,
        approve_btn: &gtk4::Button,
        deny_btn: &gtk4::Button,
        limb_popover: &gtk4::Popover,
        limb_box: &gtk4::Box,
    ) {
        {
            let this = self.clone();
            approve_btn.connect_clicked(move |_| {
                this.finish_active_pairing(true);
            });
        }
        {
            let this = self.clone();
            deny_btn.connect_clicked(move |_| {
                this.finish_active_pairing(false);
            });
        }
        {
            let this = self.clone();
            let pop = limb_popover.clone();
            let container = limb_box.clone();
            limb_popover.connect_show(move |_| {
                this.rebuild_limb_popover(&pop, &container);
            });
        }
    }

    fn selected_pairing_mode(&self) -> crate::ipc::ConsentMode {
        match self.inner.pairing_mode_dropdown.selected() {
            1 => crate::ipc::ConsentMode::Paired,
            2 => crate::ipc::ConsentMode::ObserveOnly,
            _ => crate::ipc::ConsentMode::Ask,
        }
    }

    fn finish_active_pairing(&self, approve: bool) {
        let Some(pid) = self.inner.active_pairing_id.get() else {
            return;
        };
        let Some(ctrl) = self.inner.controller.upgrade() else {
            return;
        };
        let decision = if approve {
            Some(self.selected_pairing_mode())
        } else {
            None
        };
        if let Err(err) = ctrl.resolve_inbound_pairing(pid, decision) {
            eprintln!("warning: pairing resolution failed: {err}");
        }
    }

    fn rebuild_limb_popover(&self, popover: &gtk4::Popover, container: &gtk4::Box) {
        while let Some(child) = container.first_child() {
            container.remove(&child);
        }
        let Some(ctrl) = self.inner.controller.upgrade() else {
            return;
        };

        let heading = gtk4::Label::new(None);
        heading.set_markup("<b>Paired Limbs</b>");
        heading.set_xalign(0.0);
        container.append(&heading);

        if let Some(pairing_lbl) = ctrl.pairing_outbound_label() {
            let row =
                gtk4::Label::new(Some(&format!("◐ haven:{pairing_lbl} (awaiting approval…)")));
            row.set_xalign(0.0);
            container.append(&row);
        }

        let listing = ctrl.list_peers();
        if listing.outbound.is_empty() && listing.inbound.is_empty() {
            let empty = gtk4::Label::new(Some("No paired peers."));
            empty.add_css_class("dim-label");
            empty.set_xalign(0.0);
            container.append(&empty);
        }

        let has_any_outbound = !listing.outbound.is_empty();
        for out in listing.outbound {
            let row = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
            let signer_badge = if out.has_signing_key { " [signer]" } else { "" };
            let status_text = match &out.status {
                crate::peers::OutboundLimbStatus::Connected {
                    remote_version,
                    rtt_ms,
                    ..
                } => {
                    let ver = remote_version
                        .as_deref()
                        .map(|v| format!(" v{v}"))
                        .unwrap_or_default();
                    format!(
                        "● haven:{}{} ({} ms){} {}",
                        out.label, ver, rtt_ms, signer_badge, out.url
                    )
                }
                crate::peers::OutboundLimbStatus::Pairing { .. } => {
                    format!(
                        "◐ haven:{} (pairing…){} {}",
                        out.label, signer_badge, out.url
                    )
                }
                crate::peers::OutboundLimbStatus::Offline { last_error, .. } => {
                    let err = last_error.as_deref().unwrap_or("offline");
                    format!(
                        "◌ haven:{} ({}){} {}",
                        out.label,
                        truncate_label(err, 36),
                        signer_badge,
                        out.url
                    )
                }
            };
            let lbl = gtk4::Label::new(Some(&status_text));
            lbl.set_xalign(0.0);
            lbl.set_hexpand(true);
            row.append(&lbl);

            let enroll_label = if out.has_signing_key {
                "Re-enroll Signer"
            } else {
                "Enroll Consent Signer"
            };
            let enroll_btn = gtk4::Button::with_label(enroll_label);
            let ctrl_enroll = Rc::downgrade(&ctrl);
            let pop_enroll = popover.clone();
            let enroll_target = out.id.clone();
            enroll_btn.connect_clicked(move |_| {
                pop_enroll.popdown();
                if let Some(c) = ctrl_enroll.upgrade()
                    && let Err(e) = c.begin_enroll_haven_signer(enroll_target.clone(), None)
                {
                    eprintln!("warning: enroll signer failed: {e}");
                }
            });
            row.append(&enroll_btn);

            let revoke_btn = gtk4::Button::with_label("Revoke");
            revoke_btn.add_css_class("destructive-action");
            let ctrl_weak = Rc::downgrade(&ctrl);
            let pop_clone = popover.clone();
            let target_id = out.id.clone();
            revoke_btn.connect_clicked(move |_| {
                pop_clone.popdown();
                if let Some(c) = ctrl_weak.upgrade()
                    && let Err(e) = c.revoke_peer(&target_id)
                {
                    eprintln!("warning: revoke failed: {e}");
                }
            });
            row.append(&revoke_btn);
            container.append(&row);

            if out.rate_limited_count > 0 {
                let rl_note = gtk4::Label::new(Some(&format!(
                    "  Outbound notifications rate-limited: {}",
                    out.rate_limited_count
                )));
                rl_note.add_css_class("dim-label");
                rl_note.set_xalign(0.0);
                container.append(&rl_note);
            }
            if let Some(shade_note) = &out.shade_status {
                let note_lbl = gtk4::Label::new(Some(&format!("  {shade_note}")));
                note_lbl.add_css_class("dim-label");
                note_lbl.set_xalign(0.0);
                note_lbl.set_wrap(true);
                note_lbl.set_max_width_chars(56);
                container.append(&note_lbl);
            }
        }

        for inb in listing.inbound {
            let row = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
            let mode = crate::http_mcp::mode_label(inb.consent_mode);
            let eph = if inb.ephemeral { ", cli" } else { "" };
            let lbl = gtk4::Label::new(Some(&format!(
                "● inbound:{} ({}, {}{})",
                inb.label, inb.client_name, mode, eph
            )));
            lbl.set_xalign(0.0);
            lbl.set_hexpand(true);
            row.append(&lbl);

            let revoke_btn = gtk4::Button::with_label("Revoke");
            revoke_btn.add_css_class("destructive-action");
            let ctrl_weak = Rc::downgrade(&ctrl);
            let pop_clone = popover.clone();
            let target_id = inb.id.clone();
            revoke_btn.connect_clicked(move |_| {
                pop_clone.popdown();
                if let Some(c) = ctrl_weak.upgrade()
                    && let Err(e) = c.revoke_peer(&target_id)
                {
                    eprintln!("warning: revoke failed: {e}");
                }
            });
            row.append(&revoke_btn);
            container.append(&row);
        }

        let alerts = ctrl.list_peer_alerts();
        let unread_count = alerts.iter().filter(|a| a.unread).count();
        container.append(&gtk4::Separator::new(gtk4::Orientation::Horizontal));

        let alerts_header = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
        let alerts_title = gtk4::Label::new(None);
        if unread_count > 0 {
            alerts_title.set_markup(&format!("<b>Haven Alerts ({unread_count} unread)</b>"));
        } else {
            alerts_title.set_markup("<b>Haven Alerts</b>");
        }
        alerts_title.set_xalign(0.0);
        alerts_title.set_hexpand(true);
        alerts_header.append(&alerts_title);

        if !alerts.is_empty() {
            let clear_btn = gtk4::Button::with_label("Clear / Mark Read");
            clear_btn.set_sensitive(unread_count > 0);
            let this_clear = self.clone();
            let ctrl_clear = Rc::downgrade(&ctrl);
            let pop_clear = popover.clone();
            let box_clear = container.clone();
            clear_btn.connect_clicked(move |_| {
                if let Some(c) = ctrl_clear.upgrade() {
                    c.clear_peer_alerts();
                    this_clear.rebuild_limb_popover(&pop_clear, &box_clear);
                }
            });
            alerts_header.append(&clear_btn);
        }
        container.append(&alerts_header);

        if alerts.is_empty() {
            let none_lbl = gtk4::Label::new(Some("No Haven alerts."));
            none_lbl.add_css_class("dim-label");
            none_lbl.set_xalign(0.0);
            container.append(&none_lbl);
        } else {
            for alert in alerts.iter().take(8) {
                let tag = alert.package_name.as_deref().unwrap_or(match alert.kind {
                    crate::peers::PeerAlertKind::LimbOffline => "offline",
                    crate::peers::PeerAlertKind::LimbReconnected => "reconnected",
                    crate::peers::PeerAlertKind::AuthPrompt => "ssh-auth",
                    crate::peers::PeerAlertKind::PendingConsent => "consent",
                    crate::peers::PeerAlertKind::PeerNotice => "notice",
                    crate::peers::PeerAlertKind::AndroidShade => "shade",
                });
                let summary_raw = if alert.body.is_empty() {
                    format!("[{}:{}] {}", alert.peer_label, tag, alert.title)
                } else {
                    format!(
                        "[{}:{}] {}: {}",
                        alert.peer_label,
                        tag,
                        alert.title,
                        truncate_label(&alert.body, 56)
                    )
                };
                let item_lbl = gtk4::Label::new(None);
                item_lbl.set_xalign(0.0);
                item_lbl.set_ellipsize(gtk4::pango::EllipsizeMode::End);
                item_lbl.set_max_width_chars(64);
                if alert.unread {
                    item_lbl.set_markup(&format!(
                        "<b>• {}</b>",
                        glib::markup_escape_text(&summary_raw)
                    ));
                } else {
                    item_lbl.set_text(&format!("  {summary_raw}"));
                    item_lbl.add_css_class("dim-label");
                }
                container.append(&item_lbl);
            }
        }

        container.append(&gtk4::Separator::new(gtk4::Orientation::Horizontal));
        if has_any_outbound {
            let handoff_btn = gtk4::Button::with_label("Handoff Active Session (Ctrl+Shift+H)");
            let ctrl_handoff = Rc::downgrade(&ctrl);
            let pop_handoff = popover.clone();
            let win_id = self.inner.window_id;
            handoff_btn.connect_clicked(move |_| {
                pop_handoff.popdown();
                if let Some(c) = ctrl_handoff.upgrade() {
                    c.execute_action(ActionId::HandoffToPeer, Some(win_id));
                }
            });
            container.append(&handoff_btn);

            let present_btn = gtk4::Button::with_label("Present to Haven…");
            let this_present = self.clone();
            let pop_present = popover.clone();
            present_btn.connect_clicked(move |_| {
                pop_present.popdown();
                this_present.show_present_to_peer_dialog();
            });
            container.append(&present_btn);

            let totp_btn = gtk4::Button::with_label("Insert Haven TOTP Code (Ctrl+Shift+Y)");
            let ctrl_totp = Rc::downgrade(&ctrl);
            let pop_totp = popover.clone();
            let win_id = self.inner.window_id;
            totp_btn.connect_clicked(move |_| {
                pop_totp.popdown();
                if let Some(c) = ctrl_totp.upgrade() {
                    c.execute_action(ActionId::InsertPeerTotp, Some(win_id));
                }
            });
            container.append(&totp_btn);
        }
        let pair_btn = gtk4::Button::with_label("Pair Haven Limb…");
        let this = self.clone();
        let pop_clone = popover.clone();
        pair_btn.connect_clicked(move |_| {
            pop_clone.popdown();
            this.show_pair_haven_dialog();
        });
        container.append(&pair_btn);
    }

    pub fn show_pair_haven_dialog(&self) {
        let Some(ctrl) = self.inner.controller.upgrade() else {
            return;
        };
        let dialog = gtk4::Window::builder()
            .transient_for(&self.inner.gtk_window)
            .modal(true)
            .destroy_with_parent(true)
            .title("Pair Haven Limb")
            .default_width(460)
            .build();

        let vbox = gtk4::Box::new(gtk4::Orientation::Vertical, 8);
        vbox.set_margin_top(12);
        vbox.set_margin_bottom(12);
        vbox.set_margin_start(12);
        vbox.set_margin_end(12);

        let grid = gtk4::Grid::builder()
            .column_spacing(10)
            .row_spacing(8)
            .build();

        let lbl_name = gtk4::Label::new(Some("Label"));
        lbl_name.set_xalign(0.0);
        let entry_name = gtk4::Entry::new();
        entry_name.set_text("haven");
        entry_name.set_hexpand(true);
        grid.attach(&lbl_name, 0, 0, 1, 1);
        grid.attach(&entry_name, 1, 0, 1, 1);

        let lbl_url = gtk4::Label::new(Some("Haven MCP URL"));
        lbl_url.set_xalign(0.0);
        let entry_url = gtk4::Entry::new();
        entry_url.set_text("http://127.0.0.1:8740/mcp");
        entry_url.set_hexpand(true);
        entry_url.set_activates_default(true);
        grid.attach(&lbl_url, 0, 1, 1, 1);
        grid.attach(&entry_url, 1, 1, 1, 1);

        let lbl_tok = gtk4::Label::new(Some("Bearer Token (optional)"));
        lbl_tok.set_xalign(0.0);
        let entry_tok = gtk4::Entry::new();
        entry_tok.set_placeholder_text(Some("Leave empty for interactive tap-to-pair"));
        entry_tok.set_visibility(false);
        entry_tok.set_hexpand(true);
        grid.attach(&lbl_tok, 0, 2, 1, 1);
        grid.attach(&entry_tok, 1, 2, 1, 1);

        vbox.append(&grid);

        let enroll_check = gtk4::CheckButton::with_label("Enroll consent signer after pairing");
        enroll_check.set_active(false);
        vbox.append(&enroll_check);

        let hint = gtk4::Label::new(Some(
            "Use a loopback SSH forward (127.0.0.1) or an encrypted WireGuard/Yggdrasil tunnel address.",
        ));
        hint.add_css_class("dim-label");
        hint.set_xalign(0.0);
        hint.set_wrap(true);
        vbox.append(&hint);

        let buttons = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
        buttons.set_halign(gtk4::Align::End);
        let cancel = gtk4::Button::with_label("Cancel");
        let pair = gtk4::Button::with_label("Pair");
        pair.add_css_class("suggested-action");
        buttons.append(&cancel);
        buttons.append(&pair);
        vbox.append(&buttons);

        dialog.set_child(Some(&vbox));
        dialog.set_default_widget(Some(&pair));

        {
            let d = dialog.clone();
            cancel.connect_clicked(move |_| d.close());
        }
        {
            let d = dialog.clone();
            let ctrl_weak = Rc::downgrade(&ctrl);
            pair.connect_clicked(move |_| {
                let label = entry_name.text().to_string();
                let url = entry_url.text().to_string();
                let enroll_signer = enroll_check.is_active();
                let tok = {
                    let raw = entry_tok.text().to_string();
                    if raw.trim().is_empty() {
                        None
                    } else {
                        Some(raw)
                    }
                };
                d.close();
                if let Some(c) = ctrl_weak.upgrade()
                    && let Err(err) = c.begin_pair_haven(label, url, tok, enroll_signer, None)
                {
                    eprintln!("warning: could not start Haven pairing: {err}");
                }
            });
        }

        attach_escape_to_close(&dialog);
        dialog.present();
    }

    pub fn show_present_to_peer_dialog(&self) {
        let Some(ctrl) = self.inner.controller.upgrade() else {
            return;
        };
        let initial_selection = {
            let ws = ctrl.workspace.borrow();
            ws.window(self.inner.window_id)
                .and_then(|w| w.active_pane())
                .and_then(|pid| ws.pane(pid).and_then(|e| e.selection_text()))
                .unwrap_or_default()
        };

        let dialog = gtk4::Window::builder()
            .transient_for(&self.inner.gtk_window)
            .modal(true)
            .destroy_with_parent(true)
            .title("Present to Haven")
            .default_width(480)
            .build();

        let vbox = gtk4::Box::new(gtk4::Orientation::Vertical, 8);
        vbox.set_margin_top(12);
        vbox.set_margin_bottom(12);
        vbox.set_margin_start(12);
        vbox.set_margin_end(12);

        let mode_drop = gtk4::DropDown::from_strings(&[
            "URL or File Path (Media / Web / PDF)",
            "Clipboard Text",
        ]);
        mode_drop.set_selected(if initial_selection.is_empty() { 0 } else { 1 });
        vbox.append(&mode_drop);

        let target_entry = gtk4::Entry::new();
        target_entry.set_placeholder_text(Some(
            "https://example.com, /abs/path/chart.png, or text to copy",
        ));
        target_entry.set_text(&initial_selection);
        target_entry.set_activates_default(true);
        target_entry.set_hexpand(true);
        vbox.append(&target_entry);

        let caption_entry = gtk4::Entry::new();
        caption_entry.set_placeholder_text(Some("Optional caption"));
        caption_entry.set_hexpand(true);
        vbox.append(&caption_entry);

        let buttons = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
        buttons.set_halign(gtk4::Align::End);
        let cancel = gtk4::Button::with_label("Cancel");
        let send = gtk4::Button::with_label("Send to Haven");
        send.add_css_class("suggested-action");
        buttons.append(&cancel);
        buttons.append(&send);
        vbox.append(&buttons);

        dialog.set_child(Some(&vbox));
        dialog.set_default_widget(Some(&send));

        {
            let d = dialog.clone();
            cancel.connect_clicked(move |_| d.close());
        }
        {
            let d = dialog.clone();
            let ctrl_weak = Rc::downgrade(&ctrl);
            let target_for_send = target_entry.clone();
            send.connect_clicked(move |_| {
                let raw = target_for_send.text().to_string();
                let cap_raw = caption_entry.text().to_string();
                let caption = if cap_raw.trim().is_empty() {
                    None
                } else {
                    Some(cap_raw)
                };
                let selected_mode = mode_drop.selected();
                d.close();
                if raw.trim().is_empty() {
                    return;
                }
                let Some(c) = ctrl_weak.upgrade() else {
                    return;
                };
                let res = if selected_mode == 1 {
                    c.begin_present_to_peer(None, None, None, Some(raw), false, None, caption, None)
                } else if raw.trim().starts_with("http://") || raw.trim().starts_with("https://") {
                    c.begin_present_to_peer(
                        None,
                        Some(raw.trim().to_owned()),
                        None,
                        None,
                        false,
                        None,
                        caption,
                        None,
                    )
                } else {
                    c.begin_present_to_peer(
                        None,
                        None,
                        Some(std::path::PathBuf::from(raw.trim())),
                        None,
                        false,
                        None,
                        caption,
                        None,
                    )
                };
                if let Err(err) = res {
                    eprintln!("warning: present-to-peer: {err}");
                }
            });
        }

        attach_escape_to_close(&dialog);
        dialog.present();
        target_entry.grab_focus();
    }

    pub(crate) fn show_totp_chooser_dialog(
        &self,
        peer_id: &str,
        target_pane: PaneId,
        secrets: &[crate::peer_client::TotpSecretItem],
    ) {
        let Some(ctrl) = self.inner.controller.upgrade() else {
            return;
        };
        let dialog = gtk4::Window::builder()
            .transient_for(&self.inner.gtk_window)
            .modal(true)
            .destroy_with_parent(true)
            .title("Insert Haven TOTP Code")
            .default_width(440)
            .default_height(320)
            .build();

        let vbox = gtk4::Box::new(gtk4::Orientation::Vertical, 8);
        vbox.set_margin_top(10);
        vbox.set_margin_bottom(10);
        vbox.set_margin_start(10);
        vbox.set_margin_end(10);

        let prompt = gtk4::Label::new(Some(
            "Select an authenticator secret on Haven to type into the active pane:",
        ));
        prompt.set_xalign(0.0);
        prompt.set_wrap(true);
        vbox.append(&prompt);

        let list = gtk4::ListBox::new();
        list.set_selection_mode(gtk4::SelectionMode::Single);
        let ids: Vec<String> = secrets.iter().map(|s| s.id.clone()).collect();
        for item in secrets {
            let sub = match (&item.issuer, &item.account_name) {
                (Some(iss), Some(acc)) => format!("{iss} ({acc})"),
                (Some(iss), None) => iss.clone(),
                (None, Some(acc)) => acc.clone(),
                (None, None) => String::new(),
            };
            list.append(&session_row(&item.label, &sub, "", true));
        }
        if let Some(first) = list.row_at_index(0) {
            list.select_row(Some(&first));
        }

        let scroller = gtk4::ScrolledWindow::builder()
            .vexpand(true)
            .hexpand(true)
            .child(&list)
            .build();
        vbox.append(&scroller);
        dialog.set_child(Some(&vbox));

        let peer_owned = peer_id.to_owned();
        let ctrl_weak = Rc::downgrade(&ctrl);
        let dialog_for_row = dialog.clone();
        list.connect_row_activated(move |_, row| {
            let idx = row.index();
            let chosen = if idx >= 0 {
                ids.get(idx as usize).cloned()
            } else {
                None
            };
            dialog_for_row.close();
            if let (Some(secret_id), Some(c)) = (chosen, ctrl_weak.upgrade())
                && let Err(err) =
                    c.request_haven_totp_code(peer_owned.clone(), secret_id, target_pane)
            {
                eprintln!("warning: could not request Haven TOTP code: {err}");
            }
        });

        attach_escape_to_close(&dialog);
        dialog.present();
        list.grab_focus();
    }

    pub fn is_pairing_bar_visible(&self) -> bool {
        self.inner.pairing_bar.is_visible()
    }

    pub fn pairing_bar_text(&self) -> String {
        self.inner.pairing_label.text().to_string()
    }

    pub fn select_pairing_mode_index(&self, idx: u32) {
        self.inner.pairing_mode_dropdown.set_selected(idx);
    }

    pub fn is_consent_bar_visible(&self) -> bool {
        self.inner.consent_bar.is_visible()
    }

    pub fn consent_bar_text(&self) -> String {
        self.inner.consent_label.text().to_string()
    }

    pub fn is_send_to_haven_visible(&self) -> bool {
        self.inner.send_haven_btn.is_visible()
    }

    pub fn click_send_to_haven(&self) {
        self.inner.send_haven_btn.emit_clicked();
    }

    pub fn is_haven_auth_bar_visible(&self) -> bool {
        self.inner.haven_auth_bar.is_visible()
    }

    pub fn haven_auth_bar_text(&self) -> String {
        self.inner.haven_auth_label.text().to_string()
    }

    pub fn haven_auth_entry_text(&self) -> String {
        self.inner.haven_auth_entry.text().to_string()
    }

    pub fn unlock_haven_auth_bar_via_ui(&self, password: &str, remember: bool) {
        self.inner.haven_auth_entry.set_text(password);
        self.inner.haven_auth_remember.set_active(remember);
        self.inner.haven_auth_unlock_btn.emit_clicked();
    }

    pub fn dismiss_haven_auth_bar_via_ui(&self) {
        self.inner.haven_auth_dismiss_btn.emit_clicked();
    }

    pub fn is_limb_pill_visible(&self) -> bool {
        self.inner.limb_pill_btn.is_visible()
    }

    pub fn limb_pill_label(&self) -> String {
        self.inner
            .limb_pill_btn
            .label()
            .map(|s| s.to_string())
            .unwrap_or_default()
    }

    pub fn popup_limb_popover(&self) {
        self.inner.limb_pill_btn.popup();
    }
}

/// A Settings field's write-back: read the widget, set the config key.
type ConfigSetter = Box<dyn Fn(&mut crate::config::Config)>;
/// A Settings field's typed write-back, before the widget is captured.
type TextSetter = Box<dyn Fn(&mut crate::config::Config, String)>;
type NumSetter = Box<dyn Fn(&mut crate::config::Config, f64)>;
type BoolSetter = Box<dyn Fn(&mut crate::config::Config, bool)>;

/// Live state of an open session picker. Stored on the window so the controller
/// can refresh it when discovery results or Haven limb inventories land, and
/// cleared on close so a stale dialog cannot open sessions into a dead window.
struct SessionPicker {
    dialog: gtk4::Window,
    list: gtk4::ListBox,
    search: gtk4::SearchEntry,
    status: gtk4::Label,
    /// One entry per ListBox row, `None` for group-header and status rows, so
    /// row indices line up with the GTK ListBox children.
    rows: RefCell<Vec<Option<crate::session::SessionTreeItem>>>,
    group_headers: RefCell<Vec<String>>,
    item_labels: RefCell<Vec<String>>,
    selected: RefCell<Option<String>>,
    last_gen: Cell<u64>,
}

impl SessionPicker {
    fn selected_row_item(&self) -> Option<crate::session::SessionTreeItem> {
        let row = self.list.selected_row()?;
        let index = row.index();
        if index < 0 {
            return None;
        }
        self.rows
            .borrow()
            .get(index as usize)
            .and_then(|s| s.clone())
    }
}

/// Rebuilds the hierarchical session tree rows from a snapshot. Rows are built
/// first and the index borrow is dropped before any `append`/`select_row`, so
/// the selection handler never hits a double borrow.
fn rebuild_session_rows(
    picker: &Rc<SessionPicker>,
    tree: &crate::session::SessionTree,
    inventory: &MuxInventory,
    query: &str,
) {
    while let Some(child) = picker.list.first_child() {
        picker.list.remove(&child);
    }
    let needle = query.trim().to_ascii_lowercase();
    let terms: Vec<&str> = needle.split_whitespace().collect();

    let mut entries: Vec<Option<crate::session::SessionTreeItem>> = Vec::new();
    let mut headers_out: Vec<String> = Vec::new();
    let mut labels_out: Vec<String> = Vec::new();
    let mut widgets: Vec<gtk4::ListBoxRow> = Vec::new();

    for group in &tree.groups {
        let mut group_widgets = Vec::new();
        let mut group_entries = Vec::new();
        let mut group_labels = Vec::new();

        for item in &group.items {
            let hay = format!(
                "{} {} {} {}",
                group.title, item.name, item.subtitle, item.detail
            )
            .to_ascii_lowercase();
            if !terms.is_empty() && !terms.iter().all(|t| hay.contains(t)) {
                continue;
            }
            let display_name = if item.depth > 0 {
                format!("  ↳ {}", item.name)
            } else {
                item.name.clone()
            };
            group_labels.push(format!("{} | {}", display_name, item.detail));
            group_widgets.push(session_item_row(
                &display_name,
                &item.subtitle,
                &item.detail,
                item.depth,
                item.activatable,
            ));
            group_entries.push(item.activatable.then(|| item.clone()));
        }

        if group_widgets.is_empty() {
            continue;
        }

        let header_text = format!("{} ({})", group.title, group.rollup.summary_badge());
        headers_out.push(header_text.clone());
        widgets.push(session_group_header_row(&header_text));
        entries.push(None);
        widgets.extend(group_widgets);
        entries.extend(group_entries);
        labels_out.extend(group_labels);
    }

    {
        let mut rows = picker.rows.borrow_mut();
        rows.clear();
        rows.extend(entries);
    }
    *picker.group_headers.borrow_mut() = headers_out;
    *picker.item_labels.borrow_mut() = labels_out;

    for widget in &widgets {
        picker.list.append(widget);
    }

    // Restore selection by stable item ID, falling back to the first
    // activatable row (never a non-selectable group header).
    let wanted = picker.selected.borrow().clone();
    let index = wanted.and_then(|identity| {
        picker
            .rows
            .borrow()
            .iter()
            .position(|entry| entry.as_ref().map(|i| &i.id) == Some(&identity))
    });
    let first_activatable = picker
        .rows
        .borrow()
        .iter()
        .position(|entry| entry.is_some());
    let target = index
        .or(first_activatable)
        .and_then(|i| picker.list.row_at_index(i as i32));
    if let Some(row) = target {
        picker.list.select_row(Some(&row));
    }

    let live: usize = tree.groups.iter().map(|g| g.rollup.total_count).sum();
    let failed = inventory
        .hosts
        .iter()
        .filter(|h| h.state == HostState::Error)
        .count();
    let scanning = inventory
        .hosts
        .iter()
        .filter(|h| h.state == HostState::Scanning)
        .count();
    let mut status = format!("{live} session{}", if live == 1 { "" } else { "s" });
    if scanning > 0 {
        status.push_str(&format!(" · scanning {scanning}"));
    }
    if failed > 0 {
        status.push_str(&format!(" · {failed} unavailable"));
    }
    if inventory.partial {
        status.push_str(" · partial");
    }
    picker.status.set_text(&status);
}

fn session_group_header_row(text: &str) -> gtk4::ListBoxRow {
    let lbl = gtk4::Label::new(None);
    lbl.set_markup(&format!("<b>{}</b>", glib::markup_escape_text(text)));
    lbl.set_xalign(0.0);
    lbl.set_margin_top(8);
    lbl.set_margin_bottom(2);
    lbl.set_margin_start(8);
    lbl.set_margin_end(8);
    let row = gtk4::ListBoxRow::new();
    row.set_child(Some(&lbl));
    row.set_activatable(false);
    row.set_selectable(false);
    row
}

fn session_item_row(
    title: &str,
    detail: &str,
    right: &str,
    depth: u8,
    activatable: bool,
) -> gtk4::ListBoxRow {
    let row = session_row(title, detail, right, activatable);
    if depth > 0
        && let Some(child) = row.child()
    {
        child.set_margin_start(8 + i32::from(depth) * 14);
    }
    row
}

fn session_row(title: &str, detail: &str, right: &str, activatable: bool) -> gtk4::ListBoxRow {
    let row_box = gtk4::Box::new(gtk4::Orientation::Horizontal, 12);
    row_box.set_margin_top(6);
    row_box.set_margin_bottom(6);
    row_box.set_margin_start(8);
    row_box.set_margin_end(8);

    let left = gtk4::Box::new(gtk4::Orientation::Vertical, 2);
    left.set_hexpand(true);
    let name = gtk4::Label::new(Some(title));
    name.set_xalign(0.0);
    name.set_ellipsize(gtk4::pango::EllipsizeMode::End);
    let sub = gtk4::Label::new(Some(detail));
    sub.set_xalign(0.0);
    sub.set_ellipsize(gtk4::pango::EllipsizeMode::End);
    sub.add_css_class("dim-label");
    left.append(&name);
    left.append(&sub);
    row_box.append(&left);
    if !right.is_empty() {
        let host = gtk4::Label::new(Some(right));
        host.add_css_class("dim-label");
        row_box.append(&host);
    }

    let row = gtk4::ListBoxRow::new();
    row.set_child(Some(&row_box));
    row.set_activatable(activatable);
    row.set_selectable(activatable);
    row
}

fn attach_escape_to_close(dialog: &gtk4::Window) {
    let key_escape = gtk4::EventControllerKey::new();
    let dialog_for_esc = dialog.clone();
    key_escape.connect_key_pressed(move |_, key, _, _| {
        if key == gdk::Key::Escape {
            dialog_for_esc.close();
            return glib::Propagation::Stop;
        }
        glib::Propagation::Proceed
    });
    dialog.add_controller(key_escape);
}

const TAB_DND_MIME: &str = "application/x-mutiny-tab";

fn attach_tab_drag_source(btn: &gtk4::Button, tid: TabId, ctrl: &Rc<AppController>) {
    let drag = gtk4::DragSource::new();
    drag.set_actions(gdk::DragAction::MOVE);
    let dragged_id = tid.0;
    drag.connect_prepare(move |_, _, _| {
        let val_provider = gdk::ContentProvider::for_value(&glib::Value::from(dragged_id));
        let mime_provider = gdk::ContentProvider::for_bytes(
            TAB_DND_MIME,
            &glib::Bytes::from(&dragged_id.to_le_bytes()),
        );
        Some(gdk::ContentProvider::new_union(&[
            val_provider,
            mime_provider,
        ]))
    });
    let btn_for_icon = btn.clone();
    drag.connect_drag_begin(move |src, _| {
        let paintable = gtk4::WidgetPaintable::new(Some(&btn_for_icon));
        src.set_icon(Some(&paintable), 12, 12);
    });
    // Tear-off: releasing the dragged tab outside all Mutiny drop targets fires
    // `DragCancelReason::NoTarget` (pressing Escape fires `UserCancelled`, which
    // returns `false` and cancels without detaching). Deferring `detach_tab` onto
    // an idle callback lets GTK finish unwinding `drag-cancel` / `drag-end` on
    // `btn` before `rebuild_from_model` replaces the source tab strip.
    let ctrl_cancel = Rc::downgrade(ctrl);
    drag.connect_drag_cancel(move |_, _, reason| {
        if reason != gdk::DragCancelReason::NoTarget {
            return false;
        }
        let Some(c) = ctrl_cancel.upgrade() else {
            return false;
        };
        let can_tear_off = c
            .workspace
            .borrow()
            .windows()
            .iter()
            .find(|w| w.tabs.iter().any(|t| t.id == tid))
            .is_some_and(|w| !w.quick && w.tabs.len() > 1);
        if !can_tear_off {
            return false;
        }
        glib::idle_add_local_once(move || {
            let _ = c.detach_tab(tid);
        });
        true
    });
    btn.add_controller(drag);
}

fn attach_tab_drop_target<W, F>(widget: &W, on_drop: F)
where
    W: IsA<gtk4::Widget>,
    F: Fn(TabId) -> bool + 'static,
{
    let on_drop = Rc::new(on_drop);
    let formats = gdk::ContentFormatsBuilder::new()
        .add_type(glib::Type::U64)
        .add_mime_type(TAB_DND_MIME)
        .build();
    let target = gtk4::DropTargetAsync::new(Some(formats), gdk::DragAction::MOVE);
    target.connect_drop(move |_, drop, _, _| {
        // Fast in-process path when GDK preserves the local GdkDrag object.
        if let Some(local_drag) = drop.drag()
            && let Ok(val) = local_drag.content().value(glib::Type::U64)
            && let Ok(raw_id) = val.get::<u64>()
        {
            let ok = on_drop(TabId(raw_id));
            drop.finish(if ok {
                gdk::DragAction::MOVE
            } else {
                gdk::DragAction::empty()
            });
            return ok;
        }
        // Wayland/X11 compositor pipe fallback over `application/x-mutiny-tab`.
        if !drop.formats().contain_mime_type(TAB_DND_MIME) {
            drop.finish(gdk::DragAction::empty());
            return false;
        }
        let drop_cl = drop.clone();
        let on_drop_cl = Rc::clone(&on_drop);
        drop.read_async(
            &[TAB_DND_MIME],
            glib::Priority::DEFAULT,
            None::<&gtk4::gio::Cancellable>,
            move |res| {
                let ok = match res {
                    Ok((stream, _)) => stream
                        .read_bytes(8, None::<&gtk4::gio::Cancellable>)
                        .ok()
                        .and_then(|b| <[u8; 8]>::try_from(b.as_ref()).ok())
                        .is_some_and(|arr| on_drop_cl(TabId(u64::from_le_bytes(arr)))),
                    Err(_) => false,
                };
                drop_cl.finish(if ok {
                    gdk::DragAction::MOVE
                } else {
                    gdk::DragAction::empty()
                });
            },
        );
        true
    });
    widget.add_controller(target);
}

/// Tight padding for the glyph-only toolbar buttons, scalable session tab
/// buttons, and subtle drop-target highlight when dragging tabs.
fn ensure_compact_toolbar_css() {
    use std::cell::Cell;
    thread_local! { static INSTALLED: Cell<bool> = const { Cell::new(false) }; }
    if INSTALLED.get() {
        return;
    }
    let Some(display) = gtk4::gdk::Display::default() else {
        return;
    };
    let provider = gtk4::CssProvider::new();
    provider.load_from_string(
        "button.compact-toolbar { padding-left: 6px; padding-right: 6px; \
         padding-top: 2px; padding-bottom: 2px; min-width: 0; } \
         button.session-tab { min-width: 0; min-height: 0; } \
         button.session-tab-compact { padding-left: 4px; padding-right: 4px; \
         padding-top: 1px; padding-bottom: 1px; } \
         button.session-tab-large { padding-left: 10px; padding-right: 10px; \
         padding-top: 4px; padding-bottom: 4px; } \
         label.tab-subtitle { font-size: 0.82em; } \
         *:drop(active) { outline: 1px solid @theme_selected_bg_color; outline-offset: -1px; }",
    );
    gtk4::style_context_add_provider_for_display(
        &display,
        &provider,
        gtk4::STYLE_PROVIDER_PRIORITY_APPLICATION,
    );
    INSTALLED.set(true);
}

fn apply_session_tab_scale(
    btn: &gtk4::Button,
    title_label: &gtk4::Label,
    sub_label: &gtk4::Label,
    term_font_pt: u32,
) {
    let term_pt = f64::from(term_font_pt);
    let title_pt = (term_pt * (10.0 / 11.0)).clamp(5.5, 26.0);
    let sub_pt = (title_pt * 0.80).clamp(4.5, 20.0);

    let title_attrs = gtk4::pango::AttrList::new();
    title_attrs.insert(gtk4::pango::AttrSize::new(
        (title_pt * f64::from(gtk4::pango::SCALE)).round() as i32,
    ));
    title_label.set_attributes(Some(&title_attrs));

    let sub_attrs = gtk4::pango::AttrList::new();
    sub_attrs.insert(gtk4::pango::AttrSize::new(
        (sub_pt * f64::from(gtk4::pango::SCALE)).round() as i32,
    ));
    sub_label.set_attributes(Some(&sub_attrs));

    btn.add_css_class("session-tab");
    if term_font_pt <= 9 {
        btn.add_css_class("session-tab-compact");
        btn.remove_css_class("session-tab-large");
    } else if term_font_pt >= 15 {
        btn.add_css_class("session-tab-large");
        btn.remove_css_class("session-tab-compact");
    } else {
        btn.remove_css_class("session-tab-compact");
        btn.remove_css_class("session-tab-large");
    }
}

fn make_action_button(label: &str, action_name: &str, tooltip: &str) -> gtk4::Button {
    let btn = gtk4::Button::with_label(label);
    btn.set_action_name(Some(action_name));
    btn.set_tooltip_text(Some(tooltip));
    btn.set_can_focus(false);
    btn
}

pub fn build_split_widget(
    node: &SplitNode,
    ctrl: &Rc<AppController>,
    views_out: &mut HashMap<PaneId, TerminalView>,
    tab_id: TabId,
    path: &[bool],
) -> gtk4::Widget {
    match node {
        SplitNode::Leaf(pane_id) => {
            let view = TerminalView::new(*pane_id, ctrl);
            views_out.insert(*pane_id, view.clone());
            view.upcast::<gtk4::Widget>()
        }
        SplitNode::Split {
            axis,
            ratio,
            first,
            second,
        } => {
            let orientation = match axis {
                SplitAxis::Horizontal => gtk4::Orientation::Horizontal,
                SplitAxis::Vertical => gtk4::Orientation::Vertical,
            };
            let paned = gtk4::Paned::new(orientation);
            paned.set_wide_handle(true);
            paned.set_hexpand(true);
            paned.set_vexpand(true);
            let mut path_first = path.to_vec();
            path_first.push(false);
            let mut path_second = path.to_vec();
            path_second.push(true);
            let child1 = build_split_widget(first, ctrl, views_out, tab_id, &path_first);
            let child2 = build_split_widget(second, ctrl, views_out, tab_id, &path_second);
            paned.set_start_child(Some(&child1));
            paned.set_end_child(Some(&child2));
            paned.set_resize_start_child(true);
            paned.set_resize_end_child(true);
            paned.set_shrink_start_child(false);
            paned.set_shrink_end_child(false);

            let horizontal = *axis == SplitAxis::Horizontal;
            let total_of = move |p: &gtk4::Paned| {
                if horizontal { p.width() } else { p.height() }
            };

            // Apply the stored ratio once the paned has a real allocation.
            // `map` fires before size allocation, so a tick callback waits for
            // a non-zero size and then applies exactly once.
            let stored = *ratio;
            let applying = Rc::new(std::cell::Cell::new(false));
            let applying_tick = Rc::clone(&applying);
            let total_tick = total_of;
            let applied = Rc::new(std::cell::Cell::new(false));
            let applied_tick = Rc::clone(&applied);
            paned.add_tick_callback(move |p, _| {
                if applied_tick.get() {
                    return glib::ControlFlow::Break;
                }
                let total = total_tick(p);
                if total <= 0 {
                    return glib::ControlFlow::Continue;
                }
                applying_tick.set(true);
                p.set_position((stored * f64::from(total)) as i32);
                applying_tick.set(false);
                applied_tick.set(true);
                glib::ControlFlow::Break
            });

            // Write user drags back to the tree so the ratio survives rebuilds.
            let ctrl_weak = Rc::downgrade(ctrl);
            let path_owned = path.to_vec();
            let applying_notify = Rc::clone(&applying);
            let total_notify = total_of;
            paned.connect_notify_local(Some("position"), move |p, _| {
                if applying_notify.get() {
                    return;
                }
                let total = total_notify(p);
                if total <= 0 {
                    return;
                }
                let ratio = f64::from(p.position()) / f64::from(total);
                if let Some(c) = ctrl_weak.upgrade() {
                    c.set_split_ratio(tab_id, &path_owned, ratio);
                }
            });

            paned.upcast::<gtk4::Widget>()
        }
    }
}

fn truncate_label(s: &str, max_chars: usize) -> String {
    let count = s.chars().count();
    if count <= max_chars {
        s.to_owned()
    } else {
        let mut out: String = s.chars().take(max_chars.saturating_sub(1)).collect();
        out.push('…');
        out
    }
}
