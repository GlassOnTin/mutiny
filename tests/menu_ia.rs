//! The menu information architecture: four non-overlapping archetypes, every
//! action in exactly one home, and the action slugs and keybinds unchanged by
//! the redesign.
//!
//! The slug/chord table below is a golden list taken from before the IA change.
//! It is the guard for "keep action slugs + keybinds stable": a rename or a
//! rebound chord fails here rather than silently breaking someone's muscle
//! memory or an agent's `run_action` call.

use gtk4::gio::prelude::*;
use mutiny::action::{
    ACTIONS, MenuCategory, build_menubar_model, parse_session_target, populate_sessions_menu,
    session_target,
};
use mutiny::mux::{MuxInventory, MuxKind, MuxSession};

/// slug, chord, home. Chords are exactly as they were before the redesign.
const EXPECTED: &[(&str, &str, &str)] = &[
    ("new-window", "Ctrl+Shift+N", "Session"),
    ("new-tab", "Ctrl+Shift+T", "Session"),
    ("session-picker", "Ctrl+Shift+S", "Session"),
    // No chord: reachable from the Session menu, the tab menu, and the palette.
    ("new-session", "", "Session"),
    ("copy", "Ctrl+Shift+C", "Edit"),
    ("paste", "Ctrl+Shift+V", "Edit"),
    ("search", "Ctrl+Shift+F", "Edit"),
    ("jump-prev-prompt", "Ctrl+Shift+Up", "Edit"),
    ("jump-next-prompt", "Ctrl+Shift+Down", "Edit"),
    ("split-right", "Ctrl+Shift+R", "Pane"),
    ("split-down", "Ctrl+Shift+E", "Pane"),
    ("detach-split", "Ctrl+Shift+D", "Pane"),
    ("close-pane", "Ctrl+Shift+W", "Pane"),
    ("focus-next-pane", "Ctrl+Shift+]", "Pane"),
    ("focus-prev-pane", "Ctrl+Shift+[", "Pane"),
    ("urgent-next", "Ctrl+Shift+A", "Pane"),
    ("toggle-menubar", "Ctrl+Shift+M", "View"),
    ("command-palette", "Ctrl+Shift+P", "View"),
    ("agent-audit", "Ctrl+Shift+J", "View"),
    ("keybind-cheatsheet", "Ctrl+Shift+?", "View"),
];

fn home(cat: MenuCategory) -> &'static str {
    match cat {
        MenuCategory::Session => "Session",
        MenuCategory::Edit => "Edit",
        MenuCategory::Pane => "Pane",
        MenuCategory::View => "View",
    }
}

#[test]
fn every_action_keeps_its_slug_and_chord_and_has_one_home() {
    assert_eq!(
        ACTIONS.len(),
        EXPECTED.len(),
        "the action count changed; update the golden list deliberately"
    );
    for (slug, chord, expected_home) in EXPECTED {
        let matches: Vec<_> = ACTIONS.iter().filter(|a| a.slug == *slug).collect();
        assert_eq!(matches.len(), 1, "slug {slug} must appear exactly once");
        let spec = matches[0];
        assert_eq!(spec.human_chord, *chord, "chord for {slug} changed");
        assert_eq!(
            home(spec.category),
            *expected_home,
            "home for {slug} changed"
        );
    }
    // Exactly the four archetypes, in order, with no leftover Shell/Help.
    let labels: Vec<&str> = MenuCategory::ORDERED.iter().map(|c| c.label()).collect();
    assert_eq!(labels, vec!["_Session", "_Edit", "_Pane", "_View"]);
}

#[test]
fn session_target_round_trips_including_awkward_names() {
    for (kind, host, name) in [
        (MuxKind::Tmux, None, "plain"),
        (MuxKind::Zellij, Some("meteor"), "with space"),
        (MuxKind::Tmux, Some("user@host"), "a;echo x"),
        (MuxKind::Tmux, None, "a|b'c"),
    ] {
        let target = session_target(kind, host, name);
        let (k, h, n) = parse_session_target(&target).expect("round trip");
        assert_eq!(k, kind);
        assert_eq!(h.as_deref(), host);
        assert_eq!(n, name);
    }
    assert!(parse_session_target("nonsense").is_none());
    assert!(parse_session_target("tmux\thost\t").is_none(), "empty name");
}

fn session(name: &str, host: Option<&str>, exited: bool) -> MuxSession {
    MuxSession {
        host: host.map(str::to_owned),
        kind: MuxKind::Tmux,
        name: name.to_owned(),
        windows: 1,
        attached: 0,
        created: 0,
        path: None,
        exited,
    }
}

#[test]
fn sessions_submenu_lists_sessions_inline_and_keeps_the_picker_reachable() {
    let (root, sessions) = build_menubar_model();
    assert!(root.n_items() == 4, "four top-level menus");

    let inventory = MuxInventory {
        sessions: vec![
            session("local-work", None, false),
            session("remote-work", Some("meteor"), false),
            session("old", None, true),
        ],
        ..Default::default()
    };
    populate_sessions_menu(&sessions, &inventory);

    // Three sessions plus the picker entry.
    assert_eq!(sessions.n_items(), 4);
    let mut labels = Vec::new();
    let mut targets = Vec::new();
    for i in 0..sessions.n_items() {
        labels.push(
            sessions
                .item_attribute_value(i, "label", None)
                .and_then(|v| v.get::<String>())
                .unwrap_or_default(),
        );
        targets.push(
            sessions
                .item_attribute_value(i, "target", None)
                .and_then(|v| v.get::<String>()),
        );
    }
    assert_eq!(labels[0], "local-work", "title is the session name");
    assert_eq!(labels[1], "remote-work (meteor)", "remote shows its host");
    assert!(labels[2].contains("exited"), "exited is badged: {labels:?}");
    assert_eq!(labels[3], "Browse All Sessions…");
    assert!(targets[0].is_some(), "a session item carries a target");
    assert!(targets[3].is_none(), "the picker entry is a plain action");

    // Live sessions come before exited ones.
    let (k, h, n) = parse_session_target(&targets[0].clone().unwrap()).unwrap();
    assert_eq!((k, h, n.as_str()), (MuxKind::Tmux, None, "local-work"));

    // An empty inventory still offers the picker, and says why it is empty.
    populate_sessions_menu(&sessions, &MuxInventory::default());
    assert_eq!(sessions.n_items(), 2);
    let first = sessions
        .item_attribute_value(0, "label", None)
        .and_then(|v| v.get::<String>())
        .unwrap();
    assert_eq!(first, "No sessions found");
}

/// The tab menu must not offer to rename or copy a session that does not exist,
/// and must never offer to kill one.
#[test]
fn tab_menu_offers_session_actions_only_when_attached() {
    let labels = |attached: bool| -> Vec<String> {
        let menu = mutiny::action::build_tab_menu_model(attached, 7);
        (0..menu.n_items())
            .map(|i| {
                menu.item_attribute_value(i, "label", None)
                    .and_then(|v| v.get::<String>())
                    .unwrap_or_default()
            })
            .collect()
    };
    let attached = labels(true);
    assert!(attached.iter().any(|l| l.starts_with("Rename Session")));
    assert!(attached.iter().any(|l| l == "Copy Session Name"));
    assert!(attached.iter().any(|l| l == "Close Tab"));
    assert!(attached.iter().any(|l| l == "Detach to Window"));
    assert!(attached.iter().any(|l| l.starts_with("New Session")));

    let plain = labels(false);
    assert!(
        !plain.iter().any(|l| l.contains("Rename")),
        "nothing to rename: {plain:?}"
    );
    assert!(
        !plain.iter().any(|l| l.contains("Copy")),
        "nothing to copy: {plain:?}"
    );
    assert!(plain.iter().any(|l| l == "Close Tab"));
    assert!(plain.iter().any(|l| l.starts_with("New Session")));

    for list in [&attached, &plain] {
        assert!(
            !list.iter().any(|l| l.to_ascii_lowercase().contains("kill")),
            "the menu must never kill a session: {list:?}"
        );
    }
}

/// A parameterised action needs its target ON THE MENU ITEM. Without it GTK
/// renders the item insensitive and the click does nothing, which is exactly
/// how this first shipped before a live right-click caught it.
#[test]
fn every_tab_menu_item_carries_its_tab_target() {
    let menu = mutiny::action::build_tab_menu_model(true, 42);
    let mut saw_targeted = 0;
    for i in 0..menu.n_items() {
        let action = menu
            .item_attribute_value(i, "action", None)
            .and_then(|v| v.get::<String>())
            .unwrap_or_default();
        let target = menu
            .item_attribute_value(i, "target", None)
            .and_then(|v| v.get::<u64>());
        if action.ends_with("-tab") || action.ends_with("-tab-target") {
            assert_eq!(
                target,
                Some(42),
                "item {action} must carry the tab id, or GTK disables it"
            );
            saw_targeted += 1;
        } else {
            assert_eq!(target, None, "the plain New Session item takes no target");
        }
    }
    assert_eq!(
        saw_targeted, 4,
        "rename, close, detach, copy are all targeted"
    );
}
