use std::cell::{Cell, RefCell};
use std::rc::{Rc, Weak};

use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::{Column, Line, Point};
use alacritty_terminal::selection::SelectionType;
use alacritty_terminal::term::cell::Flags as CellFlags;
use alacritty_terminal::vte::ansi::{Color as AnsiColor, NamedColor, Rgb};
use gtk4::gdk;
use gtk4::glib;
use gtk4::graphene;
use gtk4::pango;
use gtk4::prelude::*;
use gtk4::subclass::prelude::*;

use crate::action::build_context_menu_model;
use crate::app::AppController;
use crate::config::ResolvedConfig;
use crate::engine::{GridSize, KeyInput, KeyKind};
use crate::graphics::{Image, ImageFormat};
use crate::topology::PaneId;

// Font, palette, and padding come from the config store, not constants here.
// See src/config.rs for the key set and defaults.

mod imp {
    use super::*;

    #[derive(Default)]
    pub struct TerminalViewInner {
        pub pane_id: Cell<Option<PaneId>>,
        pub controller: RefCell<Weak<AppController>>,
        pub font_desc: RefCell<Option<pango::FontDescription>>,
        /// Test-only: record where each glyph was drawn, so a test can prove
        /// text lands on the grid. Off in normal use.
        pub record_glyphs: Cell<bool>,
        pub glyph_positions: RefCell<Vec<(usize, usize, f32)>>,
        pub cell_w: Cell<f32>,
        pub cell_h: Cell<f32>,
        pub drag_origin_px: Cell<(f64, f64)>,
        /// Last pointer position, so a wheel event knows which cell it is over.
        pub pointer_px: Cell<(f64, f64)>,
        pub context_popover: RefCell<Option<gtk4::PopoverMenu>>,
        pub im_context: RefCell<Option<gtk4::IMMulticontext>>,
        /// In-progress composition text, drawn at the cursor until committed.
        pub preedit: RefCell<String>,
        /// Decoded textures keyed by image id, invalidated when the graphics
        /// store's generation changes so images are not decoded every frame.
        pub texture_cache: RefCell<std::collections::HashMap<u32, gdk::Texture>>,
        pub graphics_generation: Cell<u64>,
        /// Per-row render nodes. GtkSnapshot is immediate-mode, so "repaint only
        /// the damaged region" means rebuilding only damaged row nodes and
        /// re-appending cached ones, not partial framebuffer updates.
        pub row_cache: RefCell<Vec<Option<gtk4::gsk::RenderNode>>>,
        pub(crate) cache_key: RefCell<Option<RenderCacheKey>>,
        /// Rows rebuilt by the most recent snapshot. Test instrumentation.
        pub rows_rebuilt: Cell<usize>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for TerminalViewInner {
        const NAME: &'static str = "MutinyTerminalView";
        type Type = super::TerminalView;
        type ParentType = gtk4::Widget;
    }

    impl ObjectImpl for TerminalViewInner {
        fn constructed(&self) {
            self.parent_constructed();
            let obj = self.obj();
            obj.set_focusable(true);
            obj.set_can_focus(true);
            obj.set_hexpand(true);
            obj.set_vexpand(true);

            let popover = gtk4::PopoverMenu::from_model(Some(&build_context_menu_model()));
            popover.set_parent(&*obj);
            popover.set_has_arrow(false);
            *self.context_popover.borrow_mut() = Some(popover);
        }

        fn dispose(&self) {
            if let Some(popover) = self.context_popover.borrow_mut().take() {
                popover.unparent();
            }
        }
    }

    impl WidgetImpl for TerminalViewInner {
        fn size_allocate(&self, width: i32, height: i32, baseline: i32) {
            self.parent_size_allocate(width, height, baseline);
            self.obj().recompute_grid_dimensions(width, height);
        }

        fn snapshot(&self, snapshot: &gtk4::Snapshot) {
            self.obj().render_snapshot(snapshot);
        }
    }
}

glib::wrapper! {
    pub struct TerminalView(ObjectSubclass<imp::TerminalViewInner>)
        @extends gtk4::Widget,
        @implements gtk4::Accessible, gtk4::Buildable, gtk4::ConstraintTarget;
}

impl TerminalView {
    pub fn new(pane_id: PaneId, controller: &Rc<AppController>) -> Self {
        let view: Self = glib::Object::new();
        let imp = view.imp();
        imp.pane_id.set(Some(pane_id));
        *imp.controller.borrow_mut() = Rc::downgrade(controller);
        view.refresh_font();
        view.install_controllers();
        view
    }

    /// Rebuilds the Pango font and cell metrics from the config store. Called
    /// at construction and again on live reload, so a font change takes effect
    /// without recreating the pane.
    pub fn refresh_font(&self) {
        let Some(ctrl) = self.upgrade_controller() else {
            return;
        };
        let desc = pango::FontDescription::from_string(&ctrl.config_ref().font_description);
        let ctx = self.pango_context();
        let metrics = ctx.metrics(Some(&desc), None);
        let cw = ((metrics.approximate_char_width() as f32) / (pango::SCALE as f32))
            .ceil()
            .max(7.0);
        let ch = (((metrics.ascent() + metrics.descent()) as f32) / (pango::SCALE as f32))
            .ceil()
            .max(14.0);
        self.imp().cell_w.set(cw);
        self.imp().cell_h.set(ch);
        *self.imp().font_desc.borrow_mut() = Some(desc);
        // Cell size changed, so the grid must be recomputed and the PTY resized.
        self.recompute_grid_dimensions(self.width(), self.height());
        self.update_im_cursor_location();
        self.queue_draw();
    }

    pub fn pane_id(&self) -> Option<PaneId> {
        self.imp().pane_id.get()
    }

    /// The IM context wired to this view's key controller. Exposed so tests can
    /// drive composition and assert the commit path.
    pub fn im_context(&self) -> Option<gtk4::IMMulticontext> {
        self.imp().im_context.borrow().clone()
    }

    /// Current in-progress composition text, empty when nothing is composing.
    pub fn preedit_text(&self) -> String {
        self.imp().preedit.borrow().clone()
    }

    fn upgrade_controller(&self) -> Option<Rc<AppController>> {
        self.imp().controller.borrow().upgrade()
    }

    /// Text committed by the IME: composed CJK, dead-key results, compose
    /// sequences. Written as UTF-8 directly rather than bracketed, because it
    /// is typed input, not a paste.
    pub fn commit_im_text(&self, text: &str) {
        if text.is_empty() {
            return;
        }
        let (Some(ctrl), Some(pid)) = (self.upgrade_controller(), self.pane_id()) else {
            return;
        };
        *self.imp().preedit.borrow_mut() = String::new();
        {
            let mut ws = ctrl.workspace.borrow_mut();
            if let Some(eng) = ws.pane_mut(pid) {
                eng.scroll(Scroll::Bottom);
                let _ = eng.write_to_pty(text.as_bytes());
            }
        }
        self.queue_draw();
    }

    /// Tells the IME where the cursor is so its candidate window lands nearby.
    fn update_im_cursor_location(&self) {
        let (Some(ctrl), Some(pid)) = (self.upgrade_controller(), self.pane_id()) else {
            return;
        };
        let Some(im) = self.imp().im_context.borrow().clone() else {
            return;
        };
        let (row, col) = {
            let ws = ctrl.workspace.borrow();
            let Some(eng) = ws.pane(pid) else { return };
            let point = eng.term().grid().cursor.point;
            (point.line.0, point.column.0)
        };
        let cw = self.imp().cell_w.get();
        let ch = self.imp().cell_h.get();
        let padding = self.padding_px();
        im.set_cursor_location(&gdk::Rectangle::new(
            (padding + col as f32 * cw) as i32,
            (padding + row as f32 * ch) as i32,
            cw as i32,
            ch as i32,
        ));
    }

    fn texture_for(&self, eng: &crate::engine::PaneEngine, image_id: u32) -> Option<gdk::Texture> {
        if let Some(found) = self.imp().texture_cache.borrow().get(&image_id) {
            return Some(found.clone());
        }
        let image = eng.graphics().image(image_id)?;
        let texture = decode_texture(image)?;
        self.imp()
            .texture_cache
            .borrow_mut()
            .insert(image_id, texture.clone());
        Some(texture)
    }

    fn padding_px(&self) -> f32 {
        self.upgrade_controller()
            .map(|c| c.config_ref().padding_px)
            .unwrap_or_else(|| crate::config::ResolvedConfig::default().padding_px)
    }

    fn px_to_cell(&self, x: f64, y: f64) -> (usize, usize) {
        let cw = self.imp().cell_w.get().max(1.0) as f64;
        let ch = self.imp().cell_h.get().max(1.0) as f64;
        let padding = self.padding_px() as f64;
        let col = ((x - padding).max(0.0) / cw).floor() as usize;
        let row = ((y - padding).max(0.0) / ch).floor() as usize;
        (col, row)
    }

    fn recompute_grid_dimensions(&self, width_px: i32, height_px: i32) {
        // An unrealized widget has no allocation. Resizing from 0x0 would
        // shrink the pane to the minimum grid and wrap every wide glyph, so
        // wait for a real allocation instead.
        if width_px <= 0 || height_px <= 0 {
            return;
        }
        let Some(pane_id) = self.pane_id() else {
            return;
        };
        let Some(ctrl) = self.upgrade_controller() else {
            return;
        };
        let cw = self.imp().cell_w.get().max(1.0);
        let ch = self.imp().cell_h.get().max(1.0);
        let padding = ctrl.config_ref().padding_px;
        let usable_w = (width_px as f32 - 2.0 * padding).max(cw * 4.0);
        let usable_h = (height_px as f32 - 2.0 * padding).max(ch * 2.0);
        let cols = (usable_w / cw).floor() as usize;
        let rows = (usable_h / ch).floor() as usize;
        let grid_size = GridSize::new(cols, rows, cw as u16, ch as u16);

        let mut ws = ctrl.workspace.borrow_mut();
        if let Some(eng) = ws.pane_mut(pane_id) {
            eng.resize(grid_size);
        }
    }

    fn install_controllers(&self) {
        let key_ctrl = gtk4::EventControllerKey::new();

        // IME: dead keys, compose sequences, and CJK input. IMMulticontext
        // delegates to the system IM module (ibus, fcitx) and falls back to
        // GTK's simple context, which handles compose and dead keys itself.
        // set_im_context makes the controller filter keys through the IM
        // before connect_key_pressed sees them, so consumed keys never reach
        // the raw key encoder.
        let im = gtk4::IMMulticontext::new();
        im.set_client_widget(Some(self));
        key_ctrl.set_im_context(Some(&im));

        let view_weak = self.downgrade();
        im.connect_commit(move |_, text| {
            if let Some(view) = view_weak.upgrade() {
                view.commit_im_text(text);
            }
        });

        let view_weak = self.downgrade();
        im.connect_preedit_changed(move |im_ctx| {
            let Some(view) = view_weak.upgrade() else {
                return;
            };
            let (text, _attrs, _cursor) = im_ctx.preedit_string();
            *view.imp().preedit.borrow_mut() = text.to_string();
            view.queue_draw();
        });

        let im_focus = im.clone();
        let focus = gtk4::EventControllerFocus::new();
        focus.connect_enter(move |_| im_focus.focus_in());
        let im_leave = im.clone();
        focus.connect_leave(move |_| im_leave.focus_out());
        self.add_controller(focus);
        *self.imp().im_context.borrow_mut() = Some(im);

        let view_weak = self.downgrade();
        key_ctrl.connect_key_pressed(move |_, keyval, _keycode, state| {
            let Some(view) = view_weak.upgrade() else {
                return glib::Propagation::Proceed;
            };
            view.on_key_pressed(keyval, state)
        });
        self.add_controller(key_ctrl);

        // Left-click focuses pane and clears selection; drag selects into Primary Selection.
        let drag = gtk4::GestureDrag::new();
        drag.set_button(gdk::BUTTON_PRIMARY);
        let view_weak = self.downgrade();
        drag.connect_drag_begin(move |gesture, start_x, start_y| {
            let Some(view) = view_weak.upgrade() else {
                return;
            };
            view.grab_focus();
            view.imp().drag_origin_px.set((start_x, start_y));
            view.imp().pointer_px.set((start_x, start_y));
            if let (Some(pid), Some(ctrl)) = (view.pane_id(), view.upgrade_controller()) {
                let _ = ctrl.workspace.borrow_mut().focus_pane(pid);
                let shift = gesture
                    .current_event()
                    .map(|e| e.modifier_state().contains(gdk::ModifierType::SHIFT_MASK))
                    .unwrap_or(false);
                let (col, row) = view.px_to_cell(start_x, start_y);
                let mut ws = ctrl.workspace.borrow_mut();
                if let Some(eng) = ws.pane_mut(pid) {
                    if eng.mouse_mode() && !shift {
                        // The program owns the mouse: report the press rather
                        // than starting a mutiny selection.
                        let report =
                            crate::engine::mouse_report(0, col, row, false, eng.sgr_mouse());
                        let _ = eng.write_to_pty(&report);
                        return;
                    }
                    eng.clear_selection();
                }
                drop(ws);
                view.queue_draw();
            }
        });

        let view_weak = self.downgrade();
        drag.connect_drag_update(move |gesture, off_x, off_y| {
            let Some(view) = view_weak.upgrade() else {
                return;
            };
            let (sx, sy) = view.imp().drag_origin_px.get();
            view.imp().pointer_px.set((sx + off_x, sy + off_y));
            if let (Some(pid), Some(ctrl)) = (view.pane_id(), view.upgrade_controller()) {
                let shift = gesture
                    .current_event()
                    .map(|e| e.modifier_state().contains(gdk::ModifierType::SHIFT_MASK))
                    .unwrap_or(false);
                let mut ws = ctrl.workspace.borrow_mut();
                if let Some(eng) = ws.pane_mut(pid)
                    && eng.mouse_mode()
                    && !shift
                {
                    let (col, row) = view.px_to_cell(sx + off_x, sy + off_y);
                    let report = crate::engine::mouse_report(
                        crate::engine::MOUSE_MOTION_FLAG,
                        col,
                        row,
                        false,
                        eng.sgr_mouse(),
                    );
                    let _ = eng.write_to_pty(&report);
                    return;
                }
            }
            if off_x.hypot(off_y) < 3.0 {
                return;
            }
            let (sc, sr) = view.px_to_cell(sx, sy);
            let (ec, er) = view.px_to_cell(sx + off_x, sy + off_y);
            if let (Some(pid), Some(ctrl)) = (view.pane_id(), view.upgrade_controller()) {
                if let Some(eng) = ctrl.workspace.borrow_mut().pane_mut(pid) {
                    if eng.term().selection.is_none() {
                        eng.start_selection(sc, sr, SelectionType::Simple);
                    }
                    eng.update_selection(ec, er);
                }
                view.queue_draw();
            }
        });

        let view_weak = self.downgrade();
        drag.connect_drag_end(move |gesture, off_x, off_y| {
            let Some(view) = view_weak.upgrade() else {
                return;
            };
            if let (Some(pid), Some(ctrl)) = (view.pane_id(), view.upgrade_controller()) {
                let shift = gesture
                    .current_event()
                    .map(|e| e.modifier_state().contains(gdk::ModifierType::SHIFT_MASK))
                    .unwrap_or(false);
                let (sx, sy) = view.imp().drag_origin_px.get();
                let (col, row) = view.px_to_cell(sx + off_x, sy + off_y);
                let mut ws = ctrl.workspace.borrow_mut();
                if let Some(eng) = ws.pane_mut(pid)
                    && eng.mouse_mode()
                    && !shift
                {
                    let report = crate::engine::mouse_report(0, col, row, true, eng.sgr_mouse());
                    let _ = eng.write_to_pty(&report);
                    return;
                }
                drop(ws);
                let selected = ctrl
                    .workspace
                    .borrow()
                    .pane(pid)
                    .and_then(|e| e.selection_text());
                if let (Some(text), Some(display)) = (selected, gdk::Display::default()) {
                    // Linux primary selection on mouse highlight
                    display.primary_clipboard().set_text(&text);
                }
            }
        });
        self.add_controller(drag);

        // Middle-click pastes X11/Wayland primary selection.
        let middle = gtk4::GestureClick::new();
        middle.set_button(gdk::BUTTON_MIDDLE);
        let view_weak = self.downgrade();
        middle.connect_pressed(move |_, _, _, _| {
            let Some(view) = view_weak.upgrade() else {
                return;
            };
            view.grab_focus();
            let (Some(pid), Some(ctrl), Some(display)) = (
                view.pane_id(),
                view.upgrade_controller(),
                gdk::Display::default(),
            ) else {
                return;
            };
            let ctrl_weak: Weak<AppController> = Rc::downgrade(&ctrl);
            display
                .primary_clipboard()
                .read_text_async(gtk4::gio::Cancellable::NONE, move |res| {
                    if let (Ok(Some(text)), Some(c)) = (res, ctrl_weak.upgrade()) {
                        c.paste_into_pane(pid, &text);
                    }
                });
        });
        self.add_controller(middle);

        // Right-click opens generated context menu.
        let secondary = gtk4::GestureClick::new();
        secondary.set_button(gdk::BUTTON_SECONDARY);
        let view_weak = self.downgrade();
        secondary.connect_pressed(move |_, _, x, y| {
            let Some(view) = view_weak.upgrade() else {
                return;
            };
            view.grab_focus();
            if let (Some(pid), Some(ctrl)) = (view.pane_id(), view.upgrade_controller()) {
                let _ = ctrl.workspace.borrow_mut().focus_pane(pid);
            }
            if let Some(popover) = view.imp().context_popover.borrow().as_ref() {
                popover.set_pointing_to(Some(&gdk::Rectangle::new(x as i32, y as i32, 1, 1)));
                popover.popup();
            }
        });
        self.add_controller(secondary);

        // Track the pointer so a wheel event knows which cell it is over. A
        // scroll event carries no coordinates of its own.
        let motion = gtk4::EventControllerMotion::new();
        let view_weak = self.downgrade();
        motion.connect_motion(move |_, x, y| {
            if let Some(view) = view_weak.upgrade() {
                view.imp().pointer_px.set((x, y));
            }
        });
        self.add_controller(motion);

        // Mouse wheel. When the program has asked for mouse events (tmux, vim,
        // less), the wheel belongs to IT: scrolling mutiny's own scrollback
        // instead made tmux scrolling appear broken. Shift is the conventional
        // override that keeps local scrollback reachable.
        let scroll = gtk4::EventControllerScroll::new(gtk4::EventControllerScrollFlags::VERTICAL);
        let view_weak = self.downgrade();
        scroll.connect_scroll(move |controller, _dx, dy| {
            let Some(view) = view_weak.upgrade() else {
                return glib::Propagation::Proceed;
            };
            let shift = controller
                .current_event()
                .map(|e| e.modifier_state().contains(gdk::ModifierType::SHIFT_MASK))
                .unwrap_or(false);
            if let (Some(pid), Some(ctrl)) = (view.pane_id(), view.upgrade_controller()) {
                let (px, py) = view.imp().pointer_px.get();
                let (col, row) = view.px_to_cell(px, py);
                let mut ws = ctrl.workspace.borrow_mut();
                let Some(eng) = ws.pane_mut(pid) else {
                    return glib::Propagation::Proceed;
                };
                match crate::engine::wheel_action(eng.mouse_mode(), shift, dy) {
                    crate::engine::WheelAction::Report(button) => {
                        let report =
                            crate::engine::mouse_report(button, col, row, false, eng.sgr_mouse());
                        let _ = eng.write_to_pty(&report);
                        return glib::Propagation::Stop;
                    }
                    crate::engine::WheelAction::LocalScroll(lines) => {
                        eng.scroll(Scroll::Delta(lines));
                        drop(ws);
                        view.queue_draw();
                        return glib::Propagation::Stop;
                    }
                    crate::engine::WheelAction::Ignore => return glib::Propagation::Proceed,
                }
            }
            glib::Propagation::Proceed
        });
        self.add_controller(scroll);
    }

    fn on_key_pressed(&self, keyval: gdk::Key, state: gdk::ModifierType) -> glib::Propagation {
        let ctrl_mod = state.contains(gdk::ModifierType::CONTROL_MASK);
        let shift_mod = state.contains(gdk::ModifierType::SHIFT_MASK);
        let alt_mod = state.contains(gdk::ModifierType::ALT_MASK);
        let super_mod = state.contains(gdk::ModifierType::SUPER_MASK);

        let Some(kind) = map_gdk_key(keyval) else {
            return glib::Propagation::Proceed;
        };

        let (Some(pid), Some(ctrl)) = (self.pane_id(), self.upgrade_controller()) else {
            return glib::Propagation::Proceed;
        };

        let encoded = {
            let mut ws = ctrl.workspace.borrow_mut();
            let Some(eng) = ws.pane_mut(pid) else {
                return glib::Propagation::Proceed;
            };
            eng.scroll(Scroll::Bottom);
            eng.encode_key(KeyInput {
                kind,
                shift: shift_mod,
                ctrl: ctrl_mod,
                alt: alt_mod,
                super_key: super_mod,
            })
        };

        if let Some(bytes) = encoded {
            if let Some(eng) = ctrl.workspace.borrow_mut().pane_mut(pid) {
                let _ = eng.write_to_pty(&bytes);
            }
            self.queue_draw();
            return glib::Propagation::Stop;
        }
        glib::Propagation::Proceed
    }

    /// Renders the pane into a GtkSnapshot. Public so tests can inspect the
    /// resulting render-node tree for texture placements.
    pub fn render_snapshot(&self, snapshot: &gtk4::Snapshot) {
        let w = self.width() as f32;
        let h = self.height() as f32;
        if w <= 0.0 || h <= 0.0 {
            return;
        }

        let (Some(pid), Some(ctrl)) = (self.pane_id(), self.upgrade_controller()) else {
            return;
        };
        let cfg = ctrl.config_ref();
        let padding = cfg.padding_px;
        let bg = cfg.background;
        let fg = cfg.foreground;

        snapshot.append_color(&rgb_to_gdk(bg, 1.0), &graphene::Rect::new(0.0, 0.0, w, h));

        let cw = self.imp().cell_w.get();
        let ch = self.imp().cell_h.get();
        let base_font = self
            .imp()
            .font_desc
            .borrow()
            .clone()
            .unwrap_or_else(|| pango::FontDescription::from_string(&cfg.font_description));

        // Take damage before the immutable render borrow; it needs &mut Term.
        let damage = ctrl
            .workspace
            .borrow_mut()
            .pane_mut(pid)
            .map(|e| e.take_damage());

        let ws = ctrl.workspace.borrow();
        let is_active_pane = ws.active_pane_id() == Some(pid);
        let Some(eng) = ws.pane(pid) else {
            return;
        };
        let term = eng.term();
        let grid = term.grid();
        let display_offset = grid.display_offset() as i32;
        let cols = eng.size().cols;
        let rows = eng.size().rows;
        let sel_range = term.selection.as_ref().and_then(|s| s.to_range(term));
        let preedit = self.imp().preedit.borrow().clone();

        if self.imp().record_glyphs.get() {
            self.imp().glyph_positions.borrow_mut().clear();
        }

        let key = RenderCacheKey {
            cols,
            rows,
            cell_w: cw,
            cell_h: ch,
            display_offset: grid.display_offset(),
            config: cfg.clone(),
            graphics_generation: eng.graphics().generation(),
            preedit: preedit.clone(),
            selection: sel_range.as_ref().map(|r| {
                (
                    r.start.line.0,
                    r.start.column.0,
                    r.end.line.0,
                    r.end.column.0,
                )
            }),
        };
        let full_invalidate = self.imp().cache_key.borrow().as_ref() != Some(&key);
        *self.imp().cache_key.borrow_mut() = Some(key);

        let mut dirty = vec![full_invalidate; rows];
        if !full_invalidate {
            match damage {
                Some(crate::engine::RowDamage::Full) | None => dirty.fill(true),
                Some(crate::engine::RowDamage::Rows(list)) => {
                    for r in list {
                        if r < rows {
                            dirty[r] = true;
                        }
                    }
                }
            }
        }

        let pango_ctx = self.pango_context();
        let layout = pango::Layout::new(&pango_ctx);

        let mut rebuilt = 0usize;
        {
            let mut cache = self.imp().row_cache.borrow_mut();
            cache.resize(rows, None);
            for row_idx in 0..rows {
                if dirty[row_idx] || cache[row_idx].is_none() {
                    let sub = gtk4::Snapshot::new();
                    let line = Line(row_idx as i32 - display_offset);
                    let grid_row = &grid[line];
                    let y = padding + (row_idx as f32) * ch;

                    // A blank row appends nothing, and an empty snapshot yields
                    // no node, which would defeat caching for every empty line.
                    // A transparent rect guarantees a node without drawing.
                    sub.append_color(
                        &gdk::RGBA::new(0.0, 0.0, 0.0, 0.0),
                        &graphene::Rect::new(0.0, y, w, ch),
                    );

                    // Pass 1: contiguous non-default background runs + selection highlight
                    let mut col = 0usize;
                    while col < cols {
                        let cell = &grid_row[Column(col)];
                        let pt = Point::new(line, Column(col));
                        let selected = sel_range.is_some_and(|r| r.contains(pt));
                        let inverse = cell.flags.contains(CellFlags::INVERSE);
                        let bg_rgb = if selected {
                            cfg.selection
                        } else if inverse {
                            resolve_color(cell.fg, fg, &cfg)
                        } else {
                            resolve_color(cell.bg, bg, &cfg)
                        };

                        if bg_rgb != bg {
                            let start_col = col;
                            col += 1;
                            while col < cols {
                                let next_cell = &grid_row[Column(col)];
                                let next_sel = sel_range
                                    .is_some_and(|r| r.contains(Point::new(line, Column(col))));
                                let next_bg = if next_sel {
                                    cfg.selection
                                } else if next_cell.flags.contains(CellFlags::INVERSE) {
                                    resolve_color(next_cell.fg, fg, &cfg)
                                } else {
                                    resolve_color(next_cell.bg, bg, &cfg)
                                };
                                if next_bg != bg_rgb {
                                    break;
                                }
                                col += 1;
                            }
                            let span_w = ((col - start_col) as f32) * cw;
                            sub.append_color(
                                &rgb_to_gdk(bg_rgb, 1.0),
                                &graphene::Rect::new(
                                    padding + (start_col as f32) * cw,
                                    y,
                                    span_w,
                                    ch,
                                ),
                            );
                        } else {
                            col += 1;
                        }
                    }

                    // Pass 2: styled text, one glyph cluster per cell, each
                    // placed at its OWN grid origin.
                    //
                    // Drawing a run as a single Pango layout used the font's
                    // natural advance (8.83 px for Monospace 11) while the grid
                    // and the cursor use the cell width (ceil -> 9 px). A run
                    // therefore ended short of its grid span and the cursor
                    // appeared to drift past the end of the text, by about
                    // 0.17 px per column. Per-cell placement is exact for any
                    // font, including fallbacks whose advance differs.
                    let mut col = 0usize;
                    let mut current: Option<RunStyle> = None;
                    let mut current_underline = false;
                    let mut cluster = String::new();
                    let recording = self.imp().record_glyphs.get();
                    while col < cols {
                        let cell = &grid_row[Column(col)];
                        if cell.flags.contains(CellFlags::WIDE_CHAR_SPACER) {
                            col += 1;
                            continue;
                        }
                        let underline = cell.flags.contains(CellFlags::UNDERLINE);
                        if cell.c == ' ' && !underline {
                            col += 1;
                            continue;
                        }

                        let style = RunStyle::from_cell(cell, &cfg);
                        let fg_rgba = rgb_to_gdk(style.fg, 1.0);
                        if current.as_ref() != Some(&style) || underline != current_underline {
                            let mut desc = base_font.clone();
                            if style.bold {
                                desc.set_weight(pango::Weight::Bold);
                            }
                            if style.italic {
                                desc.set_style(pango::Style::Italic);
                            }
                            layout.set_font_description(Some(&desc));
                            current = Some(style);
                            current_underline = underline;
                        }

                        // Combining marks belong to this cell's cluster, so they
                        // render on the base glyph instead of being dropped.
                        cluster.clear();
                        cluster.push(cell.c);
                        if let Some(marks) = cell.zerowidth() {
                            for mark in marks {
                                cluster.push(*mark);
                            }
                        }
                        layout.set_text(&cluster);

                        let x = padding + (col as f32) * cw;
                        sub.save();
                        sub.translate(&graphene::Point::new(x, y));
                        sub.append_layout(&layout, &fg_rgba);
                        sub.restore();

                        if underline {
                            sub.append_color(
                                &rgb_to_gdk(current.as_ref().unwrap().fg, 0.85),
                                &graphene::Rect::new(x, y + ch - 1.5, cw, 1.0),
                            );
                        }
                        if recording {
                            self.imp()
                                .glyph_positions
                                .borrow_mut()
                                .push((row_idx, col, x));
                        }
                        col += 1;
                    }

                    cache[row_idx] = sub.to_node();
                    rebuilt += 1;
                }
                if let Some(node) = &cache[row_idx] {
                    snapshot.append_node(node);
                }
            }
        }
        self.imp().rows_rebuilt.set(rebuilt);

        // Pass 3: Cursor (visible when at bottom of scrollback)
        if display_offset == 0 {
            let cursor_pt = grid.cursor.point;
            let cx = padding + (cursor_pt.column.0 as f32) * cw;
            let cy = padding + (cursor_pt.line.0 as f32) * ch;
            let cursor_rgba = rgb_to_gdk(cfg.cursor, if is_active_pane { 0.78 } else { 0.35 });
            if is_active_pane {
                snapshot.append_color(&cursor_rgba, &graphene::Rect::new(cx, cy, cw, ch));
            } else {
                snapshot.append_color(&cursor_rgba, &graphene::Rect::new(cx, cy, cw, 1.5));
                snapshot.append_color(
                    &cursor_rgba,
                    &graphene::Rect::new(cx, cy + ch - 1.5, cw, 1.5),
                );
                snapshot.append_color(&cursor_rgba, &graphene::Rect::new(cx, cy, 1.5, ch));
                snapshot.append_color(
                    &cursor_rgba,
                    &graphene::Rect::new(cx + cw - 1.5, cy, 1.5, ch),
                );
            }
        }

        // Kitty graphics placements, drawn over the cell grid and scrolled with
        // the content they are anchored to. Sorted by z-index so ordering is
        // deterministic.
        let history = grid.history_size() as i32;
        let generation = eng.graphics().generation();
        if self.imp().graphics_generation.get() != generation {
            self.imp().texture_cache.borrow_mut().clear();
            self.imp().graphics_generation.set(generation);
        }
        let mut placements: Vec<_> = eng.graphics().placements().iter().collect();
        placements.sort_by_key(|p| p.z_index);
        for placement in placements {
            let grid_line = placement.line - history;
            let viewport_row = grid_line + display_offset;
            if viewport_row < 0 || viewport_row >= rows as i32 {
                continue;
            }
            let Some(texture) = self.texture_for(eng, placement.image_id) else {
                continue;
            };
            let (w_cells, h_cells) = if placement.cols > 0 && placement.rows > 0 {
                (f32::from(placement.cols), f32::from(placement.rows))
            } else {
                (
                    (texture.width() as f32 / cw).ceil(),
                    (texture.height() as f32 / ch).ceil(),
                )
            };
            let x = padding + placement.column as f32 * cw;
            let y = padding + viewport_row as f32 * ch;
            snapshot.append_texture(
                &texture,
                &graphene::Rect::new(x, y, w_cells * cw, h_cells * ch),
            );
        }

        // Preedit: in-progress composition drawn at the cursor, underlined, so
        // CJK and dead-key input is visible before it commits.
        let preedit = self.imp().preedit.borrow().clone();
        if !preedit.is_empty() && display_offset == 0 {
            let cursor_pt = grid.cursor.point;
            let px = padding + (cursor_pt.column.0 as f32) * cw;
            let py = padding + (cursor_pt.line.0 as f32) * ch;
            layout.set_font_description(Some(&base_font));
            layout.set_text(&preedit);
            let (ink_w, _) = layout.pixel_size();
            snapshot.save();
            snapshot.translate(&graphene::Point::new(px, py));
            snapshot.append_layout(&layout, &rgb_to_gdk(fg, 1.0));
            snapshot.restore();
            // Underline drawn as a rect, matching how cell underlines render.
            snapshot.append_color(
                &rgb_to_gdk(fg, 0.85),
                &graphene::Rect::new(px, py + ch - 1.5, ink_w as f32, 1.0),
            );
        }

        // Pass 4: Subtle top accent bar on active split pane for instant visual orientation
        if is_active_pane {
            snapshot.append_color(
                &rgb_to_gdk(cfg.accent, 0.85),
                &graphene::Rect::new(0.0, 0.0, w, 2.0),
            );
        }
    }

    /// Rows rebuilt by the most recent `render_snapshot`. Zero means the frame
    /// reused every cached row node.
    /// Test-only: record the (row, column, x) of every glyph drawn in the next
    /// frame, so a test can assert text lands exactly on the cell grid.
    pub fn record_glyph_positions(&self, on: bool) {
        self.imp().record_glyphs.set(on);
        self.imp().glyph_positions.borrow_mut().clear();
    }

    pub fn glyph_positions(&self) -> Vec<(usize, usize, f32)> {
        self.imp().glyph_positions.borrow().clone()
    }

    pub fn cell_width(&self) -> f32 {
        self.imp().cell_w.get()
    }

    pub fn cell_height(&self) -> f32 {
        self.imp().cell_h.get()
    }

    pub fn rows_rebuilt_last_frame(&self) -> usize {
        self.imp().rows_rebuilt.get()
    }

    /// Drops the row cache, forcing a full rebuild on the next frame.
    pub fn invalidate_render_cache(&self) {
        self.imp().row_cache.borrow_mut().clear();
        *self.imp().cache_key.borrow_mut() = None;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RunStyle {
    fg: Rgb,
    bold: bool,
    italic: bool,
}

impl RunStyle {
    fn from_cell(cell: &alacritty_terminal::term::cell::Cell, cfg: &ResolvedConfig) -> Self {
        let fg = if cell.flags.contains(CellFlags::INVERSE) {
            resolve_color(cell.bg, cfg.background, cfg)
        } else {
            resolve_color(cell.fg, cfg.foreground, cfg)
        };
        Self {
            fg,
            bold: cell.flags.contains(CellFlags::BOLD),
            italic: cell.flags.contains(CellFlags::ITALIC),
        }
    }
}

/// Decodes stored image bytes into a GDK texture. PNG (and JPEG) go through
/// GDK's own decoder; raw formats become memory textures.
fn decode_texture(image: &Image) -> Option<gdk::Texture> {
    match image.format {
        ImageFormat::Png => {
            let bytes = glib::Bytes::from(&image.data);
            gdk::Texture::from_bytes(&bytes).ok()
        }
        ImageFormat::Rgba | ImageFormat::Rgb => {
            if image.width == 0 || image.height == 0 {
                return None;
            }
            let rgba = if image.format == ImageFormat::Rgba {
                image.data.clone()
            } else {
                let mut out = Vec::with_capacity(image.data.len() / 3 * 4);
                let (pixels, _remainder) = image.data.as_chunks::<3>();
                for px in pixels {
                    out.extend_from_slice(&[px[0], px[1], px[2], 255]);
                }
                out
            };
            let bytes = glib::Bytes::from(&rgba);
            Some(
                gdk::MemoryTexture::new(
                    image.width as i32,
                    image.height as i32,
                    gdk::MemoryFormat::R8g8b8a8,
                    &bytes,
                    (image.width * 4) as usize,
                )
                .upcast(),
            )
        }
    }
}

/// Everything that invalidates every cached row. Scroll, resize, font, config,
/// selection, preedit, and graphics changes all move or restyle content that
/// alacritty's line damage does not track.
#[derive(Clone, PartialEq)]
pub(crate) struct RenderCacheKey {
    cols: usize,
    rows: usize,
    cell_w: f32,
    cell_h: f32,
    display_offset: usize,
    config: ResolvedConfig,
    graphics_generation: u64,
    preedit: String,
    selection: Option<(i32, usize, i32, usize)>,
}

fn rgb_to_gdk(c: Rgb, alpha: f32) -> gdk::RGBA {
    gdk::RGBA::new(
        (c.r as f32) / 255.0,
        (c.g as f32) / 255.0,
        (c.b as f32) / 255.0,
        alpha,
    )
}

fn resolve_color(color: AnsiColor, fallback: Rgb, cfg: &ResolvedConfig) -> Rgb {
    match color {
        AnsiColor::Spec(rgb) => rgb,
        AnsiColor::Indexed(idx) => indexed_color(idx, cfg),
        AnsiColor::Named(named) => match named {
            NamedColor::Foreground | NamedColor::BrightForeground => cfg.foreground,
            NamedColor::Background => cfg.background,
            NamedColor::Cursor => cfg.cursor,
            NamedColor::Black | NamedColor::DimBlack => indexed_color(0, cfg),
            NamedColor::Red | NamedColor::DimRed => indexed_color(1, cfg),
            NamedColor::Green | NamedColor::DimGreen => indexed_color(2, cfg),
            NamedColor::Yellow | NamedColor::DimYellow => indexed_color(3, cfg),
            NamedColor::Blue | NamedColor::DimBlue => indexed_color(4, cfg),
            NamedColor::Magenta | NamedColor::DimMagenta => indexed_color(5, cfg),
            NamedColor::Cyan | NamedColor::DimCyan => indexed_color(6, cfg),
            NamedColor::White | NamedColor::DimWhite => indexed_color(7, cfg),
            NamedColor::BrightBlack => indexed_color(8, cfg),
            NamedColor::BrightRed => indexed_color(9, cfg),
            NamedColor::BrightGreen => indexed_color(10, cfg),
            NamedColor::BrightYellow => indexed_color(11, cfg),
            NamedColor::BrightBlue => indexed_color(12, cfg),
            NamedColor::BrightMagenta => indexed_color(13, cfg),
            NamedColor::BrightCyan => indexed_color(14, cfg),
            NamedColor::BrightWhite => indexed_color(15, cfg),
            _ => fallback,
        },
    }
}

/// ANSI 0-15 come from the configured palette; the 6x6x6 cube and grayscale
/// ramp are computed, as they are not part of the configurable 16.
fn indexed_color(idx: u8, cfg: &ResolvedConfig) -> Rgb {
    match idx {
        0..=15 => cfg.palette[idx as usize],
        16..=231 => {
            let v = idx - 16;
            let r = v / 36;
            let g = (v % 36) / 6;
            let b = v % 6;
            let step = |n: u8| if n == 0 { 0 } else { 55 + n * 40 };
            Rgb {
                r: step(r),
                g: step(g),
                b: step(b),
            }
        }
        232..=255 => {
            let shade = 8 + (idx - 232) * 10;
            Rgb {
                r: shade,
                g: shade,
                b: shade,
            }
        }
    }
}

fn map_gdk_key(keyval: gdk::Key) -> Option<KeyKind> {
    match keyval {
        gdk::Key::Return | gdk::Key::KP_Enter => Some(KeyKind::Enter),
        gdk::Key::Tab | gdk::Key::ISO_Left_Tab => Some(KeyKind::Tab),
        gdk::Key::BackSpace => Some(KeyKind::Backspace),
        gdk::Key::Escape => Some(KeyKind::Escape),
        gdk::Key::Up | gdk::Key::KP_Up => Some(KeyKind::Up),
        gdk::Key::Down | gdk::Key::KP_Down => Some(KeyKind::Down),
        gdk::Key::Left | gdk::Key::KP_Left => Some(KeyKind::Left),
        gdk::Key::Right | gdk::Key::KP_Right => Some(KeyKind::Right),
        gdk::Key::Home | gdk::Key::KP_Home => Some(KeyKind::Home),
        gdk::Key::End | gdk::Key::KP_End => Some(KeyKind::End),
        gdk::Key::Insert | gdk::Key::KP_Insert => Some(KeyKind::Insert),
        gdk::Key::Delete | gdk::Key::KP_Delete => Some(KeyKind::Delete),
        gdk::Key::Page_Up | gdk::Key::KP_Page_Up => Some(KeyKind::PageUp),
        gdk::Key::Page_Down | gdk::Key::KP_Page_Down => Some(KeyKind::PageDown),
        gdk::Key::F1 => Some(KeyKind::Function(1)),
        gdk::Key::F2 => Some(KeyKind::Function(2)),
        gdk::Key::F3 => Some(KeyKind::Function(3)),
        gdk::Key::F4 => Some(KeyKind::Function(4)),
        gdk::Key::F5 => Some(KeyKind::Function(5)),
        gdk::Key::F6 => Some(KeyKind::Function(6)),
        gdk::Key::F7 => Some(KeyKind::Function(7)),
        gdk::Key::F8 => Some(KeyKind::Function(8)),
        gdk::Key::F9 => Some(KeyKind::Function(9)),
        gdk::Key::F10 => Some(KeyKind::Function(10)),
        gdk::Key::F11 => Some(KeyKind::Function(11)),
        gdk::Key::F12 => Some(KeyKind::Function(12)),
        other => other.to_unicode().map(KeyKind::Char),
    }
}
