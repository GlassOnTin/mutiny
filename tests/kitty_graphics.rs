//! Kitty graphics vertical slice: protocol placement, texture rendering, and
//! the present-media verb. The render assertion needs GTK and skips without a
//! display; CI runs it under Xvfb.

use std::path::PathBuf;
use std::thread;
use std::time::Duration;

use mutiny::app::AppController;
use mutiny::config::Overrides;
use mutiny::engine::PaneEngine;
use mutiny::ipc::{IpcRequest, IpcResponse, send_request};

/// An 8x8 solid red PNG, generated once and embedded so the test needs no
/// external fixture.
const PNG_BASE64: &str = "iVBORw0KGgoAAAANSUhEUgAAAAgAAAAICAYAAADED76LAAAAEklEQVR4nGP4z8DwHx9mGBkKAMLXf4EvceABAAAAAElFTkSuQmCC";

fn png_bytes() -> Vec<u8> {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD
        .decode(PNG_BASE64)
        .expect("embedded png")
}

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("mutiny-gfx-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn kitty_apc_transmit_and_place_reaches_the_store_and_replies_ok() {
    let mut eng = PaneEngine::new_headless(80, 24);
    let seq = format!("\x1b_Ga=t,f=100,i=5,c=10,r=5;{PNG_BASE64}\x1b\\");
    eng.advance_bytes(seq.as_bytes());

    let store = eng.graphics();
    let image = store.image(5).expect("image stored");
    assert_eq!(image.data, png_bytes());
    assert_eq!(store.placements().len(), 1, "a=t must place");
    let p = store.placements()[0];
    assert_eq!((p.image_id, p.cols, p.rows), (5, 10, 5));

    let reply = String::from_utf8_lossy(eng.pending_pty_writes()).into_owned();
    assert!(reply.contains("i=5;OK"), "expected OK reply, got {reply:?}");
}

#[test]
fn kitty_chunked_transmission_and_quiet_mode() {
    let mut eng = PaneEngine::new_headless(80, 24);
    let data = png_bytes();
    use base64::Engine as _;
    let b64 = base64::engine::general_purpose::STANDARD.encode(&data);
    // Kitty chunks split at 4-character base64 boundaries so each chunk is
    // independently decodable; an arbitrary midpoint is not valid base64.
    let cut = (b64.len() / 2 / 4) * 4;
    let (first, second) = b64.split_at(cut);

    eng.advance_bytes(format!("\x1b_Ga=t,f=100,m=1,q=1;{first}\x1b\\").as_bytes());
    assert!(eng.graphics().images().is_empty(), "partial must not store");
    assert!(
        eng.pending_pty_writes().is_empty(),
        "q=1 must suppress the intermediate reply"
    );

    eng.advance_bytes(format!("\x1b_Gm=0,q=1;{second}\x1b\\").as_bytes());
    assert_eq!(eng.graphics().images().len(), 1);
    assert_eq!(eng.graphics().images()[0].data, data);
    assert!(eng.pending_pty_writes().is_empty(), "q=1 suppresses OK");
}

#[test]
fn kitty_error_reply_is_sanitized() {
    let mut eng = PaneEngine::new_headless(80, 24);
    // Unsupported format produces an error reply that must not carry escapes.
    eng.advance_bytes(b"\x1b_Ga=T,f=999;i=1\x1b\\");
    let reply = String::from_utf8_lossy(eng.pending_pty_writes()).into_owned();
    assert!(reply.starts_with("\x1b_G"), "got {reply:?}");
    assert!(reply.ends_with("\x1b\\"), "got {reply:?}");
    let body = &reply[2..reply.len() - 2];
    assert!(
        !body.contains('\x1b'),
        "reply body must be escape-free: {body:?}"
    );
    assert!(body.contains("unsupported graphics format"), "got {body:?}");
}

#[test]
fn present_media_verb_places_an_image_inline() {
    let dir = temp_dir("verb");
    let image_path = dir.join("chart.png");
    std::fs::write(&image_path, png_bytes()).unwrap();
    let sock = dir.join("mutiny.sock");

    let (stop_tx, stop_rx) = std::sync::mpsc::channel();
    // Rc is not Send, so the server reports its graphics state over a channel
    // instead of returning the controller.
    let (report_tx, report_rx) = std::sync::mpsc::channel();
    let sock_clone = sock.clone();
    let audit = dir.join("audit.jsonl");
    let cfg = dir.join("config.toml");
    let server = thread::spawn(move || {
        let ctrl = AppController::with_config(
            Some((String::from("/bin/sh"), Vec::new())),
            audit,
            cfg,
            Overrides::default(),
        );
        ctrl.start_ipc_listener(&sock_clone).expect("listener");
        let (_w, _t, pid) = ctrl.create_window(None).expect("pane");
        while stop_rx.try_recv().is_err() {
            ctrl.poll_once();
            thread::sleep(Duration::from_millis(5));
        }
        let ws = ctrl.workspace.borrow();
        let store = ws.pane(pid).unwrap().graphics();
        let summary: Vec<(u32, u32, u16, u16)> = store
            .placements()
            .iter()
            .map(|p| (p.image_id, p.placement_id, p.cols, p.rows))
            .collect();
        let ids: Vec<u32> = store.images().iter().map(|i| i.id).collect();
        report_tx.send((ids, summary)).unwrap();
    });

    thread::sleep(Duration::from_millis(200));
    let resp = send_request(
        &sock,
        &IpcRequest::PresentMedia {
            pane: None,
            path: image_path.clone(),
            cols: Some(20),
            rows: Some(8),
        },
        Duration::from_secs(5),
    )
    .expect("present-media reply");

    let (image_id, placement_id) = match resp {
        IpcResponse::Ok {
            image_id: Some(i),
            placement_id: Some(p),
            ..
        } => (i, p),
        other => panic!("unexpected present-media response: {other:?}"),
    };

    // A missing file is a clean error, not a panic.
    let missing = send_request(
        &sock,
        &IpcRequest::PresentMedia {
            pane: None,
            path: dir.join("nope.png"),
            cols: None,
            rows: None,
        },
        Duration::from_secs(5),
    )
    .expect("missing-file reply");
    assert!(matches!(missing, IpcResponse::Error { .. }));

    stop_tx.send(()).unwrap();
    server.join().expect("server");
    let (ids, placements) = report_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("report");
    assert!(ids.contains(&image_id), "image must be stored: {ids:?}");
    let placement = placements
        .iter()
        .find(|(_, pid, _, _)| *pid == placement_id)
        .expect("placement");
    assert_eq!((placement.2, placement.3), (20, 8));
    let _ = std::fs::remove_dir_all(&dir);
}

/// The done-when render assertion: presenting an image produces a texture node
/// in the widget's render-node tree. Both PNG and raw RGBA are checked here in
/// one function because GTK must stay on a single thread and libtest spawns a
/// thread per test.
#[test]
fn presented_images_render_as_texture_nodes() {
    if gtk4::init().is_err() {
        eprintln!("skipping render assertion: no display (CI runs under Xvfb)");
        return;
    }
    use gtk4::prelude::*;

    let dir = temp_dir("render");
    let ctrl = AppController::with_config(
        Some((String::from("/bin/sh"), Vec::new())),
        dir.join("audit.jsonl"),
        dir.join("config.toml"),
        Overrides::default(),
    );
    let (_w, _t, pid) = ctrl.create_window(None).expect("pane");
    let view = mutiny::ui::terminal_view::TerminalView::new(pid, &ctrl);
    view.size_allocate(&gtk4::gdk::Rectangle::new(0, 0, 900, 600), -1);
    while gtk4::glib::MainContext::default().iteration(false) {}

    // No image yet: the tree must contain no texture node.
    let before = gtk4::Snapshot::new();
    view.render_snapshot(&before);
    assert!(
        !has_texture_node(&before.to_node().expect("node")),
        "an empty pane must render no texture"
    );

    // Encoded PNG via the present-media path.
    ctrl.workspace
        .borrow_mut()
        .pane_mut(pid)
        .unwrap()
        .present_image(png_bytes(), 10, 5)
        .expect("present");
    let snapshot = gtk4::Snapshot::new();
    view.render_snapshot(&snapshot);
    assert!(
        has_texture_node(&snapshot.to_node().expect("node")),
        "a presented PNG must render as a texture node"
    );

    // Raw RGBA through the protocol path, decoded into a memory texture.
    use base64::Engine as _;
    let pixels = vec![0u8; 4 * 4 * 4];
    let b64 = base64::engine::general_purpose::STANDARD.encode(&pixels);
    let seq = format!("\x1b_Ga=t,f=32,s=4,v=4,c=4,r=2;{b64}\x1b\\");
    ctrl.workspace
        .borrow_mut()
        .pane_mut(pid)
        .unwrap()
        .advance_bytes(seq.as_bytes());
    assert_eq!(
        ctrl.workspace
            .borrow()
            .pane(pid)
            .unwrap()
            .graphics()
            .images()
            .len(),
        2,
        "raw RGBA image must be stored"
    );
    let raw_snapshot = gtk4::Snapshot::new();
    view.render_snapshot(&raw_snapshot);
    assert!(
        has_texture_node(&raw_snapshot.to_node().expect("node")),
        "raw RGBA must render as a texture"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

fn has_texture_node(node: &gtk4::gsk::RenderNode) -> bool {
    if node.node_type() == gtk4::gsk::RenderNodeType::TextureNode {
        return true;
    }
    node.children().iter().any(has_texture_node)
}
