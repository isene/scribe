//! scribe — modal text editor for writers.
//!
//! Phase 0: Normal mode (hjkl, i/a/o/I/A/O), Insert mode (type to insert,
//! Esc back), Command mode (:w / :q / :wq / :q!).
//!
//! Architecture: rope-backed buffer + undo tree (buffer.rs), explicit mode
//! state (mode.rs), three crust panes (header / main / status). Cursor is
//! a (line, col) pair; rendering converts to byte / char indexes via the
//! rope's helpers.

mod buffer;
mod mode;
mod motion;
mod register;
mod search;
mod textobj;

use buffer::Buffer;
use crust::{Crust, Input, Pane};
use crust::style;
use mode::Mode;
use register::{Registers, YankKind};
use search::{Direction, SearchState};
use std::path::PathBuf;

const VERSION: &str = env!("CARGO_PKG_VERSION");

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let path: Option<PathBuf> = args.get(1).map(PathBuf::from);

    Crust::init();
    Crust::set_app_identity("Scribe");

    let mut app = App::new(path);
    app.render_all();

    loop {
        let Some(key) = Input::getchr(None) else { continue };
        let quit = match app.mode {
            Mode::Normal      => app.handle_normal(&key),
            Mode::Insert      => app.handle_insert(&key),
            Mode::Command     => app.handle_command(&key),
            Mode::Visual      |
            Mode::VisualLine  |
            Mode::VisualBlock => app.handle_visual(&key),
        };
        if quit { break; }
        app.render_all();
    }

    Crust::cleanup();
    Crust::clear_screen();
}

/// Pending Normal-mode command being assembled key-by-key.
#[derive(Default, Clone)]
struct Pending {
    count1: Option<usize>,
    count2: Option<usize>,
    operator: Option<char>,
    register: Option<char>,
    awaiting_char: Option<char>,
    g_prefix: bool,
    register_prefix: bool,
    /// After operator + `i` or `a`, awaiting the text-object selector char.
    text_object: Option<char>, // 'i' or 'a'
}

/// Captured "last change" for the `.` dot-repeat command. Replaying replays
/// the operator + motion + (for change ops) the inserted text in one go.
#[derive(Clone)]
enum LastChange {
    Op {
        op: char,
        motion: ChangeMotion,
        count: usize,
        register: Option<char>,
        /// Text inserted while in Insert mode after `c`-style ops. Empty for d/y.
        insert_text: String,
    },
    Replace { c: char, count: usize },
    Insert { text: String, append: bool },
    Paste { after: bool, count: usize, register: Option<char> },
    SimpleAction { key: String, count: usize, register: Option<char> },
}

/// Motion descriptor stable enough to replay verbatim regardless of cursor
/// position.
#[derive(Clone)]
enum ChangeMotion {
    Key(String),                              // simple key like "w", "$", "gg" (encoded), etc.
    TextObject { kind: char, target: char },  // ('i', 'w'), ('a', '"'), etc.
    Linewise { extra: usize },                // dd / yy / cc — `extra` lines below cursor
}

impl Pending {
    fn count(&self) -> usize {
        let c1 = self.count1.unwrap_or(1);
        let c2 = self.count2.unwrap_or(1);
        (c1 * c2).max(1)
    }
    fn clear(&mut self) { *self = Pending::default(); }
}

struct App {
    buf: Buffer,
    mode: Mode,
    /// Cursor as (line, col) — col is byte offset within the line, NOT char.
    cur_line: usize,
    cur_col: usize,
    want_col: usize,
    scroll: usize,
    cmdline: String,
    status: Option<(String, u8)>,
    cols: u16,
    rows: u16,
    header: Pane,
    main_p: Pane,
    footer: Pane,
    pending: Pending,
    regs: Registers,
    search: SearchState,
    /// Anchor byte offset for Visual mode selection (Visual / VisualLine).
    /// In VisualBlock the anchor is (line, col).
    visual_anchor: usize,
    visual_anchor_line: usize,
    visual_anchor_col: usize,
    /// Last completed change, for `.` repeat.
    last_change: Option<LastChange>,
    /// While true we're capturing keystrokes typed in Insert mode after a
    /// change-op for dot-repeat replay.
    capturing_insert: bool,
    captured_insert: String,
}

impl App {
    fn new(path: Option<PathBuf>) -> Self {
        let (cols, rows) = Crust::terminal_size();
        let mut header = Pane::new(1, 1, cols, 1, 255, 236);
        header.wrap = false; header.scroll = false;
        let mut main_p = Pane::new(1, 2, cols, rows.saturating_sub(2), 252, 0);
        main_p.wrap = false;
        let mut footer = Pane::new(1, rows, cols, 1, 255, 236);
        footer.wrap = false; footer.scroll = false;

        let buf = match path {
            Some(p) => Buffer::from_path(p).unwrap_or_else(|_| Buffer::empty()),
            None    => Buffer::empty(),
        };

        Self {
            buf, mode: Mode::Normal,
            cur_line: 0, cur_col: 0, want_col: 0, scroll: 0,
            cmdline: String::new(), status: None,
            cols, rows, header, main_p, footer,
            pending: Pending::default(),
            regs: Registers::new(),
            search: SearchState::new(),
            visual_anchor: 0,
            visual_anchor_line: 0,
            visual_anchor_col: 0,
            last_change: None,
            capturing_insert: false,
            captured_insert: String::new(),
        }
    }

    // ── Rendering ──────────────────────────────────────────────────────
    fn render_all(&mut self) {
        self.render_header();
        self.render_main();
        self.render_footer();
        self.position_cursor();
    }

    /// Show the host terminal's native cursor at the buffer cursor location
    /// and pick the shape for the current mode. Glass (and other terminals)
    /// render the cursor in the user-configured cursor color, so we get
    /// that for free instead of painting a fake one ourselves.
    ///
    /// CSI codes:
    ///   `\x1b[?25h`     show cursor
    ///   `\x1b[N q`      shape: 2=block, 4=underline, 6=bar
    ///   `\x1b[r;cH`     position (1-based)
    fn position_cursor(&self) {
        let pane_top = self.main_p.y;
        let row_in_pane = (self.cur_line.saturating_sub(self.scroll)) as u16;
        let row = pane_top + row_in_pane;
        let col = (self.cur_col as u16) + self.main_p.x;
        let shape = match self.mode {
            Mode::Insert      => 6,   // bar
            Mode::VisualBlock => 2,   // block
            Mode::Visual
            | Mode::VisualLine => 2,
            Mode::Normal      => 2,   // block
            Mode::Command     => 6,   // bar in :cmdline
        };
        print!("\x1b[?25h\x1b[{} q\x1b[{};{}H", shape, row, col);
        use std::io::Write as _;
        std::io::stdout().flush().ok();
    }

    fn render_header(&mut self) {
        let name = self.buf.path.as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "[no name]".into());
        let dirty = if self.buf.dirty { " *" } else { "" };
        let lines = self.buf.line_count();
        let info = format!(" {}{}  ({} lines)", name, dirty, lines);
        self.header.say(&style::bold(&info));
    }

    fn render_main(&mut self) {
        let pane_h = self.main_p.h as usize;
        if self.cur_line < self.scroll { self.scroll = self.cur_line; }
        if self.cur_line >= self.scroll + pane_h { self.scroll = self.cur_line + 1 - pane_h; }

        // Compute selection range for visual modes.
        let (sel_start, sel_end, sel_kind) = if self.mode.is_visual() {
            let cur = self.cursor_byte();
            let (lo, hi) = if cur < self.visual_anchor { (cur, self.visual_anchor) } else { (self.visual_anchor, cur) };
            // Charwise: include cursor cell.
            let s = self.buf.rope.to_string();
            let mut hi2 = hi;
            if hi2 < s.len() {
                let mut p = hi2 + 1;
                while p < s.len() && !s.is_char_boundary(p) { p += 1; }
                hi2 = p;
            }
            (Some(lo), Some(hi2), Some(self.mode))
        } else {
            (None, None, None)
        };

        let mut out = String::new();
        for i in 0..pane_h {
            let line_idx = self.scroll + i;
            if line_idx < self.buf.line_count() {
                let line = self.buf.line(line_idx);
                let line_byte_off = self.buf.line_byte_offset(line_idx);
                // Build the line char-by-char so we can apply selection bg
                // and cursor-cell highlight in one pass.
                let mut col = 0usize;
                while col < line.len() {
                    if !line.is_char_boundary(col) { col += 1; continue; }
                    let mut ce = col + 1;
                    while ce < line.len() && !line.is_char_boundary(ce) { ce += 1; }
                    let glyph = &line[col..ce];
                    let abs = line_byte_off + col;
                    let in_sel = match (sel_start, sel_end, sel_kind) {
                        (Some(s), Some(e), Some(Mode::Visual)) => abs >= s && abs < e,
                        (_, _, Some(Mode::VisualLine)) => {
                            let l1 = self.visual_anchor_line.min(self.cur_line);
                            let l2 = self.visual_anchor_line.max(self.cur_line);
                            line_idx >= l1 && line_idx <= l2
                        }
                        (_, _, Some(Mode::VisualBlock)) => {
                            let l1 = self.visual_anchor_line.min(self.cur_line);
                            let l2 = self.visual_anchor_line.max(self.cur_line);
                            let c1 = self.visual_anchor_col.min(self.cur_col);
                            let c2 = self.visual_anchor_col.max(self.cur_col);
                            line_idx >= l1 && line_idx <= l2 && col >= c1 && col <= c2
                        }
                        _ => false,
                    };
                    if in_sel {
                        out.push_str(&style::bg(glyph, 238));  // subtle gray
                    } else {
                        out.push_str(glyph);
                    }
                    col = ce;
                }
                // VisualLine extends selection past line end visually.
                if sel_kind == Some(Mode::VisualLine) {
                    let l1 = self.visual_anchor_line.min(self.cur_line);
                    let l2 = self.visual_anchor_line.max(self.cur_line);
                    if line_idx >= l1 && line_idx <= l2 {
                        let pad_w = (self.cols as usize).saturating_sub(line.chars().count());
                        if pad_w > 0 {
                            out.push_str(&style::bg(&" ".repeat(pad_w), 238));
                        }
                    }
                }
            } else {
                out.push_str(&style::fg("~", 244));
            }
            if i + 1 < pane_h { out.push('\n'); }
        }
        self.main_p.set_text(&out);
        self.main_p.full_refresh();
    }

    fn render_footer(&mut self) {
        // SGR-aware: each style::fg / style::bg helper closes with \x1b[0m,
        // which resets BACKGROUND to terminal default. After every styled
        // segment we re-assert the pane's bg so the gap spaces don't render
        // as black streaks. The whole line ends with one final \x1b[0m.
        const BG: u8 = 236;
        let bg_on = format!("\x1b[48;5;{}m", BG);

        let mode_label = style::bg(&style::fg(self.mode.label(), 0), self.mode.color());
        let pos = format!(" {}:{} ", self.cur_line + 1, self.cur_col + 1);
        let right = format!("scribe v{} ", VERSION);

        let middle_plain: String = if self.mode == Mode::Command {
            format!(" :{}", self.cmdline)
        } else if let Some((ref msg, _c)) = self.status {
            // Width-only — color is applied via style::fg inline below.
            msg.clone()
        } else {
            String::new()
        };

        let middle_styled: String = if self.mode == Mode::Command {
            format!(" :{}", self.cmdline)
        } else if let Some((ref msg, c)) = self.status {
            // Reset to pane bg AFTER fg-styling so the trailing \x1b[0m from
            // style::fg doesn't drop us to terminal-default bg.
            format!("{}{}", style::fg(msg, c), bg_on)
        } else {
            String::new()
        };

        let cols = self.cols as usize;
        let mode_w = crust::display_width(&mode_label);
        let middle_w = crust::display_width(&middle_plain);
        let pos_w = crust::display_width(&pos);
        let right_w = crust::display_width(&right);

        let total = mode_w + middle_w + pos_w + right_w;
        let line = if total <= cols {
            let gap = cols - total;
            // Order: badge (its own bg) → bg_on → middle_styled → spaces →
            // pos → right → final reset. bg_on after every helper that
            // ends in [0m. spaces inherit bg_on so the gap fills.
            format!("{}{}{}{}{}{}\x1b[0m",
                mode_label, bg_on, middle_styled, " ".repeat(gap), pos, right)
        } else {
            let visible = format!("{}{}{}", mode_label, bg_on, middle_styled);
            let visible_w = mode_w + middle_w;
            let pad = cols.saturating_sub(visible_w);
            format!("{}{}\x1b[0m", visible, " ".repeat(pad))
        };
        self.footer.say(&line);
    }

    fn set_status(&mut self, msg: &str, c: u8) { self.status = Some((msg.into(), c)); }

    // ── Cursor helpers ─────────────────────────────────────────────────
    fn current_line_len(&self) -> usize {
        self.buf.line(self.cur_line).len()
    }

    /// Maximum legal cursor column on the current line. In Insert mode the
    /// cursor can sit just past the last char (so Backspace and append work);
    /// in Normal mode it can't (so `x` always deletes a real char).
    fn col_cap(&self) -> usize {
        let len = self.current_line_len();
        if self.mode == Mode::Insert { len } else { len.saturating_sub(1) }
    }

    fn clamp_col_to_line(&mut self) {
        let cap = self.col_cap();
        if self.cur_col > cap { self.cur_col = cap; }
    }

    fn cursor_byte(&self) -> usize {
        self.buf.line_byte_offset(self.cur_line) + self.cur_col
    }

    /// Move cursor to absolute byte offset (used by undo/redo to land where
    /// the edit happened). Clamps to legal cap for current mode.
    fn cursor_to_byte(&mut self, byte: usize) {
        let total = self.buf.rope.len_bytes();
        let byte = byte.min(total);
        let (line, col) = self.buf.byte_to_line_col(byte);
        self.cur_line = line;
        self.cur_col = col;
        self.clamp_col_to_line();
        self.want_col = self.cur_col;
    }

    // ── Wrapping motion ────────────────────────────────────────────────
    /// Move one char left, wrapping to end of previous line when at column 0.
    fn move_left_wrap(&mut self) {
        if self.cur_col > 0 {
            self.cur_col -= 1;
        } else if self.cur_line > 0 {
            self.cur_line -= 1;
            self.cur_col = self.col_cap();
        }
        self.want_col = self.cur_col;
    }

    /// Move one char right, wrapping to start of next line when at line end.
    fn move_right_wrap(&mut self) {
        let cap = self.col_cap();
        if self.cur_col < cap {
            self.cur_col += 1;
        } else if self.cur_line + 1 < self.buf.line_count() {
            self.cur_line += 1;
            self.cur_col = 0;
        }
        self.want_col = self.cur_col;
    }

    fn move_up(&mut self) {
        if self.cur_line > 0 {
            self.cur_line -= 1;
            self.cur_col = self.want_col.min(self.col_cap());
        }
    }

    fn move_down(&mut self) {
        if self.cur_line + 1 < self.buf.line_count() {
            self.cur_line += 1;
            self.cur_col = self.want_col.min(self.col_cap());
        }
    }

    // ── Normal mode (pending state machine) ────────────────────────────
    fn handle_normal(&mut self, key: &str) -> bool {
        self.status = None;

        // Text-object resolution: after operator + 'i' or 'a', the next key
        // selects the object (w / W / " / ' / ( / [ / { / p / b / B).
        if let Some(kind) = self.pending.text_object.take() {
            let target = match key.chars().next() { Some(c) => c, None => { self.pending.clear(); return false; } };
            let cur = self.cursor_byte();
            let range_opt: Option<(usize, usize)> = match (kind, target) {
                ('i', 'w') => textobj::inner_word(&self.buf, cur),
                ('a', 'w') | ('a', 'W') => textobj::around_word(&self.buf, cur),
                ('i', 'W') => textobj::inner_word(&self.buf, cur),
                ('i', '"') => textobj::inner_quote(&self.buf, cur, '"'),
                ('a', '"') => textobj::around_quote(&self.buf, cur, '"'),
                ('i', '\'') => textobj::inner_quote(&self.buf, cur, '\''),
                ('a', '\'') => textobj::around_quote(&self.buf, cur, '\''),
                ('i', '`') => textobj::inner_quote(&self.buf, cur, '`'),
                ('a', '`') => textobj::around_quote(&self.buf, cur, '`'),
                ('i', '(') | ('i', ')') | ('i', 'b') => textobj::inner_pair(&self.buf, cur, '(', ')'),
                ('a', '(') | ('a', ')') | ('a', 'b') => textobj::around_pair(&self.buf, cur, '(', ')'),
                ('i', '[') | ('i', ']') => textobj::inner_pair(&self.buf, cur, '[', ']'),
                ('a', '[') | ('a', ']') => textobj::around_pair(&self.buf, cur, '[', ']'),
                ('i', '{') | ('i', '}') | ('i', 'B') => textobj::inner_pair(&self.buf, cur, '{', '}'),
                ('a', '{') | ('a', '}') | ('a', 'B') => textobj::around_pair(&self.buf, cur, '{', '}'),
                ('i', '<') | ('i', '>') => textobj::inner_pair(&self.buf, cur, '<', '>'),
                ('a', '<') | ('a', '>') => textobj::around_pair(&self.buf, cur, '<', '>'),
                ('i', 'p') => textobj::inner_paragraph(&self.buf, cur),
                ('a', 'p') => textobj::around_paragraph(&self.buf, cur),
                _ => None,
            };
            if let (Some((start, end)), Some(opc)) = (range_opt, self.pending.operator) {
                self.execute_op_charwise(opc, start, end);
                self.last_change = Some(LastChange::Op {
                    op: opc,
                    motion: ChangeMotion::TextObject { kind, target },
                    count: 1,
                    register: self.pending.register,
                    insert_text: String::new(),
                });
            }
            self.pending.clear();
            return false;
        }

        // Awaiting a single character (for r, f, F, t, T)?
        if let Some(op) = self.pending.awaiting_char {
            self.pending.awaiting_char = None;
            if key == "ESC" { self.pending.clear(); return false; }
            let c = match key.chars().next() {
                Some(ch) if !ch.is_control() => ch,
                _ => { self.pending.clear(); return false; }
            };
            match op {
                'r' => self.do_replace_char(c),
                'f' => self.do_find_forward(c, false),
                'F' => self.do_find_backward(c, false),
                't' => self.do_find_forward(c, true),
                'T' => self.do_find_backward(c, true),
                _ => {}
            }
            self.pending.clear();
            return false;
        }

        // Awaiting register name (after `"`)?
        if self.pending.register_prefix {
            self.pending.register_prefix = false;
            if let Some(c) = key.chars().next() {
                if c.is_ascii_alphanumeric() || c == '+' || c == '*' || c == '"' {
                    self.pending.register = Some(c);
                    return false;
                }
            }
            self.pending.clear();
            return false;
        }

        // ESC anywhere clears pending state.
        if key == "ESC" { self.pending.clear(); return false; }

        // Digit prefix (counts). '0' counts only when count is already in
        // progress; otherwise it's a motion to line-start.
        if let Some(d) = key.chars().next().and_then(|c| c.to_digit(10).map(|x| x as usize)) {
            let count_in_progress = if self.pending.operator.is_none() {
                self.pending.count1.is_some()
            } else {
                self.pending.count2.is_some()
            };
            if d != 0 || count_in_progress {
                if self.pending.operator.is_none() {
                    self.pending.count1 = Some(self.pending.count1.unwrap_or(0) * 10 + d);
                } else {
                    self.pending.count2 = Some(self.pending.count2.unwrap_or(0) * 10 + d);
                }
                return false;
            }
        }

        // `g` prefix.
        if self.pending.g_prefix {
            self.pending.g_prefix = false;
            match key {
                "g" => {
                    let target = 0;
                    if self.pending.operator.is_some() {
                        self.execute_op_linewise(self.cur_line, 0);
                    } else {
                        self.cursor_to_byte(target);
                    }
                }
                _ => {}
            }
            self.pending.clear();
            return false;
        }
        if key == "g" { self.pending.g_prefix = true; return false; }

        // Operator handling: `d`, `c`, `y` — doubled = linewise on count1 lines.
        if matches!(key, "d" | "c" | "y") {
            let opc = key.chars().next().unwrap();
            if self.pending.operator == Some(opc) {
                let n = self.pending.count1.unwrap_or(1);
                let extra = n.saturating_sub(1);
                let cap_count = self.pending.count1.unwrap_or(1);
                let cap_reg = self.pending.register;
                self.execute_op_linewise(self.cur_line, extra);
                self.last_change = Some(LastChange::Op {
                    op: opc,
                    motion: ChangeMotion::Linewise { extra },
                    count: cap_count,
                    register: cap_reg,
                    insert_text: String::new(),
                });
                self.pending.clear();
                return false;
            }
            self.pending.operator = Some(opc);
            return false;
        }

        // Text-object trigger: `i` or `a` AFTER an operator selects a TO.
        if (key == "i" || key == "a") && self.pending.operator.is_some() {
            self.pending.text_object = Some(key.chars().next().unwrap());
            return false;
        }

        // Visual mode entry.
        if self.pending.operator.is_none() {
            match key {
                "v"   => { self.enter_visual(Mode::Visual); return false; }
                "V"   => { self.enter_visual(Mode::VisualLine); return false; }
                "C-V" => { self.enter_visual(Mode::VisualBlock); return false; }
                _ => {}
            }
        }

        // Find-on-line: f/F/t/T expect a follow-up char. Set awaiting_char
        // BEFORE parse_motion so the pending state survives and the next
        // key triggers the find. (Previously this was inside parse_motion
        // which returned None and let pending.clear() wipe the await flag.)
        if matches!(key, "f" | "F" | "t" | "T") {
            self.pending.awaiting_char = Some(key.chars().next().unwrap());
            return false;
        }

        // Dot-repeat.
        if key == "." && self.pending.operator.is_none() {
            self.repeat_last_change();
            self.pending.clear();
            return false;
        }

        // Register prefix.
        if key == "\"" {
            self.pending.register_prefix = true;
            return false;
        }

        // Motion / action dispatch. With operator → range op; without → motion.
        let count = self.pending.count();
        let op = self.pending.operator;

        // Try as motion first.
        if let Some(target_byte) = self.parse_motion(key, count) {
            if let Some(opc) = op {
                let from = self.cursor_byte();
                let (start, end) = if from <= target_byte { (from, target_byte) } else { (target_byte, from) };
                let cap_reg = self.pending.register;
                self.execute_op_charwise(opc, start, end);
                self.last_change = Some(LastChange::Op {
                    op: opc,
                    motion: ChangeMotion::Key(key.to_string()),
                    count,
                    register: cap_reg,
                    insert_text: String::new(),
                });
            } else {
                self.cursor_to_byte(target_byte);
            }
            self.pending.clear();
            return false;
        }

        // Standalone actions.
        let r = self.pending.register;
        let n = count;
        let quit = self.handle_normal_action(key, n, r);
        self.pending.clear();
        quit
    }

    /// Parse a motion key. Returns the destination byte offset, or None if
    /// the key isn't a motion.
    fn parse_motion(&mut self, key: &str, count: usize) -> Option<usize> {
        let cur = self.cursor_byte();
        match key {
            "h" | "LEFT" => {
                let mut b = cur;
                for _ in 0..count {
                    let s = self.buf.rope.to_string();
                    if b == 0 { break; }
                    let mut p = b - 1;
                    while p > 0 && !s.is_char_boundary(p) { p -= 1; }
                    let prev_line = self.buf.rope.byte_to_line(p);
                    let cur_line = self.buf.rope.byte_to_line(b);
                    if prev_line != cur_line { break; }
                    b = p;
                }
                Some(b)
            }
            "l" | "RIGHT" => {
                let mut b = cur;
                let s = self.buf.rope.to_string();
                for _ in 0..count {
                    if b >= s.len() { break; }
                    let mut p = b + 1;
                    while p < s.len() && !s.is_char_boundary(p) { p += 1; }
                    let next_line = if p < s.len() { self.buf.rope.byte_to_line(p) } else { self.buf.rope.byte_to_line(b) };
                    let cur_line = self.buf.rope.byte_to_line(b);
                    if next_line != cur_line { break; }
                    b = p;
                }
                Some(b)
            }
            "j" | "DOWN" => {
                let target_line = (self.cur_line + count).min(self.buf.line_count().saturating_sub(1));
                let off = self.buf.line_byte_offset(target_line);
                let len = self.buf.line(target_line).len();
                Some(off + self.want_col.min(len.saturating_sub(1).max(0)))
            }
            "k" | "UP" => {
                let target_line = self.cur_line.saturating_sub(count);
                let off = self.buf.line_byte_offset(target_line);
                let len = self.buf.line(target_line).len();
                Some(off + self.want_col.min(len.saturating_sub(1).max(0)))
            }
            "0" | "HOME" => Some(motion::line_start(&self.buf, cur)),
            "^"          => Some(motion::line_first_nonblank(&self.buf, cur)),
            "$" | "END"  => {
                let mut end = motion::line_end(&self.buf, cur);
                // For motion (no operator), step back one char so we don't
                // sit past last visible char — vim semantics.
                if self.pending.operator.is_none() {
                    end = end.saturating_sub(1);
                    let line = self.buf.rope.byte_to_line(cur);
                    let line_start = self.buf.line_byte_offset(line);
                    if end < line_start { end = line_start; }
                }
                Some(end)
            }
            "w" => {
                let mut b = cur;
                for _ in 0..count { b = motion::word_forward(&self.buf, b); }
                Some(b)
            }
            "b" => {
                let mut b = cur;
                for _ in 0..count { b = motion::word_backward(&self.buf, b); }
                Some(b)
            }
            "e" => {
                let mut b = cur;
                for _ in 0..count { b = motion::word_end(&self.buf, b); }
                // Operators on `e` include the end char → bump one.
                if self.pending.operator.is_some() {
                    let s = self.buf.rope.to_string();
                    if b < s.len() {
                        let mut p = b + 1;
                        while p < s.len() && !s.is_char_boundary(p) { p += 1; }
                        b = p;
                    }
                }
                Some(b)
            }
            "W" => {
                let mut b = cur;
                for _ in 0..count { b = motion::big_word_forward(&self.buf, b); }
                Some(b)
            }
            "B" => {
                let mut b = cur;
                for _ in 0..count { b = motion::big_word_backward(&self.buf, b); }
                Some(b)
            }
            "G" => {
                let target_line = if self.pending.count1.is_some() {
                    count.saturating_sub(1).min(self.buf.line_count().saturating_sub(1))
                } else {
                    self.buf.line_count().saturating_sub(1)
                };
                Some(self.buf.line_byte_offset(target_line))
            }
            "n" => self.search_next(false),
            "N" => self.search_next(true),
            _ => None,
        }
    }

    /// Standalone Normal-mode actions that aren't motions. Returns true to
    /// quit.
    fn handle_normal_action(&mut self, key: &str, count: usize, _reg: Option<char>) -> bool {
        match key {
            // Page motion (not currently used as motion-target for ops).
            "PgDOWN" | "C-D" => {
                let step = (self.main_p.h as usize) / 2;
                self.cur_line = (self.cur_line + step).min(self.buf.line_count().saturating_sub(1));
                self.clamp_col_to_line();
            }
            "PgUP" | "C-U" => {
                let step = (self.main_p.h as usize) / 2;
                self.cur_line = self.cur_line.saturating_sub(step);
                self.clamp_col_to_line();
            }

            // Enter Insert
            "i" => self.enter_insert(),
            "a" => {
                let max = self.current_line_len();
                if self.cur_col < max { self.cur_col += 1; }
                self.enter_insert();
            }
            "I" => { self.cur_col = 0; self.enter_insert(); }
            "A" => { self.cur_col = self.current_line_len(); self.enter_insert(); }
            "o" => {
                let off = self.buf.line_byte_offset(self.cur_line) + self.current_line_len();
                self.buf.apply(off, off, "\n");
                self.cur_line += 1;
                self.cur_col = 0;
                self.enter_insert();
            }
            "O" => {
                let off = self.buf.line_byte_offset(self.cur_line);
                self.buf.apply(off, off, "\n");
                self.cur_col = 0;
                self.enter_insert();
            }

            // Edit primitives
            "x" => for _ in 0..count {
                let off = self.cursor_byte();
                let line = self.buf.line(self.cur_line);
                if self.cur_col < line.len() {
                    let mut e = self.cur_col + 1;
                    while e < line.len() && !line.is_char_boundary(e) { e += 1; }
                    let abs_end = self.buf.line_byte_offset(self.cur_line) + e;
                    self.buf.apply(off, abs_end, "");
                    self.clamp_col_to_line();
                }
            },
            "X" => for _ in 0..count {
                if self.cur_col > 0 {
                    let line = self.buf.line(self.cur_line);
                    let mut s = self.cur_col - 1;
                    while s > 0 && !line.is_char_boundary(s) { s -= 1; }
                    let abs_s = self.buf.line_byte_offset(self.cur_line) + s;
                    let abs_e = self.buf.line_byte_offset(self.cur_line) + self.cur_col;
                    self.buf.apply(abs_s, abs_e, "");
                    self.cur_col = s;
                    self.want_col = s;
                }
            },
            "D" => {
                let from = self.cursor_byte();
                let end = motion::line_end(&self.buf, from);
                self.execute_op_charwise('d', from, end);
            }
            "C" => {
                let from = self.cursor_byte();
                let end = motion::line_end(&self.buf, from);
                self.execute_op_charwise('c', from, end);
            }
            "Y" => {
                let n = count.saturating_sub(1);
                self.execute_op_linewise_yank(self.cur_line, self.cur_line + n);
            }
            "J" => for _ in 0..count.max(1) {
                if self.cur_line + 1 >= self.buf.line_count() { break; }
                let line_end_byte = motion::line_end(&self.buf, self.cursor_byte());
                let next_line_start = self.buf.line_byte_offset(self.cur_line + 1);
                // Drop any leading whitespace on next line. Replace newline +
                // ws with a single space.
                let next_line = self.buf.line(self.cur_line + 1);
                let trim_lead = next_line.chars().take_while(|c| c.is_whitespace()).map(|c| c.len_utf8()).sum::<usize>();
                let separator = if next_line.trim().is_empty() { "" } else { " " };
                self.buf.apply(line_end_byte, next_line_start + trim_lead, separator);
                self.cur_col = line_end_byte - self.buf.line_byte_offset(self.cur_line);
                self.want_col = self.cur_col;
            },
            "~" => {
                let off = self.cursor_byte();
                let line = self.buf.line(self.cur_line);
                if self.cur_col < line.len() {
                    let c = line[self.cur_col..].chars().next().unwrap();
                    let toggled: String = if c.is_ascii_uppercase() { c.to_ascii_lowercase().to_string() }
                                          else if c.is_ascii_lowercase() { c.to_ascii_uppercase().to_string() }
                                          else { c.to_string() };
                    let end = off + c.len_utf8();
                    self.buf.apply(off, end, &toggled);
                    self.cur_col += toggled.len();
                    self.want_col = self.cur_col;
                }
            }

            // Replace single char.
            "r" => self.pending.awaiting_char = Some('r'),

            // Paste.
            "p" => {
                self.do_paste(true, count);
                self.last_change = Some(LastChange::Paste { after: true, count, register: self.pending.register });
            }
            "P" => {
                self.do_paste(false, count);
                self.last_change = Some(LastChange::Paste { after: false, count, register: self.pending.register });
            }

            // Search.
            "/" => self.search_prompt(Direction::Forward),
            "?" => self.search_prompt(Direction::Backward),
            "*" => self.search_word_under_cursor(Direction::Forward),
            "#" => self.search_word_under_cursor(Direction::Backward),

            // Undo / redo
            "u" => match self.buf.undo() {
                Some(byte) => { self.cursor_to_byte(byte); self.set_status(" undo", 244); }
                None       => self.set_status(" already at oldest change", 244),
            },
            "C-R" => match self.buf.redo() {
                Some(byte) => { self.cursor_to_byte(byte); self.set_status(" redo", 244); }
                None       => self.set_status(" already at newest change", 244),
            },

            // Enter Command
            ":" => { self.cmdline.clear(); self.mode = Mode::Command; }

            // Fe2O3 harmonized quit
            "q" => {
                if self.buf.dirty {
                    if let Err(e) = self.buf.save() {
                        self.set_status(&format!(" save failed: {}", e), 196);
                        return false;
                    }
                }
                return true;
            }
            "Q" => return true,

            _ => {}
        }
        false
    }

    fn enter_insert(&mut self) {
        self.mode = Mode::Insert;
        self.capturing_insert = true;
        self.captured_insert.clear();
    }

    // ── Visual mode ────────────────────────────────────────────────────
    fn enter_visual(&mut self, kind: Mode) {
        self.mode = kind;
        self.visual_anchor = self.cursor_byte();
        self.visual_anchor_line = self.cur_line;
        self.visual_anchor_col = self.cur_col;
    }

    fn visual_range(&self) -> (usize, usize) {
        let cur = self.cursor_byte();
        let (lo, hi) = if cur < self.visual_anchor { (cur, self.visual_anchor) } else { (self.visual_anchor, cur) };
        // Charwise: include cursor cell.
        let s = self.buf.rope.to_string();
        let mut hi = hi;
        if hi < s.len() {
            let mut p = hi + 1;
            while p < s.len() && !s.is_char_boundary(p) { p += 1; }
            hi = p;
        }
        (lo, hi)
    }

    fn visual_line_range(&self) -> (usize, usize) {
        let l1 = self.visual_anchor_line.min(self.cur_line);
        let l2 = self.visual_anchor_line.max(self.cur_line);
        let start = self.buf.line_byte_offset(l1);
        let end = if l2 + 1 >= self.buf.line_count() {
            self.buf.rope.len_bytes()
        } else {
            self.buf.line_byte_offset(l2 + 1)
        };
        (start, end)
    }

    fn handle_visual(&mut self, key: &str) -> bool {
        // Esc / v(in same kind) returns to Normal.
        match (self.mode, key) {
            (_, "ESC") | (_, "C-[")
                | (Mode::Visual, "v")
                | (Mode::VisualLine, "V")
                | (Mode::VisualBlock, "C-V") => {
                self.mode = Mode::Normal;
                self.pending.clear();
                return false;
            }
            (Mode::Visual, "V") => { self.mode = Mode::VisualLine; return false; }
            (Mode::VisualLine, "v") => { self.mode = Mode::Visual; return false; }
            _ => {}
        }

        // Operator (d/c/y) or shortcut (x/X/D/C/Y/~) acts on the selection.
        match key {
            "d" | "x" | "X" | "D" => {
                self.apply_visual_op('d');
                return false;
            }
            "c" | "C" | "s" => {
                self.apply_visual_op('c');
                return false;
            }
            "y" | "Y" => {
                self.apply_visual_op('y');
                return false;
            }
            "~" => {
                self.apply_visual_case_toggle();
                return false;
            }
            "p" | "P" => {
                self.apply_visual_op('d');
                self.do_paste(key == "p", 1);
                return false;
            }
            ":" => { self.cmdline.clear(); self.mode = Mode::Command; return false; }
            "\"" => { self.pending.register_prefix = true; return false; }
            _ => {}
        }
        if self.pending.register_prefix {
            self.pending.register_prefix = false;
            if let Some(c) = key.chars().next() {
                if c.is_ascii_alphanumeric() || c == '+' || c == '*' || c == '"' {
                    self.pending.register = Some(c);
                }
            }
            return false;
        }

        // Otherwise treat as motion to extend selection.
        if let Some(target) = self.parse_motion(key, self.pending.count()) {
            self.cursor_to_byte(target);
        }
        self.pending.clear();
        false
    }

    fn apply_visual_op(&mut self, op: char) {
        let was_visual = self.mode;
        self.pending.operator = Some(op);
        match was_visual {
            Mode::Visual => {
                let (s, e) = self.visual_range();
                self.execute_op_charwise(op, s, e);
            }
            Mode::VisualLine => {
                let l1 = self.visual_anchor_line.min(self.cur_line);
                let l2 = self.visual_anchor_line.max(self.cur_line);
                self.cur_line = l1;
                self.execute_op_linewise(l1, l2 - l1);
            }
            Mode::VisualBlock => {
                self.apply_visual_block_op(op);
            }
            _ => {}
        }
        if op != 'c' { self.mode = Mode::Normal; }
        self.pending.clear();
    }

    fn apply_visual_case_toggle(&mut self) {
        let (s, e) = match self.mode {
            Mode::Visual => self.visual_range(),
            Mode::VisualLine => self.visual_line_range(),
            _ => return,
        };
        let text: String = self.buf.rope.byte_slice(s..e).to_string();
        let toggled: String = text.chars().map(|c| {
            if c.is_ascii_uppercase() { c.to_ascii_lowercase() }
            else if c.is_ascii_lowercase() { c.to_ascii_uppercase() }
            else { c }
        }).collect();
        self.buf.apply(s, e, &toggled);
        self.cursor_to_byte(s);
        self.mode = Mode::Normal;
    }

    /// Visual Block (Ctrl-v): apply op to each line at the same column range.
    fn apply_visual_block_op(&mut self, op: char) {
        let l1 = self.visual_anchor_line.min(self.cur_line);
        let l2 = self.visual_anchor_line.max(self.cur_line);
        let c1 = self.visual_anchor_col.min(self.cur_col);
        let c2 = self.visual_anchor_col.max(self.cur_col);
        // Group all per-line edits into one undo node so a single `u`
        // reverses the entire block op.
        if op != 'y' { self.buf.begin_compound(); }
        let mut yanked: Vec<String> = Vec::new();
        // Walk lines from bottom up so earlier byte offsets remain valid.
        for line in (l1..=l2).rev() {
            let line_text = self.buf.line(line);
            if c1 >= line_text.len() { yanked.push(String::new()); continue; }
            let start_col = c1;
            let mut end_col = (c2 + 1).min(line_text.len());
            while end_col < line_text.len() && !line_text.is_char_boundary(end_col) { end_col += 1; }
            let chunk = line_text[start_col..end_col].to_string();
            yanked.push(chunk.clone());
            if op != 'y' {
                let line_off = self.buf.line_byte_offset(line);
                self.buf.apply(line_off + start_col, line_off + end_col, "");
            }
        }
        yanked.reverse();
        let combined = yanked.join("\n");
        // Block-aware register: paste later overlays at column instead of
        // splicing newlines inline.
        match op {
            'y' => self.regs.yank(self.pending.register, combined, YankKind::Block),
            _   => self.regs.cut(self.pending.register, combined, YankKind::Block),
        }
        if op != 'y' { self.buf.end_compound(); }
        self.cur_line = l1;
        self.cur_col = c1;
        self.want_col = c1;
        if op == 'c' { self.enter_insert(); }
    }

    // ── Operators ──────────────────────────────────────────────────────
    /// Charwise operator on byte range [start, end). Stores the cut/copied
    /// text in the active register.
    fn execute_op_charwise(&mut self, op: char, start: usize, end: usize) {
        if start >= end { return; }
        let text: String = self.buf.rope.byte_slice(start..end).to_string();
        let reg_name = self.pending.register;
        match op {
            'd' => {
                self.regs.cut(reg_name, text, YankKind::Charwise);
                self.buf.apply(start, end, "");
                self.cursor_to_byte(start);
            }
            'c' => {
                self.regs.cut(reg_name, text, YankKind::Charwise);
                self.buf.apply(start, end, "");
                self.cursor_to_byte(start);
                self.enter_insert();
            }
            'y' => {
                self.regs.yank(reg_name, text, YankKind::Charwise);
                // Cursor doesn't move on yank.
            }
            _ => {}
        }
    }

    /// Linewise operator on lines [from..=to].
    fn execute_op_linewise(&mut self, from: usize, extra: usize) {
        let op = match self.pending.operator { Some(o) => o, None => return };
        let last = self.buf.line_count().saturating_sub(1);
        let to = (from + extra).min(last);
        let start = self.buf.line_byte_offset(from);
        let end = if to + 1 >= self.buf.line_count() {
            self.buf.rope.len_bytes()
        } else {
            self.buf.line_byte_offset(to + 1)
        };
        let mut text: String = self.buf.rope.byte_slice(start..end).to_string();
        // Linewise text always ends with a newline so paste works correctly.
        if !text.ends_with('\n') { text.push('\n'); }
        let reg_name = self.pending.register;
        match op {
            'd' => {
                self.regs.cut(reg_name, text, YankKind::Linewise);
                self.buf.apply(start, end, "");
                let new_line = from.min(self.buf.line_count().saturating_sub(1));
                self.cur_line = new_line;
                self.cur_col = 0;
                self.want_col = 0;
            }
            'c' => {
                self.regs.cut(reg_name, text, YankKind::Linewise);
                // Replace the lines with one empty line so we can insert into it.
                self.buf.apply(start, end, "\n");
                self.cur_line = from;
                self.cur_col = 0;
                self.want_col = 0;
                self.enter_insert();
            }
            'y' => {
                self.regs.yank(reg_name, text, YankKind::Linewise);
            }
            _ => {}
        }
    }

    /// Y — yank lines without an operator-doubling key.
    fn execute_op_linewise_yank(&mut self, from: usize, to: usize) {
        let last = self.buf.line_count().saturating_sub(1);
        let to = to.min(last);
        let start = self.buf.line_byte_offset(from);
        let end = if to + 1 >= self.buf.line_count() {
            self.buf.rope.len_bytes()
        } else {
            self.buf.line_byte_offset(to + 1)
        };
        let mut text: String = self.buf.rope.byte_slice(start..end).to_string();
        if !text.ends_with('\n') { text.push('\n'); }
        self.regs.yank(self.pending.register, text, YankKind::Linewise);
    }

    fn do_replace_char(&mut self, c: char) {
        let line = self.buf.line(self.cur_line);
        if self.cur_col >= line.len() { return; }
        let off = self.cursor_byte();
        let mut e = self.cur_col + 1;
        while e < line.len() && !line.is_char_boundary(e) { e += 1; }
        let abs_end = self.buf.line_byte_offset(self.cur_line) + e;
        self.buf.apply(off, abs_end, &c.to_string());
        self.last_change = Some(LastChange::Replace { c, count: 1 });
    }

    fn do_find_forward(&mut self, c: char, before: bool) {
        let cur = self.cursor_byte();
        if let Some(byte) = motion::find_forward(&self.buf, cur, c) {
            let target = if before {
                let s = self.buf.rope.to_string();
                let mut b = byte;
                if b > 0 { b -= 1; while b > 0 && !s.is_char_boundary(b) { b -= 1; } }
                b
            } else { byte };
            self.cursor_to_byte(target);
        }
    }

    fn do_find_backward(&mut self, c: char, after: bool) {
        let cur = self.cursor_byte();
        if let Some(byte) = motion::find_backward(&self.buf, cur, c) {
            let target = if after {
                let s = self.buf.rope.to_string();
                let mut b = byte + 1;
                while b < s.len() && !s.is_char_boundary(b) { b += 1; }
                b
            } else { byte };
            self.cursor_to_byte(target);
        }
    }

    fn do_paste(&mut self, after: bool, count: usize) {
        let reg = self.pending.register.unwrap_or('"');
        let yank = match self.regs.get(reg) {
            Some(y) => y.clone(),
            None    => { self.set_status(" register empty", 244); return; }
        };
        // Block paste: lay each line of the yank at the cursor column on
        // consecutive buffer lines, padding short lines with spaces. Append
        // a new buffer line if we run out.
        if yank.kind == YankKind::Block {
            self.buf.begin_compound();
            let lines: Vec<&str> = yank.text.split('\n').collect();
            let target_col = if after { self.cur_col + 1 } else { self.cur_col };
            for (i, chunk) in lines.iter().enumerate() {
                // Repeat per count.
                let mut chunk_n = String::new();
                for _ in 0..count.max(1) { chunk_n.push_str(chunk); }
                let bl = self.cur_line + i;
                if bl >= self.buf.line_count() {
                    let end = self.buf.rope.len_bytes();
                    let mut payload = String::new();
                    if !self.buf.rope.to_string().ends_with('\n') { payload.push('\n'); }
                    for _ in 0..target_col { payload.push(' '); }
                    payload.push_str(&chunk_n);
                    self.buf.apply(end, end, &payload);
                    continue;
                }
                let line_text = self.buf.line(bl);
                let line_off = self.buf.line_byte_offset(bl);
                if target_col > line_text.len() {
                    let pad = target_col - line_text.len();
                    let mut payload = " ".repeat(pad);
                    payload.push_str(&chunk_n);
                    let end_byte = line_off + line_text.len();
                    self.buf.apply(end_byte, end_byte, &payload);
                } else {
                    let insert_at = line_off + target_col;
                    self.buf.apply(insert_at, insert_at, &chunk_n);
                }
            }
            // Cursor lands at the start of the inserted block.
            self.cur_col = target_col;
            self.want_col = target_col;
            self.buf.end_compound();
            return;
        }
        let mut text = String::new();
        for _ in 0..count.max(1) { text.push_str(&yank.text); }
        match yank.kind {
            YankKind::Block => unreachable!(), // handled above
            YankKind::Charwise => {
                let cur = self.cursor_byte();
                let off = if after && self.cur_col < self.current_line_len() {
                    let line = self.buf.line(self.cur_line);
                    let mut e = self.cur_col + 1;
                    while e < line.len() && !line.is_char_boundary(e) { e += 1; }
                    self.buf.line_byte_offset(self.cur_line) + e
                } else {
                    cur
                };
                self.buf.apply(off, off, &text);
                let final_byte = off + text.len();
                let mut land = if final_byte > 0 {
                    let s = self.buf.rope.to_string();
                    let mut b = final_byte - 1;
                    while b > 0 && !s.is_char_boundary(b) { b -= 1; }
                    b
                } else { 0 };
                if land < off { land = off; }
                self.cursor_to_byte(land);
            }
            YankKind::Linewise => {
                let target_line = if after { self.cur_line + 1 } else { self.cur_line };
                let off = if target_line >= self.buf.line_count() {
                    // Paste below last line: append to end with newline.
                    let end = self.buf.rope.len_bytes();
                    let needs_nl = !self.buf.rope.to_string().ends_with('\n');
                    let mut payload = if needs_nl { "\n".to_string() } else { String::new() };
                    payload.push_str(&text);
                    self.buf.apply(end, end, &payload);
                    self.cur_line = self.buf.line_count().saturating_sub(1);
                    self.cur_col = 0;
                    self.want_col = 0;
                    return;
                } else {
                    self.buf.line_byte_offset(target_line)
                };
                self.buf.apply(off, off, &text);
                self.cur_line = target_line;
                self.cur_col = 0;
                self.want_col = 0;
            }
        }
    }

    // ── Search ─────────────────────────────────────────────────────────
    fn search_prompt(&mut self, dir: Direction) {
        let prompt = if dir == Direction::Forward { " /" } else { " ?" };
        let pattern = self.footer.ask(prompt, "");
        if pattern.is_empty() { return; }
        self.search.set(&pattern, dir);
        if let Some(byte) = self.search_next_at(self.cursor_byte(), dir) {
            self.cursor_to_byte(byte);
        } else {
            self.set_status(&format!(" pattern not found: {}", pattern), 196);
        }
    }

    fn search_next(&mut self, reverse: bool) -> Option<usize> {
        let dir = if reverse {
            match self.search.direction { Direction::Forward => Direction::Backward, Direction::Backward => Direction::Forward }
        } else {
            self.search.direction
        };
        // Start one char past current to avoid sticking.
        let s = self.buf.rope.to_string();
        let mut from = self.cursor_byte();
        if dir == Direction::Forward && from < s.len() {
            from += 1;
            while from < s.len() && !s.is_char_boundary(from) { from += 1; }
        }
        self.search_next_at(from, dir)
    }

    fn search_next_at(&self, from: usize, dir: Direction) -> Option<usize> {
        let s = self.buf.rope.to_string();
        self.search.find(&s, from, dir).map(|(start, _)| start)
    }

    fn search_word_under_cursor(&mut self, dir: Direction) {
        let cur = self.cursor_byte();
        let s = self.buf.rope.to_string();
        if cur >= s.len() { return; }
        let line = self.buf.rope.byte_to_line(cur);
        let line_start = self.buf.line_byte_offset(line);
        let line_end = motion::line_end(&self.buf, cur);
        let line_text = &s[line_start..line_end];
        let pos_in_line = cur - line_start;
        // Find word bounds.
        let bytes = line_text.as_bytes();
        let mut start = pos_in_line;
        while start > 0 {
            let prev_b = bytes[start - 1] as char;
            if !(prev_b.is_alphanumeric() || prev_b == '_') { break; }
            start -= 1;
        }
        let mut end = pos_in_line;
        while end < line_text.len() {
            let c = bytes[end] as char;
            if !(c.is_alphanumeric() || c == '_') { break; }
            end += 1;
        }
        if start == end { return; }
        let word = &line_text[start..end];
        let pattern = format!(r"\b{}\b", regex::escape(word));
        self.search.set(&pattern, dir);
        if let Some(byte) = self.search_next(false) { self.cursor_to_byte(byte); }
    }

    // ── Insert mode ────────────────────────────────────────────────────
    fn handle_insert(&mut self, key: &str) -> bool {
        match key {
            "ESC" | "C-[" | "C-C" => {
                self.mode = Mode::Normal;
                self.clamp_col_to_line();
                if self.capturing_insert {
                    let captured = std::mem::take(&mut self.captured_insert);
                    self.capturing_insert = false;
                    // If a `c`-op preceded this insert, splice the text into
                    // its captured LastChange so dot replays the full change.
                    if let Some(LastChange::Op { ref mut insert_text, .. }) = self.last_change {
                        *insert_text = captured.clone();
                    } else if !captured.is_empty() {
                        // Pure insert (i / a / o / O / I / A) — record on its own.
                        self.last_change = Some(LastChange::Insert {
                            text: captured,
                            append: false,
                        });
                    }
                }
            }
            // Arrow keys + HOME/END work in Insert mode too. LEFT/RIGHT wrap
            // across line boundaries; UP/DOWN preserve want_col.
            "LEFT"  => self.move_left_wrap(),
            "RIGHT" => self.move_right_wrap(),
            "UP"    => self.move_up(),
            "DOWN"  => self.move_down(),
            "HOME"  => { self.cur_col = 0; self.want_col = 0; }
            "END"   => { self.cur_col = self.col_cap(); self.want_col = self.cur_col; }
            "BACK" | "BACKSPACE" | "C-H" => {
                let off = self.cursor_byte();
                if off > 0 {
                    // Find prev char boundary in the rope's byte view.
                    let mut start = off - 1;
                    let s = self.buf.rope.to_string();
                    while start > 0 && !s.is_char_boundary(start) { start -= 1; }
                    self.buf.apply(start, off, "");
                    let (line, col) = self.buf.byte_to_line_col(start);
                    self.cur_line = line;
                    self.cur_col = col;
                    self.want_col = self.cur_col;
                }
            }
            // Forward-delete: remove char at cursor.
            "DEL" => {
                let off = self.cursor_byte();
                let total = self.buf.rope.len_bytes();
                if off < total {
                    let s = self.buf.rope.to_string();
                    let mut end = off + 1;
                    while end < s.len() && !s.is_char_boundary(end) { end += 1; }
                    self.buf.apply(off, end, "");
                }
            }
            "ENTER" | "\n" | "\r" | "C-M" | "C-J" => {
                let off = self.cursor_byte();
                self.buf.apply(off, off, "\n");
                self.cur_line += 1;
                self.cur_col = 0;
                self.want_col = 0;
                if self.capturing_insert { self.captured_insert.push('\n'); }
            }
            "TAB" | "\t" => {
                let off = self.cursor_byte();
                self.buf.apply(off, off, "\t");
                self.cur_col += 1;
                self.want_col = self.cur_col;
                if self.capturing_insert { self.captured_insert.push('\t'); }
            }
            other => {
                if other.chars().count() == 1 {
                    let c = other.chars().next().unwrap();
                    if !c.is_control() {
                        let off = self.cursor_byte();
                        self.buf.apply(off, off, other);
                        self.cur_col += other.len();
                        self.want_col = self.cur_col;
                        if self.capturing_insert { self.captured_insert.push_str(other); }
                    }
                }
            }
        }
        false
    }

    // ── Dot-repeat replay ──────────────────────────────────────────────
    fn repeat_last_change(&mut self) {
        let Some(change) = self.last_change.clone() else { return };
        match change {
            LastChange::Op { op, motion, count, register, insert_text } => {
                self.pending.operator = Some(op);
                self.pending.register = register;
                self.pending.count1 = Some(count);
                match motion {
                    ChangeMotion::Key(k) => {
                        if let Some(target_byte) = self.parse_motion(&k, count) {
                            let from = self.cursor_byte();
                            let (start, end) = if from <= target_byte { (from, target_byte) } else { (target_byte, from) };
                            self.execute_op_charwise(op, start, end);
                        }
                    }
                    ChangeMotion::TextObject { kind, target } => {
                        let cur = self.cursor_byte();
                        let r: Option<(usize, usize)> = match (kind, target) {
                            ('i', 'w') => textobj::inner_word(&self.buf, cur),
                            ('a', 'w') => textobj::around_word(&self.buf, cur),
                            ('i', '"') => textobj::inner_quote(&self.buf, cur, '"'),
                            ('a', '"') => textobj::around_quote(&self.buf, cur, '"'),
                            ('i', '\'') => textobj::inner_quote(&self.buf, cur, '\''),
                            ('a', '\'') => textobj::around_quote(&self.buf, cur, '\''),
                            ('i', '(') | ('i', ')') | ('i', 'b') => textobj::inner_pair(&self.buf, cur, '(', ')'),
                            ('a', '(') | ('a', ')') | ('a', 'b') => textobj::around_pair(&self.buf, cur, '(', ')'),
                            ('i', '[') | ('i', ']') => textobj::inner_pair(&self.buf, cur, '[', ']'),
                            ('a', '[') | ('a', ']') => textobj::around_pair(&self.buf, cur, '[', ']'),
                            ('i', '{') | ('i', '}') | ('i', 'B') => textobj::inner_pair(&self.buf, cur, '{', '}'),
                            ('a', '{') | ('a', '}') | ('a', 'B') => textobj::around_pair(&self.buf, cur, '{', '}'),
                            ('i', 'p') => textobj::inner_paragraph(&self.buf, cur),
                            ('a', 'p') => textobj::around_paragraph(&self.buf, cur),
                            _ => None,
                        };
                        if let Some((s, e)) = r { self.execute_op_charwise(op, s, e); }
                    }
                    ChangeMotion::Linewise { extra } => {
                        self.execute_op_linewise(self.cur_line, extra);
                    }
                }
                // Replay inserted text for `c` ops without entering Insert
                // interactively — splice it directly.
                if op == 'c' && !insert_text.is_empty() {
                    let off = self.cursor_byte();
                    self.buf.apply(off, off, &insert_text);
                    self.cursor_to_byte(off + insert_text.len());
                }
                if op == 'c' {
                    // Stay in Normal; we already applied the captured text.
                    self.mode = Mode::Normal;
                }
                self.pending.clear();
            }
            LastChange::Insert { text, .. } => {
                let off = self.cursor_byte();
                self.buf.apply(off, off, &text);
                self.cursor_to_byte(off + text.len());
            }
            LastChange::Replace { c, count } => {
                for _ in 0..count { self.do_replace_char(c); self.move_right_wrap(); }
            }
            LastChange::Paste { after, count, register } => {
                let saved = self.pending.register;
                self.pending.register = register;
                self.do_paste(after, count);
                self.pending.register = saved;
            }
            LastChange::SimpleAction { .. } => {}
        }
    }

    // ── Command mode ───────────────────────────────────────────────────
    fn handle_command(&mut self, key: &str) -> bool {
        match key {
            "ESC" | "C-[" | "C-C" => { self.cmdline.clear(); self.mode = Mode::Normal; false }
            "BACK" | "BACKSPACE" | "C-H" => {
                if self.cmdline.is_empty() { self.mode = Mode::Normal; }
                else { self.cmdline.pop(); }
                false
            }
            "ENTER" | "\n" | "\r" | "C-M" | "C-J" => {
                let cmd = self.cmdline.trim().to_string();
                self.cmdline.clear();
                self.mode = Mode::Normal;
                self.execute_command(&cmd)
            }
            other => {
                if other.chars().count() == 1 {
                    let c = other.chars().next().unwrap();
                    if !c.is_control() { self.cmdline.push(c); }
                }
                false
            }
        }
    }

    /// Returns true to quit the editor.
    fn execute_command(&mut self, cmd: &str) -> bool {
        match cmd {
            "w" => {
                match self.buf.save() {
                    Ok(_)  => self.set_status(" written", 46),
                    Err(e) => self.set_status(&format!(" save failed: {}", e), 196),
                }
                false
            }
            "q"  => { if self.buf.dirty { self.set_status(" unsaved changes (use :q! to force)", 196); false } else { true } }
            "q!" => true,
            "wq" | "x" => {
                let _ = self.buf.save();
                true
            }
            "" => false,
            other if other.starts_with("e ") => {
                let path = other[2..].trim();
                if !path.is_empty() {
                    if let Ok(b) = Buffer::from_path(PathBuf::from(path)) {
                        self.buf = b;
                        self.cur_line = 0;
                        self.cur_col = 0;
                        self.scroll = 0;
                    } else {
                        self.set_status(" open failed", 196);
                    }
                }
                false
            }
            other => {
                self.set_status(&format!(" unknown: {}", other), 196);
                false
            }
        }
    }
}
