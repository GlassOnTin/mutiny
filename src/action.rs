use gtk4::gio;
use gtk4::glib;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ConsentTier {
    Observe,
    Act,
    /// Mutates trust boundaries or standing grants (policy install/revoke,
    /// peer pairing/revocation). Always requires explicit operator consent;
    /// never auto-approved by session grants, standing policies, or paired
    /// agent carriers.
    PolicyChange,
    /// Tier 4: destroys state the operator cannot recover. Always per-action,
    /// never covered by a session grant, a standing policy, or a paired agent
    /// carrier.
    Irreversible,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ActionId {
    NewWindow,
    NewTab,
    ClosePane,
    SplitRight,
    SplitDown,
    DetachSplit,
    FocusNextPane,
    FocusPrevPane,
    UrgentNext,
    Copy,
    Paste,
    InsertPeerTotp,
    Search,
    ToggleMenuBar,
    CommandPalette,
    AgentAudit,
    SessionPicker,
    NewSession,
    PairHaven,
    QuickTerminal,
    SaveSession,
    CloseTab,
    CloseWindow,
    Settings,
    KeybindCheatsheet,
    JumpPrevPrompt,
    JumpNextPrompt,
}

impl ActionId {
    pub const ALL: &'static [ActionId] = &[
        ActionId::NewWindow,
        ActionId::NewTab,
        ActionId::ClosePane,
        ActionId::SplitRight,
        ActionId::SplitDown,
        ActionId::DetachSplit,
        ActionId::FocusNextPane,
        ActionId::FocusPrevPane,
        ActionId::UrgentNext,
        ActionId::Copy,
        ActionId::Paste,
        ActionId::InsertPeerTotp,
        ActionId::Search,
        ActionId::ToggleMenuBar,
        ActionId::CommandPalette,
        ActionId::AgentAudit,
        ActionId::SessionPicker,
        ActionId::NewSession,
        ActionId::PairHaven,
        ActionId::QuickTerminal,
        ActionId::SaveSession,
        ActionId::CloseTab,
        ActionId::CloseWindow,
        ActionId::Settings,
        ActionId::KeybindCheatsheet,
        ActionId::JumpPrevPrompt,
        ActionId::JumpNextPrompt,
    ];
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
/// Four non-overlapping archetypes. Every action has exactly one home:
///
/// - `Session`: the lifecycle of a session context. New window, new tab, and
///   attaching to a multiplexer session.
/// - `Edit`: operations on the focused pane's CONTENT. Clipboard, find, and
///   moving between commands in that content.
/// - `Pane`: the STRUCTURE and focus of panes. Split, detach, close, focus,
///   and jumping to a pane that needs attention.
/// - `View`: chrome and introspection. Menu bar, palette, agent audit,
///   cheatsheet.
///
/// The old Shell/Panes/View split overlapped: "Shell" held pane closing,
/// "View" held session attaching, and "Panes" mixed structure with content
/// navigation. Help existed for one item, which now lives in View.
pub enum MenuCategory {
    Session,
    Edit,
    Pane,
    View,
}

impl MenuCategory {
    pub const ORDERED: &'static [MenuCategory] = &[
        MenuCategory::Session,
        MenuCategory::Edit,
        MenuCategory::Pane,
        MenuCategory::View,
    ];

    pub const fn label(self) -> &'static str {
        match self {
            MenuCategory::Session => "_Session",
            MenuCategory::Edit => "_Edit",
            MenuCategory::Pane => "_Pane",
            MenuCategory::View => "_View",
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize)]
pub struct ActionSpec {
    pub id: ActionId,
    pub slug: &'static str,
    pub title: &'static str,
    pub category: MenuCategory,
    pub consent_tier: ConsentTier,
    pub default_accel: Option<&'static str>,
    pub human_chord: &'static str,
    pub in_context_menu: bool,
    pub summary: &'static str,
}

impl ActionSpec {
    pub fn gtk_detailed_name(&self) -> String {
        format!("win.{}", self.slug)
    }
}

pub const ACTIONS: &[ActionSpec] = &[
    ActionSpec {
        id: ActionId::NewWindow,
        slug: "new-window",
        title: "New Window",
        category: MenuCategory::Session,
        consent_tier: ConsentTier::Act,
        default_accel: Some("<Control><Shift>n"),
        human_chord: "Ctrl+Shift+N",
        in_context_menu: false,
        summary: "Open a top-level terminal window",
    },
    ActionSpec {
        id: ActionId::NewTab,
        slug: "new-tab",
        title: "New Session Tab",
        category: MenuCategory::Session,
        consent_tier: ConsentTier::Act,
        default_accel: Some("<Control><Shift>t"),
        human_chord: "Ctrl+Shift+T",
        in_context_menu: true,
        summary: "Open a tab inheriting active working directory",
    },
    ActionSpec {
        id: ActionId::ClosePane,
        slug: "close-pane",
        title: "Close Pane (session keeps running)",
        category: MenuCategory::Pane,
        consent_tier: ConsentTier::Act,
        default_accel: Some("<Control><Shift>w"),
        human_chord: "Ctrl+Shift+W",
        in_context_menu: true,
        summary: "Close focused split pane or tab",
    },
    ActionSpec {
        id: ActionId::Copy,
        slug: "copy",
        title: "Copy Selection",
        category: MenuCategory::Edit,
        consent_tier: ConsentTier::Observe,
        default_accel: Some("<Control><Shift>c"),
        human_chord: "Ctrl+Shift+C",
        in_context_menu: true,
        summary: "Copy active terminal selection to clipboard",
    },
    ActionSpec {
        id: ActionId::Paste,
        slug: "paste",
        title: "Paste Clipboard",
        category: MenuCategory::Edit,
        consent_tier: ConsentTier::Act,
        default_accel: Some("<Control><Shift>v"),
        human_chord: "Ctrl+Shift+V",
        in_context_menu: true,
        summary: "Paste clipboard into active pane with bracketed-paste awareness",
    },
    ActionSpec {
        id: ActionId::InsertPeerTotp,
        slug: "insert-peer-totp",
        title: "Insert Haven TOTP Code",
        category: MenuCategory::Edit,
        consent_tier: ConsentTier::PolicyChange,
        default_accel: Some("<Control><Shift>y"),
        human_chord: "Ctrl+Shift+Y",
        in_context_menu: false,
        summary: "Request a live OATH-TOTP code from the paired Haven limb and write it directly into the focused pane",
    },
    ActionSpec {
        id: ActionId::SplitRight,
        slug: "split-right",
        title: "Split Right",
        category: MenuCategory::Pane,
        consent_tier: ConsentTier::Act,
        default_accel: Some("<Control><Shift>r"),
        human_chord: "Ctrl+Shift+R",
        in_context_menu: true,
        summary: "Split active pane horizontally into left and right panes",
    },
    ActionSpec {
        id: ActionId::SplitDown,
        slug: "split-down",
        title: "Split Down",
        category: MenuCategory::Pane,
        consent_tier: ConsentTier::Act,
        default_accel: Some("<Control><Shift>e"),
        human_chord: "Ctrl+Shift+E",
        in_context_menu: true,
        summary: "Split active pane vertically into top and bottom panes",
    },
    ActionSpec {
        id: ActionId::DetachSplit,
        slug: "detach-split",
        title: "Move Pane to New Window",
        category: MenuCategory::Pane,
        consent_tier: ConsentTier::Act,
        default_accel: Some("<Control><Shift>d"),
        human_chord: "Ctrl+Shift+D",
        in_context_menu: true,
        summary: "Pop active split pane into its own top-level window without restarting its shell",
    },
    ActionSpec {
        id: ActionId::FocusNextPane,
        slug: "focus-next-pane",
        title: "Focus Next Pane",
        category: MenuCategory::Pane,
        consent_tier: ConsentTier::Act,
        default_accel: Some("<Control><Shift>bracketright"),
        human_chord: "Ctrl+Shift+]",
        in_context_menu: false,
        summary: "Move keyboard focus to next pane in current tab",
    },
    ActionSpec {
        id: ActionId::FocusPrevPane,
        slug: "focus-prev-pane",
        title: "Focus Previous Pane",
        category: MenuCategory::Pane,
        consent_tier: ConsentTier::Act,
        default_accel: Some("<Control><Shift>bracketleft"),
        human_chord: "Ctrl+Shift+[",
        in_context_menu: false,
        summary: "Move keyboard focus to previous pane in current tab",
    },
    ActionSpec {
        id: ActionId::UrgentNext,
        slug: "urgent-next",
        title: "Jump to Next Urgent Pane",
        category: MenuCategory::Pane,
        consent_tier: ConsentTier::Act,
        default_accel: Some("<Control><Shift>a"),
        human_chord: "Ctrl+Shift+A",
        in_context_menu: true,
        summary: "Jump across windows, tabs, and splits to the next pane needing attention",
    },
    ActionSpec {
        id: ActionId::JumpPrevPrompt,
        slug: "jump-prev-prompt",
        title: "Jump to Previous Prompt (OSC 133)",
        category: MenuCategory::Edit,
        consent_tier: ConsentTier::Act,
        default_accel: Some("<Control><Shift>Up"),
        human_chord: "Ctrl+Shift+Up",
        in_context_menu: false,
        summary: "Scroll to previous OSC 133 shell prompt marker",
    },
    ActionSpec {
        id: ActionId::JumpNextPrompt,
        slug: "jump-next-prompt",
        title: "Jump to Next Prompt (OSC 133)",
        category: MenuCategory::Edit,
        consent_tier: ConsentTier::Act,
        default_accel: Some("<Control><Shift>Down"),
        human_chord: "Ctrl+Shift+Down",
        in_context_menu: false,
        summary: "Scroll to next OSC 133 shell prompt marker",
    },
    ActionSpec {
        id: ActionId::Search,
        slug: "search",
        title: "Search Scrollback",
        category: MenuCategory::Edit,
        consent_tier: ConsentTier::Observe,
        default_accel: Some("<Control><Shift>f"),
        human_chord: "Ctrl+Shift+F",
        in_context_menu: true,
        summary: "Search the active pane's visible lines and scrollback",
    },
    ActionSpec {
        id: ActionId::ToggleMenuBar,
        slug: "toggle-menubar",
        title: "Toggle Menu Bar",
        category: MenuCategory::View,
        consent_tier: ConsentTier::Act,
        default_accel: Some("<Control><Shift>m"),
        human_chord: "Ctrl+Shift+M",
        in_context_menu: true,
        summary: "Show or hide horizontal menu bar (Konsole convention)",
    },
    ActionSpec {
        id: ActionId::CommandPalette,
        slug: "command-palette",
        title: "Command Palette",
        category: MenuCategory::View,
        consent_tier: ConsentTier::Observe,
        default_accel: Some("<Control><Shift>p"),
        human_chord: "Ctrl+Shift+P",
        in_context_menu: true,
        summary: "Search and run any terminal action",
    },
    ActionSpec {
        id: ActionId::Settings,
        slug: "settings",
        title: "Settings",
        category: MenuCategory::Edit,
        consent_tier: ConsentTier::Act,
        default_accel: Some("<Control><Shift>comma"),
        human_chord: "Ctrl+Shift+,",
        in_context_menu: false,
        summary: "Edit every configuration option and write it to config.toml; changes live-reload",
    },
    ActionSpec {
        id: ActionId::SaveSession,
        slug: "save-session",
        title: "Save Session Layout",
        category: MenuCategory::Session,
        consent_tier: ConsentTier::Act,
        default_accel: None,
        human_chord: "",
        in_context_menu: false,
        summary: "Write the current windows, tabs, splits, and attached sessions to the state file now",
    },
    ActionSpec {
        id: ActionId::CloseTab,
        slug: "close-tab",
        title: "Close Tab (session keeps running)",
        category: MenuCategory::Session,
        consent_tier: ConsentTier::Act,
        default_accel: None,
        human_chord: "",
        in_context_menu: false,
        summary: "Close the active tab; a multiplexer session keeps running, only its client ends",
    },
    ActionSpec {
        id: ActionId::CloseWindow,
        slug: "close-window",
        title: "Close Window (sessions keep running)",
        category: MenuCategory::Session,
        consent_tier: ConsentTier::Act,
        default_accel: None,
        human_chord: "",
        in_context_menu: false,
        summary: "Close the active window and every pane in it",
    },
    ActionSpec {
        id: ActionId::NewSession,
        slug: "new-session",
        title: "New Session",
        category: MenuCategory::Session,
        consent_tier: ConsentTier::Act,
        default_accel: None,
        human_chord: "",
        in_context_menu: false,
        summary: "Create a new tmux or zellij session, or a plain shell, with a name you choose",
    },
    ActionSpec {
        id: ActionId::PairHaven,
        slug: "pair-haven",
        title: "Pair Haven Limb…",
        category: MenuCategory::Session,
        consent_tier: ConsentTier::PolicyChange,
        default_accel: None,
        human_chord: "",
        in_context_menu: false,
        summary: "Pair an outbound Haven HTTP MCP limb over loopback or an encrypted tunnel",
    },
    ActionSpec {
        id: ActionId::QuickTerminal,
        slug: "quick-terminal",
        title: "Quick Terminal (summon or dismiss)",
        category: MenuCategory::Session,
        consent_tier: ConsentTier::Act,
        default_accel: Some("<Control><Shift>grave"),
        human_chord: "Ctrl+Shift+`",
        in_context_menu: false,
        summary: "Toggle a scratch shell in its own window; dismissing it leaves the shell running",
    },
    ActionSpec {
        id: ActionId::SessionPicker,
        slug: "session-picker",
        title: "Open Session…",
        category: MenuCategory::Session,
        consent_tier: ConsentTier::Act,
        default_accel: Some("<Control><Shift>s"),
        human_chord: "Ctrl+Shift+S",
        in_context_menu: true,
        summary: "List tmux and zellij sessions, local and over SSH, and open one in a new tab or window",
    },
    ActionSpec {
        id: ActionId::AgentAudit,
        slug: "agent-audit",
        title: "Agent Activity & Audit Log",
        category: MenuCategory::View,
        consent_tier: ConsentTier::Observe,
        default_accel: Some("<Control><Shift>j"),
        human_chord: "Ctrl+Shift+J",
        in_context_menu: true,
        summary: "Inspect live agent calls, standing session grants, and redacted audit history",
    },
    ActionSpec {
        id: ActionId::KeybindCheatsheet,
        slug: "keybind-cheatsheet",
        title: "Keybind Cheatsheet",
        category: MenuCategory::View,
        consent_tier: ConsentTier::Observe,
        default_accel: Some("<Control><Shift>question"),
        human_chord: "Ctrl+Shift+?",
        in_context_menu: false,
        summary: "Display live keyboard shortcuts generated from the action table",
    },
];

pub fn spec(id: ActionId) -> &'static ActionSpec {
    ACTIONS
        .iter()
        .find(|s| s.id == id)
        .expect("every ActionId variant must be registered in ACTIONS")
}

pub fn by_slug(slug: &str) -> Option<&'static ActionSpec> {
    ACTIONS.iter().find(|s| s.slug == slug)
}

pub fn filter_palette(query: &str) -> Vec<&'static ActionSpec> {
    let needle = query.trim().to_ascii_lowercase();
    if needle.is_empty() {
        return ACTIONS.iter().collect();
    }
    let terms: Vec<&str> = needle.split_whitespace().collect();
    ACTIONS
        .iter()
        .filter(|s| {
            let hay = format!(
                "{} {} {} {}",
                s.slug,
                s.title.to_ascii_lowercase(),
                s.human_chord.to_ascii_lowercase(),
                s.summary.to_ascii_lowercase()
            );
            terms.iter().all(|t| hay.contains(t))
        })
        .collect()
}

/// Builds the menu bar and returns the dynamic Sessions submenu alongside it,
/// so the controller can fill in discovered sessions without rebuilding the
/// whole bar.
pub fn build_menubar_model() -> (gio::Menu, gio::Menu) {
    let root = gio::Menu::new();
    let sessions = gio::Menu::new();
    for &cat in MenuCategory::ORDERED {
        let submenu = gio::Menu::new();
        for action in ACTIONS
            .iter()
            .filter(|a| a.category == cat && a.id != ActionId::SessionPicker)
        {
            submenu.append(Some(action.title), Some(&action.gtk_detailed_name()));
        }
        if cat == MenuCategory::Session {
            // The session list is a submenu, not a pop-up: Ian wants to see and
            // pick a session inline. The full picker stays reachable from here
            // and from Ctrl+Shift+S.
            submenu.append_submenu(Some("Open Session"), &sessions);
        }
        root.append_submenu(Some(cat.label()), &submenu);
    }
    (root, sessions)
}

/// Action name for opening a specific discovered session. Parameterised, so it
/// lives outside the keybind-able `ACTIONS` table: the target is a session, not
/// a verb.
pub const OPEN_SESSION_TARGET: &str = "win.open-session-target";

/// Encodes a session as the action target: kind, host, name, tab-separated so a
/// name containing spaces or punctuation survives.
pub fn session_target(kind: crate::mux::MuxKind, host: Option<&str>, name: &str) -> String {
    format!("{}\t{}\t{}", kind.label(), host.unwrap_or(""), name)
}

pub fn parse_session_target(target: &str) -> Option<(crate::mux::MuxKind, Option<String>, String)> {
    let mut parts = target.splitn(3, '\t');
    let kind = match parts.next()? {
        "tmux" => crate::mux::MuxKind::Tmux,
        "zellij" => crate::mux::MuxKind::Zellij,
        _ => return None,
    };
    let host = parts.next().filter(|h| !h.is_empty()).map(str::to_owned);
    let name = parts.next()?.to_owned();
    if name.is_empty() {
        return None;
    }
    Some((kind, host, name))
}

/// Tab-targeted context-menu actions. Parameterised by TabId, so they live
/// outside `ACTIONS`: the target is a tab, not a keybind-able verb.
pub const CLOSE_TAB_TARGET: &str = "win.close-tab-target";
pub const DETACH_TAB_TARGET: &str = "win.detach-tab-target";
pub const RENAME_TAB_TARGET: &str = "win.rename-session-tab";
pub const COPY_NAME_TAB_TARGET: &str = "win.copy-session-name-tab";
pub const END_TAB_TARGET: &str = "win.end-session-tab";

/// The right-click menu for one tab. Rename and Copy Name only appear when the
/// tab is attached to a multiplexer session, because there is nothing to rename
/// or copy otherwise. Killing a session is deliberately absent: the menu closes
/// the tab, never the session.
pub fn build_tab_menu_model(attached: bool, tab: u64) -> gio::Menu {
    let menu = gio::Menu::new();
    // A parameterised action needs its target ON THE ITEM, not just the action
    // name: without it GTK renders the item insensitive and it cannot fire.
    let targeted = |section: &gio::Menu, label: &str, action: &str| {
        let item = gio::MenuItem::new(Some(label), None);
        item.set_action_and_target_value(Some(action), Some(&glib::Variant::from(tab)));
        section.append_item(&item);
    };

    // Session identity first.
    if attached {
        let identity = gio::Menu::new();
        targeted(&identity, "Rename Session…", RENAME_TAB_TARGET);
        targeted(&identity, "Copy Session Name", COPY_NAME_TAB_TARGET);
        menu.append_section(None, &identity);
    }

    // Closing a VIEW. The wording says what survives, because with a
    // multiplexer "close" and "end" are different operations and a tmux user
    // must not have to guess which one this is.
    let views = gio::Menu::new();
    targeted(
        &views,
        "Close Tab (session keeps running)",
        CLOSE_TAB_TARGET,
    );
    // Not "Detach": to a tmux user that means detaching a CLIENT, while this
    // moves a pane to another window.
    targeted(&views, "Move to New Window", DETACH_TAB_TARGET);
    menu.append_section(None, &views);

    // Ending the SESSION is destructive and sits apart, behind a confirmation.
    if attached {
        let danger = gio::Menu::new();
        targeted(&danger, "End Session…", END_TAB_TARGET);
        menu.append_section(None, &danger);
    }
    menu
}

/// Fills the Sessions submenu from a discovery snapshot. Live sessions first,
/// exited last, capped so a huge inventory cannot make the menu unusable.
pub fn populate_sessions_menu(menu: &gio::Menu, inventory: &crate::mux::MuxInventory) {
    menu.remove_all();
    let listed = inventory.sorted_for_display();
    for session in listed.iter().take(24) {
        let label = match &session.host {
            Some(host) => format!("{} ({host})", session.name),
            None => session.name.clone(),
        };
        let label = if session.exited {
            format!("{label} (exited)")
        } else {
            label
        };
        let item = gio::MenuItem::new(Some(&label), None);
        let target = session_target(session.kind, session.host.as_deref(), &session.name);
        item.set_action_and_target_value(
            Some(OPEN_SESSION_TARGET),
            Some(&glib::Variant::from(target)),
        );
        menu.append_item(&item);
    }
    if listed.is_empty() {
        // No action, so GTK renders it insensitive rather than clickable.
        menu.append_item(&gio::MenuItem::new(Some("No sessions found"), None));
    }
    let picker = gio::MenuItem::new(Some("Browse All Sessions…"), Some("win.session-picker"));
    menu.append_item(&picker);
}

pub fn build_context_menu_model() -> gio::Menu {
    let menu = gio::Menu::new();
    for action in ACTIONS.iter().filter(|a| a.in_context_menu) {
        menu.append(Some(action.title), Some(&action.gtk_detailed_name()));
    }
    menu
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn every_action_id_is_registered_uniquely() {
        let gtk_ready = gtk4::init().is_ok();
        assert_eq!(ActionId::ALL.len(), ACTIONS.len());
        let mut slugs = HashSet::new();
        let mut chords = HashSet::new();
        for &id in ActionId::ALL {
            let s = spec(id);
            assert_eq!(s.id, id);
            assert!(slugs.insert(s.slug), "duplicate slug: {}", s.slug);
            assert_eq!(by_slug(s.slug).map(|x| x.id), Some(id));
            if let Some(accel) = s.default_accel {
                assert!(chords.insert(accel), "duplicate chord: {accel}");
                assert!(!accel.trim().is_empty(), "empty chord");
                if gtk_ready {
                    assert!(
                        gtk4::accelerator_parse(accel).is_some(),
                        "invalid GTK accelerator syntax: {accel}"
                    );
                }
            }
        }
    }

    #[test]
    fn palette_search_matches_title_slug_and_chord() {
        assert_eq!(filter_palette("").len(), ACTIONS.len());
        let detach = filter_palette("detach window");
        assert_eq!(detach.len(), 1);
        assert_eq!(detach[0].id, ActionId::DetachSplit);

        let audit = filter_palette("ctrl+shift+j");
        assert_eq!(audit.len(), 1);
        assert_eq!(audit[0].id, ActionId::AgentAudit);
    }
}
