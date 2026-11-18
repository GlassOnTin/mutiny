use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::{Rc, Weak};

use gtk4::gdk;
use gtk4::gio;
use gtk4::glib;
use gtk4::prelude::*;

use crate::action::{ACTIONS, ActionId, build_menubar_model, filter_palette};
use crate::app::{AppController, ConsentResolution};
use crate::engine::AttentionReason;
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
    tab_buttons_box: gtk4::Box,
    agent_badge_btn: gtk4::Button,
    consent_bar: gtk4::Box,
    consent_label: gtk4::Label,
    active_consent_id: Cell<Option<u64>>,
    notice_bar: gtk4::Box,
    notice_label: gtk4::Label,
    content_slot: gtk4::Box,
    pane_views: RefCell<HashMap<PaneId, TerminalView>>,
    tab_buttons: RefCell<Vec<(TabId, gtk4::Button)>>,
    controller: Weak<AppController>,
    closing_programmatically: Cell<bool>,
}

impl MutinyWindow {
    pub fn new(
        app: &gtk4::Application,
        window_id: WindowId,
        controller: Rc<AppController>,
    ) -> Self {
        // Leave titlebar unset so Wayland xdg-decoration uses KWin native Breeze SSD on KDE.
        let gtk_window = gtk4::ApplicationWindow::builder()
            .application(app)
            .title("Mutiny")
            .default_width(980)
            .default_height(640)
            .show_menubar(false)
            .handle_menubar_accel(false)
            .build();

        let menubar = gtk4::PopoverMenuBar::from_model(Some(&build_menubar_model()));

        let chrome_bar = gtk4::Box::new(gtk4::Orientation::Horizontal, 4);
        chrome_bar.set_margin_start(6);
        chrome_bar.set_margin_end(6);
        chrome_bar.set_margin_top(3);
        chrome_bar.set_margin_bottom(3);

        let tab_buttons_box = gtk4::Box::new(gtk4::Orientation::Horizontal, 4);
        tab_buttons_box.set_hexpand(true);
        chrome_bar.append(&tab_buttons_box);

        let agent_badge_btn = make_action_button(
            "Agent: idle",
            "win.agent-audit",
            "Agent Activity & Audit Log (Ctrl+Shift+J)",
        );
        agent_badge_btn.set_visible(false);
        chrome_bar.append(&agent_badge_btn);

        // Persistent SSD-safe controls: never hidden when CSD is disabled.
        chrome_bar.append(&make_action_button(
            "+ Tab",
            "win.new-tab",
            "New Tab (Ctrl+Shift+T)",
        ));
        chrome_bar.append(&make_action_button(
            "Split →",
            "win.split-right",
            "Split Right (Ctrl+Shift+R)",
        ));
        chrome_bar.append(&make_action_button(
            "Split ↓",
            "win.split-down",
            "Split Down (Ctrl+Shift+E)",
        ));
        chrome_bar.append(&make_action_button(
            "Detach ↗",
            "win.detach-split",
            "Detach Split to Window (Ctrl+Shift+D)",
        ));
        chrome_bar.append(&make_action_button(
            "⌘ Palette",
            "win.command-palette",
            "Command Palette (Ctrl+Shift+P)",
        ));
        chrome_bar.append(&make_action_button(
            "?",
            "win.keybind-cheatsheet",
            "Keybind Cheatsheet (Ctrl+Shift+?)",
        ));

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

        let allow_once_btn = gtk4::Button::with_label("Allow Once");
        allow_once_btn.set_can_focus(false);
        let allow_sess_btn = gtk4::Button::with_label("Allow Session");
        allow_sess_btn.set_can_focus(false);
        let deny_btn = gtk4::Button::with_label("Deny");
        deny_btn.set_can_focus(false);
        consent_bar.append(&allow_once_btn);
        consent_bar.append(&allow_sess_btn);
        consent_bar.append(&deny_btn);

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

        let content_slot = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
        content_slot.set_hexpand(true);
        content_slot.set_vexpand(true);

        let root_vbox = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
        root_vbox.append(&menubar);
        root_vbox.append(&chrome_bar);
        root_vbox.append(&consent_bar);
        root_vbox.append(&notice_bar);
        root_vbox.append(&gtk4::Separator::new(gtk4::Orientation::Horizontal));
        root_vbox.append(&content_slot);
        gtk_window.set_child(Some(&root_vbox));

        let this = Self {
            inner: Rc::new(MutinyWindowInner {
                window_id,
                gtk_window: gtk_window.clone(),
                menubar,
                tab_buttons_box,
                agent_badge_btn,
                consent_bar,
                consent_label,
                active_consent_id: Cell::new(None),
                notice_bar,
                notice_label,
                content_slot,
                pane_views: RefCell::new(HashMap::new()),
                tab_buttons: RefCell::new(Vec::new()),
                controller: Rc::downgrade(&controller),
                closing_programmatically: Cell::new(false),
            }),
        };

        this.wire_banner_buttons(&allow_once_btn, &allow_sess_btn, &deny_btn, &ack_btn);
        this.register_window_actions(&controller);

        let ctrl_weak = Rc::downgrade(&controller);
        gtk_window.connect_is_active_notify(move |win| {
            if win.is_active()
                && let Some(ctrl) = ctrl_weak.upgrade()
            {
                ctrl.workspace.borrow_mut().set_active_window(window_id);
            }
        });

        let inner_weak = Rc::downgrade(&this.inner);
        gtk_window.connect_close_request(move |_| {
            if let Some(inner) = inner_weak.upgrade()
                && !inner.closing_programmatically.get()
                && let Some(ctrl) = inner.controller.upgrade()
            {
                ctrl.handle_window_closed(inner.window_id);
            }
            glib::Propagation::Proceed
        });

        this
    }

    fn wire_banner_buttons(
        &self,
        allow_once: &gtk4::Button,
        allow_sess: &gtk4::Button,
        deny: &gtk4::Button,
        ack: &gtk4::Button,
    ) {
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
    }

    pub fn present(&self) {
        self.inner.gtk_window.present();
    }

    pub fn close_programmatically(&self) {
        self.inner.closing_programmatically.set(true);
        self.inner.gtk_window.close();
    }

    pub fn queue_draw_pane(&self, pane_id: PaneId) {
        if let Some(view) = self.inner.pane_views.borrow().get(&pane_id) {
            view.queue_draw();
        }
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
                .map(|e| format!("Mutiny — {}", e.title()))
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
        for (idx, (tid, label, first_pane)) in tab_ids_and_labels.into_iter().enumerate() {
            let btn = gtk4::Button::with_label(&format!("{}: {}", idx + 1, label));
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
            self.inner.tab_buttons_box.append(&btn);
            self.inner.tab_buttons.borrow_mut().push((tid, btn));
        }

        self.sync_banners(&ctrl);

        while let Some(child) = self.inner.content_slot.first_child() {
            self.inner.content_slot.remove(&child);
        }
        self.inner.pane_views.borrow_mut().clear();

        let mut views_map = HashMap::new();
        let tree_widget = build_split_widget(&active_tree, &ctrl, &mut views_map);
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
                    .set_title(Some(&format!("Mutiny — {}", ap.title())));
            }
            wm.active_tab
        };

        let labels: HashMap<TabId, String> = self
            .compute_tab_labels(&ctrl, active_tab_id)
            .into_iter()
            .map(|(tid, lbl, _)| (tid, lbl))
            .collect();

        for (idx, (tid, btn)) in self.inner.tab_buttons.borrow().iter().enumerate() {
            if let Some(lbl) = labels.get(tid) {
                btn.set_label(&format!("{}: {}", idx + 1, lbl));
            }
        }

        self.sync_banners(&ctrl);
    }

    fn sync_banners(&self, ctrl: &Rc<AppController>) {
        if let Some(agent_name) = ctrl.active_agent_badge() {
            self.inner
                .agent_badge_btn
                .set_label(&format!("Agent: {agent_name}"));
            self.inner.agent_badge_btn.set_visible(true);
        } else {
            self.inner.agent_badge_btn.set_visible(false);
        }

        if let Some((cid, client, verb, detail)) = ctrl.pending_consent_prompt() {
            self.inner.active_consent_id.set(Some(cid));
            self.inner.consent_label.set_text(&format!(
                "Agent '{client}' requests Tier-2 '{verb}' {detail}"
            ));
            self.inner.consent_bar.set_visible(true);
        } else {
            self.inner.active_consent_id.set(None);
            self.inner.consent_bar.set_visible(false);
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
    ) -> Vec<(TabId, String, PaneId)> {
        let ws = ctrl.workspace.borrow();
        let Some(wm) = ws.window(self.inner.window_id) else {
            return Vec::new();
        };

        wm.tabs
            .iter()
            .map(|tab| {
                let leaves = tab.root.leaves();
                let pane_count = leaves.len();
                let title = ws
                    .pane(tab.active_pane)
                    .map(|e| e.title().to_owned())
                    .unwrap_or_else(|| String::from("shell"));

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

                let label = if status_badges.is_empty() {
                    truncate_label(&title, 22)
                } else {
                    format!(
                        "{} [{}]",
                        truncate_label(&title, 18),
                        status_badges.join(" ")
                    )
                };
                (tab.id, label, tab.active_pane)
            })
            .collect()
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

fn make_action_button(label: &str, action_name: &str, tooltip: &str) -> gtk4::Button {
    let btn = gtk4::Button::with_label(label);
    btn.set_action_name(Some(action_name));
    btn.set_tooltip_text(Some(tooltip));
    btn.set_can_focus(false);
    btn
}

fn build_split_widget(
    node: &SplitNode,
    ctrl: &Rc<AppController>,
    views_out: &mut HashMap<PaneId, TerminalView>,
) -> gtk4::Widget {
    match node {
        SplitNode::Leaf(pane_id) => {
            let view = TerminalView::new(*pane_id, ctrl);
            views_out.insert(*pane_id, view.clone());
            view.upcast::<gtk4::Widget>()
        }
        SplitNode::Split {
            axis,
            first,
            second,
            ..
        } => {
            let orientation = match axis {
                SplitAxis::Horizontal => gtk4::Orientation::Horizontal,
                SplitAxis::Vertical => gtk4::Orientation::Vertical,
            };
            let paned = gtk4::Paned::new(orientation);
            paned.set_wide_handle(true);
            paned.set_hexpand(true);
            paned.set_vexpand(true);
            let child1 = build_split_widget(first, ctrl, views_out);
            let child2 = build_split_widget(second, ctrl, views_out);
            paned.set_start_child(Some(&child1));
            paned.set_end_child(Some(&child2));
            paned.set_resize_start_child(true);
            paned.set_resize_end_child(true);
            paned.set_shrink_start_child(false);
            paned.set_shrink_end_child(false);
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
