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
mod spell;
mod textobj;

use buffer::{Buffer, FileKind};
use crust::{Crust, Input, Pane, Popup};
use crust::style;
use mode::Mode;
use register::{Registers, Yank, YankKind};
use search::{Direction, SearchState};
use std::path::PathBuf;

const VERSION: &str = env!("CARGO_PKG_VERSION");

fn main() {
    // CLI: scribe [+N] [--theme NAME] [path]
    // `+N` opens the file with the cursor on line N (vim convention; used
    // by kastrup's compose flow to jump straight to the message body).
    // `--theme NAME` overrides the rcfile theme for this session only.
    let args: Vec<String> = std::env::args().collect();
    let mut start_line: Option<usize> = None;
    let mut path: Option<PathBuf> = None;
    let mut cli_theme: Option<String> = None;
    let mut i = 1;
    while i < args.len() {
        let arg = &args[i];
        if let Some(rest) = arg.strip_prefix('+') {
            if let Ok(n) = rest.parse::<usize>() { start_line = Some(n); }
            i += 1;
        } else if arg == "--theme" && i + 1 < args.len() {
            cli_theme = Some(args[i + 1].clone());
            i += 2;
        } else if let Some(rest) = arg.strip_prefix("--theme=") {
            cli_theme = Some(rest.to_string());
            i += 1;
        } else if path.is_none() {
            path = Some(PathBuf::from(arg));
            i += 1;
        } else {
            i += 1;
        }
    }
    install_panic_hook();
    Crust::init();
    Crust::set_app_identity("Scribe");
    // Bracketed paste: terminal wraps the pasted blob in `CSI 200~ ... 201~`
    // and crossterm parses that into a single Event::Paste. Without it each
    // pasted byte travels through the keystroke pipeline (buf.apply + new
    // undo node + render_all per char), so a 1KB paste = 1000 undo nodes
    // and 1000 renders.
    use std::io::Write;
    print!("\x1b[?2004h");
    let _ = std::io::stdout().flush();

    let mut app = App::new(path, cli_theme);
    if let Some(n) = start_line {
        if n > 0 {
            let last = app.buf.line_count().saturating_sub(1);
            app.cur_line = (n - 1).min(last);
            app.cur_col = 0;
            app.want_col = 0;
        }
    }
    app.render_all();

    loop {
        let Some(key) = Input::getchr(None) else { continue };
        if key == "RESIZE" {
            app.handle_resize();
            app.render_all();
            continue;
        }
        // Bracketed paste: insert the whole payload as one compound undo node
        // regardless of mode. Don't let it run through the per-char Insert
        // handler (would make 1 undo node per char + 1 render per char).
        if let Some(text) = key.strip_prefix("PASTE\x00") {
            app.handle_paste(text);
            app.render_all();
            continue;
        }
        // Macro capture: append the key only if recording was already
        // active BEFORE dispatch. Suppresses (a) the `M` keystroke that
        // starts/stops recording and (b) the register-name keystroke that
        // follows it. Replayed keys (replay_depth>0) are also skipped.
        let was_recording = app.recording;
        let quit = match app.mode {
            Mode::Normal      => app.handle_normal(&key),
            Mode::Insert      => app.handle_insert(&key),
            Mode::Command     => app.handle_command(&key),
            Mode::Visual      |
            Mode::VisualLine  |
            Mode::VisualBlock => app.handle_visual(&key),
        };
        if app.replay_depth == 0 && was_recording.is_some() && was_recording == app.recording {
            app.recording_buf.push_str(&key_to_macro_text(&key));
        }
        if quit { break; }
        app.render_all();
    }

    print!("\x1b[?2004l");
    let _ = std::io::stdout().flush();
    // Persist `:` command history across runs. The footer pane was the
    // editline target; its history is the live list.
    save_cmd_history(&app.footer.history);
    app.save_session();
    Crust::cleanup();
    Crust::clear_screen();
}

/// Encode an input-layer key string ("h", "ESC", "C-UP", "ENTER", …) as
/// vim-style macro text (`h`, `<Esc>`, `<C-Up>`, `<CR>`, …). The result
/// is written into a register so the user can paste, edit, and re-yank
/// macros as ordinary text.
fn key_to_macro_text(key: &str) -> String {
    match key {
        "ESC"        => "<Esc>".into(),
        "ENTER"      => "<CR>".into(),
        "BACKSPACE"  => "<BS>".into(),
        "TAB"        => "<Tab>".into(),
        "DEL" | "C-DEL" => if key == "C-DEL" { "<C-Del>".into() } else { "<Del>".into() },
        "INS" | "C-INS" => if key == "C-INS" { "<C-Ins>".into() } else { "<Ins>".into() },
        "UP"         => "<Up>".into(),
        "DOWN"       => "<Down>".into(),
        "LEFT"       => "<Left>".into(),
        "RIGHT"      => "<Right>".into(),
        "HOME"       => "<Home>".into(),
        "END"        => "<End>".into(),
        "PgUP"       => "<PageUp>".into(),
        "PgDOWN"     => "<PageDown>".into(),
        "C-UP"       => "<C-Up>".into(),
        "C-DOWN"     => "<C-Down>".into(),
        "C-LEFT"     => "<C-Left>".into(),
        "C-RIGHT"    => "<C-Right>".into(),
        "S-UP"       => "<S-Up>".into(),
        "S-DOWN"     => "<S-Down>".into(),
        "S-LEFT"     => "<S-Left>".into(),
        "S-RIGHT"    => "<S-Right>".into(),
        "C-HOME"     => "<C-Home>".into(),
        "C-END"      => "<C-End>".into(),
        "C-PgUP"     => "<C-PageUp>".into(),
        "C-PgDOWN"   => "<C-PageDown>".into(),
        "C-SPACE"    => "<C-Space>".into(),
        s if s.starts_with("C-") && s.len() > 2 => format!("<{}>", s),
        s if s.starts_with('F') && s.len() > 1 && s[1..].chars().all(|c| c.is_ascii_digit()) => {
            format!("<{}>", s)
        }
        s => {
            let mut chars = s.chars();
            let first = chars.next();
            match first {
                Some('<') if chars.next().is_none() => "<lt>".into(),
                Some(c) if chars.next().is_none() => c.to_string(),
                _ => format!("<{}>", s),
            }
        }
    }
}

/// Inverse of `key_to_macro_text`. Walks the macro text and produces the
/// stream of input-layer key strings to feed back through the mode
/// handlers. Tokens look like `<Esc>`, `<C-Up>`, `<CR>`. Anything outside
/// `<...>` is a single literal char.
fn parse_macro_text(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'<' {
            if let Some(rel) = bytes[i+1..].iter().position(|&b| b == b'>') {
                let inner = &text[i+1 .. i+1+rel];
                let key = match inner {
                    "Esc"      => "ESC".to_string(),
                    "CR" | "Enter" | "Return" => "ENTER".to_string(),
                    "BS"       => "BACKSPACE".to_string(),
                    "Tab"      => "TAB".to_string(),
                    "Del"      => "DEL".to_string(),
                    "Ins"      => "INS".to_string(),
                    "Up"       => "UP".to_string(),
                    "Down"     => "DOWN".to_string(),
                    "Left"     => "LEFT".to_string(),
                    "Right"    => "RIGHT".to_string(),
                    "Home"     => "HOME".to_string(),
                    "End"      => "END".to_string(),
                    "PageUp"   => "PgUP".to_string(),
                    "PageDown" => "PgDOWN".to_string(),
                    "C-Up"     => "C-UP".to_string(),
                    "C-Down"   => "C-DOWN".to_string(),
                    "C-Left"   => "C-LEFT".to_string(),
                    "C-Right"  => "C-RIGHT".to_string(),
                    "S-Up"     => "S-UP".to_string(),
                    "S-Down"   => "S-DOWN".to_string(),
                    "S-Left"   => "S-LEFT".to_string(),
                    "S-Right"  => "S-RIGHT".to_string(),
                    "C-Home"   => "C-HOME".to_string(),
                    "C-End"    => "C-END".to_string(),
                    "C-PageUp"   => "C-PgUP".to_string(),
                    "C-PageDown" => "C-PgDOWN".to_string(),
                    "C-Del"    => "C-DEL".to_string(),
                    "C-Ins"    => "C-INS".to_string(),
                    "C-Space"  => "C-SPACE".to_string(),
                    "lt"       => "<".to_string(),
                    s          => s.to_string(),
                };
                out.push(key);
                i += 2 + rel;
                continue;
            }
        }
        let c = text[i..].chars().next().unwrap();
        out.push(c.to_string());
        i += c.len_utf8();
    }
    out
}

/// If `line[start..]` begins with a valid `YYYY-MM-DD`, parse it,
/// add `delta` days, and return the new formatted date. Otherwise
/// returns None — the caller falls back to plain integer increment.
/// Uses Julian-day arithmetic so month-end and leap-year rollover
/// (incl. the 100/400 Gregorian rules) are exact.
fn iso_date_match(line: &str, start: usize, delta: i64) -> Option<String> {
    let bytes = line.as_bytes();
    if start + 10 > bytes.len() { return None; }
    let chunk = &bytes[start..start + 10];
    // Shape check: NNNN-NN-NN.
    if chunk[4] != b'-' || chunk[7] != b'-' { return None; }
    for &i in &[0, 1, 2, 3, 5, 6, 8, 9] {
        if !chunk[i].is_ascii_digit() { return None; }
    }
    // Reject if the next char is a digit — that would mean the "year"
    // is actually a longer number that happens to share the layout.
    if let Some(&b) = bytes.get(start + 10) {
        if b.is_ascii_digit() { return None; }
    }
    let y: i64 = std::str::from_utf8(&chunk[0..4]).ok()?.parse().ok()?;
    let m: i64 = std::str::from_utf8(&chunk[5..7]).ok()?.parse().ok()?;
    let d: i64 = std::str::from_utf8(&chunk[8..10]).ok()?.parse().ok()?;
    if !(1..=12).contains(&m) || !(1..=days_in_month(y, m as u32)).contains(&(d as i64)) {
        return None;
    }
    let jdn = ymd_to_jdn(y, m as i32, d as i32);
    let (y2, m2, d2) = jdn_to_ymd(jdn + delta);
    Some(format!("{:04}-{:02}-{:02}", y2, m2, d2))
}

/// Days in month for the proleptic Gregorian calendar. Months 1..=12.
fn days_in_month(year: i64, month: u32) -> i64 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11               => 30,
        2 => if is_leap(year) { 29 } else { 28 },
        _ => 0,
    }
}

fn is_leap(y: i64) -> bool {
    (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
}

/// Convert (year, month, day) → Julian Day Number. Standard formula
/// (Fliegel & Van Flandern); valid for all positive years.
fn ymd_to_jdn(y: i64, m: i32, d: i32) -> i64 {
    let a = (14 - m as i64) / 12;
    let y = y + 4800 - a;
    let m = m as i64 + 12 * a - 3;
    d as i64 + (153 * m + 2) / 5 + 365 * y + y / 4 - y / 100 + y / 400 - 32045
}

/// Inverse of `ymd_to_jdn`. Returns (year, month, day).
fn jdn_to_ymd(jdn: i64) -> (i64, i32, i32) {
    let a = jdn + 32044;
    let b = (4 * a + 3) / 146097;
    let c = a - (146097 * b) / 4;
    let d = (4 * c + 3) / 1461;
    let e = c - (1461 * d) / 4;
    let m_ = (5 * e + 2) / 153;
    let day = (e - (153 * m_ + 2) / 5 + 1) as i32;
    let month = (m_ + 3 - 12 * (m_ / 10)) as i32;
    let year = 100 * b + d - 4800 + m_ / 10;
    (year, month, day)
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
    /// `z` prefix: next key is a spell action (`z=`, `zg`).
    z_prefix: bool,
    /// `]` or `[` prefix: next key is a bracket motion (`]s`, `[s`, …).
    bracket_prefix: Option<char>,
    /// Spellcheck — None until first enabled (or auto-on for email). When
    /// hunspell is missing, stays None and we set a status message.
    spell: Option<spell::Spell>,
    spell_enabled: bool,
    /// Hunspell dict tag for the spawned subprocess. Switching it via
    /// `:set spelllang=NAME` (or `:set lang=NAME`) drops the current
    /// hunspell process and re-spawns with the new dict.
    spell_lang: String,
    /// Sorted by start byte, recomputed on Insert→Normal and on `:set spell`.
    misspellings: Vec<spell::MisspellRange>,
    /// `:set number` / `:set nonumber` — gutter with line numbers on the
    /// left of the main pane.
    show_numbers: bool,
    /// `:set relativenumber` / `:set rnu` — gutter shows distance from the
    /// cursor instead of absolute line numbers (cursor line stays absolute).
    relative_numbers: bool,
    /// Active highlight theme name (`monokai` / `solarized` / `nord` /
    /// `dracula` / `gruvbox` / `plain`). Mirrored via `highlight::set_theme`
    /// so the source-mode renderer picks it up; we keep a copy for
    /// status display and for saving back to scriberc.
    theme_name: String,
    /// Macro state. `M{reg}` starts capture into `recording = Some(reg)`,
    /// pressing `M` again stops and commits to register `reg` as charwise
    /// yank text. `@{reg}` replays from the register. `@@` replays
    /// last_macro. `m` is intentionally left free for marks (future).
    /// Macros live in the same register file as yanks — `"ap` pastes the
    /// captured key sequence as editable text, and you can yank edited
    /// text back into a register and replay it.
    recording: Option<char>,
    last_macro: Option<char>,
    /// In-memory text being assembled while recording. Committed to the
    /// register on stop; replay reads from the register so user edits
    /// (yank back over the slot) take effect on the next replay.
    recording_buf: String,
    /// Set after `M` while waiting for the register name.
    macro_prefix: bool,
    /// Set after `@` while waiting for the register name.
    at_prefix: bool,
    /// Bound to keep `@a` containing `@a` from infinite-recursing.
    replay_depth: usize,
    /// In-buffer marks set by `m{a-z}` and jumped to via `'{a-z}` (line
    /// start) or `` `{a-z} `` (exact col). Stored as absolute byte
    /// offsets — a stable position even when the line/col mapping
    /// shifts under edits. Session-local for now; persisting them
    /// would need invalidation on reload.
    marks: std::collections::HashMap<char, usize>,
    /// Set after `m` in Normal mode — next ascii char names the mark.
    mark_set_prefix: bool,
    /// Set after `'` or `` ` `` in Normal mode — next char names the
    /// mark to jump to. `mark_jump_exact` distinguishes the two: `'`
    /// lands on first non-blank of the mark's line, `` ` `` lands at
    /// the exact byte offset.
    mark_jump_prefix: bool,
    mark_jump_exact: bool,
    /// `:read` distraction-free mode. Hides line numbers, dims the
    /// header / footer to the bare minimum (mode + position only),
    /// and centers the text by padding the gutter. Toggled via
    /// `:read` / `:noread`. Doesn't change buffer state — pure
    /// presentation.
    reading_mode: bool,
    /// `:set readingwidth=N` — column width of the centered text in
    /// reading mode. 0 = full pane width (current behaviour). Common
    /// values: 72, 80, 100. Approximates the Goyo vim plugin.
    reading_width: usize,
    /// `:set paragraphdim` — Limelight-style: when in reading mode,
    /// every paragraph except the one containing the cursor is dimmed
    /// to a subtle grey. Toggle independently from reading_mode but
    /// only takes visual effect while reading_mode is on.
    paragraph_dim: bool,
    /// `:set textwidth=N` (alias `:set tw=N`). Auto-wraps the line
    /// during typing when it exceeds `textwidth` characters by
    /// breaking at the last whitespace before the limit. 0 = off.
    textwidth: usize,
    /// User keymaps loaded from scriberc's `[keymap]` section.
    keymaps: Vec<KeyMap>,
    /// First key of a multi-key map LHS that just fired — waiting on
    /// the second key to either match or fall through.
    map_pending: Option<String>,
    /// Recursion guard: keymap RHS is fed back through the handler,
    /// which may itself trigger maps. Cap at 4.
    map_depth: usize,
}

impl App {
    fn new(path: Option<PathBuf>, cli_theme: Option<String>) -> Self {
        let (cols, rows) = Crust::terminal_size();
        let mut header = Pane::new(1, 1, cols, 1, 255, 236);
        header.wrap = false; header.scroll = false;
        let mut main_p = Pane::new(1, 2, cols, rows.saturating_sub(2), 231, 0);
        main_p.wrap = true;
        let mut footer = Pane::new(1, rows, cols, 1, 255, 236);
        footer.wrap = false; footer.scroll = false;

        let buf = match path {
            Some(p) => Buffer::from_path(p).unwrap_or_else(|_| Buffer::empty()),
            None    => Buffer::empty(),
        };

        // rcfile: ~/.config/scribe/scriberc — simple `key = value` lines.
        // Loaded once per launch; `:set` commands stay in-session until
        // the user manually edits the rcfile. CLI `--theme NAME` overrides
        // the rcfile's theme for this session.
        let rc = load_scriberc();
        let active_theme = cli_theme.or_else(|| rc.theme.clone());
        if let Some(ref t) = active_theme { highlight::set_theme(t); }
        if let Some(c) = rc.spell_color { highlight::set_miss_color(c); }

        // Footer hosts the `:` command prompt — enable editline history so
        // Up / Down recalls past commands (per-session). Persisted history
        // is loaded from ~/.config/scribe/cmdhistory below.
        footer.record = true;
        footer.history = load_cmd_history();

        let auto_spell = matches!(buf.kind, FileKind::Email) || rc.spell;
        let mut app = Self {
            buf, mode: Mode::Normal,
            cur_line: 0, cur_col: 0, want_col: 0, scroll: 0,
            cmdline: String::new(), status: None,
            cols, rows, header, main_p, footer,
            pending: Pending::default(),
            regs: Registers::load(),
            search: SearchState::new(),
            visual_anchor: 0,
            visual_anchor_line: 0,
            visual_anchor_col: 0,
            last_change: None,
            capturing_insert: false,
            captured_insert: String::new(),
            z_prefix: false,
            bracket_prefix: None,
            spell: None,
            spell_enabled: false,
            spell_lang: rc.spell_lang.clone().unwrap_or_else(|| "en_US".into()),
            misspellings: Vec::new(),
            show_numbers: rc.number,
            relative_numbers: rc.relative_numbers,
            theme_name: active_theme.unwrap_or_else(|| "monokai".to_string()),
            recording: None,
            last_macro: None,
            recording_buf: String::new(),
            macro_prefix: false,
            at_prefix: false,
            replay_depth: 0,
            marks: std::collections::HashMap::new(),
            mark_set_prefix: false,
            mark_jump_prefix: false,
            mark_jump_exact: false,
            reading_mode: rc.reading_mode,
            reading_width: rc.reading_width,
            paragraph_dim: rc.paragraph_dim,
            textwidth: 0,
            keymaps: rc.keymaps,
            map_pending: None,
            map_depth: 0,
        };
        if auto_spell { app.spell_enable(); }
        if app.reading_mode { app.apply_layout(); }
        // Restore cursor + scroll from the last session for this path.
        // CLI `+N` (handled in main()) overrides this — checked there.
        app.restore_session();
        app
    }

    /// Look up the file's last cursor + scroll position from
    /// `~/.config/scribe/sessions.json` and apply it. Silently no-ops
    /// when the file has no path (`Buffer::empty()`), the session file
    /// doesn't exist, or the saved position is past EOF (file was
    /// edited externally).
    fn restore_session(&mut self) {
        let Some(path) = self.buf.path.clone() else { return };
        let key = path.to_string_lossy().to_string();
        let sessions = load_sessions();
        let Some(s) = sessions.get(&key) else { return };
        let total = self.buf.line_count();
        if total == 0 { return; }
        self.cur_line = s.line.min(total - 1);
        let line_len = self.buf.line(self.cur_line).len();
        self.cur_col = s.col.min(line_len);
        self.scroll = s.scroll.min(total.saturating_sub(1));
        self.snap_col_to_boundary();
        self.want_col = self.cur_col;
    }

    /// Persist the current cursor + scroll back to sessions.json. Safe
    /// to call on quit even if the file has no path — drops silently.
    fn save_session(&self) {
        let Some(path) = self.buf.path.as_ref() else { return };
        let key = path.to_string_lossy().to_string();
        let mut sessions = load_sessions();
        sessions.insert(key, SessionEntry {
            line: self.cur_line,
            col: self.cur_col,
            scroll: self.scroll,
        });
        // Cap at 200 entries — discard the oldest if we go over. We
        // don't track recency, so just trim arbitrarily; sessions.json
        // staying small matters more than perfect LRU.
        if sessions.len() > 200 {
            let drop_count = sessions.len() - 200;
            let drop_keys: Vec<String> = sessions.keys().take(drop_count).cloned().collect();
            for k in drop_keys { sessions.remove(&k); }
        }
        save_sessions(&sessions);
    }

    // ── Spellcheck ─────────────────────────────────────────────────────
    /// Spawn hunspell, load personal dict, mark enabled, run a first scan.
    fn spell_enable(&mut self) {
        if self.spell.is_none() {
            match spell::Spell::spawn(&self.spell_lang) {
                Some(mut sp) => {
                    sp.load_personal(load_personal_dict());
                    self.spell = Some(sp);
                }
                None => {
                    self.set_status(
                        &format!(" spell: hunspell + dict '{}' not available", self.spell_lang),
                        196);
                    return;
                }
            }
        }
        self.spell_enabled = true;
        self.recheck_spell();
    }

    /// Drop any existing hunspell process and re-spawn with `lang`. Called
    /// from `:set spelllang=NAME`. If spawn fails the previous state is
    /// kept (with a status message); spell stays enabled iff it was.
    fn spell_set_lang(&mut self, lang: &str) {
        let was_enabled = self.spell_enabled;
        self.spell = None;
        self.spell_lang = lang.to_string();
        if was_enabled || matches!(self.buf.kind, FileKind::Email) {
            self.spell_enable();
            if self.spell_enabled {
                self.set_status(
                    &format!(" spell: lang={} ({} words)", self.spell_lang, self.misspellings.len()),
                    244);
            }
        } else {
            self.set_status(&format!(" spell lang: {} (use :set spell to enable)", self.spell_lang), 244);
        }
    }

    fn spell_disable(&mut self) {
        self.spell_enabled = false;
        self.misspellings.clear();
    }

    /// Re-scan the buffer. Skips email headers and quoted-reply lines so we
    /// don't flag the user on text they didn't write. Cheap enough to run on
    /// every Insert→Normal transition for typical mail bodies; for large
    /// buffers this should be debounced (out of scope for v1).
    fn recheck_spell(&mut self) {
        self.misspellings.clear();
        if !self.spell_enabled { return; }
        let Some(sp) = self.spell.as_mut() else { return };
        // For email, locate the header/body boundary (first blank line) the
        // same way render_main does, so we can skip header lines.
        let header_end: Option<usize> = if matches!(self.buf.kind, FileKind::Email) {
            (0..self.buf.line_count()).find(|i| self.buf.line(*i).is_empty())
        } else { None };

        let total = self.buf.line_count();
        let mut to_check: Vec<(String, usize)> = Vec::with_capacity(total);
        for i in 0..total {
            // Skip header block.
            if let Some(end) = header_end {
                if i < end { continue; }
            }
            let line = self.buf.line(i);
            // Skip quoted-reply lines (any leading `>`) — that's not user text.
            if line.trim_start().starts_with('>') { continue; }
            // Skip the "On <date>... wrote:" attribution line that mutt and
            // most clients prepend before quoted text.
            let trimmed = line.trim();
            if trimmed.starts_with("On ") && trimmed.ends_with("wrote:") { continue; }
            let base = self.buf.line_byte_offset(i);
            to_check.push((line, base));
        }
        let mut found = sp.check_lines(&to_check);
        found.sort_by_key(|m| m.start);
        self.misspellings = found;
    }

    /// Append `word` to the personal dict file and add to the in-memory set.
    /// File: `~/.config/scribe/spell.add` (one word per line).
    fn spell_add_word(&mut self, word: &str) {
        let trimmed = word.trim();
        if trimmed.is_empty() { return; }
        if let Some(sp) = self.spell.as_mut() {
            sp.add_personal(trimmed);
        }
        let path = personal_dict_path();
        if let Some(dir) = path.parent() { let _ = std::fs::create_dir_all(dir); }
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
            use std::io::Write as _;
            let _ = writeln!(f, "{}", trimmed);
        }
        self.recheck_spell();
        self.set_status(&format!(" added '{}' to personal dict", trimmed), 244);
    }

    /// Find the misspelling the cursor is on (if any). Used by `z=` and `zg`.
    fn misspelling_at_cursor(&self) -> Option<spell::MisspellRange> {
        let cur = self.cursor_byte();
        self.misspellings.iter()
            .find(|m| cur >= m.start && cur <= m.end)
            .cloned()
    }

    /// Word at cursor — used by `zg` (which works whether or not the word is
    /// flagged as misspelled). Falls back to nothing if cursor isn't on a
    /// wordlike char.
    fn word_at_cursor(&self) -> Option<String> {
        // Prefer the misspelling range if cursor is on one (it's already the
        // exact word hunspell flagged).
        if let Some(m) = self.misspelling_at_cursor() {
            return Some(m.word);
        }
        let line = self.buf.line(self.cur_line);
        if line.is_empty() { return None; }
        let bytes = line.as_bytes();
        let cur = self.cur_col.min(bytes.len());
        let mut s = cur;
        while s > 0 {
            let prev = bytes[s - 1];
            if !is_wordchar(prev) { break; }
            s -= 1;
        }
        let mut e = cur;
        while e < bytes.len() && is_wordchar(bytes[e]) { e += 1; }
        if s >= e { return None; }
        // Snap to char boundaries (multi-byte chars).
        while s > 0 && !line.is_char_boundary(s) { s -= 1; }
        while e < bytes.len() && !line.is_char_boundary(e) { e += 1; }
        Some(line[s..e].to_string())
    }

    fn jump_next_misspelling(&mut self) {
        let cur = self.cursor_byte();
        let target = self.misspellings.iter().find(|m| m.start > cur).map(|m| m.start);
        match target {
            Some(b) => self.cursor_to_byte(b),
            None    => self.set_status(" no more misspellings", 244),
        }
    }
    fn jump_prev_misspelling(&mut self) {
        let cur = self.cursor_byte();
        let target = self.misspellings.iter().rev().find(|m| m.start < cur).map(|m| m.start);
        match target {
            Some(b) => self.cursor_to_byte(b),
            None    => self.set_status(" no previous misspellings", 244),
        }
    }

    /// `z=` — show numbered suggestions for the word at cursor, accept 1-9 to
    /// replace. Esc / any other key cancels.
    fn spell_suggest_at_cursor(&mut self) {
        let Some(m) = self.misspelling_at_cursor() else {
            self.set_status(" no misspelling at cursor", 244);
            return;
        };
        if m.suggestions.is_empty() {
            self.set_status(&format!(" no suggestions for '{}'", m.word), 244);
            return;
        }
        // Render up to 9 suggestions in the footer; numbered 1-9.
        let max = m.suggestions.len().min(9);
        let mut prompt = format!(" '{}' →", m.word);
        for (i, s) in m.suggestions.iter().take(max).enumerate() {
            prompt.push_str(&format!(" {}:{}", i + 1, s));
        }
        self.set_status(&prompt, 244);
        self.render_footer();
        // Wait for a single key.
        let key = Input::getchr(None).unwrap_or_default();
        let pick = key.chars().next()
            .and_then(|c| c.to_digit(10))
            .and_then(|d| {
                let idx = d as usize;
                if idx >= 1 && idx <= max { Some(idx - 1) } else { None }
            });
        if let Some(idx) = pick {
            let replacement = m.suggestions[idx].clone();
            self.buf.apply(m.start, m.end, &replacement);
            self.cursor_to_byte(m.start);
            self.recheck_spell();
            self.set_status(&format!(" → {}", replacement), 46);
        } else {
            self.status = None;
        }
    }
}

/// Hunspell flags ASCII word boundaries (Latin script). Use the same notion
/// for `word_at_cursor` so `zg` picks up the same span hunspell would.
fn is_wordchar(b: u8) -> bool {
    b.is_ascii_alphabetic() || b == b'\'' || b >= 0x80
}


/// `~/.config/scribe/spell.add` — append-only personal dictionary.
fn personal_dict_path() -> std::path::PathBuf {
    let home = std::env::var_os("HOME").map(std::path::PathBuf::from).unwrap_or_default();
    home.join(".config/scribe/spell.add")
}

fn load_personal_dict() -> Vec<String> {
    let path = personal_dict_path();
    std::fs::read_to_string(&path)
        .map(|s| s.lines().map(|l| l.trim().to_string()).filter(|l| !l.is_empty()).collect())
        .unwrap_or_default()
}

/// One user keymap. `mode` is "normal" / "insert" / "visual" (case-
/// insensitive); LHS is the trigger sequence in the same notation
/// macros use (`zr`, `<C-Space>`, `<Esc>`); RHS is the expanded
/// sequence to feed back through the input layer. RHS strings
/// starting with `:` are treated specially — fed straight to the
/// command executor instead of the keystroke pipeline.
#[derive(Clone, Debug)]
struct KeyMap {
    mode: String,
    lhs: Vec<String>,
    rhs: String,
}

/// Persistent settings loaded once at startup from `~/.config/scribe/scriberc`.
/// The runtime `:set` commands mutate the App's in-memory state directly
/// (without writing back) — the rcfile is the user's hand-edited source of
/// truth.
#[derive(Default)]
struct RcConfig {
    theme: Option<String>,
    number: bool,
    relative_numbers: bool,
    spell: bool,
    /// Hunspell dict tag — `en_US`, `nb_NO`, `nn_NO`, `de_DE`, … Whatever
    /// `hunspell -D` lists locally. Empty / unset → default `en_US`.
    spell_lang: Option<String>,
    /// xterm-256 palette index for the curly underline drawn on
    /// misspelled words. Default 196 (bright red). Override via
    /// `spell_color = N` (or `spellcolor = N`) in scriberc, or the
    /// config popup.
    spell_color: Option<u8>,
    /// `readingwidth = N` — column width of the centered text in
    /// reading mode. 0 = full pane width.
    reading_width: usize,
    /// `paragraphdim = true` — Limelight-style dimming when reading.
    paragraph_dim: bool,
    /// `read = true` — enter reading mode at startup.
    reading_mode: bool,
    /// User keymaps from a `[keymap]` section. Format per line:
    ///   `MODE LHS RHS`
    /// e.g. `normal zr :read` or `insert jk <Esc>`.
    keymaps: Vec<KeyMap>,
}

/// Per-file session state — where the cursor was last time scribe
/// closed this path. Indexed by absolute file path so renames don't
/// fool it. Keyed lookup on file open; written on save / quit.
#[derive(serde::Serialize, serde::Deserialize, Default, Clone)]
struct SessionEntry {
    line: usize,
    col: usize,
    scroll: usize,
}

fn sessions_path() -> std::path::PathBuf {
    let home = std::env::var_os("HOME").map(std::path::PathBuf::from).unwrap_or_default();
    home.join(".config/scribe/sessions.json")
}

fn load_sessions() -> std::collections::HashMap<String, SessionEntry> {
    let Ok(content) = std::fs::read_to_string(sessions_path()) else {
        return std::collections::HashMap::new();
    };
    serde_json::from_str(&content).unwrap_or_default()
}

fn save_sessions(map: &std::collections::HashMap<String, SessionEntry>) {
    let path = sessions_path();
    if let Some(dir) = path.parent() { let _ = std::fs::create_dir_all(dir); }
    let Ok(json) = serde_json::to_string_pretty(map) else { return };
    let tmp = path.with_extension("json.tmp");
    if std::fs::write(&tmp, json).is_ok() {
        let _ = std::fs::rename(&tmp, &path);
    }
}

/// Path to the persistent error log. Append-only; rotated by hand if it
/// grows too large.
fn log_path() -> std::path::PathBuf {
    let home = std::env::var_os("HOME").map(std::path::PathBuf::from).unwrap_or_default();
    home.join(".config/scribe/scribe.log")
}

/// Append a timestamped line to `~/.config/scribe/scribe.log`. Silent on
/// failure (no point recursing into the log when the log itself is the
/// problem). Used by the panic hook and any non-fatal error site.
fn log_msg(level: &str, msg: &str) {
    let path = log_path();
    if let Some(dir) = path.parent() { let _ = std::fs::create_dir_all(dir); }
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        use std::io::Write as _;
        let _ = writeln!(f, "[{}] {} v{} pid={} {}",
            ts, level, VERSION, std::process::id(), msg);
    }
}

/// Install a panic hook that:
/// 1. Restores the terminal (Crust::cleanup, disable bracketed paste)
///    so the user lands on a usable prompt instead of garbage state.
/// 2. Captures the panic message + location + a backtrace into
///    `~/.config/scribe/scribe.log`.
/// 3. Re-prints a one-line summary to stderr so the user sees
///    "scribe panicked at ... — see ~/.config/scribe/scribe.log".
fn install_panic_hook() {
    // Force a backtrace if the user hasn't set one — the log is useless
    // without it. Doesn't override an explicit RUST_BACKTRACE=0.
    if std::env::var_os("RUST_BACKTRACE").is_none() {
        std::env::set_var("RUST_BACKTRACE", "1");
    }
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        // Restore terminal first so the panic text isn't eaten by
        // alt-screen / raw mode.
        use std::io::Write as _;
        let _ = std::io::stdout().write_all(b"\x1b[?2004l");
        let _ = std::io::stdout().flush();
        Crust::cleanup();

        let payload = info.payload();
        let msg: &str = if let Some(s) = payload.downcast_ref::<&str>() { *s }
                        else if let Some(s) = payload.downcast_ref::<String>() { s.as_str() }
                        else { "<non-string panic payload>" };
        let loc = info.location()
            .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
            .unwrap_or_else(|| "<unknown>".into());
        let bt = std::backtrace::Backtrace::force_capture();
        let entry = format!("PANIC at {}: {}\n{}", loc, msg, bt);
        log_msg("PANIC", &entry);

        eprintln!("\nscribe panicked at {}: {}", loc, msg);
        eprintln!("see {} for the full backtrace", log_path().display());

        // Hand off to the default hook so any chained behavior still
        // runs (test framework hook, etc.).
        default_hook(info);
    }));
}

fn scriberc_path() -> std::path::PathBuf {
    let home = std::env::var_os("HOME").map(std::path::PathBuf::from).unwrap_or_default();
    home.join(".config/scribe/scriberc")
}

/// Parse the rcfile: simple `key = value` per line, `#` starts a comment.
/// Unknown keys are ignored silently. Boolean values: `true` / `1` / `yes`
/// / `on` are truthy; anything else is falsy.
fn load_scriberc() -> RcConfig {
    let mut cfg = RcConfig::default();
    let path = scriberc_path();
    let Ok(content) = std::fs::read_to_string(&path) else { return cfg; };
    let truthy = |v: &str| matches!(v.trim(), "true" | "1" | "yes" | "on");
    let mut in_keymap = false;
    for line in content.lines() {
        let stripped = line.split('#').next().unwrap_or("").trim();
        if stripped.is_empty() { continue; }
        // Section headers: `[keymap]` switches to whitespace-delimited
        // 3-tuple parsing; any other `[...]` returns to the default
        // `key = value` parser.
        if let Some(s) = stripped.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
            in_keymap = matches!(s.trim().to_lowercase().as_str(), "keymap" | "keymaps" | "map");
            continue;
        }
        if in_keymap {
            let mut it = stripped.splitn(3, char::is_whitespace);
            let mode = it.next().unwrap_or("").trim();
            let lhs  = it.next().unwrap_or("").trim();
            let rhs  = it.next().unwrap_or("").trim();
            if mode.is_empty() || lhs.is_empty() || rhs.is_empty() { continue; }
            cfg.keymaps.push(KeyMap {
                mode: mode.to_lowercase(),
                lhs: parse_macro_text(lhs),
                rhs: rhs.to_string(),
            });
            continue;
        }
        let Some((k, v)) = stripped.split_once('=') else { continue };
        let k = k.trim();
        let v = v.trim();
        match k {
            "theme"          => cfg.theme = Some(v.to_string()),
            "number" | "nu"  => cfg.number = truthy(v),
            "relativenumber" | "rnu" => {
                cfg.relative_numbers = truthy(v);
                if cfg.relative_numbers { cfg.number = true; }
            }
            "spell"          => cfg.spell = truthy(v),
            "spelllang" | "lang" => {
                if !v.is_empty() { cfg.spell_lang = Some(v.to_string()); }
            }
            "spellcolor" | "spell_color" => {
                if let Ok(n) = v.parse::<u8>() { cfg.spell_color = Some(n); }
            }
            "readingwidth" | "rw" => {
                if let Ok(n) = v.parse::<usize>() { cfg.reading_width = n; }
            }
            "paragraphdim" | "pdim" => cfg.paragraph_dim = truthy(v),
            "read" | "reading" => cfg.reading_mode = truthy(v),
            _ => {}
        }
    }
    cfg
}

/// `~/.config/scribe/cmdhistory` — newline-delimited list of past `:`
/// commands, oldest first. Capped at 100 entries (mirroring editline's
/// in-memory cap). Empty / missing file → empty history.
fn cmd_history_path() -> std::path::PathBuf {
    let home = std::env::var_os("HOME").map(std::path::PathBuf::from).unwrap_or_default();
    home.join(".config/scribe/cmdhistory")
}

fn load_cmd_history() -> Vec<String> {
    std::fs::read_to_string(cmd_history_path())
        .map(|s| s.lines().map(str::to_string).filter(|l| !l.is_empty()).collect())
        .unwrap_or_default()
}

fn save_cmd_history(hist: &[String]) {
    let path = cmd_history_path();
    if let Some(dir) = path.parent() { let _ = std::fs::create_dir_all(dir); }
    // Cap on disk too, in case the in-memory list grew beyond 100.
    let start = hist.len().saturating_sub(100);
    let body = hist[start..].join("\n");
    let _ = std::fs::write(&path, body);
}

/// Width of the line-number gutter for a buffer with `line_count` lines.
/// Returns 0 when numbers are off. Format: " NN │ " — N digits + space +
/// vertical bar + trailing space, with N at least 2 so single-digit
/// buffers don't shift layout when they grow past 9 lines.
fn gutter_width(line_count: usize, show: bool) -> usize {
    if !show { return 0; }
    let digits = line_count.to_string().len().max(2);
    digits + 3
}

/// Render the gutter cell for `line_idx`. `cur_line` and `relative`
/// determine whether non-cursor rows display absolute or relative numbers.
fn gutter_cell(line_idx: usize, cur_line: usize, line_count: usize, relative: bool) -> String {
    let digits = line_count.to_string().len().max(2);
    let n = if relative && line_idx != cur_line {
        line_idx.abs_diff(cur_line)
    } else {
        line_idx + 1
    };
    // Dim the gutter (240 = gray) so it doesn't compete with content.
    format!("\x1b[38;5;240m{:>width$} │ \x1b[39m", n, width = digits)
}

impl App {

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
        let pane_x = self.main_p.x;
        let pane_y = self.main_p.y;
        let pane_w = self.main_p.w as usize;
        // When line numbers are on, the gutter prepends each line's text
        // before the pane wraps. The first wrap row of every line has
        // gutter+content; subsequent wrap rows have content only. We
        // approximate cursor placement assuming the cursor is on the first
        // wrap row (typical for short lines); long-wrapped lines can be
        // off by gutter_w on continuation rows — acceptable for v1.
        let gutter_w = gutter_width(self.buf.line_count(), self.show_numbers && !self.reading_mode);

        let mut visual_row: usize = 0;
        for ln in self.scroll..self.cur_line {
            if ln >= self.buf.line_count() { break; }
            let w = self.buf.line(ln).chars().count() + gutter_w;
            visual_row += ((w.max(1) - 1) / pane_w) + 1;
        }
        let cur_disp_col = self.buf.line(self.cur_line)[..self.cur_col.min(self.current_line_len())]
            .chars().count();
        let visual_col = cur_disp_col + gutter_w;
        let row_in_line = visual_col / pane_w;
        let col_in_row = visual_col % pane_w;
        visual_row += row_in_line;

        let row = pane_y + visual_row as u16;
        let col = pane_x + col_in_row as u16;

        // Cursor shape per mode — handed to crust so the raw DECSCUSR / CUP
        // escapes don't leak into scribe's source. Insert / Command get a
        // bar (6); everything else gets a steady block (2).
        let shape = match self.mode {
            Mode::Insert | Mode::Command => 6,
            _ => 2,
        };
        crust::Cursor::show();
        crust::Cursor::shape(shape);
        crust::Cursor::set(col, row);
    }

    fn render_header(&mut self) {
        if self.reading_mode {
            // Distraction-free: just a thin dim divider so the eye has
            // a top boundary without a full chrome bar.
            self.header.say(&style::fg(&" ".repeat(self.cols as usize), 240));
            return;
        }
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

        // Email-mode pre-pass: locate the boundary between header block and
        // body (first blank line), and the signature delimiter (`-- `). Both
        // are passed to line_color_email() so per-line lookups stay O(1).
        let is_email = matches!(self.buf.kind, FileKind::Email);
        let header_end: Option<usize> = if is_email {
            (0..self.buf.line_count()).find(|&i| self.buf.line(i).trim().is_empty())
        } else { None };
        let sig_start: Option<usize> = if is_email {
            let body_start = header_end.unwrap_or(0);
            let line_count = self.buf.line_count();
            highlight::find_sig_start(line_count, body_start, |i| self.buf.line(i))
        } else { None };
        // Source mode: render the whole buffer through highlight crate, then
        // slice the visible window. Pointer's hand-rolled highlighter is
        // line-stateless, so this is line-count work — fast enough for the
        // file sizes a writer's editor sees. A future per-line cache
        // invalidated on edit would let big repos stay snappy.
        let source_lines: Vec<String> = match &self.buf.kind {
            FileKind::Source(ext) => {
                let line_count = self.buf.line_count();
                let mut all = String::new();
                for i in 0..line_count {
                    all.push_str(&self.buf.line(i));
                    all.push('\n');
                }
                let rendered = highlight::highlight(&all, ext, line_count + 1);
                rendered.split('\n')
                    .skip(self.scroll)
                    .take(pane_h)
                    .map(str::to_string)
                    .collect()
            }
            _ => Vec::new(),
        };

        let line_count = self.buf.line_count();
        let show_numbers = self.show_numbers && !self.reading_mode;
        let relative_numbers = self.relative_numbers && !self.reading_mode;
        // Limelight-style: dim every paragraph except the cursor's
        // when reading_mode + paragraph_dim are both on.
        let dim_others = self.reading_mode && self.paragraph_dim;
        let (para_lo, para_hi) = if dim_others {
            self.current_paragraph_bounds()
        } else { (0, usize::MAX) };

        let mut out = String::new();
        for i in 0..pane_h {
            let line_idx = self.scroll + i;
            if line_idx < line_count {
                // Gutter prefix per visible line (line numbers / relative
                // numbers). Emitted as part of the rendered text so the
                // pane treats it as the start of the line.
                if show_numbers {
                    out.push_str(&gutter_cell(line_idx, self.cur_line, line_count, relative_numbers));
                }
                let line = self.buf.line(line_idx);
                let line_byte_off = self.buf.line_byte_offset(line_idx);
                // Per-line fg color when email mode says so. None → default.
                let line_style = if is_email {
                    highlight::line_style_email(&line, line_idx, header_end, sig_start)
                } else { highlight::EmailLineStyle::None };
                // Resolve the base fg + KEY-bold extent for the unified line
                // emitter. HeaderBold(c): line in c, KEY (up to colon+1) bold.
                let (mut base_fg, bold_until): (Option<u8>, Option<usize>) = match line_style {
                    highlight::EmailLineStyle::None        => (None, None),
                    highlight::EmailLineStyle::Solid(c)    => (Some(c), None),
                    highlight::EmailLineStyle::HeaderBold(c) => (Some(c), line.find(':').map(|p| p + 1)),
                };
                // Limelight: outside the cursor's paragraph → dim. Force
                // base_fg to a subtle grey, overriding email/source styling.
                let line_is_dim = dim_others && (line_idx < para_lo || line_idx > para_hi);
                if line_is_dim { base_fg = Some(240); }
                // Fast path: when no selection touches this line, we can
                // emit the whole line in one styled span instead of doing
                // per-char ANSI emit.
                let line_in_sel = match (sel_start, sel_end, sel_kind) {
                    (Some(s), Some(e), Some(Mode::Visual)) => {
                        let line_end = line_byte_off + line.len();
                        e > line_byte_off && s < line_end
                    }
                    (_, _, Some(Mode::VisualLine)) | (_, _, Some(Mode::VisualBlock)) => {
                        let l1 = self.visual_anchor_line.min(self.cur_line);
                        let l2 = self.visual_anchor_line.max(self.cur_line);
                        line_idx >= l1 && line_idx <= l2
                    }
                    _ => false,
                };
                if !line_in_sel {
                    // Compute miss ranges first; we use them in two places.
                    let line_end_byte = line_byte_off + line.len();
                    let miss_ranges: Vec<(usize, usize)> = self.misspellings.iter()
                        .filter(|m| m.end > line_byte_off && m.start < line_end_byte)
                        .map(|m| {
                            let s = m.start.saturating_sub(line_byte_off).min(line.len());
                            let e = m.end.saturating_sub(line_byte_off).min(line.len());
                            (s, e)
                        })
                        .collect();
                    // Source mode: emit the highlight-styled line straight
                    // from the pre-built `source_lines` buffer. If the line
                    // has any misspellings we fall through to the plain
                    // emit path so curly underlines show; lines without
                    // misses keep their syntax colors.
                    if !source_lines.is_empty() && miss_ranges.is_empty() && !line_is_dim {
                        if let Some(styled) = source_lines.get(i) {
                            out.push_str(styled);
                            if i + 1 < pane_h { out.push('\n'); }
                            continue;
                        }
                    }
                    // Unified emit: base_fg + KEY-bold + inline tokens
                    // (addresses → magenta 201, URLs → blue 4 + OSC 8) +
                    // misspelling overlay. Single function handles all
                    // attribute combinations and minimises SGR transitions.
                    let tokens = highlight::inline_tokens(&line);
                    highlight::emit_email_line(&mut out, &line, base_fg, bold_until, &tokens, &miss_ranges);
                    if i + 1 < pane_h { out.push('\n'); }
                    continue;
                }
                // Selection touches this line: char-by-char so we can apply
                // selection bg precisely. (Email-mode fg is dropped on the
                // selected slice — selection bg is the dominant signal.)
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
        // Diff-render: only repaints rows whose rendered output actually
        // changed. full_refresh used to be called here; on hold-down j/k
        // that wipes-and-repaints the whole pane every keystroke, which
        // shows up as a slight flash. Header/footer already use the diff
        // path via Pane::say.
        self.main_p.refresh();
    }

    fn render_footer(&mut self) {
        if self.reading_mode {
            // Reading mode: only show transient status (the user
            // pressed a key that produced one), otherwise dim line.
            let bg = format!("\x1b[48;5;{}m", 234u8);
            let line = match &self.status {
                Some((msg, c)) => format!("{} {}{}\x1b[0m", bg, style::fg(msg, *c), bg),
                None => format!("{}{}\x1b[0m", bg, " ".repeat(self.cols as usize)),
            };
            self.footer.say(&line);
            return;
        }
        // SGR-aware: each style::fg / style::bg helper closes with \x1b[0m,
        // which resets BACKGROUND to terminal default. After every styled
        // segment we re-assert the pane's bg so the gap spaces don't render
        // as black streaks. The whole line ends with one final \x1b[0m.
        const BG: u8 = 236;
        let bg_on = format!("\x1b[48;5;{}m", BG);

        let mode_label = style::bg(&style::fg(self.mode.label(), 0), self.mode.color());
        let pos = format!(" {}:{} ", self.cur_line + 1, self.cur_col + 1);
        let right = format!("scribe v{} ", VERSION);

        // Persistent stats segment: word count + spell status. Sits to the
        // left of the position indicator so the status message in the
        // middle slot can shrink without overlapping. Cheap on prose-sized
        // buffers; if it ever becomes the hot path, gate behind `dirty`.
        // In Visual mode, swap whole-buffer stats for selection stats —
        // matches vim's `g Ctrl-G` flash but live + always-on. Lines /
        // words / chars are all measured over the active selection.
        let (stats_plain, stats_styled) = if self.mode.is_visual() {
            let (lines, words, chars) = self.compute_selection_stats();
            let plain = format!(" sel: {}l {}w {}c ", lines, words, chars);
            let styled = format!(" sel: {}l {}w {}c{} ",
                style::fg(&lines.to_string(), 252),
                style::fg(&words.to_string(), 252),
                style::fg(&chars.to_string(), 244),
                bg_on);
            (plain, styled)
        } else {
            let words = self.compute_wordcount();
            let chars = self.buf.rope.len_chars();
            let spell_lbl = if self.spell_enabled {
                format!("spell:{}", self.spell_lang)
            } else {
                "spell:off".to_string()
            };
            let plain = format!(" {}w  {}c  {} ", words, chars, spell_lbl);
            let styled = format!(" {}w  {}c  {}{} ",
                style::fg(&words.to_string(), 252),
                style::fg(&chars.to_string(), 244),
                style::fg(&spell_lbl, if self.spell_enabled { 35 } else { 244 }),
                bg_on);
            (plain, styled)
        };

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
        let stats_w = crust::display_width(&stats_plain);
        let pos_w = crust::display_width(&pos);
        let right_w = crust::display_width(&right);

        let total = mode_w + middle_w + stats_w + pos_w + right_w;
        let line = if total <= cols {
            let gap = cols - total;
            // Order: badge → bg_on → middle → gap → stats → pos → right →
            // final reset. bg_on after every helper that ends in [0m so
            // the trailing reset doesn't drop the bar to terminal-default
            // bg mid-line.
            format!("{}{}{}{}{}{}{}\x1b[0m",
                mode_label, bg_on, middle_styled, " ".repeat(gap),
                stats_styled, pos, right)
        } else if mode_w + middle_w + stats_w + pos_w + right_w
                  .saturating_sub(stats_w) <= cols
        {
            // Too tight for the stats segment — drop it.
            let visible_w = mode_w + middle_w + pos_w + right_w;
            let pad = cols.saturating_sub(visible_w);
            format!("{}{}{}{}{}{}\x1b[0m",
                mode_label, bg_on, middle_styled, " ".repeat(pad), pos, right)
        } else {
            let visible = format!("{}{}{}", mode_label, bg_on, middle_styled);
            let visible_w = mode_w + middle_w;
            let pad = cols.saturating_sub(visible_w);
            format!("{}{}\x1b[0m", visible, " ".repeat(pad))
        };
        self.footer.say(&line);
    }

    /// Whitespace-delimited token count over the whole buffer. Cheap
    /// enough for prose-sized files; if a 1MB log is ever opened, swap
    /// for a `dirty`-gated cache.
    fn compute_wordcount(&self) -> usize {
        let mut n = 0usize;
        for i in 0..self.buf.line_count() {
            n += self.buf.line(i).split_whitespace().count();
        }
        n
    }

    /// Lines / words / chars over the current Visual selection. For
    /// charwise (`v`) the span is the byte range between cursor and
    /// anchor, inclusive of the cell under the cursor. For linewise
    /// (`V`) it's whole lines. For block (`Ctrl-V`) it's the column
    /// range on each spanned line. Caller only invokes this in
    /// Visual modes — outside Visual the result is meaningless.
    fn compute_selection_stats(&self) -> (usize, usize, usize) {
        match self.mode {
            Mode::VisualLine => {
                let l1 = self.visual_anchor_line.min(self.cur_line);
                let l2 = self.visual_anchor_line.max(self.cur_line);
                let mut chars = 0usize;
                let mut words = 0usize;
                for i in l1..=l2 {
                    let line = self.buf.line(i);
                    chars += line.chars().count() + 1; // +1 for the newline
                    words += line.split_whitespace().count();
                }
                let lines = l2 - l1 + 1;
                (lines, words, chars.saturating_sub(1)) // drop final newline
            }
            Mode::VisualBlock => {
                let l1 = self.visual_anchor_line.min(self.cur_line);
                let l2 = self.visual_anchor_line.max(self.cur_line);
                let c1 = self.visual_anchor_col.min(self.cur_col);
                let c2 = self.visual_anchor_col.max(self.cur_col);
                let mut chars = 0usize;
                let mut words = 0usize;
                for i in l1..=l2 {
                    let line = self.buf.line(i);
                    let lo = c1.min(line.len());
                    let hi = (c2 + 1).min(line.len());
                    let mut a = lo;
                    while a < hi && !line.is_char_boundary(a) { a += 1; }
                    let mut b = hi;
                    while b < line.len() && !line.is_char_boundary(b) { b += 1; }
                    let slice = &line[a..b];
                    chars += slice.chars().count();
                    words += slice.split_whitespace().count();
                }
                (l2 - l1 + 1, words, chars)
            }
            _ => {
                // Charwise.
                let cur = self.cursor_byte();
                let (lo, hi) = if cur < self.visual_anchor {
                    (cur, self.visual_anchor)
                } else {
                    (self.visual_anchor, cur)
                };
                // Include the cell under the cursor (vim's inclusive end).
                let total = self.buf.rope.len_bytes();
                let mut hi2 = (hi + 1).min(total);
                while hi2 < total && !self.buf.rope.to_string().is_char_boundary(hi2) { hi2 += 1; }
                let span: String = self.buf.rope.byte_slice(lo..hi2).to_string();
                let chars = span.chars().count();
                let words = span.split_whitespace().count();
                let lines = span.matches('\n').count() + 1;
                (lines, words, chars)
            }
        }
    }

    fn set_status(&mut self, msg: &str, c: u8) { self.status = Some((msg.into(), c)); }

    /// Statusline confirmation for yank / delete / change. Mirrors vim's
    /// "N lines yanked" so the user has visible feedback that the
    /// register was written. `verb` is "yanked" / "deleted" / "changed".
    /// Color: 46 (green) for yank, 178 (orange) for cut.
    fn say_yank(&mut self, verb: &str, reg: Option<char>, kind: YankKind, text: &str) {
        let count_label = match kind {
            YankKind::Charwise => {
                let n = text.chars().count();
                if n == 1 { "1 char".into() } else { format!("{} chars", n) }
            }
            YankKind::Linewise => {
                // Linewise text ends with `\n`; line count = newlines.
                let n = text.matches('\n').count().max(1);
                if n == 1 { "1 line".into() } else { format!("{} lines", n) }
            }
            YankKind::Block => {
                let n = text.matches('\n').count() + 1;
                if n == 1 { "1 block row".into() } else { format!("{} block rows", n) }
            }
        };
        let reg_part = match reg {
            Some(c) => format!(r#" into "{}"#, c),
            None    => String::new(),
        };
        let color = if verb == "yanked" { 46 } else { 178 };
        self.set_status(&format!(" {} {}{}", count_label, verb, reg_part), color);
    }

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
        self.snap_col_to_boundary();
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
            let line = self.buf.line(self.cur_line);
            let mut p = self.cur_col - 1;
            while p > 0 && !line.is_char_boundary(p) { p -= 1; }
            self.cur_col = p;
        } else if self.cur_line > 0 {
            self.cur_line -= 1;
            self.cur_col = self.col_cap();
            self.snap_col_to_boundary();
        }
        self.want_col = self.cur_col;
    }

    /// Move one char right, wrapping to start of next line when at line end.
    fn move_right_wrap(&mut self) {
        let cap = self.col_cap();
        if self.cur_col < cap {
            let line = self.buf.line(self.cur_line);
            let mut p = self.cur_col + 1;
            while p < line.len() && !line.is_char_boundary(p) { p += 1; }
            self.cur_col = p.min(cap);
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
            self.snap_col_to_boundary();
        }
    }

    fn move_down(&mut self) {
        if self.cur_line + 1 < self.buf.line_count() {
            self.cur_line += 1;
            self.cur_col = self.want_col.min(self.col_cap());
            self.snap_col_to_boundary();
        }
    }

    /// Round `cur_col` DOWN to the nearest UTF-8 char boundary on the
    /// current line. Cheap defence against any path that advanced
    /// `cur_col` by 1 byte instead of 1 char — without this guard,
    /// `position_cursor`'s `line[..cur_col]` slice panics when the
    /// buffer contains multi-byte chars (æ, ø, å, emoji, …).
    fn snap_col_to_boundary(&mut self) {
        let line = self.buf.line(self.cur_line);
        let mut c = self.cur_col.min(line.len());
        while c > 0 && !line.is_char_boundary(c) { c -= 1; }
        self.cur_col = c;
    }

    // ── Bracketed paste ────────────────────────────────────────────────
    /// Insert the entire pasted payload as a single compound undo node and
    /// advance the cursor to the end of what was inserted. Strips terminal
    /// `\r` so CRLF clipboards don't double-line. Works in any mode; mode
    /// is left unchanged.
    fn handle_paste(&mut self, raw: &str) {
        if raw.is_empty() { return; }
        let cleaned: String = raw.replace("\r\n", "\n").replace('\r', "\n");
        let off = self.cursor_byte();
        // One apply = one undo node + one render. Per-char trickle-in is what
        // made paste feel laggy and produced thousands of undo nodes.
        self.buf.apply(off, off, &cleaned);
        let new_off = off + cleaned.len();
        self.cursor_to_byte(new_off);
        if self.mode == Mode::Insert && self.capturing_insert {
            self.captured_insert.push_str(&cleaned);
        }
    }

    // ── Normal mode (pending state machine) ────────────────────────────
    fn handle_normal(&mut self, key: &str) -> bool {
        if let Some(q) = self.try_keymap("normal", key) { return q; }
        self.status = None;

        // Macro register-prefix dispatch. Done first so a count / operator
        // already in flight (which we don't want anyway) is not considered.
        if self.macro_prefix {
            self.macro_prefix = false;
            if let Some(c) = key.chars().next() {
                if c.is_ascii_alphanumeric() {
                    self.recording_buf.clear();
                    self.recording = Some(c);
                    self.set_status(&format!(" recording @{}", c), 178);
                }
            }
            return false;
        }
        if self.at_prefix {
            self.at_prefix = false;
            let reg = if key == "@" {
                self.last_macro
            } else {
                key.chars().next().filter(|c| c.is_ascii_alphanumeric())
            };
            if let Some(r) = reg { self.replay_macro(r); }
            return false;
        }
        // `M` toggles macro recording. While recording, a second `M` stops;
        // otherwise it primes for the register name.
        if key == "M" && self.pending.operator.is_none()
            && self.pending.count1.is_none() && self.pending.text_object.is_none()
        {
            if let Some(reg) = self.recording.take() {
                let text = std::mem::take(&mut self.recording_buf);
                let bytes = text.len();
                self.regs.put(reg, Yank { text, kind: YankKind::Charwise });
                self.set_status(&format!(" recorded @{} ({} bytes)", reg, bytes), 244);
            } else {
                self.macro_prefix = true;
            }
            return false;
        }
        if key == "@" && self.pending.operator.is_none()
            && self.pending.count1.is_none() && self.pending.text_object.is_none()
        {
            self.at_prefix = true;
            return false;
        }

        // Mark dispatch.
        if self.mark_set_prefix {
            self.mark_set_prefix = false;
            if let Some(c) = key.chars().next() {
                if c.is_ascii_alphabetic() {
                    self.marks.insert(c, self.cursor_byte());
                    self.set_status(&format!(" mark '{} set", c), 244);
                }
            }
            return false;
        }
        if self.mark_jump_prefix {
            self.mark_jump_prefix = false;
            let exact = self.mark_jump_exact;
            self.mark_jump_exact = false;
            if let Some(c) = key.chars().next() {
                if c.is_ascii_alphabetic() {
                    if let Some(&byte) = self.marks.get(&c) {
                        self.cursor_to_byte(byte);
                        if !exact {
                            // `'a` lands on first non-blank of the
                            // mark's line; `` `a `` lands at exact col.
                            let off = motion::line_first_nonblank(
                                &self.buf, self.cursor_byte());
                            self.cursor_to_byte(off);
                        }
                    } else {
                        self.set_status(&format!(" mark '{} not set", c), 196);
                    }
                }
            }
            return false;
        }
        if key == "m" && self.pending.operator.is_none()
            && self.pending.count1.is_none() && self.pending.text_object.is_none()
        {
            self.mark_set_prefix = true;
            return false;
        }
        if (key == "'" || key == "`") && self.pending.operator.is_none()
            && self.pending.count1.is_none() && self.pending.text_object.is_none()
        {
            self.mark_jump_prefix = true;
            self.mark_jump_exact = key == "`";
            return false;
        }

        // `z` prefix: spell + reading-mode shortcuts.
        //   z=  suggest replacements for misspelled word
        //   zg  add word at cursor to personal dictionary
        //   zr  toggle :read (distraction-free reading mode)
        //   zq  save + quit (equivalent to :wq)
        //   zn  jump to next misspelling (mirrors `]s`)
        //   zp  jump to previous misspelling (mirrors `[s`)
        if self.z_prefix {
            self.z_prefix = false;
            match key {
                "=" => self.spell_suggest_at_cursor(),
                "g" => {
                    if let Some(word) = self.word_at_cursor() {
                        self.spell_add_word(&word);
                    }
                }
                "r" => {
                    self.reading_mode = !self.reading_mode;
                    self.apply_layout();
                    self.set_status(
                        if self.reading_mode { " reading mode" } else { " reading mode off" },
                        244);
                }
                "q" => {
                    let _ = self.buf.save();
                    return true;
                }
                "n" => self.jump_next_misspelling(),
                "p" => self.jump_prev_misspelling(),
                _ => {}
            }
            return false;
        }
        if key == "z" && self.pending.operator.is_none() && self.pending.count1.is_none() {
            self.z_prefix = true;
            return false;
        }

        // `]` / `[` prefix: dispatch on the follow-up key.
        if let Some(open) = self.bracket_prefix.take() {
            match (open, key) {
                (']', "s") => self.jump_next_misspelling(),
                ('[', "s") => self.jump_prev_misspelling(),
                _ => {}
            }
            return false;
        }
        if (key == "]" || key == "[") && self.pending.operator.is_none()
            && self.pending.count1.is_none() && self.pending.text_object.is_none()
        {
            self.bracket_prefix = Some(key.chars().next().unwrap());
            return false;
        }

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
                if matches!(opc, 'Q' | '>' | '<') {
                    // Linewise: derive [from..to] line range from object span.
                    let (l1, _) = self.buf.byte_to_line_col(start);
                    let (l2, _) = self.buf.byte_to_line_col(end.saturating_sub(1).max(start));
                    let (lo, hi) = if l1 <= l2 { (l1, l2) } else { (l2, l1) };
                    let extra = hi - lo;
                    self.execute_op_linewise(lo, extra);
                } else {
                    self.execute_op_charwise(opc, start, end);
                }
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
                    self.pending.clear();
                }
                "q" => {
                    // Enter `gq` operator-pending. Don't clear pending — count
                    // and register survive into the next motion / `q` shortcut.
                    self.pending.operator = Some('Q');
                }
                _ => { self.pending.clear(); }
            }
            return false;
        }
        if key == "g" { self.pending.g_prefix = true; return false; }

        // gqq shortcut — current line gq. Must precede the q-quit fallback in
        // handle_normal_action; only fires when gq operator is pending.
        if key == "q" && self.pending.operator == Some('Q') {
            let n = self.pending.count1.unwrap_or(1);
            let extra = n.saturating_sub(1);
            self.execute_op_linewise(self.cur_line, extra);
            self.last_change = Some(LastChange::Op {
                op: 'Q',
                motion: ChangeMotion::Linewise { extra },
                count: n,
                register: None,
                insert_text: String::new(),
            });
            self.pending.clear();
            return false;
        }

        // Operator handling: `d`, `c`, `y`, `>`, `<` — doubled = linewise on
        // count1 lines. (`gq` doubles via the `gqq` shortcut handled above.)
        if matches!(key, "d" | "c" | "y" | ">" | "<") {
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
                let cap_reg = self.pending.register;
                if matches!(opc, 'Q' | '>' | '<') {
                    // Linewise operators: snap motion target to whole-line
                    // range and dispatch via execute_op_linewise.
                    let (l1, _) = self.buf.byte_to_line_col(from);
                    let (l2, _) = self.buf.byte_to_line_col(target_byte);
                    let (lo, hi) = if l1 <= l2 { (l1, l2) } else { (l2, l1) };
                    let extra = hi - lo;
                    self.execute_op_linewise(lo, extra);
                    self.last_change = Some(LastChange::Op {
                        op: opc,
                        motion: ChangeMotion::Linewise { extra },
                        count,
                        register: cap_reg,
                        insert_text: String::new(),
                    });
                } else {
                    let (start, end) = if from <= target_byte { (from, target_byte) } else { (target_byte, from) };
                    self.execute_op_charwise(opc, start, end);
                    self.last_change = Some(LastChange::Op {
                        op: opc,
                        motion: ChangeMotion::Key(key.to_string()),
                        count,
                        register: cap_reg,
                        insert_text: String::new(),
                    });
                }
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

            // Move current line up / down (vim users often map this to
            // Alt+j/k; user wants Ctrl+arrows). One compound undo node.
            "C-UP"   => for _ in 0..count { self.move_line_up(); },
            "C-DOWN" => for _ in 0..count { self.move_line_down(); },

            // Ctrl-A / Ctrl-X — increment / decrement the number under
            // or after the cursor. Recognises plain integers (with
            // optional leading `-`) AND ISO 8601 dates (YYYY-MM-DD)
            // with proper month / leap-year rollover. Replays via dot.
            "C-A" => { self.change_number(count as i64); }
            "C-X" => { self.change_number(-(count as i64)); }

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
            // s — substitute `count` chars at cursor (vim equiv. of `cl` with
            // count). Delete the chars then enter Insert. Last_change recorded
            // so dot replays.
            "s" => {
                let from = self.cursor_byte();
                let line = self.buf.line(self.cur_line);
                let mut e = self.cur_col;
                let mut taken = 0;
                while taken < count && e < line.len() {
                    e += 1;
                    while e < line.len() && !line.is_char_boundary(e) { e += 1; }
                    taken += 1;
                }
                let abs_end = self.buf.line_byte_offset(self.cur_line) + e;
                self.last_change = Some(LastChange::Op {
                    op: 'c',
                    motion: ChangeMotion::Key("l".to_string()),
                    count: taken.max(1),
                    register: None,
                    insert_text: String::new(),
                });
                self.execute_op_charwise('c', from, abs_end);
            }
            // S — substitute `count` lines (vim equiv. of `cc`). Replace the
            // line(s) with one empty line and enter Insert.
            "S" => {
                let extra = count.saturating_sub(1);
                self.last_change = Some(LastChange::Op {
                    op: 'c',
                    motion: ChangeMotion::Linewise { extra },
                    count,
                    register: None,
                    insert_text: String::new(),
                });
                let saved_op = self.pending.operator;
                self.pending.operator = Some('c');
                self.execute_op_linewise(self.cur_line, extra);
                self.pending.operator = saved_op;
            }
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

            // Enter Command — pointer-style: footer.ask handles the prompt
            // line entirely (line editor, cursor placement, no flicker).
            // execute_command runs once the user hits Enter.
            ":" => {
                return self.run_command_prompt();
            }

            // Fe2O3 harmonized quit, but with a guard against silent
            // overwrites: `q` quits when clean, refuses with a status
            // message when dirty. The user used to lose original files
            // when hitting `q` after a destructive `:claude` turn — that
            // saved the response over the source on disk. Now they must
            // explicitly `:wq` to commit or `Q` to discard.
            "q" => {
                if self.buf.dirty {
                    self.set_status(" unsaved changes — :wq to save+quit, Q to discard", 196);
                    return false;
                }
                return true;
            }
            "Q" => return true,

            _ => {}
        }
        false
    }

    /// Replay every key captured in macro register `reg`, dispatching it
    /// through the current mode handler. Replay sets `replay_depth` so
    /// keys produced by replay are NOT re-captured into a recording.
    /// Try the keymap layer for `key` in mode `mode_name`. Returns:
    ///   `Some(quit)` — a map fired and consumed the key (RHS ran)
    ///   `None`       — no map matched; caller proceeds with built-in
    ///                  handling for `key`.
    /// Supports 1- and 2-key LHS sequences. With a 2-key map, after
    /// the first key matches a known prefix we set `map_pending` and
    /// consume; the next call resolves the pair (matched RHS fires;
    /// otherwise the buffered prefix is replayed through normal
    /// dispatch and the new key falls through).
    fn try_keymap(&mut self, mode_name: &str, key: &str) -> Option<bool> {
        if self.map_depth > 0 || self.keymaps.is_empty() { return None; }
        // Resolve pending 2-key prefix first.
        if let Some(prev) = self.map_pending.take() {
            // Look for an exact 2-key match.
            for m in &self.keymaps.clone() {
                if m.mode != mode_name { continue; }
                if m.lhs.len() == 2 && m.lhs[0] == prev && m.lhs[1] == key {
                    return Some(self.run_keymap_rhs(&m.rhs));
                }
            }
            // No match — replay the prefix as if the user typed it,
            // then fall through so the caller handles `key`.
            self.dispatch_key_no_map(mode_name, &prev);
            return None;
        }
        // Check for 1-key exact match.
        for m in &self.keymaps.clone() {
            if m.mode != mode_name { continue; }
            if m.lhs.len() == 1 && m.lhs[0] == key {
                return Some(self.run_keymap_rhs(&m.rhs));
            }
        }
        // Check for 2-key prefix; if any map starts with `key`,
        // hold the key and wait.
        let any_prefix = self.keymaps.iter()
            .any(|m| m.mode == mode_name && m.lhs.len() == 2 && m.lhs[0] == key);
        if any_prefix {
            self.map_pending = Some(key.to_string());
            return Some(false);
        }
        None
    }

    /// Execute the RHS of a triggered keymap. RHS strings starting
    /// with `:` run as ex commands (so a user can map `zq` to `:wq`).
    /// Other RHS strings are parsed via `parse_macro_text` and fed
    /// back through the current mode's handler with `map_depth`
    /// incremented so we don't infinite-loop on a mapping that
    /// rewrites itself. Returns true if the editor should quit.
    fn run_keymap_rhs(&mut self, rhs: &str) -> bool {
        if let Some(cmd) = rhs.strip_prefix(':') {
            return self.execute_command(cmd.trim());
        }
        let keys = parse_macro_text(rhs);
        self.map_depth += 1;
        let mut quit = false;
        for k in keys {
            quit = match self.mode {
                Mode::Normal      => self.handle_normal(&k),
                Mode::Insert      => self.handle_insert(&k),
                Mode::Visual      |
                Mode::VisualLine  |
                Mode::VisualBlock => self.handle_visual(&k),
                Mode::Command     => false,
            };
            if quit { break; }
        }
        self.map_depth -= 1;
        quit
    }

    /// Dispatch a single key through the active mode's handler with
    /// the keymap layer suppressed. Used when a 2-key prefix was
    /// staged but the second key didn't match — we replay the
    /// stored prefix as a normal keystroke.
    fn dispatch_key_no_map(&mut self, _mode_name: &str, key: &str) {
        self.map_depth += 1;
        match self.mode {
            Mode::Normal      => { self.handle_normal(key); }
            Mode::Insert      => { self.handle_insert(key); }
            Mode::Visual      |
            Mode::VisualLine  |
            Mode::VisualBlock => { self.handle_visual(key); }
            Mode::Command     => {}
        }
        self.map_depth -= 1;
    }

    fn replay_macro(&mut self, reg: char) {
        if self.replay_depth >= 4 {
            self.set_status(" macro recursion too deep", 196);
            return;
        }
        let text = match self.regs.get(reg) {
            Some(y) if !y.text.is_empty() => y.text.clone(),
            _ => { self.set_status(&format!(" register @{} is empty", reg), 244); return; }
        };
        let keys = parse_macro_text(&text);
        if keys.is_empty() { return; }
        self.last_macro = Some(reg);
        self.replay_depth += 1;
        for key in keys {
            if key == "RESIZE" || key.starts_with("PASTE\x00") { continue; }
            match self.mode {
                Mode::Normal => { self.handle_normal(&key); }
                Mode::Insert => { self.handle_insert(&key); }
                Mode::Visual | Mode::VisualLine | Mode::VisualBlock => { self.handle_visual(&key); }
                Mode::Command => {}
            }
        }
        self.replay_depth -= 1;
    }

    /// Swap the current line with the one above. One compound undo node.
    fn move_line_up(&mut self) {
        if self.cur_line == 0 { return; }
        let total = self.buf.line_count();
        let a = self.cur_line - 1;
        let b = self.cur_line;
        let line_a = self.buf.line(a);
        let line_b = self.buf.line(b);
        let start = self.buf.line_byte_offset(a);
        let end = if b + 1 < total {
            self.buf.line_byte_offset(b + 1)
        } else {
            self.buf.rope.len_bytes()
        };
        let block_has_trailing_nl = {
            let cs = self.buf.rope.byte_to_char(start);
            let ce = self.buf.rope.byte_to_char(end);
            let span: String = self.buf.rope.slice(cs..ce).into();
            span.ends_with('\n')
        };
        let mut rep = String::with_capacity(line_a.len() + line_b.len() + 2);
        rep.push_str(&line_b);
        rep.push('\n');
        rep.push_str(&line_a);
        if block_has_trailing_nl { rep.push('\n'); }
        self.buf.begin_compound();
        self.buf.apply(start, end, &rep);
        self.buf.end_compound();
        self.cur_line -= 1;
        self.clamp_col_to_line();
    }

    /// Swap the current line with the one below. One compound undo node.
    fn move_line_down(&mut self) {
        let total = self.buf.line_count();
        if self.cur_line + 1 >= total { return; }
        let a = self.cur_line;
        let b = self.cur_line + 1;
        let line_a = self.buf.line(a);
        let line_b = self.buf.line(b);
        let start = self.buf.line_byte_offset(a);
        let end = if b + 1 < total {
            self.buf.line_byte_offset(b + 1)
        } else {
            self.buf.rope.len_bytes()
        };
        let block_has_trailing_nl = {
            let cs = self.buf.rope.byte_to_char(start);
            let ce = self.buf.rope.byte_to_char(end);
            let span: String = self.buf.rope.slice(cs..ce).into();
            span.ends_with('\n')
        };
        let mut rep = String::with_capacity(line_a.len() + line_b.len() + 2);
        rep.push_str(&line_b);
        rep.push('\n');
        rep.push_str(&line_a);
        if block_has_trailing_nl { rep.push('\n'); }
        self.buf.begin_compound();
        self.buf.apply(start, end, &rep);
        self.buf.end_compound();
        self.cur_line += 1;
        self.clamp_col_to_line();
    }

    /// Increment or decrement the first number / date at-or-after the
    /// cursor on the current line by `delta`. Recognises:
    ///   * ISO 8601 dates `YYYY-MM-DD` — adds/subtracts days, with
    ///     proper month-end and leap-year rollover via Julian-day
    ///     arithmetic.
    ///   * Decimal integers, with an optional leading minus that's
    ///     part of the literal (i.e. `-` is included only if the char
    ///     before it isn't alphanumeric).
    /// Cursor lands on the last char of the new value (vim semantics).
    /// Records dot-repeat so `.` repeats the increment.
    fn change_number(&mut self, delta: i64) {
        let line = self.buf.line(self.cur_line);
        let line_off = self.buf.line_byte_offset(self.cur_line);
        let bytes = line.as_bytes();
        let cur = self.cur_col.min(bytes.len());

        // ISO-date detection FIRST. The cursor can sit anywhere within
        // a YYYY-MM-DD span (last digit, dash, anywhere) — try every
        // candidate start position from `cur-9` up to and including
        // `cur` so we'll find the date no matter where the cursor
        // landed. Then a small forward scan in case the cursor is
        // just before a date on the same line.
        let try_date = |cs: usize| iso_date_match(&line, cs, delta);
        let mut date_hit: Option<(usize, String)> = None;
        let max_back = cur.min(9);
        for back in 0..=max_back {
            let cs = cur - back;
            if let Some(new) = try_date(cs) { date_hit = Some((cs, new)); break; }
        }
        if date_hit.is_none() {
            for fwd in 1.. {
                let cs = cur + fwd;
                if cs + 10 > bytes.len() { break; }
                if let Some(new) = try_date(cs) { date_hit = Some((cs, new)); break; }
            }
        }
        if let Some((cs, new)) = date_hit {
            let abs_start = line_off + cs;
            let abs_end   = line_off + cs + 10;
            self.buf.apply(abs_start, abs_end, &new);
            self.cur_col = cs + new.len() - 1;
            self.want_col = self.cur_col;
            self.last_change = Some(LastChange::SimpleAction {
                key: if delta >= 0 { "C-A".into() } else { "C-X".into() },
                count: delta.unsigned_abs() as usize,
                register: None,
            });
            return;
        }

        // No date — fall back to plain integer. Walk back through any
        // digit run we may already be inside, then forward to the
        // next digit (or `-` sign of a negative literal).
        let mut start = cur;
        while start > 0 && bytes[start - 1].is_ascii_digit() { start -= 1; }
        while start < bytes.len() {
            let b = bytes[start];
            if b.is_ascii_digit() { break; }
            if b == b'-' && start + 1 < bytes.len() && bytes[start + 1].is_ascii_digit()
                && (start == 0 || !bytes[start - 1].is_ascii_alphanumeric())
            { break; }
            start += 1;
        }
        if start >= bytes.len() { return; }

        // Plain integer.
        let neg = bytes[start] == b'-';
        let num_start = if neg { start + 1 } else { start };
        let mut end = num_start;
        while end < bytes.len() && bytes[end].is_ascii_digit() { end += 1; }
        if end == num_start { return; } // no digits
        let digits = &line[num_start..end];
        let parsed: i64 = match digits.parse::<i64>() {
            Ok(n) => if neg { -n } else { n },
            Err(_) => return,
        };
        let new_val = parsed.saturating_add(delta);
        // Preserve zero-padding width for non-negative inputs (vim
        // `nrformats` would only do this in `octal`/`hex` modes; we
        // do it for any leading zero so `001` increments to `002`).
        let pad = digits.len();
        let new_text = if !neg && digits.starts_with('0') && new_val >= 0 {
            format!("{:0width$}", new_val, width = pad)
        } else {
            new_val.to_string()
        };
        let abs_start = line_off + start;
        let abs_end   = line_off + end;
        self.buf.apply(abs_start, abs_end, &new_text);
        self.cur_col = start + new_text.len().saturating_sub(1);
        self.want_col = self.cur_col;
        self.last_change = Some(LastChange::SimpleAction {
            key: if delta >= 0 { "C-A".into() } else { "C-X".into() },
            count: delta.unsigned_abs() as usize,
            register: None,
        });
    }

    /// Copy the char at the cursor's character-column from line
    /// `cur_line + dir` (-1 = above, +1 = below) and insert it at the
    /// cursor. No-op when there's no source line (top/bottom of buf)
    /// or when the source line is too short to have a char at that
    /// column. Used by Insert-mode Ctrl-Y / Ctrl-E.
    fn copy_char_from(&mut self, dir: i32) {
        let src_idx: isize = self.cur_line as isize + dir as isize;
        if src_idx < 0 { return; }
        let src_idx = src_idx as usize;
        if src_idx >= self.buf.line_count() { return; }
        let cur_line = self.buf.line(self.cur_line);
        let chars_before = cur_line[..self.cur_col.min(cur_line.len())].chars().count();
        let src = self.buf.line(src_idx);
        let Some(ch) = src.chars().nth(chars_before) else { return };
        let s = ch.to_string();
        let off = self.cursor_byte();
        self.buf.apply(off, off, &s);
        self.cur_col += s.len();
        self.want_col = self.cur_col;
        if self.capturing_insert { self.captured_insert.push_str(&s); }
    }

    fn enter_insert(&mut self) {
        self.mode = Mode::Insert;
        self.capturing_insert = true;
        self.captured_insert.clear();
    }

    /// Re-query terminal dimensions and resize the three panes. Triggered by
    /// the `RESIZE` event from crust (SIGWINCH wrapper). Without this scribe
    /// keeps drawing to the old pane width and the host terminal physically
    /// truncates lines at the new edge → looks like wrap is broken.
    fn handle_resize(&mut self) {
        let (cols, rows) = Crust::terminal_size();
        self.cols = cols;
        self.rows = rows;
        Crust::clear_screen();
        self.apply_layout();
        // Keep cursor in view after resize.
        self.scroll = self.cur_line.saturating_sub((self.main_p.h as usize) / 2);
    }

    /// Recompute pane geometry from `cols` / `rows` / reading mode /
    /// reading_width. Called from handle_resize and from any toggle
    /// that changes the layout (`:read`, `:set readingwidth=`).
    fn apply_layout(&mut self) {
        let cols = self.cols;
        let rows = self.rows;
        let (main_x, main_w) = if self.reading_mode && self.reading_width > 0
            && (self.reading_width as u16) < cols
        {
            let w = self.reading_width as u16;
            let x = ((cols - w) / 2).max(1);
            (x, w)
        } else {
            (1, cols)
        };
        Crust::clear_screen();
        self.header = Pane::new(1, 1, cols, 1, 255, 236);
        self.header.wrap = false; self.header.scroll = false;
        self.main_p = Pane::new(main_x, 2, main_w, rows.saturating_sub(2), 231, 0);
        self.main_p.wrap = true;
        // Preserve record + history across layout recompute — recreating
        // the footer would otherwise blow them away on every `:read`
        // toggle, killing command-line Up/Down recall.
        let saved_history = std::mem::take(&mut self.footer.history);
        self.footer = Pane::new(1, rows, cols, 1, 255, 236);
        self.footer.wrap = false; self.footer.scroll = false;
        self.footer.record = true;
        self.footer.history = saved_history;
    }

    /// Inclusive line range of the paragraph the cursor is in.
    /// Paragraphs are separated by blank (whitespace-only) lines.
    /// Used by Limelight-style dimming.
    fn current_paragraph_bounds(&self) -> (usize, usize) {
        let total = self.buf.line_count();
        if total == 0 { return (0, 0); }
        let cur = self.cur_line.min(total - 1);
        let mut start = cur;
        while start > 0 {
            let prev = self.buf.line(start - 1);
            if prev.trim().is_empty() { break; }
            start -= 1;
        }
        let mut end = cur;
        while end + 1 < total {
            let next = self.buf.line(end + 1);
            if next.trim().is_empty() { break; }
            end += 1;
        }
        (start, end)
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
        if let Some(q) = self.try_keymap("visual", key) { return q; }
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
            ":" => { return self.run_command_prompt(); }
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
        let reg = self.pending.register;
        let combined_for_msg = combined.clone();
        match op {
            'y' => self.regs.yank(reg, combined, YankKind::Block),
            _   => self.regs.cut(reg, combined, YankKind::Block),
        }
        let verb = if op == 'y' { "yanked" } else if op == 'c' { "changed" } else { "deleted" };
        self.say_yank(verb, reg, YankKind::Block, &combined_for_msg);
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
        let msg_text = text.clone();
        let verb;
        match op {
            'd' => {
                self.regs.cut(reg_name, text, YankKind::Charwise);
                self.buf.apply(start, end, "");
                self.cursor_to_byte(start);
                verb = Some("deleted");
            }
            'c' => {
                self.regs.cut(reg_name, text, YankKind::Charwise);
                self.buf.apply(start, end, "");
                self.cursor_to_byte(start);
                self.enter_insert();
                verb = Some("changed");
            }
            'y' => {
                self.regs.yank(reg_name, text, YankKind::Charwise);
                verb = Some("yanked");
            }
            _ => { verb = None; }
        }
        if let Some(v) = verb {
            self.say_yank(v, reg_name, YankKind::Charwise, &msg_text);
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
        let msg_text = text.clone();
        match op {
            'd' => {
                self.regs.cut(reg_name, text, YankKind::Linewise);
                self.buf.apply(start, end, "");
                let new_line = from.min(self.buf.line_count().saturating_sub(1));
                self.cur_line = new_line;
                self.cur_col = 0;
                self.want_col = 0;
                self.say_yank("deleted", reg_name, YankKind::Linewise, &msg_text);
            }
            'c' => {
                self.regs.cut(reg_name, text, YankKind::Linewise);
                // Replace the lines with one empty line so we can insert into it.
                self.buf.apply(start, end, "\n");
                self.cur_line = from;
                self.cur_col = 0;
                self.want_col = 0;
                self.enter_insert();
                self.say_yank("changed", reg_name, YankKind::Linewise, &msg_text);
            }
            'y' => {
                self.regs.yank(reg_name, text, YankKind::Linewise);
                self.say_yank("yanked", reg_name, YankKind::Linewise, &msg_text);
            }
            'Q' => {
                // gq: paragraph reformat. Width 72 in email mode (RFC 5322
                // soft limit + room for `> ` indenting on reply); plain mode
                // also gets 72 — adjust later via :set textwidth.
                let width = 72usize;
                let reformatted = reformat_paragraphs(&text, width);
                if reformatted != text {
                    self.buf.apply(start, end, &reformatted);
                }
                self.cur_line = from;
                self.cur_col = 0;
                self.want_col = 0;
            }
            '>' | '<' => {
                let dir = if op == '>' { 1 } else { -1 };
                let kind = self.buf.kind.clone();
                let mut new_text = String::new();
                for raw in text.split_inclusive('\n') {
                    let (body, nl) = match raw.strip_suffix('\n') {
                        Some(s) => (s, "\n"),
                        None    => (raw, ""),
                    };
                    let shifted = if dir > 0 {
                        shift_right(body, &kind)
                    } else {
                        shift_left(body, &kind)
                    };
                    new_text.push_str(&shifted);
                    new_text.push_str(nl);
                }
                if new_text != text {
                    self.buf.apply(start, end, &new_text);
                }
                // Land cursor at first non-whitespace col of first line, like
                // vim's `>>` behavior.
                self.cur_line = from;
                let new_first = self.buf.line(from);
                let col = new_first.find(|c: char| !c.is_whitespace()).unwrap_or(0);
                self.cur_col = col;
                self.want_col = col;
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
        let reg = self.pending.register;
        let msg_text = text.clone();
        self.regs.yank(reg, text, YankKind::Linewise);
        self.say_yank("yanked", reg, YankKind::Linewise, &msg_text);
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
        if let Some(q) = self.try_keymap("insert", key) { return q; }
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
                // Recheck spelling on Insert→Normal: cheap for typical mail
                // bodies, gives instant feedback the moment the user pauses.
                if self.spell_enabled { self.recheck_spell(); }
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
            // Vim Insert-mode helpers:
            // Ctrl-Y inserts the char from the SAME column on the line
            // ABOVE; Ctrl-E inserts the char from the line BELOW. The
            // column is character-based (not byte) so multi-byte text
            // (æ, ø, å, emoji) lines up the way the user sees it.
            "C-Y" => { self.copy_char_from(-1); }
            "C-E" => { self.copy_char_from( 1); }
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
                        // Auto-wrap on space when the line outgrew tw.
                        // Only fires when the user just typed a space —
                        // breaking on every char would cut words.
                        if self.textwidth > 0 && c == ' ' {
                            self.auto_wrap();
                        }
                    }
                }
            }
        }
        false
    }

    /// If the current line exceeds `textwidth` characters, replace
    /// the last whitespace at-or-before the cursor with a newline so
    /// typing continues on the next line. Adjusts cursor position.
    /// Skips when no suitable break point exists (e.g. one giant
    /// word) so we never mid-word break. Records nothing in the dot
    /// register — the user kept typing; this is presentation glue.
    fn auto_wrap(&mut self) {
        let line = self.buf.line(self.cur_line);
        if line.chars().count() <= self.textwidth { return; }
        let bytes = line.as_bytes();
        let cur = self.cur_col.min(bytes.len());
        // Walk back from just-before-cursor to find a whitespace
        // that's not the very first non-whitespace position
        // (preserves leading indent on the wrapped line).
        let leading_ws = bytes.iter().take_while(|b| **b == b' ' || **b == b'\t').count();
        let mut break_at: Option<usize> = None;
        let mut i = cur;
        while i > leading_ws {
            i -= 1;
            if bytes[i] == b' ' || bytes[i] == b'\t' {
                break_at = Some(i);
                break;
            }
        }
        let Some(b) = break_at else { return };
        let line_off = self.buf.line_byte_offset(self.cur_line);
        let abs = line_off + b;
        self.buf.apply(abs, abs + 1, "\n");
        // Cursor was at byte `cur` of the original line (cur > b
        // since we walked back from cur). After replacing one byte
        // with '\n', the cursor is on the new line at column
        // `cur - b - 1`.
        let new_col = cur.saturating_sub(b + 1);
        self.cur_line += 1;
        self.cur_col = new_col;
        self.want_col = self.cur_col;
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
                            if matches!(op, 'Q' | '>' | '<') {
                                let (l1, _) = self.buf.byte_to_line_col(from);
                                let (l2, _) = self.buf.byte_to_line_col(target_byte);
                                let (lo, hi) = if l1 <= l2 { (l1, l2) } else { (l2, l1) };
                                let extra = hi - lo;
                                self.execute_op_linewise(lo, extra);
                            } else {
                                let (start, end) = if from <= target_byte { (from, target_byte) } else { (target_byte, from) };
                                self.execute_op_charwise(op, start, end);
                            }
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
                        if let Some((s, e)) = r {
                            if matches!(op, 'Q' | '>' | '<') {
                                let (l1, _) = self.buf.byte_to_line_col(s);
                                let (l2, _) = self.buf.byte_to_line_col(e.saturating_sub(1).max(s));
                                let (lo, hi) = if l1 <= l2 { (l1, l2) } else { (l2, l1) };
                                let extra = hi - lo;
                                self.execute_op_linewise(lo, extra);
                            } else {
                                self.execute_op_charwise(op, s, e);
                            }
                        }
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
    /// Pointer-style command prompt — match pointer's flow exactly:
    ///   let cmd = pane.ask_with_bg(":", "", BG);
    ///   render the pane back to its normal state;
    ///   run the command.
    /// No mid-ask render_all and no position_cursor calls — crust's
    /// editline owns the cursor for the duration of the prompt.
    fn run_command_prompt(&mut self) -> bool {
        let cmd = self.footer.ask_with_bg(":", "", 17);
        // Ask painted the prompt line; restore the regular status bar.
        // Editline already turned the cursor off on exit (see editline's
        // tail `cursor::Hide`). render_all below will reposition + show.
        self.render_footer();
        let quit = self.execute_command(cmd.trim());
        if !quit { self.render_all(); }
        quit
    }

    /// Footer-line text prompt. Returns the entered string (empty on
    /// ESC). Used by the config popup for value-entry slots (spell
    /// language, color number, theme name).
    fn footer_prompt(&mut self, label: &str) -> String {
        let s = self.footer.ask_with_bg(label, "", 17);
        self.render_footer();
        s
    }

    /// Modal config popup, modelled on pointer's `show_config`. Cycle
    /// through a fixed key set, mutate the in-session state, optionally
    /// write the rcfile back. ESC / `q` close. The footer-line prompt
    /// helper is shared with the rest of the editor so it inherits all
    /// the editline niceties (history, cursor position, ESC-restore).
    fn show_config_popup(&mut self) {
        let themes = highlight::available_themes();
        let popup_w = 60u16;
        let popup_h = 18u16;
        let mut popup = Popup::centered(popup_w, popup_h, 252, 236);

        loop {
            let theme_idx = themes.iter().position(|t| **t == self.theme_name).unwrap_or(0);
            let on  = |b: bool| if b { style::fg("on",  35)  } else { style::fg("off", 196) };
            let val = |s: &str| style::fg(s, 81);
            let key = |k: &str| style::fg(k, 220);

            let mut lines: Vec<String> = Vec::new();
            lines.push(String::new());
            lines.push(format!("  {}", style::bold("Preferences")));
            lines.push(format!("  {}", style::fg(&"-".repeat(popup_w as usize - 4), 238)));
            lines.push(format!("  {}  Theme:        {}",  key("t"), val(themes[theme_idx])));
            lines.push(format!("  {}  Number col:   {}",  key("n"), on(self.show_numbers)));
            lines.push(format!("  {}  Relative no:  {}",  key("r"), on(self.relative_numbers)));
            lines.push(String::new());
            lines.push(format!("  {}  Spell:        {}",  key("s"), on(self.spell_enabled)));
            lines.push(format!("  {}  Spell lang:   {}",  key("l"), val(&self.spell_lang)));
            let mc = highlight::miss_color();
            lines.push(format!("  {}  Spell color:  {} {}",
                key("c"),
                val(&format!("{}", mc)),
                style::fg("\u{2588}\u{2588}\u{2588}", mc)));
            lines.push(String::new());
            lines.push(format!("  {}", style::fg(&"-".repeat(popup_w as usize - 4), 238)));
            lines.push(format!("  {}  Save to scriberc       {}  Close",
                key("W"), key("ESC")));

            popup.show(&lines.join("\n"));

            let Some(k) = Input::getchr(None) else { break };
            match k.as_str() {
                "ESC" | "q" => break,
                "t" => {
                    let next = (theme_idx + 1) % themes.len();
                    self.theme_name = themes[next].to_string();
                    highlight::set_theme(themes[next]);
                }
                "n" => {
                    self.show_numbers = !self.show_numbers;
                    if !self.show_numbers { self.relative_numbers = false; }
                }
                "r" => {
                    self.relative_numbers = !self.relative_numbers;
                    if self.relative_numbers { self.show_numbers = true; }
                }
                "s" => {
                    if self.spell_enabled {
                        self.spell_disable();
                        self.set_status(" spell off", 244);
                    } else {
                        self.spell_enable();
                        if self.spell_enabled {
                            self.set_status(
                                &format!(" spell on ({} words flagged)", self.misspellings.len()),
                                46);
                        }
                    }
                }
                "l" => {
                    let s = self.footer_prompt(&format!("spell lang [{}]: ", self.spell_lang));
                    let s = s.trim();
                    if !s.is_empty() { self.spell_set_lang(s); }
                }
                "c" => {
                    let s = self.footer_prompt(&format!("spell color 0-255 [{}]: ", highlight::miss_color()));
                    if let Ok(v) = s.trim().parse::<u8>() {
                        highlight::set_miss_color(v);
                    }
                }
                "W" => self.save_scriberc(),
                _ => {}
            }
        }
        // Wipe the popup, repaint everything underneath.
        popup.dismiss(&mut [&mut self.header, &mut self.main_p, &mut self.footer]);
        self.render_all();
    }

    /// Write current preferences back to `~/.config/scribe/scriberc`.
    /// Preserves comments and unknown keys from the existing file by
    /// rewriting only the keys we manage; everything else is left as-is.
    fn save_scriberc(&mut self) {
        let path = scriberc_path();
        let existing = std::fs::read_to_string(&path).unwrap_or_default();
        let managed = ["theme", "number", "relativenumber", "spell", "lang", "spellcolor"];
        let mut out = String::new();
        for line in existing.lines() {
            let stripped = line.split('#').next().unwrap_or("").trim();
            if let Some((k, _)) = stripped.split_once('=') {
                if managed.iter().any(|m| *m == k.trim()) { continue; }
            }
            out.push_str(line);
            out.push('\n');
        }
        out.push_str(&format!("theme = {}\n", self.theme_name));
        out.push_str(&format!("number = {}\n", self.show_numbers));
        out.push_str(&format!("relativenumber = {}\n", self.relative_numbers));
        out.push_str(&format!("spell = {}\n", self.spell_enabled));
        out.push_str(&format!("lang = {}\n", self.spell_lang));
        out.push_str(&format!("spellcolor = {}\n", highlight::miss_color()));
        if let Some(dir) = path.parent() { let _ = std::fs::create_dir_all(dir); }
        match std::fs::write(&path, out) {
            Ok(_)  => self.set_status(" scriberc saved", 46),
            Err(e) => self.set_status(&format!(" scriberc save failed: {}", e), 196),
        }
    }

    /// Open the bundled README as an in-memory help buffer. The text is
    /// embedded at compile time (`include_str!`) so `:help` works
    /// without filesystem access. Buffer has no path → `:w` would
    /// fail safely; users searching for terms use `/`, `n`, `N` as
    /// usual. Close with `q` (clean — no save warning) to return.
    fn open_help(&mut self) {
        const HELP: &str = include_str!("../README.md");
        if self.buf.dirty {
            self.set_status(" save current buffer first (or Q to discard)", 196);
            return;
        }
        self.buf = Buffer::from_str(HELP, FileKind::Source("md".into()));
        self.cur_line = 0;
        self.cur_col = 0;
        self.scroll = 0;
        self.want_col = 0;
        self.set_status(" :help — / to search, q to close, :e <file> to return", 244);
    }

    /// `:reg` popup — list contents of all named registers (and the
    /// unnamed / last-yank slots) so the user can inspect macros and
    /// yanks. Read-only: scrolls if the list is taller than the popup.
    /// Preview text is escaped (`<Esc>`, `<CR>`, …) so macros stay
    /// human-readable; long content is truncated at 60 chars.
    fn show_reg_popup(&mut self) {
        let popup_w = 70u16;
        let popup_h = 22u16;
        let mut popup = Popup::centered(popup_w, popup_h, 252, 236);

        let mut keys: Vec<char> = vec!['"', '0'];
        for c in 'a'..='z' { keys.push(c); }
        for c in '1'..='9' { keys.push(c); }

        let mut lines: Vec<String> = Vec::new();
        lines.push(String::new());
        lines.push(format!("  {}", style::bold("Registers")));
        lines.push(format!("  {}", style::fg(&"-".repeat(popup_w as usize - 4), 238)));
        for k in keys {
            let entry = self.regs.get(k);
            let (kind_lbl, preview) = match entry {
                None => continue, // skip empty slots so the popup isn't a sea of blanks
                Some(y) => {
                    let kind = match y.kind {
                        YankKind::Charwise => "c",
                        YankKind::Linewise => "l",
                        YankKind::Block    => "b",
                    };
                    // Show literal newlines as `\n` so the line stays one row.
                    let mut p = y.text.replace('\n', "\\n");
                    if p.chars().count() > 60 {
                        p = p.chars().take(60).collect::<String>() + "…";
                    }
                    (kind, p)
                }
            };
            lines.push(format!("  {}  [{}]  {}",
                style::fg(&format!(r#""{}"#, k), 220),
                style::fg(kind_lbl, 244),
                style::fg(&preview, 81)));
        }
        if lines.len() == 3 {
            lines.push(format!("  {}", style::fg("(no registers set)", 244)));
        }
        lines.push(String::new());
        lines.push(format!("  {}", style::fg(&"-".repeat(popup_w as usize - 4), 238)));
        lines.push(format!("  {}  Close", style::fg("ESC", 220)));

        popup.show(&lines.join("\n"));
        loop {
            let Some(k) = Input::getchr(None) else { break };
            match k.as_str() {
                "ESC" | "q" => break,
                "j" | "DOWN"   => { popup.pane.ix = popup.pane.ix.saturating_add(1); popup.pane.refresh(); }
                "k" | "UP"     => { popup.pane.ix = popup.pane.ix.saturating_sub(1); popup.pane.refresh(); }
                "PgDOWN" | " " => popup.pane.pagedown(),
                "PgUP"         => popup.pane.pageup(),
                "g" | "HOME"   => popup.pane.top(),
                "G" | "END"    => popup.pane.bottom(),
                _ => {}
            }
        }
        popup.dismiss(&mut [&mut self.header, &mut self.main_p, &mut self.footer]);
        self.render_all();
    }

    /// Legacy keystroke-by-keystroke command handler — no longer reached
    /// (Mode::Command is never set in v0.1.14+; `:` calls
    /// `run_command_prompt` directly). Kept only because the main loop's
    /// match still has a `Mode::Command` arm; removing the variant is a
    /// follow-up cleanup.
    #[allow(dead_code)]
    fn handle_command(&mut self, _key: &str) -> bool { false }

    /// Returns true to quit the editor.
    fn execute_command(&mut self, cmd: &str) -> bool {
        match cmd {
            "w" | "W" => {
                match self.buf.save() {
                    Ok(_)  => self.set_status(" written", 46),
                    Err(e) => self.set_status(&format!(" save failed: {}", e), 196),
                }
                false
            }
            "q"  => { if self.buf.dirty { self.set_status(" unsaved changes (use :q! to force)", 196); false } else { true } }
            "q!" => true,
            // `:Wq` / `:WQ` / `:wQ` accepted as aliases for `:wq` so a
            // sticky shift key (or muscle memory) doesn't bounce the user
            // out with "unknown: Wq".
            "wq" | "Wq" | "wQ" | "WQ" | "x" | "X" => {
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
            "set spell" => {
                self.spell_enable();
                if self.spell_enabled {
                    self.set_status(&format!(" spell on ({} words)", self.misspellings.len()), 244);
                }
                false
            }
            "set nospell" => {
                self.spell_disable();
                self.set_status(" spell off", 244);
                false
            }
            "help" | "h" => {
                self.open_help();
                false
            }
            "config" | "Config" => {
                self.show_config_popup();
                false
            }
            "reg" | "registers" | "display" => {
                self.show_reg_popup();
                false
            }
            "read" | "reading" => {
                self.reading_mode = true;
                self.apply_layout();
                self.set_status(" reading mode (use :noread to exit)", 244);
                false
            }
            "noread" | "noreading" => {
                self.reading_mode = false;
                self.apply_layout();
                self.set_status(" reading mode off", 244);
                false
            }
            other if other.starts_with("set readingwidth") || other.starts_with("set rw") => {
                let key = if other.starts_with("set readingwidth") { "set readingwidth" } else { "set rw" };
                let v = other.trim_start_matches(key).trim_start_matches('=').trim();
                if v.is_empty() {
                    self.set_status(&format!(" readingwidth={} (0=full)", self.reading_width), 244);
                } else if let Ok(n) = v.parse::<usize>() {
                    self.reading_width = n;
                    self.apply_layout();
                    self.set_status(&format!(" readingwidth={}", n), 244);
                } else {
                    self.set_status(" usage: :set readingwidth=N", 196);
                }
                false
            }
            other if matches!(other, "set paragraphdim" | "set pdim")
                  || other.starts_with("set paragraphdim=") || other.starts_with("set pdim=")
                  || matches!(other, "set noparagraphdim" | "set nopdim") =>
            {
                let val = if other == "set noparagraphdim" || other == "set nopdim" {
                    false
                } else if let Some(v) = other.split('=').nth(1) {
                    matches!(v.trim(), "true" | "1" | "yes" | "on")
                } else {
                    true
                };
                self.paragraph_dim = val;
                self.set_status(if val { " paragraph dim on" } else { " paragraph dim off" }, 244);
                false
            }
            other if other.starts_with("set textwidth") || other.starts_with("set tw") => {
                let key = if other.starts_with("set textwidth") { "set textwidth" } else { "set tw" };
                let v = other.trim_start_matches(key).trim_start_matches('=').trim();
                if v.is_empty() {
                    self.set_status(&format!(" textwidth={} (0=off)", self.textwidth), 244);
                } else if let Ok(n) = v.parse::<usize>() {
                    self.textwidth = n;
                    self.set_status(&format!(" textwidth={}", n), 244);
                } else {
                    self.set_status(" usage: :set textwidth=N", 196);
                }
                false
            }
            other if other.starts_with("set spelllang") || other.starts_with("set lang") => {
                let key = if other.starts_with("set spelllang") { "set spelllang" } else { "set lang" };
                let name = other.trim_start_matches(key)
                    .trim_start_matches('=')
                    .trim();
                if name.is_empty() {
                    self.set_status(
                        &format!(" spell lang: {} | :set spelllang=NAME (e.g. nb_NO, nn_NO, en_US)", self.spell_lang),
                        244);
                } else {
                    self.spell_set_lang(name);
                }
                false
            }
            "set number" | "set nu" => {
                self.show_numbers = true;
                false
            }
            "set nonumber" | "set nonu" => {
                self.show_numbers = false;
                self.relative_numbers = false;
                false
            }
            "set relativenumber" | "set rnu" => {
                self.show_numbers = true;
                self.relative_numbers = true;
                false
            }
            "set norelativenumber" | "set nornu" => {
                self.relative_numbers = false;
                false
            }
            other if other.starts_with("set syntax") => {
                // `:set syntax=NAME` — override the buffer's detected file
                // kind so the renderer treats the content as `NAME`.
                // Useful after `:claude` has rewritten code into prose, or
                // when scribe guessed Plain for an unrecognised extension.
                let name = other.trim_start_matches("set syntax")
                    .trim_start_matches('=')
                    .trim();
                if name.is_empty() {
                    let label = match &self.buf.kind {
                        FileKind::Plain => "plain".to_string(),
                        FileKind::Email => "email".to_string(),
                        FileKind::Source(s) => s.clone(),
                    };
                    self.set_status(&format!(" syntax: {} | use :set syntax=NAME", label), 244);
                } else {
                    match name {
                        "plain" | "text" | "txt" | "none" => {
                            self.buf.kind = FileKind::Plain;
                            self.set_status(" syntax: plain", 244);
                        }
                        "email" | "mail" | "eml" => {
                            self.buf.kind = FileKind::Email;
                            self.set_status(" syntax: email", 244);
                        }
                        n => {
                            if highlight::lang_known(n).is_some() {
                                self.buf.kind = FileKind::Source(n.to_string());
                                self.set_status(&format!(" syntax: {}", n), 244);
                            } else {
                                self.set_status(&format!(" unknown syntax: {}", n), 196);
                            }
                        }
                    }
                }
                false
            }
            other if other.starts_with("set theme") => {
                let name = other.trim_start_matches("set theme")
                    .trim_start_matches('=')
                    .trim();
                if name.is_empty() {
                    let avail = highlight::available_themes().join(", ");
                    self.set_status(&format!(" theme: {} | available: {}", self.theme_name, avail), 244);
                } else {
                    highlight::set_theme(name);
                    self.theme_name = name.to_string();
                    self.set_status(&format!(" theme: {}", name), 244);
                }
                false
            }
            other if other.starts_with('s') || other.starts_with("%s") => {
                // :s/PAT/REPL/[FLAGS]   — current line
                // :%s/PAT/REPL/[FLAGS]  — whole buffer
                // Falls through to "unknown" for plain `:s` without args.
                let whole_buffer = other.starts_with("%s");
                let rest = if whole_buffer { &other[2..] } else { &other[1..] };
                if rest.starts_with('/') {
                    self.execute_substitute(whole_buffer, rest);
                    false
                } else {
                    self.set_status(&format!(" unknown: {}", other), 196);
                    false
                }
            }
            other if other == "claude" || other.starts_with("claude ") => {
                let raw_prompt = if other == "claude" { "" } else { other[7..].trim() };
                self.run_claude_command(raw_prompt);
                false
            }
            "chat" => { self.run_chat_session(); false }
            other => {
                self.set_status(&format!(" unknown: {}", other), 196);
                false
            }
        }
    }

    /// `:s/PAT/REPL/[FLAGS]` (current line) or `:%s/...` (whole buffer).
    /// Flags: `g` = global within line, `i` = case-insensitive. The first
    /// `/` after `s` / `%s` opens the pattern; the next two `/` close
    /// pattern + replacement. Trailing flags optional.
    ///
    /// All edits are wrapped in a compound undo node so a buffer-wide
    /// substitute undoes as one atomic step regardless of how many lines
    /// it changed.
    fn execute_substitute(&mut self, whole_buffer: bool, rest: &str) {
        debug_assert!(rest.starts_with('/'));
        let parts: Vec<&str> = rest[1..].splitn(3, '/').collect();
        if parts.len() < 2 {
            self.set_status(" :s expects /PATTERN/REPLACEMENT/[FLAGS]", 196);
            return;
        }
        let pattern = parts[0];
        let replacement = parts[1];
        let flags = parts.get(2).copied().unwrap_or("");
        let global = flags.contains('g');
        let case_insensitive = flags.contains('i');

        let mut re_str = String::new();
        if case_insensitive { re_str.push_str("(?i)"); }
        re_str.push_str(pattern);
        let re = match regex::Regex::new(&re_str) {
            Ok(r) => r,
            Err(e) => {
                self.set_status(&format!(" bad pattern: {}", e), 196);
                return;
            }
        };

        self.buf.begin_compound();
        let mut count = 0usize;
        if whole_buffer {
            // Iterate bottom-up so each apply leaves earlier line offsets
            // intact (lines above the current one are untouched).
            let total = self.buf.line_count();
            for line_idx in (0..total).rev() {
                count += self.substitute_on_line(line_idx, &re, replacement, global);
            }
        } else {
            count += self.substitute_on_line(self.cur_line, &re, replacement, global);
        }
        self.buf.end_compound();

        if count == 0 {
            self.set_status(&format!(" pattern not found: {}", pattern), 196);
        } else {
            let scope = if whole_buffer { "buffer" } else { "line" };
            self.set_status(&format!(" {} substitution(s) on {}", count, scope), 46);
        }
    }

    fn substitute_on_line(
        &mut self,
        line_idx: usize,
        re: &regex::Regex,
        replacement: &str,
        global: bool,
    ) -> usize {
        let line = self.buf.line(line_idx);
        if line.is_empty() { return 0; }
        let count = if global {
            re.find_iter(&line).count()
        } else if re.is_match(&line) { 1 } else { 0 };
        if count == 0 { return 0; }
        let new_line = if global {
            re.replace_all(&line, replacement).into_owned()
        } else {
            re.replace(&line, replacement).into_owned()
        };
        let line_start = self.buf.line_byte_offset(line_idx);
        let line_end = line_start + line.len();
        self.buf.apply(line_start, line_end, &new_line);
        count
    }

    // ── Claude integration ────────────────────────────────────────────
    /// `:claude {prompt}` — pipe a slice of the buffer through `claude -p`
    /// and splice the response back in.
    ///
    /// Input scope (deliberately conservative — whole-buffer replacement
    /// requires an explicit selection like `ggVG`):
    ///   * `:claude` (no args) or `:claude continue` — input is the buffer
    ///     up to the cursor; the response is INSERTED at the cursor (no
    ///     replacement). Use this to extend a draft.
    ///   * Otherwise, if a Visual selection is active when `:` was pressed,
    ///     input is the selection and the response REPLACES it.
    ///   * Otherwise, input is the CURRENT PARAGRAPH (text-object `ap`)
    ///     and the response replaces just that paragraph. To rewrite the
    ///     whole buffer, select it explicitly first: `ggVG:claude …`.
    ///
    /// Verb shortcuts (the second word after `:claude`):
    ///   * `grammar`  — fix grammar/punctuation, preserve meaning + tone
    ///   * `tighten`  — make it more concise
    ///   * `plain`    — rewrite in plainer English
    ///   * anything else is sent verbatim as the prompt
    ///
    /// All edits go through one `begin_compound` / `end_compound` pair so
    /// a single `u` reverses the entire Claude turn.
    fn run_claude_command(&mut self, raw_prompt: &str) {
        let (input_start, input_end, input_text, replace) = self.claude_input(raw_prompt);
        let prompt = self.claude_prompt(raw_prompt);

        // Show "asking…" status now and force a footer paint so the user
        // sees something while claude -p runs (which can take 5-30s).
        self.set_status(" asking claude…", 244);
        self.render_footer();
        use std::io::Write as _;
        let _ = std::io::stdout().flush();

        match claude_run(&prompt, &input_text) {
            Ok(response) => {
                // Normalise trailing newlines: trim everything claude
                // appended, then add exactly one back IF the input we
                // replaced ended with `\n`. Otherwise the response's
                // last line concatenates with the next byte after the
                // replaced range — typically the `\n` of a trailing
                // blank line, which gets consumed and the blank line
                // disappears. Tightening "para1\npara2\n…" then losing
                // the blank between paragraphs was the v0.1.18 bug.
                let response = response.trim_end_matches('\n').to_string();
                if response.is_empty() {
                    self.set_status(" claude returned empty response", 196);
                    return;
                }
                let response = if input_text.ends_with('\n') {
                    let mut s = response;
                    s.push('\n');
                    s
                } else {
                    response
                };
                self.buf.begin_compound();
                if replace {
                    self.buf.apply(input_start, input_end, &response);
                    self.cursor_to_byte(input_start + response.len());
                } else {
                    let cur = self.cursor_byte();
                    self.buf.apply(cur, cur, &response);
                    self.cursor_to_byte(cur + response.len());
                }
                self.buf.end_compound();
                self.set_status(&format!(" claude: {} chars  (u to undo)", response.len()), 46);
            }
            Err(e) => self.set_status(&format!(" claude: {}", e), 196),
        }
        // Exit visual after running so the user lands back in Normal with
        // the cursor at the end of the inserted/replaced region.
        if self.mode.is_visual() { self.mode = Mode::Normal; }
    }

    /// Resolve the input range + text + replace-vs-insert flag for the
    /// `:claude {raw_prompt}` invocation. See `run_claude_command` doc for
    /// the rules.
    fn claude_input(&self, raw_prompt: &str) -> (usize, usize, String, bool) {
        // continue / empty → insert at cursor, input = preceding buffer.
        if raw_prompt.is_empty() || raw_prompt == "continue" {
            let cur = self.cursor_byte();
            let text = self.buf.rope.byte_slice(0..cur).to_string();
            return (cur, cur, text, false);
        }
        // Visual selection wins when active.
        if self.mode.is_visual() {
            let cur = self.cursor_byte();
            let (lo, hi) = if cur < self.visual_anchor {
                (cur, self.visual_anchor)
            } else {
                (self.visual_anchor, cur)
            };
            let s = self.buf.rope.to_string();
            let mut hi2 = hi;
            if hi2 < s.len() {
                let mut p = hi2 + 1;
                while p < s.len() && !s.is_char_boundary(p) { p += 1; }
                hi2 = p;
            }
            let text = self.buf.rope.byte_slice(lo..hi2).to_string();
            return (lo, hi2, text, true);
        }
        // No selection: use the current paragraph (text-object `ap`).
        // Whole-buffer replacement requires an explicit selection
        // (`ggVG`) so a stray `:claude rewrite` can't silently destroy
        // the whole file.
        let cur = self.cursor_byte();
        if let Some((lo, hi)) = textobj::around_paragraph(&self.buf, cur) {
            let text = self.buf.rope.byte_slice(lo..hi).to_string();
            return (lo, hi, text, true);
        }
        // Empty buffer / cursor on a blank-only line: insert at cursor.
        (cur, cur, String::new(), false)
    }

    /// Map verb shortcuts to a longer prompt; passthrough anything else.
    /// All shortcut prompts end with "Output only the …" so claude -p
    /// returns a clean drop-in replacement (no preamble, no trailing
    /// commentary).
    fn claude_prompt(&self, raw: &str) -> String {
        match raw {
            "" | "continue" => "Continue the text naturally from where it leaves off. Output only the continuation, no preamble.".to_string(),
            "grammar"       => "Fix grammar, spelling, and punctuation. Preserve meaning, tone, and voice. Output only the corrected text.".to_string(),
            "tighten"       => "Rewrite to be more concise. Preserve meaning and voice. Output only the rewritten text.".to_string(),
            "plain"         => "Rewrite in plainer English. Preserve meaning. Output only the rewritten text.".to_string(),
            _ => raw.to_string(),
        }
    }

    /// `:chat` — suspend scribe and open an interactive Claude Code
    /// session in the same terminal. The current buffer content is
    /// snapshotted to a tempfile and the path is included in the initial
    /// message so claude can read it on demand. When the user exits the
    /// chat (`/exit` etc.), scribe regains the terminal and the buffer is
    /// untouched.
    fn run_chat_session(&mut self) {
        // Snapshot the live buffer (including unsaved changes) to a
        // tempfile so claude can read it during the chat.
        let pid = std::process::id();
        let tmpfile = format!("/tmp/scribe-chat-{}.txt", pid);
        let mut content = String::new();
        for chunk in self.buf.rope.chunks() { content.push_str(chunk); }
        let _ = std::fs::write(&tmpfile, &content);

        let path_label = self.buf.path.as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "[no name]".into());
        let initial = format!(
            "I'm editing {} in scribe. The current buffer content (including unsaved edits) is in {}. \
            Help me work with this text — feel free to read the snapshot when you need to. \
            When you're done, /exit returns me to the editor.",
            path_label, tmpfile
        );

        // Release the terminal so claude has full keyboard control.
        // Bracketed-paste mode would interfere with claude's own input
        // handling, so disable it for the duration.
        use std::io::Write as _;
        print!("\x1b[?2004l");
        let _ = std::io::stdout().flush();
        Crust::cleanup();
        Crust::clear_screen();

        let _ = std::process::Command::new("claude")
            .arg(&initial)
            .status();

        // Restore scribe's terminal state and force a full repaint.
        Crust::init();
        Crust::set_app_identity("Scribe");
        print!("\x1b[?2004h");
        let _ = std::io::stdout().flush();
        let _ = std::fs::remove_file(&tmpfile);
        self.handle_resize();
        self.set_status(" back from chat", 244);
    }
}

/// Spawn `claude -p PROMPT` with `input` on stdin, return the captured
/// stdout on success or a short error message on failure. Synchronous —
/// blocks until claude exits.
fn claude_run(prompt: &str, input: &str) -> Result<String, String> {
    use std::io::Write as _;
    use std::process::{Command, Stdio};
    let mut child = Command::new("claude")
        .args(["-p", prompt])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => "binary not on PATH".to_string(),
            _ => format!("spawn: {}", e),
        })?;
    if !input.is_empty() {
        if let Some(stdin) = child.stdin.as_mut() {
            stdin.write_all(input.as_bytes())
                .map_err(|e| format!("stdin write: {}", e))?;
        }
    }
    drop(child.stdin.take());
    let output = child.wait_with_output()
        .map_err(|e| format!("wait: {}", e))?;
    if !output.status.success() {
        let err = String::from_utf8_lossy(&output.stderr);
        let snippet = err.lines().next().unwrap_or("(no message)");
        return Err(snippet.chars().take(80).collect());
    }
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

// EmailLineStyle, line_style_email, find_sig_start, InlineToken,
// inline_tokens, emit_email_line are now provided by the `highlight`
// crate and re-exported via `use highlight::*` at the top of this file.
// See highlight/src/email.rs for the implementations.

/// Detect a leading quote prefix on a line. Returns (prefix, body) where
/// prefix is the leading `>`+whitespace block (e.g. `> `, `> > `, `>>>`)
/// and body is the rest. Used for `gq` reformat (preserves prefix per line)
/// and `>>` / `<<` indent-as-quote in email mode.
fn split_quote_prefix(line: &str) -> (String, &str) {
    let bytes = line.as_bytes();
    let mut i = 0;
    let mut last_gt = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'>' => { i += 1; last_gt = i; }
            b' ' | b'\t' => { i += 1; }
            _ => break,
        }
    }
    if last_gt == 0 { return (String::new(), line); }
    // Include trailing single space after the final `>` if present.
    let mut end = last_gt;
    if end < bytes.len() && bytes[end] == b' ' { end += 1; }
    (line[..end].to_string(), &line[end..])
}

/// Rewrap text into paragraphs at `width`. A paragraph = consecutive non-blank
/// lines sharing the same quote prefix. Blank lines separate paragraphs and
/// are preserved verbatim. Each paragraph is joined and rewrapped at
/// (width - prefix_len) so quoted reply levels stay readable.
fn reformat_paragraphs(text: &str, width: usize) -> String {
    let trailing_nl = text.ends_with('\n');
    let body = if trailing_nl { &text[..text.len()-1] } else { text };
    let lines: Vec<&str> = body.split('\n').collect();

    let mut out = String::new();
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        if line.trim().is_empty() {
            out.push_str(line);
            out.push('\n');
            i += 1;
            continue;
        }
        let (prefix, _) = split_quote_prefix(line);
        // Greedy collect: lines with same prefix and non-empty body form one paragraph.
        let mut joined = String::new();
        let mut j = i;
        while j < lines.len() {
            let l = lines[j];
            if l.trim().is_empty() { break; }
            let (p, b) = split_quote_prefix(l);
            if p != prefix { break; }
            if !joined.is_empty() { joined.push(' '); }
            joined.push_str(b.trim());
            j += 1;
        }
        let body_width = width.saturating_sub(prefix.chars().count()).max(20);
        let mut current = String::new();
        for w in joined.split_whitespace() {
            if current.is_empty() {
                current.push_str(w);
            } else if current.chars().count() + 1 + w.chars().count() <= body_width {
                current.push(' ');
                current.push_str(w);
            } else {
                out.push_str(&prefix);
                out.push_str(&current);
                out.push('\n');
                current = w.to_string();
            }
        }
        if !current.is_empty() {
            out.push_str(&prefix);
            out.push_str(&current);
            out.push('\n');
        }
        i = j;
    }
    if !trailing_nl && out.ends_with('\n') { out.pop(); }
    out
}

/// `>>` shift-right one line. In email mode adds a `> ` quote level; in plain
/// mode prepends a tab.
fn shift_right(line: &str, kind: &FileKind) -> String {
    if matches!(kind, FileKind::Email) {
        // Add one quote level. Empty lines also get `>` so quoted blank lines
        // stay visually attached to their paragraph.
        if line.is_empty() { return ">".to_string(); }
        format!("> {}", line)
    } else {
        format!("\t{}", line)
    }
}

/// `<<` shift-left one line. In email mode strips one `> ` (or bare `>`); in
/// plain mode strips one leading tab or up to 4 leading spaces. Returns the
/// line unchanged if there's nothing to strip.
fn shift_left(line: &str, kind: &FileKind) -> String {
    if matches!(kind, FileKind::Email) {
        if let Some(rest) = line.strip_prefix("> ") { return rest.to_string(); }
        if let Some(rest) = line.strip_prefix(">")  { return rest.to_string(); }
        line.to_string()
    } else {
        if let Some(rest) = line.strip_prefix('\t') { return rest.to_string(); }
        let n_spaces = line.bytes().take(4).take_while(|&b| b == b' ').count();
        if n_spaces > 0 { return line[n_spaces..].to_string(); }
        line.to_string()
    }
}
