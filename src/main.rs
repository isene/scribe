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

use buffer::Buffer;
use crust::{Crust, Input, Pane};
use crust::style;
use mode::Mode;
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
            Mode::Normal  => app.handle_normal(&key),
            Mode::Insert  => app.handle_insert(&key),
            Mode::Command => app.handle_command(&key),
        };
        if quit { break; }
        app.render_all();
    }

    Crust::cleanup();
    Crust::clear_screen();
}

struct App {
    buf: Buffer,
    mode: Mode,
    /// Cursor as (line, col) — col is byte offset within the line, NOT char.
    /// Rendering converts to display column.
    cur_line: usize,
    cur_col: usize,
    /// Sticky col for vertical motions: when moving j/k across short lines,
    /// remember the column we *want* and snap back when lines are wide enough.
    want_col: usize,
    /// Top-of-pane line index (vertical scroll).
    scroll: usize,
    /// `:` command buffer.
    cmdline: String,
    /// Status line message (transient; cleared on next key).
    status: Option<(String, u8)>,
    cols: u16,
    rows: u16,
    header: Pane,
    main_p: Pane,
    footer: Pane,
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
        }
    }

    // ── Rendering ──────────────────────────────────────────────────────
    fn render_all(&mut self) {
        self.render_header();
        self.render_main();
        self.render_footer();
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
        // Keep cursor in view.
        if self.cur_line < self.scroll { self.scroll = self.cur_line; }
        if self.cur_line >= self.scroll + pane_h { self.scroll = self.cur_line + 1 - pane_h; }

        let mut out = String::new();
        for i in 0..pane_h {
            let line_idx = self.scroll + i;
            if line_idx < self.buf.line_count() {
                let line = self.buf.line(line_idx);
                if line_idx == self.cur_line {
                    // Highlight cursor cell. Compute byte → char cursor for
                    // styling. Display cursor as inverse on the byte at
                    // cur_col, or as a trailing block at line end.
                    let col = self.cur_col.min(line.len());
                    let (before, rest) = line.split_at(col);
                    let (cell, after) = if rest.is_empty() {
                        ("", "")
                    } else {
                        // Find next char boundary.
                        let mut e = 1;
                        while e < rest.len() && !rest.is_char_boundary(e) { e += 1; }
                        (&rest[..e], &rest[e..])
                    };
                    let cur_glyph = if cell.is_empty() { " " } else { cell };
                    out.push_str(before);
                    out.push_str(&style::bg(cur_glyph, 240));
                    out.push_str(after);
                } else {
                    out.push_str(&line);
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
        let mode_label = style::bg(&style::fg(self.mode.label(), 0), self.mode.color());
        let pos = format!(" {}:{} ", self.cur_line + 1, self.cur_col + 1);
        let right = format!("scribe v{} ", VERSION);

        let middle: String = if self.mode == Mode::Command {
            format!(" :{}", self.cmdline)
        } else if let Some((ref msg, c)) = self.status {
            style::fg(msg, c)
        } else {
            String::new()
        };

        // Build a status line that EXACTLY fills `cols` display columns, so
        // the bg of the footer pane covers the full bottom row even when
        // text content is short.
        let cols = self.cols as usize;
        let left_w = crust::display_width(&mode_label) + crust::display_width(&middle);
        let right_w = crust::display_width(&pos) + crust::display_width(&right);
        let line = if left_w + right_w <= cols {
            let gap = cols - left_w - right_w;
            format!("{}{}{}{}{}", mode_label, middle, " ".repeat(gap), pos, right)
        } else {
            // Tight terminal: drop right-side scribe-version, keep mode + msg
            // and pad to full width.
            let visible = format!("{}{}", mode_label, middle);
            let visible_w = crust::display_width(&visible);
            let pad = cols.saturating_sub(visible_w);
            format!("{}{}", visible, " ".repeat(pad))
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

    // ── Normal mode ────────────────────────────────────────────────────
    fn handle_normal(&mut self, key: &str) -> bool {
        self.status = None;
        match key {
            // Motion — h/l and arrows wrap across line boundaries (vim's
            // whichwrap=h,l,<,>,[,] convention).
            "h" | "LEFT"  => self.move_left_wrap(),
            "l" | "RIGHT" => self.move_right_wrap(),
            "j" | "DOWN"  => self.move_down(),
            "k" | "UP"    => self.move_up(),
            "0" | "HOME" => { self.cur_col = 0; self.want_col = 0; }
            "$" | "END"  => {
                self.cur_col = self.col_cap();
                self.want_col = self.cur_col;
            }
            "g" => {
                if let Some(k2) = Input::getchr(Some(20)) {
                    if k2 == "g" { self.cur_line = 0; self.cur_col = 0; self.want_col = 0; }
                }
            }
            "G" => {
                self.cur_line = self.buf.line_count().saturating_sub(1);
                self.cur_col = 0;
                self.want_col = 0;
            }
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

            // Delete a single character at cursor
            "x" => {
                let off = self.cursor_byte();
                let line = self.buf.line(self.cur_line);
                if self.cur_col < line.len() {
                    let mut e = self.cur_col + 1;
                    while e < line.len() && !line.is_char_boundary(e) { e += 1; }
                    let abs_end = self.buf.line_byte_offset(self.cur_line) + e;
                    self.buf.apply(off, abs_end, "");
                    self.clamp_col_to_line();
                }
            }

            // Undo / redo — cursor follows the edit site.
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

            _ => {}
        }
        false
    }

    fn enter_insert(&mut self) {
        self.mode = Mode::Insert;
    }

    // ── Insert mode ────────────────────────────────────────────────────
    fn handle_insert(&mut self, key: &str) -> bool {
        match key {
            "ESC" | "C-[" | "C-C" => {
                self.mode = Mode::Normal;
                self.clamp_col_to_line();
            }
            // Arrow keys + HOME/END work in Insert mode too. LEFT/RIGHT wrap
            // across line boundaries; UP/DOWN preserve want_col.
            "LEFT"  => self.move_left_wrap(),
            "RIGHT" => self.move_right_wrap(),
            "UP"    => self.move_up(),
            "DOWN"  => self.move_down(),
            "HOME"  => { self.cur_col = 0; self.want_col = 0; }
            "END"   => { self.cur_col = self.col_cap(); self.want_col = self.cur_col; }
            "BACKSPACE" | "C-H" => {
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
            "ENTER" | "\n" | "\r" | "C-M" | "C-J" => {
                let off = self.cursor_byte();
                self.buf.apply(off, off, "\n");
                self.cur_line += 1;
                self.cur_col = 0;
                self.want_col = 0;
            }
            "TAB" | "\t" => {
                let off = self.cursor_byte();
                self.buf.apply(off, off, "\t");
                self.cur_col += 1;
                self.want_col = self.cur_col;
            }
            other => {
                // Ordinary printable: getchr returns the literal string.
                if other.chars().count() == 1 {
                    let c = other.chars().next().unwrap();
                    if !c.is_control() {
                        let off = self.cursor_byte();
                        self.buf.apply(off, off, other);
                        self.cur_col += other.len();
                        self.want_col = self.cur_col;
                    }
                }
            }
        }
        false
    }

    // ── Command mode ───────────────────────────────────────────────────
    fn handle_command(&mut self, key: &str) -> bool {
        match key {
            "ESC" | "C-[" | "C-C" => { self.cmdline.clear(); self.mode = Mode::Normal; false }
            "BACKSPACE" | "C-H" => {
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
