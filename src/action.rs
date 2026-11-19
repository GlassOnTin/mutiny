use gtk4::gio;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ConsentTier {
    Observe,
    Act,
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
    Search,
    ToggleMenuBar,
    CommandPalette,
    AgentAudit,
    SessionPicker,
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
        ActionId::Search,
        ActionId::ToggleMenuBar,
        ActionId::CommandPalette,
        ActionId::AgentAudit,
        ActionId::SessionPicker,
        ActionId::KeybindCheatsheet,
        ActionId::JumpPrevPrompt,
        ActionId::JumpNextPrompt,
    ];
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MenuCategory {
    File,
    Edit,
    Pane,
    View,
    Help,
}

impl MenuCategory {
    pub const ORDERED: &'static [MenuCategory] = &[
        MenuCategory::File,
        MenuCategory::Edit,
        MenuCategory::Pane,
        MenuCategory::View,
        MenuCategory::Help,
    ];

    pub const fn label(self) -> &'static str {
        match self {
            MenuCategory::File => "_Shell",
            MenuCategory::Edit => "_Edit",
            MenuCategory::Pane => "_Panes",
            MenuCategory::View => "_View",
            MenuCategory::Help => "_Help",
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
        category: MenuCategory::File,
        consent_tier: ConsentTier::Act,
        default_accel: Some("<Control><Shift>n"),
        human_chord: "Ctrl+Shift+N",
        in_context_menu: false,
        summary: "Open a top-level terminal window",
    },
    ActionSpec {
        id: ActionId::NewTab,
        slug: "new-tab",
        title: "New Tab",
        category: MenuCategory::File,
        consent_tier: ConsentTier::Act,
        default_accel: Some("<Control><Shift>t"),
        human_chord: "Ctrl+Shift+T",
        in_context_menu: true,
        summary: "Open a tab inheriting active working directory",
    },
    ActionSpec {
        id: ActionId::ClosePane,
        slug: "close-pane",
        title: "Close Active Pane",
        category: MenuCategory::File,
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
        title: "Detach Split to Window",
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
        category: MenuCategory::Pane,
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
        category: MenuCategory::Pane,
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
        id: ActionId::SessionPicker,
        slug: "session-picker",
        title: "Open Multiplexer Session",
        category: MenuCategory::View,
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
        category: MenuCategory::Help,
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

pub fn build_menubar_model() -> gio::Menu {
    let root = gio::Menu::new();
    for &cat in MenuCategory::ORDERED {
        let submenu = gio::Menu::new();
        for action in ACTIONS.iter().filter(|a| a.category == cat) {
            submenu.append(Some(action.title), Some(&action.gtk_detailed_name()));
        }
        root.append_submenu(Some(cat.label()), &submenu);
    }
    root
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
