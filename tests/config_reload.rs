//! Config store behavior through the controller: fresh install, live reload,
//! and CLI precedence. Uses explicit paths, so no process-wide env mutation
//! and these tests can run in parallel.

use std::fs;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, SystemTime};

use mutiny::app::AppController;
use mutiny::config::{self, Overrides, ResolvedConfig};

struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("mutiny-cfgtest-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }
    fn config(&self) -> PathBuf {
        self.0.join("config.toml")
    }
    fn audit(&self) -> PathBuf {
        self.0.join("audit.jsonl")
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Writes the file and forces a distinct mtime, so reload detection does not
/// depend on filesystem timestamp granularity.
fn write_config(path: &Path, body: &str) {
    fs::write(path, body).unwrap();
    let later = SystemTime::now() + Duration::from_secs(2);
    fs::File::options()
        .write(true)
        .open(path)
        .unwrap()
        .set_modified(later)
        .unwrap();
}

/// The reload check is throttled to 250 ms; wait past it before asking.
fn settle() {
    thread::sleep(Duration::from_millis(320));
}

fn controller(dir: &TempDir, overrides: Overrides) -> std::rc::Rc<AppController> {
    AppController::with_config(None, dir.audit(), dir.config(), overrides)
}

#[test]
fn fresh_install_runs_on_defaults_then_reads_a_config_file() {
    let dir = TempDir::new("fresh");
    let ctrl = controller(&dir, Overrides::default());
    assert_eq!(
        *ctrl.config_ref(),
        ResolvedConfig::default(),
        "no config file means built-in defaults"
    );

    write_config(
        &dir.config(),
        "[font]\nfamily = \"Iosevka\"\nsize = 15\n[colors]\nbackground = \"#000000\"\n",
    );
    settle();
    assert!(
        ctrl.reload_config_if_changed(),
        "a newly created config file must be picked up"
    );
    let cfg = ctrl.config_ref();
    assert_eq!(cfg.font_description, "Iosevka 15");
    assert_eq!(cfg.background, config::parse_hex("#000000").unwrap());
    // Untouched keys still come from defaults.
    assert_eq!(cfg.initial_cols, ResolvedConfig::default().initial_cols);
}

#[test]
fn live_reload_applies_font_and_palette_changes() {
    let dir = TempDir::new("reload");
    write_config(
        &dir.config(),
        "[font]\nsize = 11\n[colors]\npalette = [\n\
         \"#000000\",\"#800000\",\"#008000\",\"#808000\",\n\
         \"#000080\",\"#800080\",\"#008080\",\"#c0c0c0\",\n\
         \"#808080\",\"#ff0000\",\"#00ff00\",\"#ffff00\",\n\
         \"#0000ff\",\"#ff00ff\",\"#00ffff\",\"#ffffff\"]\n",
    );
    let ctrl = controller(&dir, Overrides::default());
    assert_eq!(ctrl.config_ref().font_size, 11);
    assert_eq!(
        ctrl.config_ref().palette[1],
        config::parse_hex("#800000").unwrap()
    );

    // Edit both the font and the palette in place.
    write_config(
        &dir.config(),
        "[font]\nsize = 18\n[colors]\npalette = [\n\
         \"#111111\",\"#222222\",\"#333333\",\"#444444\",\n\
         \"#555555\",\"#666666\",\"#777777\",\"#888888\",\n\
         \"#999999\",\"#aaaaaa\",\"#bbbbbb\",\"#cccccc\",\n\
         \"#dddddd\",\"#eeeeee\",\"#f0f0f0\",\"#ffffff\"]\n",
    );
    settle();
    assert!(
        ctrl.reload_config_if_changed(),
        "edit must trigger a reload"
    );
    let cfg = ctrl.config_ref();
    assert_eq!(cfg.font_size, 18);
    assert_eq!(cfg.font_description, "Monospace 18");
    assert_eq!(cfg.palette[1], config::parse_hex("#222222").unwrap());
    assert_eq!(cfg.palette[15], config::parse_hex("#ffffff").unwrap());

    // No edit means no reload, so the tick is not doing work every time.
    assert!(!ctrl.reload_config_if_changed());
}

#[test]
fn cli_flags_beat_the_file_and_survive_live_reload() {
    let dir = TempDir::new("precedence");
    write_config(
        &dir.config(),
        "[font]\nfamily = \"FromFile\"\nsize = 12\n[colors]\nbackground = \"#101010\"\n",
    );
    let overrides = Overrides {
        font_family: Some(String::from("FromFlag")),
        font_size: Some(20),
        initial_cols: None,
        initial_rows: None,
    };
    let ctrl = controller(&dir, overrides);
    assert_eq!(ctrl.config_ref().font_description, "FromFlag 20");
    assert_eq!(
        ctrl.config_ref().background,
        config::parse_hex("#101010").unwrap()
    );

    // Edit the file: the overridden keys must not move, the rest must.
    write_config(
        &dir.config(),
        "[font]\nfamily = \"EditedFile\"\nsize = 9\n[colors]\nbackground = \"#202020\"\n",
    );
    settle();
    assert!(ctrl.reload_config_if_changed());
    let cfg = ctrl.config_ref();
    assert_eq!(
        cfg.font_description, "FromFlag 20",
        "a live reload must not discard a launch flag"
    );
    assert_eq!(
        cfg.background,
        config::parse_hex("#202020").unwrap(),
        "a key with no flag must follow the file"
    );
}

#[test]
fn invalid_edit_keeps_the_previous_config() {
    let dir = TempDir::new("invalid");
    write_config(&dir.config(), "[font]\nsize = 16\n");
    let ctrl = controller(&dir, Overrides::default());
    assert_eq!(ctrl.config_ref().font_size, 16);

    write_config(&dir.config(), "[font\nsize = ");
    settle();
    assert!(
        !ctrl.reload_config_if_changed(),
        "a malformed file must not replace the working config"
    );
    assert_eq!(ctrl.config_ref().font_size, 16);

    // Fixing the file recovers.
    write_config(&dir.config(), "[font]\nsize = 17\n");
    settle();
    assert!(ctrl.reload_config_if_changed());
    assert_eq!(ctrl.config_ref().font_size, 17);
}

#[test]
fn deleting_the_config_reverts_to_defaults() {
    let dir = TempDir::new("delete");
    write_config(&dir.config(), "[font]\nsize = 22\n");
    let ctrl = controller(&dir, Overrides::default());
    assert_eq!(ctrl.config_ref().font_size, 22);
    fs::remove_file(dir.config()).unwrap();
    settle();
    assert!(ctrl.reload_config_if_changed());
    assert_eq!(*ctrl.config_ref(), ResolvedConfig::default());
}

#[test]
fn geometry_and_scrollback_from_config_apply_to_new_panes() {
    let dir = TempDir::new("geometry");
    write_config(
        &dir.config(),
        "[geometry]\ninitial_cols = 120\ninitial_rows = 40\n[terminal]\nscrollback_lines = 500\n",
    );
    let ctrl = controller(&dir, Overrides::default());
    let (_w, _t, pid) = ctrl.create_window(None).expect("spawn pane");

    let size = ctrl.workspace.borrow().pane(pid).unwrap().size();
    assert_eq!(size.cols, 120, "initial cols must come from config");
    assert_eq!(size.rows, 40, "initial rows must come from config");

    // Scrollback depth is fixed at construction, so it caps at the configured
    // value rather than the built-in 10 000.
    let mut buf = String::new();
    for i in 0..2000 {
        buf.push_str(&format!("line {i}\r\n"));
    }
    ctrl.workspace
        .borrow_mut()
        .pane_mut(pid)
        .unwrap()
        .advance_bytes(buf.as_bytes());
    let history = ctrl
        .workspace
        .borrow()
        .pane(pid)
        .unwrap()
        .snapshot(None)
        .scrollback_lines;
    assert!(
        history <= 500,
        "configured scrollback must cap history, got {history}"
    );
    assert!(history > 0, "expected some history to be retained");
}

#[test]
fn cli_geometry_flags_beat_config_geometry() {
    let dir = TempDir::new("geomflag");
    write_config(
        &dir.config(),
        "[geometry]\ninitial_cols = 90\ninitial_rows = 25\n",
    );
    let ctrl = controller(
        &dir,
        Overrides {
            initial_cols: Some(132),
            initial_rows: Some(43),
            ..Overrides::default()
        },
    );
    let (_w, _t, pid) = ctrl.create_window(None).expect("spawn pane");
    let size = ctrl.workspace.borrow().pane(pid).unwrap().size();
    assert_eq!(size.cols, 132);
    assert_eq!(size.rows, 43);
}
