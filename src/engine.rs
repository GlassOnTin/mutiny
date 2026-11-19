use std::collections::{HashMap, VecDeque};
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use alacritty_terminal::Term;
use alacritty_terminal::event::{Event, EventListener, OnResize, WindowSize};
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::{Column, Direction, Line, Point};
use alacritty_terminal::selection::{Selection, SelectionType};
use alacritty_terminal::term::{ClipboardType, Config as TermConfig, TermDamage, TermMode};
use alacritty_terminal::tty::{
    self, ChildEvent, EventedPty, EventedReadWrite, Options as PtyOptions, Pty, Shell,
};
use alacritty_terminal::vte::ansi::Processor;
use serde::{Deserialize, Serialize};

use crate::shell_integration;

const MAX_COMMAND_RECORDS: usize = 64;
const MAX_PROMPT_MARKERS: usize = 256;
const MAX_OSC_LEN: usize = 8192;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GridSize {
    pub cols: usize,
    pub rows: usize,
    pub cell_width_px: u16,
    pub cell_height_px: u16,
}

impl GridSize {
    pub fn new(cols: usize, rows: usize, cell_width_px: u16, cell_height_px: u16) -> Self {
        Self {
            cols: cols.max(2),
            rows: rows.max(1),
            cell_width_px: cell_width_px.max(1),
            cell_height_px: cell_height_px.max(1),
        }
    }

    pub fn to_window_size(self) -> WindowSize {
        WindowSize {
            num_lines: self.rows as u16,
            num_cols: self.cols as u16,
            cell_width: self.cell_width_px,
            cell_height: self.cell_height_px,
        }
    }
}

impl Dimensions for GridSize {
    fn total_lines(&self) -> usize {
        self.rows
    }

    fn screen_lines(&self) -> usize {
        self.rows
    }

    fn columns(&self) -> usize {
        self.cols
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandRecord {
    pub seq: u64,
    pub output: String,
    pub exit_code: Option<i32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProgressState {
    Hidden,
    Normal(u8),
    Error(u8),
    Indeterminate,
    Paused(u8),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AttentionReason {
    Bell,
    CommandFailed(i32),
    AgentNotice,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneSnapshot {
    pub cols: usize,
    pub rows: usize,
    pub cursor_row: i32,
    pub cursor_col: usize,
    pub title: String,
    pub scrollback_lines: usize,
    pub display_offset: usize,
    pub bracketed_paste: bool,
    pub app_cursor: bool,
    pub kitty_keyboard: bool,
    pub cwd: Option<String>,
    pub progress: ProgressState,
    pub attention: Option<AttentionReason>,
    pub agent_notice: Option<String>,
    pub command_running: bool,
    pub prompt_seq: u64,
    pub output_seq: u64,
    pub lines: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScrollbackMatch {
    pub grid_line: i32,
    pub text: String,
}

/// Lines one tick may spend scanning. The M0.5 baseline measured a full
/// 10 000-line no-match scan at about 3.2 ms, roughly 0.32 us per line, against
/// a 4 ms tick. 512 lines is about 0.16 ms, leaving headroom for PTY draining
/// and rendering. A full-history miss therefore spreads over about 20 ticks
/// instead of occupying one.
pub const SEARCH_LINES_PER_TICK: usize = 512;

/// A resumable bottom-up scrollback scan. Holds its own cursor so several
/// searches (the viewport bar, an in-flight IPC request) can progress
/// independently without clobbering each other.
#[derive(Debug, Clone)]
pub struct ScrollbackSearch {
    needle: String,
    ignore_case: bool,
    max_matches: usize,
    first_line: i32,
    next_line: i32,
    matches: Vec<ScrollbackMatch>,
    cursor: Option<usize>,
    lines_scanned: usize,
    done: bool,
    output_seq: u64,
}

impl ScrollbackSearch {
    pub fn new(
        needle: &str,
        ignore_case: bool,
        max_matches: usize,
        first_line: i32,
        last_line: i32,
        output_seq: u64,
    ) -> Self {
        let folded = if ignore_case {
            needle.to_lowercase()
        } else {
            needle.to_owned()
        };
        Self {
            needle: folded,
            ignore_case,
            max_matches,
            first_line,
            next_line: last_line,
            matches: Vec::new(),
            cursor: None,
            lines_scanned: 0,
            done: needle.is_empty() || max_matches == 0,
            output_seq,
        }
    }

    pub fn is_done(&self) -> bool {
        self.done
    }

    pub fn matches(&self) -> &[ScrollbackMatch] {
        &self.matches
    }

    pub fn cursor(&self) -> Option<usize> {
        self.cursor
    }

    pub fn lines_scanned(&self) -> usize {
        self.lines_scanned
    }

    /// Restarts against a grid that moved under us. Grid line indices are
    /// relative to the current screen, so output arriving mid-scan invalidates
    /// both the cursor and every match already collected.
    fn restart(&mut self, first_line: i32, last_line: i32, output_seq: u64) {
        self.first_line = first_line;
        self.next_line = last_line;
        self.matches.clear();
        self.cursor = None;
        self.lines_scanned = 0;
        self.done = self.needle.is_empty() || self.max_matches == 0;
        self.output_seq = output_seq;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SearchProgress {
    pub matches: usize,
    pub lines_scanned: usize,
    pub done: bool,
    /// True when output arrived mid-scan and the scan started over, so results
    /// always describe the current grid rather than a stale one.
    pub restarted: bool,
}

#[derive(Debug, Clone)]
pub enum EngineNotification {
    TitleChanged(String),
    ClipboardStore { primary: bool, text: String },
    Bell,
    PromptReached { seq: u64 },
    CommandFinished(CommandRecord),
    ChildExited(Option<i32>),
}

#[derive(Clone, Default)]
pub struct EventCollector {
    queue: Arc<Mutex<Vec<Event>>>,
}

impl EventListener for EventCollector {
    fn send_event(&self, event: Event) {
        if let Ok(mut guard) = self.queue.lock() {
            guard.push(event);
        }
    }
}

#[derive(Debug, Default)]
enum OscScanState {
    #[default]
    Ground,
    SawEsc,
    InOsc {
        buf: Vec<u8>,
        saw_esc: bool,
    },
}

#[derive(Debug, Clone, Copy)]
struct ActiveCommandStart {
    line_from_history_top: i32,
    column: Column,
}

pub struct PaneEngine {
    term: Term<EventCollector>,
    processor: Processor,
    events: EventCollector,
    pty: Option<Pty>,
    size: GridSize,
    title: String,
    cwd: Option<PathBuf>,
    progress: ProgressState,
    attention: Option<AttentionReason>,
    agent_notice: Option<String>,
    command_running: bool,
    osc_state: OscScanState,
    active_cmd_start: Option<ActiveCommandStart>,
    commands: VecDeque<CommandRecord>,
    prompt_lines_from_top: VecDeque<i32>,
    prompt_seq: u64,
    command_seq: u64,
    output_seq: u64,
    last_output_at: Option<Instant>,
    last_child_exit_code: Option<i32>,
    pending_pty_writes: Vec<u8>,
    search: Option<ScrollbackSearch>,
}

impl PaneEngine {
    pub fn new_headless(cols: usize, rows: usize) -> Self {
        Self::new_headless_with_scrollback(cols, rows, 10_000)
    }

    /// Scrollback depth is fixed at construction because alacritty's TermConfig
    /// is, so a config change applies to new panes rather than existing ones.
    pub fn new_headless_with_scrollback(cols: usize, rows: usize, scrollback_lines: usize) -> Self {
        let size = GridSize::new(cols, rows, 9, 18);
        let events = EventCollector::default();
        let config = TermConfig {
            scrolling_history: scrollback_lines,
            kitty_keyboard: true,
            ..TermConfig::default()
        };
        let term = Term::new(config, &size, events.clone());
        Self {
            term,
            processor: Processor::new(),
            events,
            pty: None,
            size,
            title: String::from("shell"),
            cwd: None,
            progress: ProgressState::Hidden,
            attention: None,
            agent_notice: None,
            command_running: false,
            osc_state: OscScanState::Ground,
            active_cmd_start: None,
            commands: VecDeque::new(),
            prompt_lines_from_top: VecDeque::new(),
            prompt_seq: 0,
            command_seq: 0,
            output_seq: 0,
            last_output_at: None,
            last_child_exit_code: None,
            pending_pty_writes: Vec::new(),
            search: None,
        }
    }

    pub fn spawn_pty(
        size: GridSize,
        working_directory: Option<PathBuf>,
        shell_override: Option<(String, Vec<String>)>,
        pane_id_num: u64,
        scrollback_lines: usize,
    ) -> io::Result<Self> {
        tty::setup_env();
        let mut env = HashMap::new();
        env.insert(String::from("TERM_PROGRAM"), String::from("mutiny"));
        env.insert(String::from("MUTINY_PANE_ID"), pane_id_num.to_string());

        // Resolve the program so integration can be classified. When we ship
        // nothing for it, fall through to alacritty's own default resolution
        // so existing behavior is unchanged.
        let resolved = match &shell_override {
            Some((prog, args)) => Some((prog.clone(), args.clone())),
            None => std::env::var("SHELL")
                .ok()
                .filter(|s| !s.is_empty())
                .map(|s| (s, Vec::new())),
        };

        let mut shell = None;
        let mut injected = false;
        if let Some((prog, args)) = &resolved {
            let kind = shell_integration::classify(prog);
            if kind != shell_integration::ShellKind::Other {
                let dir = shell_integration::integration_dir();
                // Best-effort: an unwritable runtime dir must not stop a pane
                // from spawning, it only costs the integration markers.
                match shell_integration::install_assets(&dir)
                    .and_then(|()| shell_integration::prepare(kind, &dir, args))
                {
                    Ok(Some(inj)) => {
                        for (key, value) in inj.env {
                            env.insert(key, value);
                        }
                        shell = Some(Shell::new(prog.clone(), inj.args));
                        injected = true;
                    }
                    Ok(None) => {}
                    Err(err) => {
                        eprintln!("warning: shell integration unavailable: {err}");
                    }
                }
            }
        }
        if !injected {
            shell = shell_override.map(|(prog, args)| Shell::new(prog, args));
        }

        let opts = PtyOptions {
            shell,
            working_directory: working_directory.clone(),
            drain_on_exit: true,
            env,
        };
        let pty = tty::new(&opts, size.to_window_size(), pane_id_num)?;
        let mut engine = Self::new_headless_with_scrollback(size.cols, size.rows, scrollback_lines);
        engine.size = size;
        engine.cwd = working_directory;
        engine.pty = Some(pty);
        Ok(engine)
    }

    pub fn pty_raw_fd(&self) -> Option<i32> {
        self.pty.as_ref().map(|pty| pty.file().as_raw_fd())
    }

    pub fn title(&self) -> &str {
        &self.title
    }

    pub fn cwd(&self) -> Option<&Path> {
        self.cwd.as_deref()
    }

    pub fn progress(&self) -> ProgressState {
        self.progress
    }

    pub fn attention(&self) -> Option<AttentionReason> {
        self.attention
    }

    pub fn clear_attention(&mut self) {
        self.attention = None;
        self.agent_notice = None;
    }

    pub fn set_agent_notice(&mut self, message: Option<String>) {
        match message.filter(|m| !m.trim().is_empty()) {
            Some(msg) => {
                self.agent_notice = Some(msg);
                self.attention = Some(AttentionReason::AgentNotice);
            }
            None => {
                self.agent_notice = None;
                if self.attention == Some(AttentionReason::AgentNotice) {
                    self.attention = None;
                }
            }
        }
    }

    pub fn agent_notice(&self) -> Option<&str> {
        self.agent_notice.as_deref()
    }

    pub fn output_seq(&self) -> u64 {
        self.output_seq
    }

    pub fn last_child_exit_code(&self) -> Option<i32> {
        self.last_child_exit_code
    }

    /// True when either OSC 133 reached a fresh prompt after `since_prompt_seq`,
    /// or raw PTY output arrived after `since_output_seq` and stayed quiet for `settle`.
    pub fn has_turn_settled(
        &self,
        since_prompt_seq: u64,
        since_output_seq: u64,
        settle: Duration,
    ) -> bool {
        if self.prompt_seq > since_prompt_seq && !self.command_running {
            return true;
        }
        if self.output_seq > since_output_seq
            && !self.command_running
            && let Some(last_at) = self.last_output_at
        {
            return last_at.elapsed() >= settle;
        }
        false
    }

    pub fn is_command_running(&self) -> bool {
        self.command_running
    }

    pub fn prompt_seq(&self) -> u64 {
        self.prompt_seq
    }

    pub fn last_command(&self) -> Option<&CommandRecord> {
        self.commands.back()
    }

    pub fn term(&self) -> &Term<EventCollector> {
        &self.term
    }

    pub fn term_mut(&mut self) -> &mut Term<EventCollector> {
        &mut self.term
    }

    pub fn size(&self) -> GridSize {
        self.size
    }

    pub fn has_damaged_lines(&mut self) -> bool {
        match self.term.damage() {
            TermDamage::Full => true,
            TermDamage::Partial(mut iter) => iter.next().is_some(),
        }
    }

    pub fn reset_damage(&mut self) {
        self.term.reset_damage();
    }

    pub fn resize(&mut self, new_size: GridSize) {
        if new_size == self.size {
            return;
        }
        self.size = new_size;
        self.term.resize(new_size);
        if let Some(pty) = self.pty.as_mut() {
            pty.on_resize(new_size.to_window_size());
        }
    }

    pub fn scroll(&mut self, delta: Scroll) {
        self.term.scroll_display(delta);
    }

    pub fn jump_prev_prompt(&mut self) -> bool {
        let grid = self.term.grid();
        let history = grid.history_size() as i32;
        let current_top_line = -(grid.display_offset() as i32);
        let current_from_top = history + current_top_line;

        if let Some(&target_from_top) = self
            .prompt_lines_from_top
            .iter()
            .rev()
            .find(|&&line_top| line_top < current_from_top)
        {
            let desired_line = (target_from_top - history).max(-history);
            let desired_offset = (-desired_line).max(0);
            let delta = desired_offset - (grid.display_offset() as i32);
            if delta != 0 {
                self.term.scroll_display(Scroll::Delta(delta));
                return true;
            }
        }
        false
    }

    pub fn jump_next_prompt(&mut self) -> bool {
        let grid = self.term.grid();
        let history = grid.history_size() as i32;
        let current_top_line = -(grid.display_offset() as i32);
        let current_from_top = history + current_top_line;

        if let Some(&target_from_top) = self
            .prompt_lines_from_top
            .iter()
            .find(|&&line_top| line_top > current_from_top)
        {
            let desired_line = (target_from_top - history).min(0);
            let desired_offset = (-desired_line).max(0);
            let delta = desired_offset - (grid.display_offset() as i32);
            if delta != 0 {
                self.term.scroll_display(Scroll::Delta(delta));
                return true;
            }
        } else if grid.display_offset() > 0 {
            self.term.scroll_display(Scroll::Bottom);
            return true;
        }
        false
    }

    pub fn selection_text(&self) -> Option<String> {
        self.term.selection_to_string().filter(|s| !s.is_empty())
    }

    pub fn start_selection(&mut self, col: usize, viewport_row: usize, ty: SelectionType) {
        let offset = self.term.grid().display_offset() as i32;
        let point = Point::new(
            Line(viewport_row as i32 - offset),
            Column(col.min(self.size.cols.saturating_sub(1))),
        );
        self.term.selection = Some(Selection::new(ty, point, Direction::Left));
    }

    pub fn update_selection(&mut self, col: usize, viewport_row: usize) {
        let offset = self.term.grid().display_offset() as i32;
        let point = Point::new(
            Line(viewport_row as i32 - offset),
            Column(col.min(self.size.cols.saturating_sub(1))),
        );
        if let Some(sel) = self.term.selection.as_mut() {
            sel.update(point, Direction::Right);
        }
    }

    pub fn clear_selection(&mut self) {
        self.term.selection = None;
    }

    pub fn write_to_pty(&mut self, data: &[u8]) -> io::Result<()> {
        if data.is_empty() {
            return Ok(());
        }
        if let Some(pty) = self.pty.as_mut() {
            pty.writer().write_all(data)?;
            pty.writer().flush()?;
        } else {
            self.pending_pty_writes.extend_from_slice(data);
        }
        Ok(())
    }

    pub fn drain_pty(&mut self) -> io::Result<(usize, Vec<EngineNotification>)> {
        let mut total_read = 0usize;
        let mut notifications = Vec::new();
        let mut buf = [0u8; 32 * 1024];

        let mut slave_eof = false;
        while let Some(pty) = self.pty.as_mut() {
            let read_res = pty.reader().read(&mut buf);
            match read_res {
                Ok(0) => {
                    slave_eof = true;
                    break;
                }
                Ok(n) => {
                    total_read += n;
                    notifications.extend(self.advance_bytes(&buf[..n]));
                }
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => break,
                Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
                // On Linux, EIO on a PTY master means the slave fd closed (shell exited).
                Err(err) if err.raw_os_error() == Some(libc::EIO) => {
                    slave_eof = true;
                    break;
                }
                Err(err) => return Err(err),
            }
        }

        let mut replies = Vec::new();
        std::mem::swap(&mut replies, &mut self.pending_pty_writes);
        if !replies.is_empty() && !slave_eof {
            let _ = self.write_to_pty(&replies);
        }

        let mut emitted_exit = false;
        if let Some(pty) = self.pty.as_mut() {
            let attempts = if slave_eof { 10 } else { 1 };
            for attempt in 0..attempts {
                while let Some(ev) = pty.next_child_event() {
                    let ChildEvent::Exited(status) = ev;
                    let code = status.and_then(|s| s.code());
                    self.last_child_exit_code = code;
                    notifications.push(EngineNotification::ChildExited(code));
                    emitted_exit = true;
                }
                if emitted_exit || !slave_eof {
                    break;
                }
                if attempt + 1 < attempts {
                    std::thread::sleep(Duration::from_millis(2));
                }
            }
        }
        if slave_eof && !emitted_exit {
            notifications.push(EngineNotification::ChildExited(self.last_child_exit_code));
        }

        Ok((total_read, notifications))
    }

    /// Feeds raw VT output into `alacritty_terminal` while slicing at OSC
    /// terminators so OSC 133 / 7 / 9;4 markers see the exact grid cursor position.
    pub fn advance_bytes(&mut self, bytes: &[u8]) -> Vec<EngineNotification> {
        if !bytes.is_empty() {
            self.output_seq = self.output_seq.wrapping_add(1);
            self.last_output_at = Some(Instant::now());
        }
        let mut notifications = Vec::new();
        let mut chunk_start = 0usize;

        for (idx, &b) in bytes.iter().enumerate() {
            let mut completed_osc: Option<Vec<u8>> = None;
            match &mut self.osc_state {
                OscScanState::Ground => {
                    if b == 0x1b {
                        self.osc_state = OscScanState::SawEsc;
                    }
                }
                OscScanState::SawEsc => {
                    if b == b']' {
                        self.osc_state = OscScanState::InOsc {
                            buf: Vec::with_capacity(64),
                            saw_esc: false,
                        };
                    } else if b == 0x1b {
                        self.osc_state = OscScanState::SawEsc;
                    } else {
                        self.osc_state = OscScanState::Ground;
                    }
                }
                OscScanState::InOsc { buf, saw_esc } => {
                    if b == 0x07 {
                        completed_osc = Some(std::mem::take(buf));
                        self.osc_state = OscScanState::Ground;
                    } else if *saw_esc && b == b'\\' {
                        buf.pop(); // remove trailing ESC before '\'
                        completed_osc = Some(std::mem::take(buf));
                        self.osc_state = OscScanState::Ground;
                    } else if b == 0x18 || b == 0x1a {
                        self.osc_state = OscScanState::Ground;
                    } else {
                        *saw_esc = b == 0x1b;
                        if buf.len() < MAX_OSC_LEN {
                            buf.push(b);
                        }
                    }
                }
            }

            if let Some(osc_payload) = completed_osc {
                self.processor
                    .advance(&mut self.term, &bytes[chunk_start..=idx]);
                chunk_start = idx + 1;
                self.handle_osc_payload(&osc_payload, &mut notifications);
            }
        }

        if chunk_start < bytes.len() {
            self.processor
                .advance(&mut self.term, &bytes[chunk_start..]);
        }

        self.collect_term_events(&mut notifications);
        notifications
    }

    fn handle_osc_payload(&mut self, payload: &[u8], out: &mut Vec<EngineNotification>) {
        let Ok(text) = std::str::from_utf8(payload) else {
            return;
        };
        let Some((code, rest)) = text.split_once(';') else {
            return;
        };

        match code {
            "133" => self.handle_osc_133(rest, out),
            "7" => {
                if let Some(path) = parse_osc7_uri(rest) {
                    self.cwd = Some(path);
                }
            }
            "9" => {
                if let Some(state) = parse_osc9_progress(rest) {
                    self.progress = state;
                }
            }
            _ => {}
        }
    }

    fn handle_osc_133(&mut self, rest: &str, out: &mut Vec<EngineNotification>) {
        let mut parts = rest.split(';');
        let Some(kind) = parts.next() else {
            return;
        };

        let history = self.term.grid().history_size() as i32;
        let cursor = self.term.grid().cursor.point;
        let line_from_top = history + cursor.line.0;

        match kind {
            "A" => {
                self.command_running = false;
                self.prompt_seq += 1;
                if self.prompt_lines_from_top.back().copied() != Some(line_from_top) {
                    if self.prompt_lines_from_top.len() >= MAX_PROMPT_MARKERS {
                        self.prompt_lines_from_top.pop_front();
                    }
                    self.prompt_lines_from_top.push_back(line_from_top);
                }
                out.push(EngineNotification::PromptReached {
                    seq: self.prompt_seq,
                });
            }
            "B" => {
                self.command_running = false;
            }
            "C" => {
                self.command_running = true;
                self.active_cmd_start = Some(ActiveCommandStart {
                    line_from_history_top: line_from_top,
                    column: cursor.column,
                });
            }
            "D" => {
                let exit_code = parts.next().and_then(|s| s.parse::<i32>().ok());
                self.command_running = false;
                let output = self
                    .active_cmd_start
                    .take()
                    .map(|start| self.extract_command_output(start))
                    .unwrap_or_default();

                self.command_seq += 1;
                let record = CommandRecord {
                    seq: self.command_seq,
                    output,
                    exit_code,
                };
                if let Some(code) = exit_code.filter(|&c| c != 0) {
                    self.attention = Some(AttentionReason::CommandFailed(code));
                }
                if self.commands.len() >= MAX_COMMAND_RECORDS {
                    self.commands.pop_front();
                }
                self.commands.push_back(record.clone());
                out.push(EngineNotification::CommandFinished(record));
            }
            _ => {}
        }
    }

    fn extract_command_output(&self, start: ActiveCommandStart) -> String {
        let grid = self.term.grid();
        let history = grid.history_size() as i32;
        let min_line = -history;
        let start_line = (start.line_from_history_top - history).max(min_line);
        let mut end_point = grid.cursor.point;

        if end_point.column.0 == 0 && end_point.line.0 > start_line {
            end_point.line = Line(end_point.line.0 - 1);
            end_point.column = Column(self.size.cols.saturating_sub(1));
        } else if end_point.column.0 > 0 {
            end_point.column = Column(end_point.column.0 - 1);
        }

        let start_point = Point::new(Line(start_line), start.column);
        if (start_point.line, start_point.column) > (end_point.line, end_point.column) {
            return String::new();
        }
        self.term
            .bounds_to_string(start_point, end_point)
            .trim_matches('\n')
            .trim_end_matches('\r')
            .to_owned()
    }

    fn collect_term_events(&mut self, out: &mut Vec<EngineNotification>) {
        let drained: Vec<Event> = match self.events.queue.lock() {
            Ok(mut g) => g.drain(..).collect(),
            Err(_) => return,
        };

        for ev in drained {
            match ev {
                Event::Title(title) => {
                    self.title = title.clone();
                    out.push(EngineNotification::TitleChanged(title));
                }
                Event::ResetTitle => {
                    self.title = String::from("shell");
                    out.push(EngineNotification::TitleChanged(self.title.clone()));
                }
                Event::ClipboardStore(ty, text) => {
                    let primary = matches!(ty, ClipboardType::Selection);
                    out.push(EngineNotification::ClipboardStore { primary, text });
                }
                Event::PtyWrite(reply) => {
                    self.pending_pty_writes.extend_from_slice(reply.as_bytes());
                }
                Event::Bell => {
                    self.attention = Some(AttentionReason::Bell);
                    out.push(EngineNotification::Bell);
                }
                Event::ChildExit(status) => {
                    out.push(EngineNotification::ChildExited(status.code()));
                }
                _ => {}
            }
        }
    }

    pub fn format_paste(&self, text: &str) -> Vec<u8> {
        let sanitized = text.replace("\x1b[201~", "").replace("\x1b[200~", "");
        if self.term.mode().contains(TermMode::BRACKETED_PASTE) {
            let mut out = Vec::with_capacity(sanitized.len() + 12);
            out.extend_from_slice(b"\x1b[200~");
            out.extend_from_slice(sanitized.as_bytes());
            out.extend_from_slice(b"\x1b[201~");
            out
        } else {
            sanitized.into_bytes()
        }
    }

    pub fn encode_key(&self, key: KeyInput) -> Option<Vec<u8>> {
        let mode = *self.term.mode();
        let kitty_disambiguate = mode.contains(TermMode::DISAMBIGUATE_ESC_CODES)
            || mode.contains(TermMode::REPORT_ALL_KEYS_AS_ESC);

        let mod_param = 1u8
            + if key.shift { 1 } else { 0 }
            + if key.alt { 2 } else { 0 }
            + if key.ctrl { 4 } else { 0 }
            + if key.super_key { 8 } else { 0 };

        match key.kind {
            KeyKind::Enter => {
                if kitty_disambiguate && mod_param > 1 {
                    return Some(format!("\x1b[13;{mod_param}u").into_bytes());
                }
                Some(if key.alt {
                    b"\x1b\r".to_vec()
                } else {
                    b"\r".to_vec()
                })
            }
            KeyKind::Tab => {
                if key.shift {
                    return Some(b"\x1b[Z".to_vec());
                }
                if kitty_disambiguate && mod_param > 1 {
                    return Some(format!("\x1b[9;{mod_param}u").into_bytes());
                }
                Some(b"\t".to_vec())
            }
            KeyKind::Backspace => {
                if kitty_disambiguate && mod_param > 1 {
                    return Some(format!("\x1b[127;{mod_param}u").into_bytes());
                }
                let base = if key.ctrl { 0x08u8 } else { 0x7fu8 };
                Some(if key.alt {
                    vec![0x1b, base]
                } else {
                    vec![base]
                })
            }
            KeyKind::Escape => {
                if kitty_disambiguate {
                    return Some(if mod_param > 1 {
                        format!("\x1b[27;{mod_param}u").into_bytes()
                    } else {
                        b"\x1b[27u".to_vec()
                    });
                }
                Some(b"\x1b".to_vec())
            }
            KeyKind::Up => Some(encode_cursor_key(
                b'A',
                mod_param,
                mode.contains(TermMode::APP_CURSOR),
            )),
            KeyKind::Down => Some(encode_cursor_key(
                b'B',
                mod_param,
                mode.contains(TermMode::APP_CURSOR),
            )),
            KeyKind::Right => Some(encode_cursor_key(
                b'C',
                mod_param,
                mode.contains(TermMode::APP_CURSOR),
            )),
            KeyKind::Left => Some(encode_cursor_key(
                b'D',
                mod_param,
                mode.contains(TermMode::APP_CURSOR),
            )),
            KeyKind::Home => Some(encode_cursor_key(
                b'H',
                mod_param,
                mode.contains(TermMode::APP_CURSOR),
            )),
            KeyKind::End => Some(encode_cursor_key(
                b'F',
                mod_param,
                mode.contains(TermMode::APP_CURSOR),
            )),
            KeyKind::Insert => Some(encode_tilde_key(2, mod_param)),
            KeyKind::Delete => Some(encode_tilde_key(3, mod_param)),
            KeyKind::PageUp => Some(encode_tilde_key(5, mod_param)),
            KeyKind::PageDown => Some(encode_tilde_key(6, mod_param)),
            KeyKind::Function(n @ 1..=12) => Some(encode_function_key(n, mod_param)),
            KeyKind::Function(_) => None,
            KeyKind::Char(ch) => {
                if kitty_disambiguate && (key.ctrl || key.alt || key.super_key) {
                    let cp = ch.to_ascii_lowercase() as u32;
                    return Some(format!("\x1b[{cp};{mod_param}u").into_bytes());
                }
                if key.ctrl {
                    let lower = ch.to_ascii_lowercase();
                    if lower.is_ascii_lowercase() {
                        let ctrl_byte = (lower as u8) - b'a' + 1;
                        return Some(if key.alt {
                            vec![0x1b, ctrl_byte]
                        } else {
                            vec![ctrl_byte]
                        });
                    }
                    if ch == ' ' || ch == '@' {
                        return Some(vec![0x00]);
                    }
                }
                let mut utf8 = [0u8; 4];
                let encoded = ch.encode_utf8(&mut utf8);
                if key.alt {
                    let mut out = Vec::with_capacity(1 + encoded.len());
                    out.push(0x1b);
                    out.extend_from_slice(encoded.as_bytes());
                    Some(out)
                } else {
                    Some(encoded.as_bytes().to_vec())
                }
            }
        }
    }

    pub fn screen_text(&self) -> String {
        let start = Point::new(Line(0), Column(0));
        let end = Point::new(
            Line(self.size.rows.saturating_sub(1) as i32),
            Column(self.size.cols.saturating_sub(1)),
        );
        self.term.bounds_to_string(start, end)
    }

    pub fn trimmed_screen_lines(&self) -> Vec<String> {
        let mut lines: Vec<String> = self
            .screen_text()
            .lines()
            .map(|l| l.trim_end().to_owned())
            .collect();
        while lines.last().is_some_and(|l| l.is_empty()) {
            lines.pop();
        }
        lines
    }

    pub fn snapshot(&self, max_lines: Option<usize>) -> PaneSnapshot {
        let grid = self.term.grid();
        let mode = *self.term.mode();
        let mut lines = self.trimmed_screen_lines();
        if let Some(cap) = max_lines
            && lines.len() > cap
        {
            let start = lines.len() - cap;
            lines = lines.split_off(start);
        }
        PaneSnapshot {
            cols: self.size.cols,
            rows: self.size.rows,
            cursor_row: grid.cursor.point.line.0,
            cursor_col: grid.cursor.point.column.0,
            title: self.title.clone(),
            scrollback_lines: grid.history_size(),
            display_offset: grid.display_offset(),
            bracketed_paste: mode.contains(TermMode::BRACKETED_PASTE),
            app_cursor: mode.contains(TermMode::APP_CURSOR),
            kitty_keyboard: mode.intersects(
                TermMode::DISAMBIGUATE_ESC_CODES
                    | TermMode::REPORT_EVENT_TYPES
                    | TermMode::REPORT_ALTERNATE_KEYS
                    | TermMode::REPORT_ALL_KEYS_AS_ESC
                    | TermMode::REPORT_ASSOCIATED_TEXT,
            ),
            cwd: self.cwd.as_ref().map(|p| p.display().to_string()),
            progress: self.progress,
            attention: self.attention,
            agent_notice: self.agent_notice.clone(),
            command_running: self.command_running,
            prompt_seq: self.prompt_seq,
            output_seq: self.output_seq,
            lines,
        }
    }

    /// Scans at most `line_budget` lines of `search`, newest first. Takes `&self`
    /// and an external search state so the viewport bar and an in-flight IPC
    /// request can each hold their own cursor over the same grid.
    pub fn scan_search_chunk(
        &self,
        search: &mut ScrollbackSearch,
        line_budget: usize,
    ) -> SearchProgress {
        let last_col = Column(self.size.cols.saturating_sub(1));
        let mut scanned = 0usize;
        while scanned < line_budget && !search.done {
            if search.next_line < search.first_line {
                search.done = true;
                break;
            }
            let line_idx = search.next_line;
            search.next_line -= 1;
            scanned += 1;

            let row_text = self.term.bounds_to_string(
                Point::new(Line(line_idx), Column(0)),
                Point::new(Line(line_idx), last_col),
            );
            let trimmed = row_text.trim_end();
            if trimmed.is_empty() {
                continue;
            }
            let hit = if search.ignore_case {
                trimmed.to_lowercase().contains(&search.needle)
            } else {
                trimmed.contains(&search.needle)
            };
            if hit {
                search.matches.push(ScrollbackMatch {
                    grid_line: line_idx,
                    text: trimmed.to_owned(),
                });
                if search.matches.len() >= search.max_matches {
                    search.done = true;
                }
            }
        }
        search.lines_scanned += scanned;
        SearchProgress {
            matches: search.matches.len(),
            lines_scanned: search.lines_scanned,
            done: search.done,
            restarted: false,
        }
    }

    fn search_bounds(&self) -> (i32, i32) {
        let history = self.term.grid().history_size() as i32;
        let last_line = self.size.rows.saturating_sub(1) as i32;
        (-history, last_line)
    }

    /// Builds a search cursor bound to this grid, for a caller that keeps its
    /// own state (an in-flight IPC request) rather than the pane's own slot.
    pub fn new_search(
        &self,
        needle: &str,
        ignore_case: bool,
        max_matches: usize,
    ) -> ScrollbackSearch {
        let (first, last) = self.search_bounds();
        ScrollbackSearch::new(
            needle,
            ignore_case,
            max_matches,
            first,
            last,
            self.output_seq,
        )
    }

    /// Starts the pane's own resumable search, replacing any previous one.
    pub fn begin_search(&mut self, needle: &str, ignore_case: bool, max_matches: usize) {
        let (first, last) = self.search_bounds();
        self.search = Some(ScrollbackSearch::new(
            needle,
            ignore_case,
            max_matches,
            first,
            last,
            self.output_seq,
        ));
    }

    pub fn cancel_search(&mut self) {
        self.search = None;
    }

    pub fn search(&self) -> Option<&ScrollbackSearch> {
        self.search.as_ref()
    }

    /// Advances an externally-held cursor, restarting it if output moved the
    /// grid since the scan began. Shared by the pane's own search and by
    /// in-flight IPC searches so both get the same staleness guard.
    pub fn advance_external_search(
        &self,
        search: &mut ScrollbackSearch,
        line_budget: usize,
    ) -> SearchProgress {
        let mut restarted = false;
        if search.output_seq != self.output_seq {
            let (first, last) = self.search_bounds();
            search.restart(first, last, self.output_seq);
            restarted = true;
        }
        let mut progress = self.scan_search_chunk(search, line_budget.max(1));
        progress.restarted = restarted;
        progress
    }

    /// Advances the pane's search by at most `line_budget` lines.
    pub fn advance_search(&mut self, line_budget: usize) -> Option<SearchProgress> {
        let mut search = self.search.take()?;
        let progress = self.advance_external_search(&mut search, line_budget);
        self.search = Some(search);
        Some(progress)
    }

    /// Moves the match cursor, scrolls the target line into view, and selects
    /// it so the existing selection rendering highlights it.
    pub fn navigate_search(&mut self, forward: bool) -> Option<usize> {
        let (next, line) = {
            let search = self.search.as_mut()?;
            let len = search.matches.len();
            if len == 0 {
                return None;
            }
            let next = match search.cursor {
                None => {
                    if forward {
                        0
                    } else {
                        len - 1
                    }
                }
                Some(i) => {
                    if forward {
                        (i + 1) % len
                    } else {
                        (i + len - 1) % len
                    }
                }
            };
            search.cursor = Some(next);
            (next, search.matches[next].grid_line)
        };
        self.scroll_to_grid_line(line);
        self.highlight_grid_line(line);
        Some(next)
    }

    /// Scrolls so `line` sits near the middle of the viewport. Returns whether
    /// the display offset actually changed.
    pub fn scroll_to_grid_line(&mut self, line: i32) -> bool {
        let grid = self.term.grid();
        let history = grid.history_size() as i32;
        let current = grid.display_offset() as i32;
        let rows = self.size.rows as i32;
        let desired_top = line - rows / 2;
        let offset = (-desired_top).clamp(0, history);
        if offset == current {
            return false;
        }
        self.term.scroll_display(Scroll::Delta(offset - current));
        true
    }

    fn highlight_grid_line(&mut self, line: i32) {
        let offset = self.term.grid().display_offset() as i32;
        let row = line + offset;
        if row < 0 || row as usize >= self.size.rows {
            return;
        }
        let last_col = self.size.cols.saturating_sub(1);
        self.start_selection(0, row as usize, SelectionType::Simple);
        self.update_selection(last_col, row as usize);
    }

    /// Runs a scan to completion. Kept for callers that are not tick-bound;
    /// the GUI and IPC paths use the chunked form instead.
    pub fn search_scrollback(
        &self,
        needle: &str,
        ignore_case: bool,
        max_matches: usize,
    ) -> Vec<ScrollbackMatch> {
        let (first, last) = self.search_bounds();
        let mut search = ScrollbackSearch::new(
            needle,
            ignore_case,
            max_matches,
            first,
            last,
            self.output_seq,
        );
        while !search.done {
            self.scan_search_chunk(&mut search, usize::MAX);
        }
        search.matches
    }
}

pub fn parse_named_key(spec: &str) -> Option<KeyInput> {
    let normalized = spec.trim().to_ascii_lowercase();
    let mut ctrl = false;
    let mut alt = false;
    let mut shift = false;
    let mut base = normalized.as_str();

    loop {
        if let Some(rest) = base.strip_prefix("ctrl-") {
            ctrl = true;
            base = rest;
        } else if let Some(rest) = base.strip_prefix("alt-") {
            alt = true;
            base = rest;
        } else if let Some(rest) = base.strip_prefix("shift-") {
            shift = true;
            base = rest;
        } else {
            break;
        }
    }

    let kind = match base {
        "enter" | "return" => KeyKind::Enter,
        "esc" | "escape" => KeyKind::Escape,
        "tab" => KeyKind::Tab,
        "space" => KeyKind::Char(' '),
        "backspace" => KeyKind::Backspace,
        "delete" | "del" => KeyKind::Delete,
        "insert" | "ins" => KeyKind::Insert,
        "up" => KeyKind::Up,
        "down" => KeyKind::Down,
        "left" => KeyKind::Left,
        "right" => KeyKind::Right,
        "home" => KeyKind::Home,
        "end" => KeyKind::End,
        "pageup" | "pgup" => KeyKind::PageUp,
        "pagedown" | "pgdn" => KeyKind::PageDown,
        "f1" => KeyKind::Function(1),
        "f2" => KeyKind::Function(2),
        "f3" => KeyKind::Function(3),
        "f4" => KeyKind::Function(4),
        "f5" => KeyKind::Function(5),
        "f6" => KeyKind::Function(6),
        "f7" => KeyKind::Function(7),
        "f8" => KeyKind::Function(8),
        "f9" => KeyKind::Function(9),
        "f10" => KeyKind::Function(10),
        "f11" => KeyKind::Function(11),
        "f12" => KeyKind::Function(12),
        s if s.chars().count() == 1 => KeyKind::Char(s.chars().next()?),
        _ => return None,
    };

    Some(KeyInput {
        kind,
        shift,
        ctrl,
        alt,
        super_key: false,
    })
}

#[derive(Debug, Clone, Copy)]
pub struct KeyInput {
    pub kind: KeyKind,
    pub shift: bool,
    pub ctrl: bool,
    pub alt: bool,
    pub super_key: bool,
}

#[derive(Debug, Clone, Copy)]
pub enum KeyKind {
    Char(char),
    Enter,
    Tab,
    Backspace,
    Escape,
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    Insert,
    Delete,
    PageUp,
    PageDown,
    Function(u8),
}

fn encode_cursor_key(letter: u8, mod_param: u8, app_cursor: bool) -> Vec<u8> {
    let ch = letter as char;
    if mod_param > 1 {
        format!("\x1b[1;{mod_param}{ch}").into_bytes()
    } else if app_cursor {
        vec![0x1b, b'O', letter]
    } else {
        vec![0x1b, b'[', letter]
    }
}

fn encode_tilde_key(code: u8, mod_param: u8) -> Vec<u8> {
    if mod_param > 1 {
        format!("\x1b[{code};{mod_param}~").into_bytes()
    } else {
        format!("\x1b[{code}~").into_bytes()
    }
}

fn encode_function_key(n: u8, mod_param: u8) -> Vec<u8> {
    match n {
        1..=4 => {
            let letter = b'P' + (n - 1);
            if mod_param > 1 {
                format!("\x1b[1;{mod_param}{}", letter as char).into_bytes()
            } else {
                vec![0x1b, b'O', letter]
            }
        }
        5 => encode_tilde_key(15, mod_param),
        6 => encode_tilde_key(17, mod_param),
        7 => encode_tilde_key(18, mod_param),
        8 => encode_tilde_key(19, mod_param),
        9 => encode_tilde_key(20, mod_param),
        10 => encode_tilde_key(21, mod_param),
        11 => encode_tilde_key(23, mod_param),
        12 => encode_tilde_key(24, mod_param),
        _ => Vec::new(),
    }
}

fn parse_osc7_uri(uri: &str) -> Option<PathBuf> {
    let rest = uri.strip_prefix("file://")?;
    let slash_pos = rest.find('/')?;
    let raw_path = &rest[slash_pos..];
    Some(PathBuf::from(percent_decode(raw_path)))
}

fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let (Some(hi), Some(lo)) = (hex_val(bytes[i + 1]), hex_val(bytes[i + 2]))
        {
            out.push((hi << 4) | lo);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

fn parse_osc9_progress(rest: &str) -> Option<ProgressState> {
    let stripped = rest.strip_prefix("4;")?;
    let mut parts = stripped.split(';');
    let state_code = parts.next()?.parse::<u8>().ok()?;
    let pct = parts
        .next()
        .and_then(|p| p.parse::<u8>().ok())
        .unwrap_or(0)
        .min(100);

    match state_code {
        0 => Some(ProgressState::Hidden),
        1 => Some(ProgressState::Normal(pct)),
        2 => Some(ProgressState::Error(pct)),
        3 => Some(ProgressState::Indeterminate),
        4 => Some(ProgressState::Paused(pct)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn captures_chunked_osc_133_command_output_and_exit_code() {
        let mut engine = PaneEngine::new_headless(40, 4);
        engine.advance_bytes(b"\x1b]133;A\x07$ \x1b]133;B\x07echo hi\r\n\x1b]133;");
        let evs =
            engine.advance_bytes(b"C\x07line one\r\nline two\r\n\x1b]133;D;17\x07\x1b]133;A\x07$ ");

        let last = engine
            .last_command()
            .expect("expected captured CommandRecord");
        assert_eq!(last.exit_code, Some(17));
        assert_eq!(last.output, "line one\nline two");
        assert_eq!(engine.attention(), Some(AttentionReason::CommandFailed(17)));
        assert!(
            evs.iter()
                .any(|e| matches!(e, EngineNotification::CommandFinished(_)))
        );
        assert_eq!(engine.prompt_seq(), 2);
    }

    #[test]
    fn captures_osc_133_output_across_scrollback_rotation() {
        let mut engine = PaneEngine::new_headless(30, 3);
        engine.advance_bytes(
            b"\x1b]133;A\x07$ \x1b]133;C\x07r1\r\nr2\r\nr3\r\nr4\r\nr5\r\n\x1b]133;D;0\x07",
        );
        let last = engine
            .last_command()
            .expect("expected scrolled command record");
        assert_eq!(last.exit_code, Some(0));
        assert_eq!(last.output, "r1\nr2\nr3\nr4\nr5");
    }

    #[test]
    fn tracks_osc7_cwd_and_osc9_progress() {
        let mut engine = PaneEngine::new_headless(40, 5);
        engine.advance_bytes(b"\x1b]7;file://myhost/home/user/Code/my%20repo\x1b\\");
        assert_eq!(engine.cwd(), Some(Path::new("/home/user/Code/my repo")));

        engine.advance_bytes(b"\x1b]9;4;1;65\x07");
        assert_eq!(engine.progress(), ProgressState::Normal(65));
    }

    #[test]
    fn encodes_kitty_disambiguated_keys_and_sanitizes_bracketed_paste() {
        let mut engine = PaneEngine::new_headless(40, 5);
        // Enable Kitty keyboard flag 1 (DISAMBIGUATE_ESC_CODES) + bracketed paste
        engine.advance_bytes(b"\x1b[>1u\x1b[?2004h");

        let seq = engine
            .encode_key(KeyInput {
                kind: KeyKind::Char('c'),
                shift: false,
                ctrl: true,
                alt: false,
                super_key: false,
            })
            .unwrap();
        assert_eq!(seq, b"\x1b[99;5u");

        let pasted = engine.format_paste("safe\x1b[201~evil");
        assert_eq!(pasted, b"\x1b[200~safeevil\x1b[201~");
    }

    #[test]
    fn snapshot_quiescence_named_keys_and_scrollback_search_work() {
        let mut engine = PaneEngine::new_headless(40, 4);
        let initial_out = engine.output_seq();
        assert!(!engine.has_turn_settled(0, initial_out, Duration::from_millis(5)));

        engine.advance_bytes("first token\r\nNEEDLE_ROW_42\r\n❯ ".as_bytes());
        std::thread::sleep(Duration::from_millis(12));
        assert!(engine.has_turn_settled(0, initial_out, Duration::from_millis(5)));

        let snap = engine.snapshot(None);
        assert_eq!(snap.cols, 40);
        assert_eq!(snap.lines.last().map(String::as_str), Some("❯"));

        let hits = engine.search_scrollback("needle_row", true, 5);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].text, "NEEDLE_ROW_42");

        let enter = parse_named_key("enter").unwrap();
        assert_eq!(engine.encode_key(enter).unwrap(), b"\r");
        let ctrl_c = parse_named_key("ctrl-c").unwrap();
        assert_eq!(engine.encode_key(ctrl_c).unwrap(), vec![0x03]);
    }

    fn fill_history(engine: &mut PaneEngine, lines: usize) {
        let mut buf = String::new();
        for i in 0..lines {
            buf.push_str(&format!("row {i} some searchable content here\r\n"));
        }
        engine.advance_bytes(buf.as_bytes());
    }

    #[test]
    fn chunked_scan_equals_whole_scan_for_hits_and_misses() {
        let mut engine = PaneEngine::new_headless(40, 6);
        fill_history(&mut engine, 500);
        for needle in ["searchable", "absent-needle"] {
            let whole = engine.search_scrollback(needle, true, 20);
            let mut cursor = engine.new_search(needle, true, 20);
            while !cursor.is_done() {
                engine.scan_search_chunk(&mut cursor, 7);
            }
            assert_eq!(cursor.matches(), whole.as_slice(), "needle {needle}");
        }
    }

    #[test]
    fn one_chunk_cannot_scan_a_full_history() {
        let mut engine = PaneEngine::new_headless(40, 6);
        fill_history(&mut engine, 12_000);
        let history = engine.snapshot(None).scrollback_lines;
        assert!(
            history >= 10_000,
            "expected a full scrollback, got {history}"
        );

        engine.begin_search("absent-needle", true, 20);
        let first = engine
            .advance_search(SEARCH_LINES_PER_TICK)
            .expect("search");
        assert!(
            !first.done,
            "a full-history miss must not finish in one chunk"
        );
        assert_eq!(first.lines_scanned, SEARCH_LINES_PER_TICK);
        assert_eq!(first.matches, 0);

        let mut chunks = 1usize;
        loop {
            let p = engine
                .advance_search(SEARCH_LINES_PER_TICK)
                .expect("search");
            chunks += 1;
            assert!(
                p.lines_scanned <= chunks * SEARCH_LINES_PER_TICK,
                "chunk exceeded its line budget"
            );
            if p.done {
                break;
            }
            assert!(chunks < 1_000, "scan must terminate");
        }
        let total_lines = history + 6;
        assert!(
            chunks >= total_lines / SEARCH_LINES_PER_TICK,
            "expected the miss to spread over many ticks, took {chunks}"
        );
        assert_eq!(engine.search().unwrap().matches().len(), 0);
    }

    #[test]
    fn search_restarts_when_output_shifts_the_grid() {
        let mut engine = PaneEngine::new_headless(40, 6);
        fill_history(&mut engine, 200);
        engine.begin_search("row 150", true, 20);
        let first = engine.advance_search(50).expect("search");
        assert!(!first.restarted);

        engine.advance_bytes(b"new output line\r\n");
        let after = engine.advance_search(50).expect("search");
        assert!(after.restarted, "output must invalidate the stale cursor");

        while !engine.search().unwrap().is_done() {
            engine.advance_search(SEARCH_LINES_PER_TICK);
        }
        let matches = engine.search().unwrap().matches();
        assert!(
            matches.iter().any(|m| m.text.contains("row 150")),
            "restarted scan must still find the needle: {matches:?}"
        );
    }

    #[test]
    fn navigate_search_scrolls_to_a_scrollback_match_and_highlights_it() {
        let mut engine = PaneEngine::new_headless(40, 6);
        fill_history(&mut engine, 300);
        engine.begin_search("row 5 ", true, 5);
        while !engine.search().unwrap().is_done() {
            engine.advance_search(64);
        }
        assert_eq!(engine.search().unwrap().matches().len(), 1);

        let before = engine.snapshot(None).display_offset;
        assert_eq!(engine.navigate_search(true), Some(0));
        let after = engine.snapshot(None).display_offset;
        assert_ne!(before, after, "navigating into history must scroll");
        assert!(
            engine
                .selection_text()
                .is_some_and(|t| t.contains("row 5 ")),
            "the matched line should be selected for highlighting"
        );
        assert_eq!(engine.search().unwrap().cursor(), Some(0));
    }

    #[test]
    fn empty_query_and_zero_cap_finish_immediately() {
        let mut engine = PaneEngine::new_headless(40, 6);
        fill_history(&mut engine, 50);
        engine.begin_search("", true, 20);
        assert!(engine.advance_search(1).unwrap().done);
        engine.begin_search("row", true, 0);
        assert!(engine.advance_search(1).unwrap().done);
        assert!(engine.search().unwrap().matches().is_empty());
        engine.cancel_search();
        assert!(engine.search().is_none());
        assert!(engine.advance_search(64).is_none());
    }
}
