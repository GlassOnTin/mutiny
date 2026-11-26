//! Split ratio application and drag write-back. Needs GTK, so it skips without
//! a display; CI runs it under Xvfb. All GTK work stays in one test function
//! because libtest spawns a thread per test and GTK requires a single thread.

use std::collections::HashMap;
use std::rc::Rc;

use gtk4::prelude::*;
use mutiny::app::AppController;
use mutiny::config::Overrides;
use mutiny::topology::{PaneId, SplitAxis, SplitNode, TabId};
use mutiny::ui::window::build_split_widget;

fn find_paned(widget: &gtk4::Widget) -> Option<gtk4::Paned> {
    if let Ok(paned) = widget.clone().downcast::<gtk4::Paned>() {
        return Some(paned);
    }
    let mut child = widget.first_child();
    while let Some(c) = child {
        if let Some(found) = find_paned(&c) {
            return Some(found);
        }
        child = c.next_sibling();
    }
    None
}

fn root_ratio(ctrl: &Rc<AppController>, tab: TabId) -> f64 {
    let ws = ctrl.workspace.borrow();
    for win in ws.windows() {
        if let Some(t) = win.tabs.iter().find(|t| t.id == tab)
            && let SplitNode::Split { ratio, .. } = &t.root
        {
            return *ratio;
        }
    }
    f64::NAN
}

fn settle() {
    for _ in 0..50 {
        while gtk4::glib::MainContext::default().iteration(false) {}
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

#[test]
fn split_renders_at_stored_ratio_and_drag_writes_back() {
    if gtk4::init().is_err() {
        eprintln!("skipping: no display (CI runs under Xvfb)");
        return;
    }

    let dir = std::env::temp_dir().join(format!("mutiny-ratio-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let ctrl = AppController::with_config(
        Some((String::from("/bin/sh"), Vec::new())),
        dir.join("audit.jsonl"),
        dir.join("config.toml"),
        Overrides::default(),
    );
    let (_w, tab, p1): (mutiny::topology::WindowId, TabId, PaneId) =
        ctrl.create_window(None).expect("pane");
    let p2 = ctrl
        .split_pane(Some(p1), SplitAxis::Horizontal, None)
        .expect("split")
        .2;
    let _ = p2;

    // Store an uneven ratio, then build the widget tree from the model.
    assert!(ctrl.set_split_ratio(tab, &[], 0.30));
    let tree = ctrl
        .workspace
        .borrow()
        .windows()
        .iter()
        .find_map(|w| w.tabs.iter().find(|t| t.id == tab))
        .map(|t| t.root.clone())
        .expect("tree");
    let mut views = HashMap::new();
    let widget = build_split_widget(&tree, &ctrl, &mut views, tab, &[]);

    let window = gtk4::Window::new();
    window.set_default_size(800, 600);
    window.set_child(Some(&widget));
    window.present();
    settle();

    let paned = find_paned(&widget).expect("a Paned must exist for a split");
    let total = paned.width();
    assert!(total > 0, "paned must be allocated, got {total}");
    let applied = f64::from(paned.position()) / f64::from(total);
    eprintln!(
        "applied ratio {applied:.3} (position {}, total {total})",
        paned.position()
    );
    assert!(
        (applied - 0.30).abs() < 0.06,
        "split must render at its stored ratio 0.30, got {applied:.3}"
    );

    // Simulate a user drag by moving the divider, which is exactly what the
    // handle does. The write-back handler must update the model.
    let target = (0.62 * f64::from(total)) as i32;
    paned.set_position(target);
    settle();
    let written = root_ratio(&ctrl, tab);
    eprintln!("written-back ratio {written:.3}");
    assert!(
        (written - 0.62).abs() < 0.06,
        "a drag must write the ratio back to the tree, got {written:.3}"
    );

    // And the written ratio is what a rebuild would apply again.
    assert!(ctrl.set_split_ratio(tab, &[], written));
    let tree2 = ctrl
        .workspace
        .borrow()
        .windows()
        .iter()
        .find_map(|w| w.tabs.iter().find(|t| t.id == tab))
        .map(|t| t.root.clone())
        .expect("tree");
    let mut views2 = HashMap::new();
    let widget2 = build_split_widget(&tree2, &ctrl, &mut views2, tab, &[]);
    window.set_child(Some(&widget2));
    settle();
    let paned2 = find_paned(&widget2).expect("paned after rebuild");
    let rebuilt = f64::from(paned2.position()) / f64::from(paned2.width().max(1));
    assert!(
        (rebuilt - written).abs() < 0.06,
        "a rebuild must reapply the persisted ratio, got {rebuilt:.3} vs {written:.3}"
    );

    window.close();
    settle();
    let _ = std::fs::remove_dir_all(&dir);
}

/// The model operations the drag handlers call, exercised headlessly. The GTK
/// gesture wiring itself cannot be driven without a real seat, so this covers
/// the behavior the gestures invoke rather than the gestures.
#[test]
fn drag_operations_work_through_the_controller() {
    let dir = std::env::temp_dir().join(format!("mutiny-dragops-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let ctrl = AppController::with_config(
        Some((String::from("/bin/sh"), Vec::new())),
        dir.join("audit.jsonl"),
        dir.join("config.toml"),
        Overrides::default(),
    );
    let (w, t1, p1) = ctrl.create_window(None).expect("window");
    let t2 = ctrl.create_tab(Some(w), None).expect("tab2").1;
    let t3 = ctrl.create_tab(Some(w), None).expect("tab3").1;

    let order = || {
        ctrl.workspace
            .borrow()
            .window(w)
            .unwrap()
            .tabs
            .iter()
            .map(|t| t.id)
            .collect::<Vec<_>>()
    };
    assert_eq!(order(), vec![t1, t2, t3]);

    // Drag-to-reorder: drop the last tab on the first slot.
    ctrl.reorder_tab(w, t3, 0);
    assert_eq!(order(), vec![t3, t1, t2]);

    // Drag-to-detach: split a pane off, then detach it as the drop handler does.
    let p2 = ctrl
        .split_pane(Some(p1), SplitAxis::Horizontal, None)
        .expect("split")
        .2;
    let before = ctrl.workspace.borrow().windows().len();
    let detached = ctrl.detach_split(Some(p2)).expect("detach");
    assert_eq!(ctrl.workspace.borrow().windows().len(), before + 1);
    assert_ne!(detached.0, w);
    // The detached pane keeps its identity and its engine.
    assert!(ctrl.workspace.borrow().pane(p2).is_some());

    // Cross-window drag-to-redock: drop the detached window's sole tab back onto
    // slot 1 of window `w` -> redocks without restarting `p2` and closes the
    // now-empty detached window.
    assert!(ctrl.move_tab_to_window(detached.1, w, Some(1)));
    assert_eq!(ctrl.workspace.borrow().windows().len(), before);
    assert!(ctrl.workspace.borrow().window(detached.0).is_none());
    assert_eq!(order(), vec![t3, detached.1, t1, t2]);
    assert!(ctrl.workspace.borrow().pane(p2).is_some());

    let _ = std::fs::remove_dir_all(&dir);
}
