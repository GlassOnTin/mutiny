use std::cell::{Cell, RefCell};
use std::rc::{Rc, Weak};

use alacritty_terminal::grid::Scroll;
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
use crate::engine::{GridSize, KeyInput, KeyKind};
use crate::topology::PaneId;

const PADDING_PX: f32 = 6.0;
const DEFAULT_FONT: &str = "Monospace 11";

const DEFAULT_BG: Rgb = Rgb {
    r: 0x16,
    g: 0x18,
    b: 0x1d,
};
const DEFAULT_FG: Rgb = Rgb {
    r: 0xe6,
    g: 0xe9,
    b: 0xef,
};

mod imp {
    use super::*;

    #[derive(Default)]
    pub struct TerminalViewInner {
        pub pane_id: Cell<Option<PaneId>>,
        pub controller: RefCell<Weak<AppController>>,
        pub font_desc: RefCell<Option<pango::FontDescription>>,
        pub cell_w: Cell<f32>,
        pub cell_h: Cell<f32>,
        pub drag_origin_px: Cell<(f64, f64)>,
        pub context_popover: RefCell<Option<gtk4::PopoverMenu>>,
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

            let desc = pango::FontDescription::from_string(DEFAULT_FONT);
            let ctx = obj.pango_context();
            let metrics = ctx.metrics(Some(&desc), None);
            let cw = ((metrics.approximate_char_width() as f32) / (pango::SCALE as f32))
                .ceil()
                .max(7.0);
            let ch = (((metrics.ascent() + metrics.descent()) as f32) / (pango::SCALE as f32))
                .ceil()
                .max(14.0);
            self.cell_w.set(cw);
            self.cell_h.set(ch);
            *self.font_desc.borrow_mut() = Some(desc);

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
        view.install_controllers();
        view
    }

    pub fn pane_id(&self) -> Option<PaneId> {
        self.imp().pane_id.get()
    }

    fn upgrade_controller(&self) -> Option<Rc<AppController>> {
        self.imp().controller.borrow().upgrade()
    }

    fn px_to_cell(&self, x: f64, y: f64) -> (usize, usize) {
        let cw = self.imp().cell_w.get().max(1.0) as f64;
        let ch = self.imp().cell_h.get().max(1.0) as f64;
        let col = ((x - PADDING_PX as f64).max(0.0) / cw).floor() as usize;
        let row = ((y - PADDING_PX as f64).max(0.0) / ch).floor() as usize;
        (col, row)
    }

    fn recompute_grid_dimensions(&self, width_px: i32, height_px: i32) {
        let Some(pane_id) = self.pane_id() else {
            return;
        };
        let Some(ctrl) = self.upgrade_controller() else {
            return;
        };
        let cw = self.imp().cell_w.get().max(1.0);
        let ch = self.imp().cell_h.get().max(1.0);
        let usable_w = (width_px as f32 - 2.0 * PADDING_PX).max(cw * 4.0);
        let usable_h = (height_px as f32 - 2.0 * PADDING_PX).max(ch * 2.0);
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
        drag.connect_drag_begin(move |_, start_x, start_y| {
            let Some(view) = view_weak.upgrade() else {
                return;
            };
            view.grab_focus();
            view.imp().drag_origin_px.set((start_x, start_y));
            if let (Some(pid), Some(ctrl)) = (view.pane_id(), view.upgrade_controller()) {
                let _ = ctrl.workspace.borrow_mut().focus_pane(pid);
                if let Some(eng) = ctrl.workspace.borrow_mut().pane_mut(pid) {
                    eng.clear_selection();
                }
                view.queue_draw();
            }
        });

        let view_weak = self.downgrade();
        drag.connect_drag_update(move |_, off_x, off_y| {
            let Some(view) = view_weak.upgrade() else {
                return;
            };
            if off_x.hypot(off_y) < 3.0 {
                return;
            }
            let (sx, sy) = view.imp().drag_origin_px.get();
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
        drag.connect_drag_end(move |_, _, _| {
            let Some(view) = view_weak.upgrade() else {
                return;
            };
            if let (Some(pid), Some(ctrl)) = (view.pane_id(), view.upgrade_controller()) {
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

        // Mouse wheel scrolls scrollback history.
        let scroll = gtk4::EventControllerScroll::new(gtk4::EventControllerScrollFlags::VERTICAL);
        let view_weak = self.downgrade();
        scroll.connect_scroll(move |_, _dx, dy| {
            let Some(view) = view_weak.upgrade() else {
                return glib::Propagation::Proceed;
            };
            if let (Some(pid), Some(ctrl)) = (view.pane_id(), view.upgrade_controller()) {
                let lines = if dy < 0.0 { 3 } else { -3 };
                if let Some(eng) = ctrl.workspace.borrow_mut().pane_mut(pid) {
                    eng.scroll(Scroll::Delta(lines));
                }
                view.queue_draw();
                return glib::Propagation::Stop;
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

    fn render_snapshot(&self, snapshot: &gtk4::Snapshot) {
        let w = self.width() as f32;
        let h = self.height() as f32;
        if w <= 0.0 || h <= 0.0 {
            return;
        }

        snapshot.append_color(
            &rgb_to_gdk(DEFAULT_BG, 1.0),
            &graphene::Rect::new(0.0, 0.0, w, h),
        );

        let (Some(pid), Some(ctrl)) = (self.pane_id(), self.upgrade_controller()) else {
            return;
        };
        let cw = self.imp().cell_w.get();
        let ch = self.imp().cell_h.get();
        let base_font = self
            .imp()
            .font_desc
            .borrow()
            .clone()
            .unwrap_or_else(|| pango::FontDescription::from_string(DEFAULT_FONT));

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

        let pango_ctx = self.pango_context();
        let layout = pango::Layout::new(&pango_ctx);

        for row_idx in 0..rows {
            let line = Line(row_idx as i32 - display_offset);
            let grid_row = &grid[line];
            let y = PADDING_PX + (row_idx as f32) * ch;

            // Pass 1: contiguous non-default background runs + selection highlight
            let mut col = 0usize;
            while col < cols {
                let cell = &grid_row[Column(col)];
                let pt = Point::new(line, Column(col));
                let selected = sel_range.is_some_and(|r| r.contains(pt));
                let inverse = cell.flags.contains(CellFlags::INVERSE);
                let bg_rgb = if selected {
                    Rgb {
                        r: 0x3b,
                        g: 0x52,
                        b: 0x78,
                    }
                } else if inverse {
                    resolve_color(cell.fg, DEFAULT_FG)
                } else {
                    resolve_color(cell.bg, DEFAULT_BG)
                };

                if bg_rgb != DEFAULT_BG {
                    let start_col = col;
                    col += 1;
                    while col < cols {
                        let next_cell = &grid_row[Column(col)];
                        let next_sel =
                            sel_range.is_some_and(|r| r.contains(Point::new(line, Column(col))));
                        let next_bg = if next_sel {
                            Rgb {
                                r: 0x3b,
                                g: 0x52,
                                b: 0x78,
                            }
                        } else if next_cell.flags.contains(CellFlags::INVERSE) {
                            resolve_color(next_cell.fg, DEFAULT_FG)
                        } else {
                            resolve_color(next_cell.bg, DEFAULT_BG)
                        };
                        if next_bg != bg_rgb {
                            break;
                        }
                        col += 1;
                    }
                    let span_w = ((col - start_col) as f32) * cw;
                    snapshot.append_color(
                        &rgb_to_gdk(bg_rgb, 1.0),
                        &graphene::Rect::new(PADDING_PX + (start_col as f32) * cw, y, span_w, ch),
                    );
                } else {
                    col += 1;
                }
            }

            // Pass 2: styled text runs rendered as GskTextNode via Pango Layout
            let mut col = 0usize;
            let mut run_text = String::new();
            while col < cols {
                let cell = &grid_row[Column(col)];
                if cell.flags.contains(CellFlags::WIDE_CHAR_SPACER) {
                    col += 1;
                    continue;
                }
                if cell.c == ' ' && !cell.flags.contains(CellFlags::UNDERLINE) {
                    col += 1;
                    continue;
                }

                let start_col = col;
                let style = RunStyle::from_cell(cell);
                run_text.clear();
                run_text.push(cell.c);
                let underline = cell.flags.contains(CellFlags::UNDERLINE);
                col += 1;

                while col < cols {
                    let next = &grid_row[Column(col)];
                    if next.flags.contains(CellFlags::WIDE_CHAR_SPACER) {
                        col += 1;
                        continue;
                    }
                    if RunStyle::from_cell(next) != style
                        || next.flags.contains(CellFlags::UNDERLINE) != underline
                    {
                        break;
                    }
                    run_text.push(next.c);
                    col += 1;
                }

                let mut desc = base_font.clone();
                if style.bold {
                    desc.set_weight(pango::Weight::Bold);
                }
                if style.italic {
                    desc.set_style(pango::Style::Italic);
                }
                layout.set_font_description(Some(&desc));
                layout.set_text(&run_text);

                let x = PADDING_PX + (start_col as f32) * cw;
                snapshot.save();
                snapshot.translate(&graphene::Point::new(x, y));
                snapshot.append_layout(&layout, &rgb_to_gdk(style.fg, 1.0));
                snapshot.restore();

                if underline {
                    let span_w = ((col - start_col) as f32) * cw;
                    snapshot.append_color(
                        &rgb_to_gdk(style.fg, 0.85),
                        &graphene::Rect::new(x, y + ch - 1.5, span_w, 1.0),
                    );
                }
            }
        }

        // Pass 3: Cursor (visible when at bottom of scrollback)
        if display_offset == 0 {
            let cursor_pt = grid.cursor.point;
            let cx = PADDING_PX + (cursor_pt.column.0 as f32) * cw;
            let cy = PADDING_PX + (cursor_pt.line.0 as f32) * ch;
            let cursor_rgba =
                gdk::RGBA::new(0.54, 0.71, 0.98, if is_active_pane { 0.78 } else { 0.35 });
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

        // Pass 4: Subtle top accent bar on active split pane for instant visual orientation
        if is_active_pane {
            snapshot.append_color(
                &gdk::RGBA::new(0.36, 0.62, 0.96, 0.85),
                &graphene::Rect::new(0.0, 0.0, w, 2.0),
            );
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RunStyle {
    fg: Rgb,
    bold: bool,
    italic: bool,
}

impl RunStyle {
    fn from_cell(cell: &alacritty_terminal::term::cell::Cell) -> Self {
        let fg = if cell.flags.contains(CellFlags::INVERSE) {
            resolve_color(cell.bg, DEFAULT_BG)
        } else {
            resolve_color(cell.fg, DEFAULT_FG)
        };
        Self {
            fg,
            bold: cell.flags.contains(CellFlags::BOLD),
            italic: cell.flags.contains(CellFlags::ITALIC),
        }
    }
}

fn rgb_to_gdk(c: Rgb, alpha: f32) -> gdk::RGBA {
    gdk::RGBA::new(
        (c.r as f32) / 255.0,
        (c.g as f32) / 255.0,
        (c.b as f32) / 255.0,
        alpha,
    )
}

fn resolve_color(color: AnsiColor, fallback: Rgb) -> Rgb {
    match color {
        AnsiColor::Spec(rgb) => rgb,
        AnsiColor::Indexed(idx) => indexed_color(idx),
        AnsiColor::Named(named) => match named {
            NamedColor::Foreground | NamedColor::BrightForeground => DEFAULT_FG,
            NamedColor::Background => DEFAULT_BG,
            NamedColor::Cursor => Rgb {
                r: 0x89,
                g: 0xb4,
                b: 0xfa,
            },
            NamedColor::Black | NamedColor::DimBlack => indexed_color(0),
            NamedColor::Red | NamedColor::DimRed => indexed_color(1),
            NamedColor::Green | NamedColor::DimGreen => indexed_color(2),
            NamedColor::Yellow | NamedColor::DimYellow => indexed_color(3),
            NamedColor::Blue | NamedColor::DimBlue => indexed_color(4),
            NamedColor::Magenta | NamedColor::DimMagenta => indexed_color(5),
            NamedColor::Cyan | NamedColor::DimCyan => indexed_color(6),
            NamedColor::White | NamedColor::DimWhite => indexed_color(7),
            NamedColor::BrightBlack => indexed_color(8),
            NamedColor::BrightRed => indexed_color(9),
            NamedColor::BrightGreen => indexed_color(10),
            NamedColor::BrightYellow => indexed_color(11),
            NamedColor::BrightBlue => indexed_color(12),
            NamedColor::BrightMagenta => indexed_color(13),
            NamedColor::BrightCyan => indexed_color(14),
            NamedColor::BrightWhite => indexed_color(15),
            _ => fallback,
        },
    }
}

fn indexed_color(idx: u8) -> Rgb {
    const PALETTE_16: [Rgb; 16] = [
        Rgb {
            r: 0x1e,
            g: 0x22,
            b: 0x2a,
        },
        Rgb {
            r: 0xe0,
            g: 0x6c,
            b: 0x75,
        },
        Rgb {
            r: 0x98,
            g: 0xc3,
            b: 0x79,
        },
        Rgb {
            r: 0xe5,
            g: 0xc0,
            b: 0x7b,
        },
        Rgb {
            r: 0x61,
            g: 0xaf,
            b: 0xef,
        },
        Rgb {
            r: 0xc6,
            g: 0x78,
            b: 0xdd,
        },
        Rgb {
            r: 0x56,
            g: 0xb6,
            b: 0xc2,
        },
        Rgb {
            r: 0xab,
            g: 0xb2,
            b: 0xbf,
        },
        Rgb {
            r: 0x5c,
            g: 0x63,
            b: 0x70,
        },
        Rgb {
            r: 0xf3,
            g: 0x8b,
            b: 0xa8,
        },
        Rgb {
            r: 0xa6,
            g: 0xe3,
            b: 0xa1,
        },
        Rgb {
            r: 0xf9,
            g: 0xe2,
            b: 0xaf,
        },
        Rgb {
            r: 0x89,
            g: 0xb4,
            b: 0xfa,
        },
        Rgb {
            r: 0xcb,
            g: 0xa6,
            b: 0xf7,
        },
        Rgb {
            r: 0x94,
            g: 0xe2,
            b: 0xd5,
        },
        Rgb {
            r: 0xff,
            g: 0xff,
            b: 0xff,
        },
    ];
    match idx {
        0..=15 => PALETTE_16[idx as usize],
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
