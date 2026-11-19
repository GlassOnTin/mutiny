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
}

impl PaneEngine {
    pub fn new_headless(cols: usize, rows: usize) -> Self {
        let size = GridSize::new(cols, rows, 9, 18);
        let events = EventCollector::default();
        let config = TermConfig {
            scrolling_history: 10_000,
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
        }
    }

    pub fn spawn_pty(
        size: GridSize,
        working_directory: Option<PathBuf>,
        shell_override: Option<(String, Vec<String>)>,
        pane_id_num: u64,
    ) -> io::Result<Self> {
        tty::setup_env();
        let mut env = HashMap::new();
        env.insert(String::from("TERM_PROGRAM"), String::from("mutiny"));
        env.insert(String::from("MUTINY_PANE_ID"), pane_id_num.to_string());

        let opts = PtyOptions {
            shell: shell_override.map(|(prog, args)| Shell::new(prog, args)),
            working_directory: working_directory.clone(),
            drain_on_exit: true,
            env,
        };
        let pty = tty::new(&opts, size.to_window_size(), pane_id_num)?;
        let mut engine = Self::new_headless(size.cols, size.rows);
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

    pub fn search_scrollback(
        &self,
        needle: &str,
        ignore_case: bool,
        max_matches: usize,
    ) -> Vec<ScrollbackMatch> {
        if needle.is_empty() || max_matches == 0 {
            return Vec::new();
        }
        let grid = self.term.grid();
        let history = grid.history_size() as i32;
        let last_line = self.size.rows.saturating_sub(1) as i32;
        let last_col = Column(self.size.cols.saturating_sub(1));
        let folded_needle = if ignore_case {
            needle.to_lowercase()
        } else {
            needle.to_owned()
        };

        let mut matches = Vec::new();
        for line_idx in (-history..=last_line).rev() {
            let pt_start = Point::new(Line(line_idx), Column(0));
            let pt_end = Point::new(Line(line_idx), last_col);
            let row_text = self.term.bounds_to_string(pt_start, pt_end);
            let trimmed = row_text.trim_end();
            if trimmed.is_empty() {
                continue;
            }
            let hit = if ignore_case {
                trimmed.to_lowercase().contains(&folded_needle)
            } else {
                trimmed.contains(&folded_needle)
            };
            if hit {
                matches.push(ScrollbackMatch {
                    grid_line: line_idx,
                    text: trimmed.to_owned(),
                });
                if matches.len() >= max_matches {
                    break;
                }
            }
        }
        matches
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
}
