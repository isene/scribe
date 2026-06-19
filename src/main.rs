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
mod calendar;
mod digraphs;
mod emoji_data;
mod export;
mod fold;
mod mode;
mod motion;
mod picker;
mod register;
mod search;
mod spell;
mod textobj;

use buffer::{Buffer, FileKind};
use crust::{Crust, Cursor, Input, Pane, Popup};
use crust::style;
use mode::Mode;
use register::{Registers, Yank, YankKind};
use search::{Direction, SearchState};
use std::path::PathBuf;

const VERSION: &str = env!("CARGO_PKG_VERSION");

fn main() {
    // CLI: scribe [+N] [--col N] [--insert] [--theme NAME] [--no-spell] [path]
    // `+N` opens the file with the cursor on line N (vim convention; used
    // by kastrup's compose flow to jump straight to the message body).
    // `--col N` puts the cursor on column N of that line (1-indexed,
    // counted in chars). `--insert` boots straight into Insert mode so an
    // embedder (kastrup `m`) can land the user one keystroke from typing
    // the recipient.
    // `--theme NAME` overrides the rcfile theme for this session only.
    let args: Vec<String> = std::env::args().collect();
    let mut start_line: Option<usize> = None;
    let mut start_col: Option<usize> = None;
    let mut start_insert = false;
    let mut path: Option<PathBuf> = None;
    let mut cli_theme: Option<String> = None;
    let mut no_spell = false;
    let mut export_fmt: Option<String> = None;
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
        } else if arg == "--col" && i + 1 < args.len() {
            if let Ok(n) = args[i + 1].parse::<usize>() { start_col = Some(n); }
            i += 2;
        } else if let Some(rest) = arg.strip_prefix("--col=") {
            if let Ok(n) = rest.parse::<usize>() { start_col = Some(n); }
            i += 1;
        } else if arg == "--insert" {
            start_insert = true;
            i += 1;
        } else if arg == "--export" && i + 1 < args.len() {
            export_fmt = Some(args[i + 1].clone());
            i += 2;
        } else if let Some(rest) = arg.strip_prefix("--export=") {
            export_fmt = Some(rest.to_string());
            i += 1;
        } else if arg == "--pdf" {
            export_fmt = Some("pdf".to_string());
            i += 1;
        } else if arg == "--no-spell" {
            // Skip the auto-enable-on-Email branch in App::new. Used
            // when an embedder (kastrup compose) wants the editor up
            // fast and is happy to type `:set spell` manually if
            // they want it later.
            no_spell = true;
            i += 1;
        } else if path.is_none() {
            path = Some(PathBuf::from(arg));
            i += 1;
        } else {
            i += 1;
        }
    }
    // Headless export: `scribe --export FMT FILE` / `scribe --pdf FILE`
    // renders and exits without the TUI. Used for scripting and tests.
    if let Some(fmt) = export_fmt {
        let Some(p) = path.clone() else {
            eprintln!("--export/--pdf needs a file argument");
            std::process::exit(2);
        };
        let text = std::fs::read_to_string(&p).unwrap_or_default();
        let title = p.file_stem().and_then(|s| s.to_str()).unwrap_or("HyperList").to_string();
        let (rendered, ext): (String, &str) = match fmt.as_str() {
            "html" | "h" => (export::to_html(&text, &title), "html"),
            "latex" | "tex" | "l" => (export::to_latex(&text, &title), "tex"),
            "markdown" | "md" | "m" => (export::to_markdown(&text, &title), "md"),
            "pdf" | "p" => {
                let target = p.with_extension("pdf");
                match export::latex_to_pdf(&export::to_latex(&text, &title), &target) {
                    Ok(_)  => { println!("exported → {}", target.display()); std::process::exit(0); }
                    Err(e) => { eprintln!("pdf export failed: {}", e); std::process::exit(1); }
                }
            }
            other => { eprintln!("unknown export format: {}", other); std::process::exit(2); }
        };
        let target = p.with_extension(ext);
        match std::fs::write(&target, rendered) {
            Ok(_)  => { println!("exported → {}", target.display()); std::process::exit(0); }
            Err(e) => { eprintln!("export failed: {}", e); std::process::exit(1); }
        }
    }
    install_panic_hook();
    // Detect HL dotfile encryption. Don't pre-decrypt — defer to AFTER
    // Crust::init so the password prompt happens in the editor's
    // statusbar (showing the file path in the header) and the user
    // gets up to 3 attempts with feedback.
    let encrypted_path: Option<PathBuf> = path.as_ref()
        .filter(|p| buffer::is_encrypted_dotfile(p) && p.exists())
        .cloned();
    let app_path = if encrypted_path.is_some() { None } else { path.clone() };

    Crust::init();
    Crust::set_app_identity("Scribe");
    use std::io::Write;
    print!("\x1b[?2004h");
    let _ = std::io::stdout().flush();

    let mut app = App::new(app_path, cli_theme, no_spell);
    // For encrypted files, attach the path to the empty buffer so the
    // header shows it, then prompt for the password in the footer.
    if let Some(ref p) = encrypted_path {
        app.buf.path = Some(p.clone());
    }
    app.render_all();
    if let Some(p) = encrypted_path.clone() {
        let cipher = std::fs::read_to_string(&p).unwrap_or_default();
        if !cipher.is_empty() {
            let mut decrypted = false;
            for attempt in 0..3 {
                let label = if attempt == 0 {
                    "Password: ".to_string()
                } else {
                    format!("Wrong — try again ({}/3): ", attempt + 1)
                };
                app.footer.secret = true;
                let pw = app.footer.ask_with_bg(&label, "", 17);
                app.footer.secret = false;
                if pw.is_empty() {
                    // ESC at the prompt → quit cleanly without
                    // leaking the editor on a half-loaded buffer.
                    save_cmd_history(&app.footer.history);
                    Crust::cleanup();
                    Crust::clear_screen();
                    return;
                }
                match buffer::decrypt(&cipher, &pw) {
                    Ok(plain) => {
                        let openssl_fmt = buffer::is_openssl_blob(&cipher);
                        app.buf = buffer::Buffer::from_decrypted(p.clone(), plain, pw, openssl_fmt);
                        // Defensive: force HL kind when the path's
                        // extension says HL. `detect_kind` already
                        // does this, but if the path got mangled
                        // (e.g. some opener renames the tempfile)
                        // we still want HL syntax + tabstop=3.
                        if let Some(ext) = p.extension().and_then(|e| e.to_str()) {
                            let lower = ext.to_lowercase();
                            if lower == "hl" || lower == "woim" {
                                app.buf.kind = buffer::FileKind::Source(lower);
                            }
                        }
                        app.cur_line = 0;
                        app.cur_col = 0;
                        app.scroll = 0;
                        app.want_col = 0;
                        // Encrypted files are typically password
                        // managers / secrets — collapse every fold
                        // so opening doesn't flash the contents on
                        // screen. User opens the branch they need
                        // with <SPACE>.
                        let total = app.buf.line_count();
                        let all: Vec<String> = (0..total).map(|i| app.buf.line(i)).collect();
                        app.folds.set_level(0, &all);
                        app.set_status(" decrypted (folds collapsed)", 46);
                        app.render_all();
                        decrypted = true;
                        break;
                    }
                    Err(_) => {
                        app.set_status(&format!(" wrong password ({} of 3)", attempt + 1), 196);
                        app.render_all();
                    }
                }
            }
            if !decrypted {
                app.set_status(" too many failed attempts — quitting", 196);
                app.render_all();
                save_cmd_history(&app.footer.history);
                Crust::cleanup();
                Crust::clear_screen();
                eprintln!("scribe: too many failed password attempts for {}", p.display());
                return;
            }
        }
    }
    if let Some(n) = start_line {
        if n > 0 {
            let last = app.buf.line_count().saturating_sub(1);
            app.cur_line = (n - 1).min(last);
            app.cur_col = 0;
            app.want_col = 0;
        }
    }
    if let Some(c) = start_col {
        if c > 0 {
            // `cur_col` is a byte offset; treat the requested column as
            // a 1-indexed char position and map it to the byte boundary.
            // Bound by the current line length (clamp, don't error).
            let line = app.buf.line(app.cur_line);
            let target_chars = c - 1;
            let byte_off = line.char_indices().nth(target_chars).map(|(b, _)| b)
                .unwrap_or(line.len());
            app.cur_col = byte_off;
            app.want_col = byte_off;
        }
    }
    if start_insert {
        app.mode = Mode::Insert;
    }
    app.render_all();

    loop {
        let Some(key) = Input::getchr(None) else { continue };
        // External-change check. Runs on every keystroke (one stat()
        // per key — sub-microsecond). When another writer touched the
        // file (kastrup triage appending to a hyperlist, git checkout,
        // etc.) and our buffer is clean, silently reload so the user
        // sees the appended lines immediately. When the buffer is
        // dirty, surface a status warning so they don't accidentally
        // overwrite the external change on next :w.
        if app.buf.external_changed() {
            if !app.buf.dirty {
                if app.buf.reload().is_ok() {
                    // Clamp the cursor in case the file shrank
                    let lc = app.buf.line_count().saturating_sub(1).max(0);
                    if app.cur_line > lc { app.cur_line = lc; }
                    app.clamp_col_to_line();
                    app.set_status("File reloaded (changed on disk)", 2);
                }
            } else {
                app.set_status(
                    "External change detected — buffer dirty. :e to reload, :w to overwrite",
                    3);
            }
        }
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
            Mode::Replace     => app.handle_replace(&key),
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

#[derive(Clone, Copy)]
enum CaseOp { Lower, Upper, Toggle }

/// Pending Visual Block change → Insert replication state. Captured
/// when a block `c`/`s` enters Insert; consumed on the next ESC to
/// copy the top-line insertion onto the rest of the block's lines.
struct BlockInsert {
    /// Display column where the insert started (block left edge).
    vcol: usize,
    /// Lines below the top line that should receive the replicated
    /// text (the top line gets it via the live Insert keystrokes).
    lines: Vec<usize>,
}

/// Resolve an input-layer key string to the single literal char it
/// represents, for `r` (replace) and `f/F/t/T` (find) targets. crust
/// delivers Tab/Enter/Shift-Tab as words, so a naïve
/// `key.chars().next()` would grab the first letter ('T'/'E'). Named
/// keys with no literal form (LEFT, UP, F1, …) return None → caller
/// cancels the pending operation.
/// Jump-class motions that should record the pre-move position in
/// the `'` mark (so `''` / `` `` `` returns there). Matches vim's
/// jumplist-setting motions. Line/word/char motions are deliberately
/// excluded.
fn is_jump_motion(key: &str) -> bool {
    matches!(key,
        "G" | "gg" | "n" | "N"
        | "{" | "}" | "(" | ")" | "%"
        | "H" | "M" | "L" | "[[" | "]]")
}

fn key_to_literal_char(key: &str) -> Option<char> {
    match key {
        " " | "SPACE" => Some(' '),
        "TAB" | "S-TAB" => Some('\t'),
        "ENTER" | "\n" | "\r" | "C-M" | "C-J" => Some('\n'),
        _ => {
            let mut it = key.chars();
            let c = it.next()?;
            if it.next().is_none() && !c.is_control() { Some(c) } else { None }
        }
    }
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
    /// VisualBlock state: anchor + current display columns. Tracked
    /// separately from `visual_anchor_col` / `cur_col` (byte offsets)
    /// so the block stays a perfect rectangle through tabs and across
    /// lines of unequal length — vim's behaviour. h/l shift the cursor
    /// vcol by exactly one display column; j/k change the row, vcol
    /// stays put. Past-line-end cells render as styled blanks.
    vblock_anchor_vcol: usize,
    vblock_cur_vcol: usize,
    /// Set when a Visual Block change (`c`/`s`/`C`) enters Insert mode.
    /// On the next ESC the text typed on the top line is replicated to
    /// the remaining block lines at the same column (vim block-insert).
    block_insert: Option<BlockInsert>,
    /// True while a Visual-mode `r` awaits its replacement char.
    visual_replace_pending: bool,
    /// Last completed change, for `.` repeat.
    last_change: Option<LastChange>,
    /// While true we're capturing keystrokes typed in Insert mode after a
    /// change-op for dot-repeat replay.
    capturing_insert: bool,
    captured_insert: String,
    /// Insert-mode `Ctrl-R` register prefix — when set, the next key is
    /// the register name to paste from (or `=` to evaluate an
    /// arithmetic expression and insert the result).
    insert_reg_prefix: bool,
    /// `z` prefix: next key is a spell action (`z=`, `zg`).
    z_prefix: bool,
    /// `]` or `[` prefix: next key is a bracket motion (`]s`, `[s`, …).
    bracket_prefix: Option<char>,
    /// `\` (vim leader) prefix: next key is a leader-mapped command
    /// (`\v` checkbox, `\h` highlight, `\0`..`\9` fold level, …).
    leader_prefix: bool,
    /// Two-key leader sequence in progress: `\e?` (encryption) and
    /// `\x?` (exports) read a second char before dispatching.
    leader_sub: Option<char>,
    /// `\p` toggle — when on, `UP`/`DOWN` arrows behave like
    /// `g<up>`/`g<down>` (presentation_step). Session-only.
    presentation: bool,
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
    /// Insert-mode text expansions (Vim-style `:abbrev`). Trigger →
    /// expansion. Fires when the user types a non-abbrev character
    /// (space, punctuation, enter) right after the trigger. Loaded
    /// from `~/.config/scribe/abbreviations`; runtime edits via
    /// `:ab trigger expansion` / `:una trigger`.
    abbrev: std::collections::HashMap<String, String>,
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
    /// Indent-based folds (HyperList + any indented format). State is
    /// per-buffer; replaced when `:e <other>` swaps the buffer.
    folds: fold::Folds,
    /// `\an` / `\#` autonumber toggle. When on, Insert-mode `<CR>`
    /// auto-increments the next item on the same level; `Ctrl-T` /
    /// `Ctrl-D` indent / outdent with renumbering.
    autonumber: bool,
    /// Show/Hide pattern from `zs` / `zh` / `:show` / `:hide`. Folds
    /// every line that doesn't match (show) or matches (hide). None
    /// disables the filter and reverts to indent folding.
    showhide: Option<(String, bool)>,
    /// State / Transition underline mode cycle: 0=none, 1=state, 2=transition.
    st_underline: u8,
    /// `g:calendar` equivalent — destination for `\G`.
    calendar: Option<String>,
    alldates: bool,
    /// `\M` markup toggle — when true, inline colour/font `<span>` tags are
    /// concealed on every line except the cursor's, so styled prose reads clean
    /// while the markup stays editable where the cursor sits.
    markup_concealed: bool,
}

impl App {
    fn new(path: Option<PathBuf>, cli_theme: Option<String>, suppress_spell: bool) -> Self {
        let (cols, rows) = Crust::terminal_size();
        let mut header = Pane::new(1, 1, cols, 1, 255, 236);
        header.wrap = false; header.scroll = false;
        let mut main_p = Pane::new(1, 2, cols, rows.saturating_sub(2), 231, 0);
        main_p.wrap = true;
        // word_wrap stays true (the default) — prose-friendly
        // line-breaks at spaces. `position_cursor` replays the
        // word-wrap algorithm via `wrap_pos()` so the cursor lands
        // on the actual rendered character, not the column that a
        // naive `visual_col / pane_w` would give.
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

        let auto_spell = !suppress_spell && (matches!(buf.kind, FileKind::Email) || rc.spell);
        let mut app = Self {
            buf, mode: Mode::Normal,
            cur_line: 0, cur_col: 0, want_col: 0, scroll: 0,
            cmdline: String::new(), status: None,
            cols, rows, header, main_p, footer,
            pending: Pending::default(),
            regs: Registers::load(),
            search: SearchState::new(),
            visual_anchor: 0,
            block_insert: None,
            visual_replace_pending: false,
            visual_anchor_line: 0,
            visual_anchor_col: 0,
            vblock_anchor_vcol: 0,
            vblock_cur_vcol: 0,
            last_change: None,
            capturing_insert: false,
            captured_insert: String::new(),
            insert_reg_prefix: false,
            z_prefix: false,
            bracket_prefix: None,
            leader_prefix: false,
            leader_sub: None,
            presentation: false,
            spell: None,
            spell_enabled: false,
            spell_lang: rc.spell_lang.clone().unwrap_or_else(|| "en_US".into()),
            misspellings: Vec::new(),
            abbrev: load_abbreviations(),
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
            folds: fold::Folds::new(),
            autonumber: false,
            showhide: None,
            st_underline: 0,
            calendar: rc.calendar.clone(),
            alldates: rc.alldates,
            markup_concealed: false,
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
            // Hunspell dict load is the slowest step (300+ ms for
            // nb_NO). Paint a status BEFORE the spawn so the user
            // sees what's happening instead of a frozen UI.
            self.set_status(&format!(" loading spell dict ({})…", self.spell_lang), 244);
            self.render_footer();
            std::io::Write::flush(&mut std::io::stdout()).ok();
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

    /// `zN` / `zE` / `zO`: enable spell with a specific language, or
    /// disable. Single keystroke for the common cases (Norwegian and
    /// English) without going through `:set spelllang=…`.
    fn quick_spell(&mut self, lang: &str) {
        if self.spell_lang != lang {
            // Drop any process running with the wrong dict.
            self.spell = None;
            self.spell_lang = lang.to_string();
        }
        self.spell_enable();
        if self.spell_enabled {
            self.set_status(
                &format!(" spell on ({}) — {} flagged", self.spell_lang, self.misspellings.len()),
                46);
        }
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

/// `~/.config/scribe/abbreviations` — Vim-style abbreviation map for
/// insert-mode text expansion. Format: one entry per line, trigger
/// and expansion separated by a TAB. Lines starting with `#` are
/// comments. Example:
///   -g\t/Geir
///   mvh\tMed vennlig hilsen
fn abbrev_path() -> std::path::PathBuf {
    let home = std::env::var_os("HOME").map(std::path::PathBuf::from).unwrap_or_default();
    home.join(".config/scribe/abbreviations")
}

fn load_abbreviations() -> std::collections::HashMap<String, String> {
    let path = abbrev_path();
    let mut map = std::collections::HashMap::new();
    if let Ok(text) = std::fs::read_to_string(&path) {
        for line in text.lines() {
            let trimmed = line.trim_start();
            if trimmed.is_empty() || trimmed.starts_with('#') { continue; }
            if let Some((trigger, expansion)) = line.split_once('\t') {
                let t = trigger.trim();
                if !t.is_empty() {
                    map.insert(t.to_string(), expansion.to_string());
                }
            }
        }
    }
    map
}

fn save_abbreviations(map: &std::collections::HashMap<String, String>) {
    let path = abbrev_path();
    if let Some(parent) = path.parent() { let _ = std::fs::create_dir_all(parent); }
    let mut entries: Vec<(&String, &String)> = map.iter().collect();
    entries.sort_by_key(|(k, _)| k.as_str());
    let mut out = String::from("# scribe insert-mode abbreviations — trigger<TAB>expansion\n");
    for (k, v) in entries { out.push_str(&format!("{}\t{}\n", k, v)); }
    let _ = std::fs::write(&path, out);
}

/// True if `c` may be part of an abbreviation TRIGGER (e.g. `-g`,
/// `mvh`, `omg2`). Anything else (whitespace, punctuation other than
/// `-` / `_`, etc.) is a boundary that fires expansion.
fn is_abbrev_char(c: char) -> bool {
    c.is_alphanumeric() || c == '-' || c == '_'
}

// ── Tiny arithmetic evaluator (used by the `=` register) ──────────
// Recursive-descent over `+ - * / ( )` and unary `-`/`+`, decimal
// numbers (`1`, `2.5`, `.5`). Returns `None` on any malformed input,
// division by zero, or trailing junk.

fn eval_math(input: &str) -> Option<f64> {
    let bytes = input.as_bytes();
    let mut pos = 0;
    let v = parse_expr(bytes, &mut pos)?;
    skip_ws(bytes, &mut pos);
    if pos != bytes.len() { return None; }
    if !v.is_finite() { return None; }
    Some(v)
}

fn skip_ws(b: &[u8], p: &mut usize) {
    while *p < b.len() && b[*p].is_ascii_whitespace() { *p += 1; }
}

fn parse_expr(b: &[u8], p: &mut usize) -> Option<f64> {
    let mut acc = parse_term(b, p)?;
    loop {
        skip_ws(b, p);
        match b.get(*p).copied() {
            Some(c @ (b'+' | b'-')) => {
                *p += 1;
                let r = parse_term(b, p)?;
                acc = if c == b'+' { acc + r } else { acc - r };
            }
            _ => return Some(acc),
        }
    }
}

fn parse_term(b: &[u8], p: &mut usize) -> Option<f64> {
    let mut acc = parse_factor(b, p)?;
    loop {
        skip_ws(b, p);
        match b.get(*p).copied() {
            Some(c @ (b'*' | b'/' | b'%')) => {
                *p += 1;
                let r = parse_factor(b, p)?;
                acc = match c {
                    b'*' => acc * r,
                    b'/' => { if r == 0.0 { return None; } acc / r }
                    _    => { if r == 0.0 { return None; } acc % r }
                };
            }
            _ => return Some(acc),
        }
    }
}

fn parse_factor(b: &[u8], p: &mut usize) -> Option<f64> {
    skip_ws(b, p);
    match b.get(*p).copied() {
        Some(b'-') => { *p += 1; Some(-parse_factor(b, p)?) }
        Some(b'+') => { *p += 1; parse_factor(b, p) }
        Some(b'(') => {
            *p += 1;
            let v = parse_expr(b, p)?;
            skip_ws(b, p);
            if b.get(*p).copied() != Some(b')') { return None; }
            *p += 1;
            Some(v)
        }
        Some(c) if c.is_ascii_digit() || c == b'.' => {
            let start = *p;
            while *p < b.len() && (b[*p].is_ascii_digit() || b[*p] == b'.') {
                *p += 1;
            }
            std::str::from_utf8(&b[start..*p]).ok()?.parse().ok()
        }
        _ => None,
    }
}

/// Format an `eval_math` result for insertion: integer if it lands on
/// a whole number under 15 digits, else a fixed-point decimal with
/// trailing zeros (and trailing `.`) trimmed.
fn fmt_math(v: f64) -> String {
    if v == v.trunc() && v.abs() < 1e15 {
        format!("{}", v as i64)
    } else {
        let mut s = format!("{:.10}", v);
        while s.ends_with('0') { s.pop(); }
        if s.ends_with('.') { s.pop(); }
        s
    }
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
    /// `calendar = email@example.com` — destination calendar for `\G`.
    calendar: Option<String>,
    /// `alldates = true` — include past events too.
    alldates: bool,
}

/// Prompt on the controlling tty for a password with echo disabled.
/// Defers to `stty -echo` since pulling in `termios` for one call is
/// excessive. Re-enables echo on return regardless of outcome.
fn read_password_tty(prompt: &str) -> std::io::Result<String> {
    use std::io::{BufRead, Write};
    let _ = std::process::Command::new("stty").arg("-echo").status();
    print!("{}", prompt);
    let _ = std::io::stdout().flush();
    let mut pw = String::new();
    let stdin = std::io::stdin();
    stdin.lock().read_line(&mut pw)?;
    let _ = std::process::Command::new("stty").arg("echo").status();
    println!();
    Ok(pw.trim_end_matches(|c| c == '\n' || c == '\r').to_string())
}

/// Parse the leading number prefix of an HL item (after indentation).
/// Returns (path_prefix, last_index, has_trailing_period).
/// Examples:
///   "1.2.3 foo" → ("1.2.", 3, false)
///   "1.2.3. foo" → ("1.2.", 3, true)
///   "5 foo"     → ("", 5, false)
///   "5. foo"    → ("", 5, true)
///   "foo"       → None
/// The character occupying display column `target` in `line` (TAB-aware):
/// the char that starts at `target`, a space if `target` lands inside a
/// TAB's span, or None past end-of-line.
fn char_at_display_col(line: &str, target: usize, ts: usize) -> Option<char> {
    let mut d = 0usize;
    for ch in line.chars() {
        if d == target { return Some(ch); }
        let w = if ch == '\t' { ts - (d % ts) } else { 1 };
        if d < target && target < d + w { return Some(' '); }
        d += w;
    }
    None
}

fn parse_number_prefix(s: &str) -> Option<(String, u64, bool)> {
    // Pattern: <digits>(.<digits>)*\.?<space>
    let bytes = s.as_bytes();
    let mut i = 0;
    if i >= bytes.len() || !bytes[i].is_ascii_digit() { return None; }
    while i < bytes.len() && (bytes[i].is_ascii_digit() || bytes[i] == b'.') { i += 1; }
    let head = &s[..i];
    // Optional trailing period inside `head` separates path from idx.
    let trailing_period = head.ends_with('.');
    let head_no_dot = if trailing_period { &head[..head.len()-1] } else { head };
    let last_dot = head_no_dot.rfind('.');
    let (prefix, idx_str) = match last_dot {
        Some(p) => (&head[..p+1], &head_no_dot[p+1..]),
        None    => ("", head_no_dot),
    };
    let idx: u64 = idx_str.parse().ok()?;
    Some((prefix.to_string(), idx, trailing_period))
}

/// Strip the leading number+space from an HL item's body (post-indent).
/// Returns (consumed_bytes, remainder). If no number, returns (0, s).
fn strip_number_prefix(s: &str) -> (usize, &str) {
    if parse_number_prefix(s).is_none() { return (0, s); }
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() && (bytes[i].is_ascii_digit() || bytes[i] == b'.') { i += 1; }
    while i < bytes.len() && bytes[i] == b' ' { i += 1; }
    (i, &s[i..])
}

/// Visual-column count for a `\t`-bearing line. Tabs advance to the
/// next multiple of `tabstop`; other chars count as 1. SGR escapes
/// are NOT expected in `s` here — call only with raw buffer text.
fn visual_width_of(s: &str, tabstop: usize) -> usize {
    let mut col = 0usize;
    for c in s.chars() {
        if c == '\t' { col += tabstop - (col % tabstop); }
        else { col += 1; }
    }
    col
}

/// Replay crust's word-wrap algorithm on a plain-text line to find
/// where byte offset `byte_col` lands after wrapping at `width`.
/// Returns `(row_in_line, col_in_row)`.
///
/// Crust's wrap is "delayed": chars are pushed onto the current row
/// optimistically, and only when one would overflow does the
/// algorithm look back for the last space and break there. This
/// means a char at byte N might TENTATIVELY be on row 0, then get
/// rolled to row 1 by a later wrap. Single-pass walk-and-peek
/// can't see that future, so we run the full simulation: build the
/// rows char-by-char, recording each rendered byte's final
/// `(row, col)` in a map, then look up `byte_col`.
///
/// `gutter_w` occupies the start of row 0 only. Tabs expand to
/// `tabstop` columns, recomputed per row.
/// Run crust's word-wrap simulation on `line` and return the rows
/// as `Vec<Vec<(char, byte_offset, col_in_row)>>`. Boundary spaces
/// dropped by the wrap algorithm are absent from the output.
/// Shared by `wrap_pos` (forward map) and `visual_to_byte` (inverse).
fn wrap_simulate(line: &str, width: usize, gutter_w: usize, tabstop: usize)
    -> Vec<Vec<(char, usize, usize)>>
{
    let char_w = |c: char, at_col: usize| -> usize {
        if c == '\t' { tabstop.saturating_sub(at_col % tabstop).max(1) }
        else { crust::cell_width(c).max(1) }
    };
    let mut rows: Vec<Vec<(char, usize, usize)>> = Vec::new();
    let mut current: Vec<(char, usize, usize)> = Vec::new();
    let mut col = gutter_w;
    let mut byte_pos = 0;
    if width == 0 {
        rows.push(current);
        return rows;
    }
    for c in line.chars() {
        let cw = char_w(c, col);
        if col + cw > width {
            let space_idx = current.iter().rposition(|&(ch, _, _)| ch == ' ');
            if let Some(sp) = space_idx {
                let tail: Vec<(char, usize, usize)> = current.drain(sp + 1..).collect();
                current.pop(); // drop boundary space
                rows.push(std::mem::take(&mut current));
                col = 0;
                for (tc, tb, _) in tail {
                    let tw = char_w(tc, col);
                    current.push((tc, tb, col));
                    col += tw;
                }
            } else {
                rows.push(std::mem::take(&mut current));
                col = 0;
            }
            let cw2 = char_w(c, col);
            current.push((c, byte_pos, col));
            col += cw2;
        } else {
            current.push((c, byte_pos, col));
            col += cw;
        }
        byte_pos += c.len_utf8();
    }
    rows.push(current);
    rows
}

/// Inverse of `wrap_pos`: given a target visual `(row_in_line,
/// col_in_row)`, return the byte offset in `line` whose char
/// renders at (or just past, if `target_col` is past row end) that
/// position. If `target_row` is past the last visual row, returns
/// `line.len()` (cursor past end).
fn visual_to_byte(line: &str, target_row: usize, target_col: usize, width: usize, gutter_w: usize, tabstop: usize) -> usize {
    let rows = wrap_simulate(line, width, gutter_w, tabstop);
    if target_row >= rows.len() { return line.len(); }
    let row = &rows[target_row];
    if row.is_empty() {
        // Empty row — cursor lands at start of next char, or line end.
        for r in rows.iter().skip(target_row + 1) {
            if let Some(&(_, b, _)) = r.first() { return b; }
        }
        return line.len();
    }
    // Walk the row picking the LAST char whose col_in_row <= target_col.
    let mut best = row[0].1;
    for &(_, b, cc) in row {
        if cc <= target_col { best = b; }
        else { return best; }
    }
    // target_col is past row end — return one-past-last byte.
    let &(c, b, _) = row.last().unwrap();
    b + c.len_utf8()
}

/// Number of visual rows `line` occupies when rendered at `width`
/// (with `gutter_w` on row 0 only).
fn wrap_row_count(line: &str, width: usize, gutter_w: usize, tabstop: usize) -> usize {
    let n = wrap_simulate(line, width, gutter_w, tabstop).len();
    n.max(1)
}

/// Display column of a byte position on `line`, treating tabs as
/// `tabstop`-aligned and every other char as 1 cell wide. Used by
/// VisualBlock so the rectangle stays uniform across tabs and lines
/// of unequal byte length. Pure ASCII / monospace assumption — same
/// as the rest of scribe's column accounting.
fn display_col(line: &str, byte_col: usize, tabstop: usize) -> usize {
    let mut col = 0usize;
    let mut byte = 0usize;
    for c in line.chars() {
        if byte >= byte_col { break; }
        col += if c == '\t' { tabstop - (col % tabstop) } else { 1 };
        byte += c.len_utf8();
    }
    col
}

/// Byte offset at (or just past) display column `target_col`. If
/// `target_col` is past the last char's right edge, returns
/// `line.len()`. If `target_col` falls inside a tab's expansion, the
/// returned byte points at that tab. The companion display column is
/// also returned so the caller can tell whether they hit the target
/// exactly or fell short (line too short, e.g. for cursor placement).
fn byte_at_or_past_col(line: &str, target_col: usize, tabstop: usize) -> (usize, usize) {
    let mut col = 0usize;
    let mut byte = 0usize;
    for c in line.chars() {
        if col >= target_col { return (byte, col); }
        let cw = if c == '\t' { tabstop - (col % tabstop) } else { 1 };
        if col + cw > target_col {
            // target lands inside this tab's expansion → snap to tab.
            return (byte, col);
        }
        col += cw;
        byte += c.len_utf8();
    }
    (byte, col)
}

fn wrap_pos(line: &str, byte_col: usize, width: usize, gutter_w: usize, tabstop: usize) -> (usize, usize) {
    if width == 0 { return (0, gutter_w); }
    // Per-cell width. Tabs expand to the next tabstop; otherwise use
    // crust::cell_width which mirrors glass's emoji-routing rules
    // (non-BMP codepoints render 2 cells, certain BMP emoji ranges
    // ditto). Without this, every emoji collapses to 1 cell here and
    // the cursor position math drifts left of where the glyph
    // actually finishes on screen.
    let char_w = |c: char, at_col: usize| -> usize {
        if c == '\t' { tabstop.saturating_sub(at_col % tabstop).max(1) }
        else { crust::cell_width(c).max(1) }
    };
    // Final position for each rendered byte. Boundary-dropped
    // spaces are absent from this map.
    let mut pos: std::collections::HashMap<usize, (usize, usize)> = std::collections::HashMap::new();
    // Chars currently on the working row. Each entry: (char, byte_offset, col_in_row).
    let mut current: Vec<(char, usize, usize)> = Vec::new();
    let mut row: usize = 0;
    let mut col: usize = gutter_w;
    let mut byte_pos: usize = 0;

    for c in line.chars() {
        let cw = char_w(c, col);
        if col + cw > width {
            // Overflow → break. Prefer last space; else hard break.
            let space_idx = current.iter().rposition(|&(ch, _, _)| ch == ' ');
            if let Some(sp) = space_idx {
                // Chars 0..sp finalise on current row.
                for i in 0..sp {
                    let (_, b, cc) = current[i];
                    pos.insert(b, (row, cc));
                }
                // The space at `sp` is dropped (no rendered position).
                // Tail moves to new row, re-laid from col 0.
                let tail: Vec<(char, usize, usize)> = current.drain(sp + 1..).collect();
                current.clear();
                row += 1;
                col = 0;
                for (tc, tb, _) in tail {
                    let tw = char_w(tc, col);
                    current.push((tc, tb, col));
                    col += tw;
                }
            } else {
                // No space: hard break at width.
                for &(_, b, cc) in &current { pos.insert(b, (row, cc)); }
                current.clear();
                row += 1;
                col = 0;
            }
            let cw2 = char_w(c, col);
            current.push((c, byte_pos, col));
            col += cw2;
        } else {
            current.push((c, byte_pos, col));
            col += cw;
        }
        byte_pos += c.len_utf8();
    }
    // Finalise the last row.
    for &(_, b, cc) in &current { pos.insert(b, (row, cc)); }

    // Direct hit: byte_col is the offset of a rendered char.
    if let Some(&p) = pos.get(&byte_col) { return p; }

    // Past end-of-line: cursor sits just past the last char.
    //
    // Wrap to (row + 1, 0) not just when last_col == width (cursor
    // genuinely off the edge) but ALSO when last_col == width - 1
    // (cursor would render in the cell immediately right of the last
    // char). The latter case visually merges with the last char in
    // terminals that draw the bar cursor at the LEFT edge of a cell —
    // glass and many others. Wrapping makes "I'm past-end" obvious.
    if byte_col >= line.len() {
        let last_col = current.last()
            .map(|&(c, _, cc)| cc + char_w(c, cc))
            .unwrap_or(col);
        if last_col + 1 >= width { return (row + 1, 0); }
        return (row, last_col);
    }

    // byte_col falls on a dropped boundary space. The cursor
    // between the last char of row N and the first char of row N+1
    // most naturally lives at the start of row N+1.
    let mut probe = byte_col + 1;
    while probe < line.len() {
        if let Some(&p) = pos.get(&probe) { return p; }
        probe += 1;
    }
    // Nothing further — return end of last row. Same one-cell slack
    // as the past-end branch above for the same bar-cursor-rendering
    // reason.
    let last_col = current.last()
        .map(|&(c, _, cc)| cc + char_w(c, cc))
        .unwrap_or(col);
    if last_col + 1 >= width { (row + 1, 0) } else { (row, last_col) }
}


/// Expand `\t` in a styled string (may contain `\e[...m` SGR sequences
/// and OSC-8 hyperlinks) to the right number of spaces, accounting
/// for visible-column tracking. SGR / OSC sequences pass through
/// untouched and don't advance the column.
fn expand_tabs_styled(s: &str, tabstop: usize) -> String {
    let mut out = String::with_capacity(s.len() + 16);
    let mut col = 0usize;
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            out.push(c);
            // CSI: ESC [ ... terminator ∈ @-~
            // OSC: ESC ] ... BEL or ESC \
            // For everything else (rare), just pass the next char.
            if let Some(next) = chars.next() {
                out.push(next);
                match next {
                    '[' => {
                        // CSI: copy through final byte (ASCII alpha or `~`).
                        for c2 in chars.by_ref() {
                            out.push(c2);
                            if (0x40..=0x7e).contains(&(c2 as u32)) { break; }
                        }
                    }
                    ']' => {
                        // OSC: copy through BEL or ESC \.
                        let mut prev_esc = false;
                        for c2 in chars.by_ref() {
                            out.push(c2);
                            if c2 == '\x07' { break; }
                            if prev_esc && c2 == '\\' { break; }
                            prev_esc = c2 == '\x1b';
                        }
                    }
                    _ => {}
                }
            }
            continue;
        }
        if c == '\t' {
            let spaces = tabstop - (col % tabstop);
            for _ in 0..spaces { out.push(' '); }
            col += spaces;
            continue;
        }
        if c == '\n' {
            out.push(c);
            col = 0;
            continue;
        }
        out.push(c);
        col += 1;
    }
    out
}

/// Find the `<…>` (or `<<…>>`) reference closest to byte offset
/// `from` on `line`. Algorithm:
///   1. If a reference brackets `from` (i.e. `<` is at or before
///      `from` and matching `>` is at or after `from`), return it.
///   2. Otherwise return the first reference at or after `from`.
///   3. Otherwise return the first reference on the line.
/// Returns the matched substring including the brackets.
/// Sniff a token for URL-ish prefixes that xdg-open can dispatch.
/// Conservative — only matches things that clearly aren't in-buffer
/// references.
/// Expand backslash escapes in a `:s/pat/rep/` replacement: `\t` →
/// `\t`, `\n` → `\n`, `\r` → `\r`, `\\` → `\`. Anything else after
/// `\` is passed through verbatim so the user can still write
/// regex-crate expansions like `\$1` if they really want a literal
/// `$1` in the output.
fn expand_replacement_escapes(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('t')  => out.push('\t'),
                Some('n')  => out.push('\n'),
                Some('r')  => out.push('\r'),
                Some('\\') => out.push('\\'),
                Some(other) => { out.push('\\'); out.push(other); }
                None => out.push('\\'),
            }
        } else {
            out.push(c);
        }
    }
    out
}

fn looks_like_url(s: &str) -> bool {
    s.starts_with("http://")
        || s.starts_with("https://")
        || s.starts_with("ftp://")
        || s.starts_with("mailto:")
        || s.starts_with("tel:")
        || s.starts_with("file://")
}

/// Sniff a token for filesystem-path-ish shape: starts with `/`, `~/`,
/// `./`, or `../`. Bare relative names are NOT treated as paths
/// because they collide too easily with prose; require a directory
/// separator or leading dot/tilde to be unambiguous.
fn looks_like_path(s: &str) -> bool {
    s.starts_with('/') || s.starts_with("~/") || s.starts_with("./") || s.starts_with("../")
}

fn find_reference(line: &str, from: usize) -> Option<String> {
    let bytes = line.as_bytes();
    let n = bytes.len();
    let from = from.min(n);
    // Collect all <...> spans on the line.
    let mut spans: Vec<(usize, usize)> = Vec::new();
    let mut i = 0;
    while i < n {
        if bytes[i] == b'<' {
            let start = i;
            let mut j = i + 1;
            if j < n && bytes[j] == b'<' { j += 1; }
            while j < n && bytes[j] != b'>' { j += 1; }
            if j < n {
                let mut end = j + 1;
                if end < n && bytes[end] == b'>' { end += 1; }
                if end - start >= 3 { spans.push((start, end)); }
                i = end;
                continue;
            }
        }
        i += 1;
    }
    if spans.is_empty() { return None; }
    // 1. enclosing
    if let Some(&(s, e)) = spans.iter().find(|&&(s, e)| s <= from && from < e) {
        return Some(line[s..e].to_string());
    }
    // 2. first at or after `from`
    if let Some(&(s, e)) = spans.iter().find(|&&(s, _)| s >= from) {
        return Some(line[s..e].to_string());
    }
    // 3. first on the line
    let (s, e) = spans[0];
    Some(line[s..e].to_string())
}

/// `YYYY-MM-DD HH.MM` for HL checkbox timestamps. Local time. No
/// dependency: divmod from Unix epoch + tz offset from libc.
/// Colon-mode tab completer. Returns every command whose name starts
/// with the typed prefix (case-sensitive). Empty prefix shows all
/// available commands so Tab on a blank `:` cycles the menu. The
/// list mirrors `App::execute_command` — keep them in sync when
/// adding a new command.
fn complete_colon_command(prefix: &str) -> Vec<String> {
    const COMMANDS: &[&str] = &[
        // Picker / glyph entry
        "digraphs", "dig", "emoji",
        // Save / quit
        "w", "wq", "x", "q",
        // Reload
        "e", "edit", "e!", "edit!",
        // Help / keys
        "help", "keys", "keybindings", "cheat",
        // AI
        "claude", "chat",
        // Email / draft handoff
        "mail", "email", "eml",
        // Display / mode
        "config", "spell", "reading", "noreading",
        "plain", "text", "display",
        // Registers / abbrevs / maps
        "registers", "reg", "abbrev", "abclear",
        "maps", "mappings", "map",
    ];
    let p = prefix.trim_start_matches(':');  // be forgiving if user typed colon
    let mut out: Vec<String> = COMMANDS.iter()
        .filter(|c| c.starts_with(p))
        .map(|c| c.to_string())
        .collect();
    out.sort();
    out
}

fn current_timestamp() -> String {
    use std::process::Command;
    // Fallback: shell out to `date` for correct local time including DST.
    if let Ok(out) = Command::new("date").arg("+%Y-%m-%d %H.%M").output() {
        if let Ok(s) = String::from_utf8(out.stdout) {
            return s.trim().to_string();
        }
    }
    "0000-00-00 00.00".into()
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
            "calendar" => if !v.is_empty() { cfg.calendar = Some(v.to_string()); },
            "alldates" => cfg.alldates = truthy(v),
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
        let gutter_w = gutter_width(self.buf.line_count(), self.show_numbers && !self.reading_mode);
        let ts = self.tabstop();

        // Walk visible lines from scroll to cur_line, skipping any
        // line hidden by a closed fold. Without this skip, after
        // `zs`/`zh` collapses most of the buffer the cursor's visual
        // row points off-screen.
        let total = self.buf.line_count();
        let all: Vec<String> = (0..total).map(|i| self.buf.line(i)).collect();
        let mut visual_row: usize = 0;
        // Use `wrap_row_count` (NOT `wrap_pos` at line.len()) for the
        // row tally: `wrap_pos` normalises the past-last-char cursor
        // position to `(row+1, 0)` when the line fills the pane
        // exactly, which is correct for cursor placement but
        // overcounts the row by 1 for the row-count purpose. The
        // dedicated counter walks the same wrap simulation and just
        // reports the number of rows that actually got rendered.
        for ln in self.scroll..self.cur_line {
            if ln >= total { break; }
            if !self.folds.is_visible(ln, &all) { continue; }
            visual_row += wrap_row_count(&all[ln], pane_w, gutter_w, ts);
        }
        let line = self.buf.line(self.cur_line);
        let target_byte = self.cur_col.min(self.current_line_len());
        let (mut row_in_line, mut col_in_row) = wrap_pos(&line, target_byte, pane_w, gutter_w, ts);

        // VisualBlock: the cursor's screen column is the moving corner
        // of the rectangle, which can sit past line-end or inside a
        // tab. Override row/col with the vblock vcol on this line.
        if matches!(self.mode, Mode::VisualBlock) {
            let v = self.vblock_cur_vcol;
            // The pane is wrap=true, but block mode is meaningful only
            // when v fits on the first wrap row. If a tab pushed v
            // past pane_w we still clamp so the cursor doesn't run
            // off the right edge.
            row_in_line = 0;
            let cap = pane_w.saturating_sub(gutter_w + 1);
            col_in_row = (gutter_w + v).min(gutter_w + cap);
        }

        // Insert/Replace bar-cursor at a soft-wrap seam: `wrap_pos` returns
        // the position of the char AT `target_byte`, which after a word-
        // break sits at column 0 of the next visual row. The bar-cursor's
        // semantics is "between target_byte-1 and target_byte" though, so
        // when the previous char is the last cell of the row above, snap
        // the cursor up to that row's trailing edge — otherwise typing at
        // the wrap seam looks like the cursor is stuck one row below the
        // line you're editing (reported 2026-05-08).
        if matches!(self.mode, Mode::Insert | Mode::Replace)
            && col_in_row == 0
            && row_in_line > 0
            && target_byte > 0
        {
            // Walk back to the previous char's byte offset (handles multi-
            // byte UTF-8 cleanly via char_indices).
            if let Some((prev_byte, prev_ch)) = line[..target_byte].char_indices().next_back() {
                let (prev_row, prev_col) = wrap_pos(&line, prev_byte, pane_w, gutter_w, ts);
                if prev_row + 1 == row_in_line {
                    let cw = if prev_ch == '\t' {
                        ts.saturating_sub(prev_col % ts).max(1)
                    } else { 1 };
                    row_in_line = prev_row;
                    col_in_row = (prev_col + cw).min(pane_w.saturating_sub(1));
                }
            }
        }

        visual_row += row_in_line;

        let row = pane_y + visual_row as u16;
        let col = pane_x + col_in_row as u16;

        // Cursor shape per mode — handed to crust so the raw DECSCUSR / CUP
        // escapes don't leak into scribe's source. Insert / Command get a
        // bar (6); everything else gets a steady block (2).
        let shape = match self.mode {
            Mode::Insert | Mode::Command => 6, // bar
            Mode::Replace                => 4, // underline
            _                            => 2, // block
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
        let pane_w = self.main_p.w as usize;
        let gutter_w = gutter_width(self.buf.line_count(), self.show_numbers && !self.reading_mode);
        let ts = self.tabstop();
        let total = self.buf.line_count();
        let all: Vec<String> = (0..total).map(|i| self.buf.line(i)).collect();
        // If cursor is above scroll, snap scroll up to cursor.
        if self.cur_line < self.scroll { self.scroll = self.cur_line; }
        // Compute how many VISIBLE visual rows the cursor sits below
        // `scroll`. Hidden (folded) lines contribute zero rows; each
        // visible logical line contributes its `wrap_row_count`. The
        // cursor's own row within `cur_line` is `cur_row_in_line`.
        let cur_buf_line = self.buf.line(self.cur_line);
        let cur_target = self.cur_col.min(cur_buf_line.len());
        let (cur_row_in_line, _) = wrap_pos(&cur_buf_line, cur_target, pane_w, gutter_w, ts);
        loop {
            let mut rows_used: usize = 0;
            for ln in self.scroll..self.cur_line {
                if !self.folds.is_visible(ln, &all) { continue; }
                rows_used += wrap_row_count(&all[ln], pane_w, gutter_w, ts);
                if rows_used >= pane_h { break; }
            }
            if rows_used + cur_row_in_line + 1 <= pane_h { break; }
            // Cursor lives past the pane bottom — advance scroll to
            // the next VISIBLE line.
            let mut next_scroll = self.scroll + 1;
            while next_scroll < total && !self.folds.is_visible(next_scroll, &all) {
                next_scroll += 1;
            }
            if next_scroll >= total || next_scroll > self.cur_line { break; }
            self.scroll = next_scroll;
        }

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
        // source_lines is indexed by ABSOLUTE file-line number so the
        // render loop can fetch it via `source_lines[line_idx]`. With
        // folding, line_idx advances past hidden lines and would be
        // out of sync with a `skip(scroll).take(pane_h)` window.
        let source_lines: Vec<String> = match &self.buf.kind {
            FileKind::Source(ext) => {
                let line_count = self.buf.line_count();
                let mut all = String::new();
                for i in 0..line_count {
                    all.push_str(&self.buf.line(i));
                    all.push('\n');
                }
                // `\M` markup toggle: conceal span tags on every line but the
                // cursor's. Set per-render since the cursor line moves.
                highlight::set_span_conceal(self.markup_concealed, self.cur_line);
                let rendered = match ext.as_str() {
                    "hl" | "woim"       => highlight::highlight_hyperlist(&all, line_count + 1),
                    "md" | "markdown"   => highlight::highlight_markdown_source(&all, line_count + 1),
                    "tex"               => highlight::highlight_tex(&all, line_count + 1),
                    // No-extension / plain-text prose still gets inline colour/
                    // font spans rendered (but no Markdown styling imposed).
                    "" | "txt" | "text" => highlight::highlight_plain_spans(&all, line_count + 1),
                    _                   => highlight::highlight(&all, ext, line_count + 1),
                };
                let mut lines: Vec<String> = rendered.split('\n').map(str::to_string).collect();
                // `\u` State / Transition underline cycle for HL only.
                // Wrap ONLY the item content (post-indent) — leading
                // TABs and `*` indent stay un-underlined to match
                // hyperlist.vim's HLstate / HLtrans patterns which
                // start AFTER the indent (`\(\(^\|\s\|\*\)\(S: \|...\)\)\@<=.*`).
                if matches!(ext.as_str(), "hl" | "woim") && self.st_underline > 0 {
                    let want_state = self.st_underline == 1;
                    for (idx, styled) in lines.iter_mut().enumerate() {
                        if idx >= line_count { break; }
                        let raw = self.buf.line(idx);
                        let trimmed = raw.trim_start_matches(|c: char| c == '\t' || c == '*');
                        let is_state = trimmed.starts_with("S: ") || trimmed.starts_with("| ");
                        let is_trans = trimmed.starts_with("T: ") || trimmed.starts_with("/ ");
                        if (want_state && is_state) || (!want_state && is_trans) {
                            // Find the byte offset where indent ends in
                            // the styled string. emit_hl_line pushes the
                            // raw indent verbatim before any SGR, so
                            // counting leading TAB / `*` / space bytes
                            // gives the right split point.
                            let bytes = styled.as_bytes();
                            let mut indent_end = 0;
                            while indent_end < bytes.len()
                                && (bytes[indent_end] == b'\t'
                                    || bytes[indent_end] == b'*'
                                    || bytes[indent_end] == b' ')
                            { indent_end += 1; }
                            let (head, tail) = styled.split_at(indent_end);
                            *styled = format!("{}\x1b[4m{}\x1b[24m", head, tail);
                        }
                    }
                }
                lines
            }
            _ => Vec::new(),
        };

        let line_count = self.buf.line_count();
        let show_numbers = self.show_numbers && !self.reading_mode;
        let relative_numbers = self.relative_numbers && !self.reading_mode;
        // Limelight-style: dim every paragraph except the cursor's.
        // Gated on reading mode — outside it the source-mode colors
        // fight the dim and the effect is jarring rather than focusing.
        let dim_others = self.paragraph_dim && self.reading_mode;
        let (para_lo, para_hi) = if dim_others {
            self.current_paragraph_bounds()
        } else { (0, usize::MAX) };

        // Snapshot the buffer's lines once for fold computation.
        // Cheap on prose-sized files; if a 1MB buffer ever shows up,
        // gate behind `folds.count() > 0` to skip the work.
        let all_lines: Vec<String> = (0..line_count).map(|i| self.buf.line(i)).collect();

        let mut out = String::new();
        let mut row = 0usize;
        let mut line_idx_walk = self.scroll;
        while row < pane_h {
            while line_idx_walk < line_count
                && !self.folds.is_visible(line_idx_walk, &all_lines)
            {
                line_idx_walk += 1;
            }
            let line_idx = line_idx_walk;
            let i = row;
            'line: {
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
                        if let Some(styled) = source_lines.get(line_idx) {
                            out.push_str(styled);
                            break 'line;
                        }
                    }
                    let tokens = highlight::inline_tokens(&line);
                    highlight::emit_email_line(&mut out, &line, base_fg, bold_until, &tokens, &miss_ranges);
                    break 'line;
                }
                // Selection touches this line: char-by-char so we can apply
                // selection bg precisely. (Email-mode fg is dropped on the
                // selected slice — selection bg is the dominant signal.)
                // For VisualBlock, also track display column so we can
                // split tabs that straddle the rectangle's edge — without
                // splitting, a tab whose expansion overlaps the v1/v2
                // boundary lights up as one wide blob and the rectangle
                // bulges visually.
                let ts_render = self.tabstop();
                let mut col = 0usize;
                let mut disp_col = 0usize;
                while col < line.len() {
                    if !line.is_char_boundary(col) { col += 1; continue; }
                    let mut ce = col + 1;
                    while ce < line.len() && !line.is_char_boundary(ce) { ce += 1; }
                    let glyph = &line[col..ce];
                    let ch = glyph.chars().next().unwrap_or(' ');
                    let cw = if ch == '\t' { ts_render - (disp_col % ts_render) } else { 1 };
                    let abs = line_byte_off + col;

                    // VisualBlock + tab → render the tab as `cw` per-cell
                    // spaces, deciding selection per cell. Keeps the
                    // rectangle perfect when v1 or v2 lands inside the
                    // tab's expansion.
                    if matches!(sel_kind, Some(Mode::VisualBlock)) && ch == '\t' {
                        let l1 = self.visual_anchor_line.min(self.cur_line);
                        let l2 = self.visual_anchor_line.max(self.cur_line);
                        let in_block_line = line_idx >= l1 && line_idx <= l2;
                        let v1 = self.vblock_anchor_vcol.min(self.vblock_cur_vcol);
                        let v2 = self.vblock_anchor_vcol.max(self.vblock_cur_vcol);
                        for i in 0..cw {
                            let cell_col = disp_col + i;
                            let cell_in = in_block_line && cell_col >= v1 && cell_col <= v2;
                            if cell_in {
                                out.push_str(&style::bg(" ", 238));
                            } else {
                                out.push(' ');
                            }
                        }
                    } else {
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
                                if !(line_idx >= l1 && line_idx <= l2) { false }
                                else {
                                    let v1 = self.vblock_anchor_vcol.min(self.vblock_cur_vcol);
                                    let v2 = self.vblock_anchor_vcol.max(self.vblock_cur_vcol);
                                    disp_col >= v1 && disp_col <= v2
                                }
                            }
                            _ => false,
                        };
                        if in_sel {
                            out.push_str(&style::bg(glyph, 238));
                        } else {
                            out.push_str(glyph);
                        }
                    }
                    disp_col += cw;
                    col = ce;
                }
                // VisualLine extends selection past line end visually.
                // Use VISUAL width (post-tab-expansion), not raw char
                // count — otherwise tabs get under-counted and the bg
                // overflows onto the next pane row.
                if sel_kind == Some(Mode::VisualLine) {
                    let l1 = self.visual_anchor_line.min(self.cur_line);
                    let l2 = self.visual_anchor_line.max(self.cur_line);
                    if line_idx >= l1 && line_idx <= l2 {
                        let pane_w = self.main_p.w as usize;
                        let used = visual_width_of(&line, self.tabstop());
                        let pad_w = pane_w.saturating_sub(used);
                        if pad_w > 0 {
                            out.push_str(&style::bg(&" ".repeat(pad_w), 238));
                        }
                    }
                }
                // VisualBlock keeps a perfect rectangle: when this
                // line's visual width is shorter than the block's
                // right edge, paint blank cells from line-end up to
                // and including v2 with the selection bg. Without
                // this, short lines look like the rectangle has
                // "bites" cut out of them.
                if sel_kind == Some(Mode::VisualBlock) {
                    let l1 = self.visual_anchor_line.min(self.cur_line);
                    let l2 = self.visual_anchor_line.max(self.cur_line);
                    if line_idx >= l1 && line_idx <= l2 {
                        let v1 = self.vblock_anchor_vcol.min(self.vblock_cur_vcol);
                        let v2 = self.vblock_anchor_vcol.max(self.vblock_cur_vcol);
                        let used = visual_width_of(&line, self.tabstop());
                        if v2 + 1 > used {
                            let start = used.max(v1);
                            let pad_w = (v2 + 1).saturating_sub(start);
                            if pad_w > 0 {
                                let pane_w = self.main_p.w as usize;
                                let pad_w = pad_w.min(pane_w.saturating_sub(used));
                                if pad_w > 0 {
                                    out.push_str(&style::bg(&" ".repeat(pad_w), 238));
                                }
                            }
                        }
                    }
                }
            } else {
                out.push_str(&style::fg("~", 244));
            }
            } // end 'line:
            // Closed-fold marker — appended after the line content so
            // any colors emitted above are already reset.
            if line_idx < line_count
                && self.folds.closed_folds_containing(line_idx + 1, &all_lines)
                       .first().copied() == Some(line_idx)
            {
                let count = fold::fold_end(line_idx, &all_lines).saturating_sub(line_idx);
                out.push_str(&style::fg(&format!("  ▸ {}", count), 244));
            }
            if i + 1 < pane_h { out.push('\n'); }
            row += 1;
            line_idx_walk += 1;
        }
        // Expand tabs in the styled output to the buffer's tabstop
        // width (3 for HyperList, 8 elsewhere). SGR escapes are
        // skipped during column accounting so colors are preserved.
        let out = expand_tabs_styled(&out, self.tabstop());
        self.main_p.set_text(&out);
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
        // Col shown as 1-based character position, not byte offset.
        // Byte-based jumps confusingly when the line contains
        // multi-byte chars (one emoji ≠ 4 columns); the codepoint
        // count matches the `Nc` buffer-char counter to its left.
        let char_col: usize = {
            let line = self.buf.line(self.cur_line);
            let byte_cap = self.cur_col.min(line.len());
            line[..byte_cap].chars().count()
        };
        let pos = format!(" {}:{} ", self.cur_line + 1, char_col + 1);
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
            // Filetype indicator: shows the kind scribe is treating
            // this buffer as. Useful when troubleshooting why syntax
            // highlighting / mode-specific bindings don't kick in.
            let kind_lbl = match &self.buf.kind {
                buffer::FileKind::Plain     => "plain".to_string(),
                buffer::FileKind::Email     => "email".to_string(),
                buffer::FileKind::Source(s) => s.clone(),
            };
            let plain = format!(" {}w  {}c  ft:{}  {} ", words, chars, kind_lbl, spell_lbl);
            let styled = format!(" {}w  {}c  ft:{}  {}{} ",
                style::fg(&words.to_string(), 252),
                style::fg(&chars.to_string(), 244),
                style::fg(&kind_lbl, 81),
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
                let v1 = self.vblock_anchor_vcol.min(self.vblock_cur_vcol);
                let v2 = self.vblock_anchor_vcol.max(self.vblock_cur_vcol);
                let ts = self.tabstop();
                let mut chars = 0usize;
                let mut words = 0usize;
                for i in l1..=l2 {
                    let line = self.buf.line(i);
                    let (a, _) = byte_at_or_past_col(&line, v1, ts);
                    let (b, _) = byte_at_or_past_col(&line, v2 + 1, ts);
                    if a >= line.len() || b <= a { continue; }
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
        // Insert + Replace can sit one past last char so Append /
        // Backspace / EOL append-extend work.
        if matches!(self.mode, Mode::Insert | Mode::Replace) { len } else { len.saturating_sub(1) }
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
        if let Some(target) = self.visual_step(-1) {
            self.cursor_to_byte(target);
        }
    }

    fn move_down(&mut self) {
        if let Some(target) = self.visual_step(1) {
            self.cursor_to_byte(target);
        }
    }

    /// One visual-row step in `dir` (-1 = up, +1 = down). Returns the
    /// target byte offset, or `None` if already at the buffer edge.
    /// On a soft-wrapped line this moves between visual rows of the
    /// SAME logical line; at the line edge it crosses into the next /
    /// previous line's first / last visual row.
    fn visual_step(&self, dir: i32) -> Option<usize> {
        let pane_w = self.main_p.w as usize;
        let gutter_w = gutter_width(self.buf.line_count(), self.show_numbers && !self.reading_mode);
        let ts = self.tabstop();
        let line = self.buf.line(self.cur_line);
        let cur_byte = self.cur_col.min(line.len());
        let (cur_row, want_col) = wrap_pos(&line, cur_byte, pane_w, gutter_w, ts);
        if dir > 0 {
            let total_rows = wrap_row_count(&line, pane_w, gutter_w, ts);
            if cur_row + 1 < total_rows {
                let nb = visual_to_byte(&line, cur_row + 1, want_col, pane_w, gutter_w, ts);
                return Some(self.buf.line_byte_offset(self.cur_line) + nb);
            }
            let next_line = self.next_visible_line_down(self.cur_line, 1);
            if next_line == self.cur_line { return None; }
            let nl = self.buf.line(next_line);
            let nb = visual_to_byte(&nl, 0, want_col, pane_w, gutter_w, ts);
            Some(self.buf.line_byte_offset(next_line) + nb)
        } else {
            if cur_row > 0 {
                let nb = visual_to_byte(&line, cur_row - 1, want_col, pane_w, gutter_w, ts);
                return Some(self.buf.line_byte_offset(self.cur_line) + nb);
            }
            if self.cur_line == 0 { return None; }
            let prev_line = self.next_visible_line_up(self.cur_line, 1);
            if prev_line == self.cur_line { return None; }
            let pl = self.buf.line(prev_line);
            let last_row = wrap_row_count(&pl, pane_w, gutter_w, ts).saturating_sub(1);
            let nb = visual_to_byte(&pl, last_row, want_col, pane_w, gutter_w, ts);
            Some(self.buf.line_byte_offset(prev_line) + nb)
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

        // Ctrl-y in Normal mode: yank the whole buffer to the system
        // clipboard. Equivalent to `ggVG"+y` in vim and obeys the same
        // OSC-52 broadcast every other yank goes through, so it lands
        // on the X selection / Wayland clipboard without external help
        // (xclip / wl-copy / etc.). The key string is `C-Y` — scribe
        // encodes every Ctrl-letter chord in uppercase regardless of
        // shift state, matching the convention used by every other
        // `C-A` / `C-D` / `C-V` binding in this file.
        if key == "C-Y" && self.pending.operator.is_none() {
            self.pending.clear();
            let last = self.buf.line_count().saturating_sub(1);
            self.execute_op_linewise_yank(0, last);
            return false;
        }

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

        // Awaiting a single character (for r, f, F, t, T)? Checked
        // BEFORE every single-key command handler (space-fold, mark
        // set/jump, z-prefix, …) so the target char is always taken
        // as data, never re-interpreted as a command. Without this
        // ordering `r<Space>` toggled a fold, `rm` set a mark, `rz`
        // started the z-prefix, etc.
        if let Some(op) = self.pending.awaiting_char {
            self.pending.awaiting_char = None;
            if key == "ESC" || key == "C-[" || key == "C-C" {
                self.pending.clear();
                return false;
            }
            // Resolve named keys to a literal char (TAB→tab,
            // ENTER→newline, SPACE→space). Named keys with no literal
            // form (LEFT, UP, F1, …) cancel.
            let c = match key_to_literal_char(key) {
                Some(ch) => ch,
                None => { self.pending.clear(); return false; }
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
                // `''` / `` `` `` (and `'`` / `` `' ``) jump to the
                // auto "previous location" mark, stored under '\''.
                // Any of `'` / `` ` `` as the target resolves to it.
                let is_prev = c == '\'' || c == '`';
                let lookup = if is_prev { '\'' } else { c };
                if c.is_ascii_alphabetic() || is_prev {
                    if let Some(&byte) = self.marks.get(&lookup) {
                        // Operator-pending: `d'a` / `y'a` / `c'a` (and
                        // ``-variants) act as a range from the cursor's
                        // current position to the mark. `'` selects a
                        // linewise range (the full lines on both ends),
                        // `` ` `` selects a charwise range (exact byte
                        // bounds). Matches vim semantics — these are
                        // common in user macros that anchor a region
                        // and then operate on it.
                        if let Some(opc) = self.pending.operator {
                            let cur_byte = self.cursor_byte();
                            if exact {
                                let (start, end) = if byte <= cur_byte {
                                    (byte, cur_byte)
                                } else {
                                    (cur_byte, byte)
                                };
                                self.execute_op_charwise(opc, start, end);
                            } else {
                                let cur_line = self.cur_line;
                                let mark_line = self.buf.byte_to_line_col(byte).0;
                                let lo = cur_line.min(mark_line);
                                let hi = cur_line.max(mark_line);
                                self.execute_op_linewise(lo, hi - lo);
                            }
                            self.pending.clear();
                            return false;
                        }
                        // For `''` / `` `` ``, swap: stash the current
                        // position back into the `'` mark so repeating
                        // `''` toggles between the two locations (vim).
                        let prev = self.cursor_byte();
                        self.cursor_to_byte(byte);
                        if !exact {
                            // `'a` lands on first non-blank of the
                            // mark's line; `` `a `` lands at exact col.
                            let off = motion::line_first_nonblank(
                                &self.buf, self.cursor_byte());
                            self.cursor_to_byte(off);
                        }
                        if is_prev {
                            self.marks.insert('\'', prev);
                        }
                    } else {
                        self.set_status(&format!(" mark '{} not set", c), 196);
                        self.pending.clear();
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
        // `'<mark>` / `` `<mark> `` enter mark-jump prefix even when an
        // operator is pending — `d'd`, `y'a`, `c'b` are linewise (`'`)
        // or charwise (`` ` ``) deletes / yanks / changes from the
        // current position to the mark. The actual op completion
        // happens in the mark_jump_prefix dispatch above.
        if (key == "'" || key == "`")
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
        //   zz  save + quit (equivalent to :wq)
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
                "z" => {
                    return self.save_guarded();
                }
                "n" => self.jump_next_misspelling(),
                "p" => self.jump_prev_misspelling(),
                // Per-language quick toggles (`zN`, `zE`, etc.) and the
                // off-switch (`zO`) live in the user's scriberc
                // [keymap] section now, not here. Building the
                // language picks into scribe baked one user's choices
                // (en_US + nb_NO) into the source; user maps via
                // `:spell <LANG>` and `:set nospell` keep it
                // configurable and discoverable via `:map`.
                "s" => self.showhide_word(true),
                "h" => self.showhide_word(false),
                "0" => {
                    self.showhide = None;
                    let total = self.buf.line_count();
                    let all: Vec<String> = (0..total).map(|i| self.buf.line(i)).collect();
                    self.folds.set_level(99, &all); // re-fold by indent
                    self.set_status(" show/hide cleared", 244);
                }
                _ => {}
            }
            return false;
        }
        if key == "z" && self.pending.operator.is_none() && self.pending.count1.is_none() {
            self.z_prefix = true;
            return false;
        }

        // Leader (`\`) prefix dispatch. Two-letter sequences (`\e?`,
        // `\x?`) set `leader_sub` and read the next char.
        if let Some(group) = self.leader_sub.take() {
            return self.handle_leader_sub(group, key);
        }
        if self.leader_prefix {
            self.leader_prefix = false;
            if key == "e" || key == "x" {
                self.leader_sub = Some(key.chars().next().unwrap());
                self.set_status(
                    if key == "e" { " \\e — e=encrypt  d=decrypt  k=rekey" }
                    else          { " \\x — h=HTML  l=LaTeX  m=Markdown  p=PDF  d=docx  o=odt" },
                    244);
                return false;
            }
            return self.handle_leader(key);
        }
        if key == "\\" && self.pending.operator.is_none()
            && self.pending.count1.is_none() && self.pending.text_object.is_none()
        {
            self.leader_prefix = true;
            return false;
        }

        // <SPACE>: toggle fold under cursor. <C-SPACE>: toggle recursive.
        if key == " " && self.pending.operator.is_none()
            && self.pending.count1.is_none() && self.pending.text_object.is_none()
        {
            let total = self.buf.line_count();
            let all: Vec<String> = (0..total).map(|i| self.buf.line(i)).collect();
            // If the cursor is on a leaf, `toggle_at` may close the
            // parent fold — in which case the cursor would land on a
            // hidden line. Pre-compute the parent so we can hop the
            // cursor up to it after the toggle.
            let move_to_parent = !fold::is_foldable(self.cur_line, &all)
                && self.folds.closed_folds_containing(self.cur_line, &all).is_empty();
            let parent = if move_to_parent {
                fold::find_parent(self.cur_line, &all)
            } else { None };
            self.folds.toggle_at(self.cur_line, &all);
            if let Some(p) = parent {
                self.cur_line = p;
                let line = self.buf.line(p);
                self.cur_col = line.bytes()
                    .position(|b| b != b'\t' && b != b' ' && b != b'*')
                    .unwrap_or(0);
                self.want_col = self.cur_col;
            }
            return false;
        }
        if key == "C-SPACE" && self.pending.operator.is_none()
            && self.pending.count1.is_none() && self.pending.text_object.is_none()
        {
            let total = self.buf.line_count();
            let all: Vec<String> = (0..total).map(|i| self.buf.line(i)).collect();
            self.folds.toggle_recursive_at(self.cur_line, &all);
            return false;
        }

        // HyperList: <RIGHT> on a closed foldable line opens it,
        // <LEFT> on an open foldable line closes it. Falls through to
        // the regular wrap-motion otherwise — so navigating across a
        // non-foldable line still works, and a count prefix still
        // routes through motion (`5l`-style use is unchanged).
        if (key == "LEFT" || key == "RIGHT")
            && self.is_hyperlist()
            && self.pending.operator.is_none()
            && self.pending.count1.is_none()
            && self.pending.text_object.is_none()
        {
            let total = self.buf.line_count();
            let all: Vec<String> = (0..total).map(|i| self.buf.line(i)).collect();
            if fold::is_foldable(self.cur_line, &all) {
                let closed = self.folds.is_closed(self.cur_line);
                if key == "RIGHT" && closed {
                    self.folds.open(self.cur_line);
                    return false;
                }
                if key == "LEFT" && !closed {
                    self.folds.close(self.cur_line);
                    return false;
                }
            }
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
                    // `Ngg` jumps to line N (1-based, vim convention).
                    // Bare `gg` (count=None) goes to the first line.
                    let target_line = match self.pending.count1 {
                        Some(n) if n > 0 => (n - 1).min(self.buf.line_count().saturating_sub(1)),
                        _ => 0,
                    };
                    if self.pending.operator.is_some() {
                        let extra = target_line.abs_diff(self.cur_line);
                        let lo = self.cur_line.min(target_line);
                        self.execute_op_linewise(lo, extra);
                    } else {
                        let off = self.buf.line_byte_offset(target_line);
                        let line = self.buf.line(target_line);
                        let first_nonblank = line.bytes()
                            .position(|b| b != b'\t' && b != b' ' && b != b'*')
                            .unwrap_or(0);
                        self.cursor_to_byte(off + first_nonblank);
                    }
                    self.pending.clear();
                }
                "q" => {
                    // Enter `gq` operator-pending. Don't clear pending — count
                    // and register survive into the next motion / `q` shortcut.
                    self.pending.operator = Some('Q');
                }
                "DOWN" | "j" => {
                    self.pending.clear();
                    self.presentation_step(1);
                }
                "UP" | "k" => {
                    self.pending.clear();
                    self.presentation_step(-1);
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
        // Same trick for `r` (replace single char): without this branch the
        // await flag handle_normal_action sets gets cleared on the way back
        // out, so the next keypress doesn't trigger the replace.
        if matches!(key, "f" | "F" | "t" | "T" | "r") {
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

        // Presentation mode: bare UP/DOWN behave like g<UP>/g<DOWN>.
        // Skip when an operator is pending — dy/dj/etc. should still
        // delete by line, not jump by item.
        if self.presentation && op.is_none() && (key == "UP" || key == "DOWN") {
            self.pending.clear();
            self.presentation_step(if key == "DOWN" { 1 } else { -1 });
            return false;
        }

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
                // Record the pre-jump position in the `'` mark so
                // `''` / `` `` `` returns here (minimal single-slot
                // jumplist). Only for jump-class motions — line/char
                // motions (h/j/k/l/w/b/…) don't set it, matching vim.
                if is_jump_motion(key) {
                    self.marks.insert('\'', self.cursor_byte());
                }
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
            "h" => {
                // `h` stops at the line edge — vim default.
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
            "l" => {
                // `l` stops at the line edge — vim default.
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
            "LEFT" => {
                // Arrow keys wrap across line boundaries (vim's
                // `whichwrap=<,>` default).
                let s = self.buf.rope.to_string();
                let mut b = cur;
                for _ in 0..count {
                    if b == 0 { break; }
                    let mut p = b - 1;
                    while p > 0 && !s.is_char_boundary(p) { p -= 1; }
                    b = p;
                }
                Some(b)
            }
            "RIGHT" => {
                // Arrows wrap across line boundaries (vim's whichwrap=<,>
                // default). At end of line, step into the first char of
                // the next line rather than snapping back via the Normal
                // mode col cap. `l` (no wrap) is a separate binding above.
                let s = self.buf.rope.to_string();
                let mut b = cur;
                for _ in 0..count {
                    if b >= s.len() { break; }
                    let mut p = b + 1;
                    while p < s.len() && !s.is_char_boundary(p) { p += 1; }
                    // If p sits on a `\n`, advance one more so the
                    // cursor lands at col 0 of the next line, not on
                    // the newline (where Normal-mode clamp would push
                    // it back to end-of-line).
                    if p < s.len() && s.as_bytes()[p] == b'\n' {
                        p += 1;
                    }
                    b = p;
                }
                Some(b)
            }
            "j" | "DOWN" => {
                let target_line = self.next_visible_line_down(self.cur_line, count);
                let off = self.buf.line_byte_offset(target_line);
                let len = self.buf.line(target_line).len();
                Some(off + self.want_col.min(len.saturating_sub(1).max(0)))
            }
            "k" | "UP" => {
                let target_line = self.next_visible_line_up(self.cur_line, count);
                let off = self.buf.line_byte_offset(target_line);
                let len = self.buf.line(target_line).len();
                Some(off + self.want_col.min(len.saturating_sub(1).max(0)))
            }
            "0" | "HOME" => Some(motion::line_start(&self.buf, cur)),
            // Ctrl-Home / Ctrl-End: vim's `gg` / `G` but on a single
            // keystroke. Linewise like `gg`/`G` so e.g. `dC-END`
            // deletes from the current line to the end of the file.
            "C-HOME" => Some(0),
            "C-END"  => Some(self.buf.line_byte_offset(
                self.buf.line_count().saturating_sub(1)
            )),
            "^"          => Some(motion::line_first_nonblank(&self.buf, cur)),
            // `-` — move to first non-blank of the previous line (vim
            // linewise motion). With a count, jumps N lines up.
            // Operator-pending it acts linewise: `d-` deletes the
            // current line + previous line. Symmetric to `+` (next line)
            // which scribe doesn't have either yet; add when needed.
            "-" => {
                let target_line = self.cur_line.saturating_sub(count.max(1));
                let off = self.buf.line_byte_offset(target_line);
                let line = self.buf.line(target_line);
                let first_nonblank = line.bytes()
                    .position(|b| b != b'\t' && b != b' ')
                    .unwrap_or(0);
                Some(off + first_nonblank)
            }
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
            // Vim's `K` — keyword lookup. Hands the word under the
            // cursor (with a paragraph of surrounding context) to
            // `claude -p` and shows the answer in a centred popup.
            // Same path as `\w`; both bindings exist because the
            // muscle-memory for `K` is too strong to give up but the
            // leader cheatsheet only lists letter mnemonics.
            "K" => self.lookup_word_with_claude(),

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
                // Fold-aware: if the cursor sits on a closed-fold
                // anchor, the new line goes AFTER the entire fold
                // body, not directly under the anchor (which would
                // bury it inside the fold and hide it on render).
                // Indent still inherits from the anchor so the new
                // line ends up as a sibling, not nested deeper.
                let target_line = if self.folds.is_closed(self.cur_line) {
                    let total = self.buf.line_count();
                    let all: Vec<String> = (0..total).map(|i| self.buf.line(i)).collect();
                    fold::fold_end(self.cur_line, &all)
                } else {
                    self.cur_line
                };
                let line_len = self.buf.line(target_line).len();
                let off = self.buf.line_byte_offset(target_line) + line_len;
                let indent = self.indent_to_inherit();
                self.buf.apply(off, off, &format!("\n{}", indent));
                self.cur_line = target_line + 1;
                self.cur_col = indent.len();
                self.want_col = self.cur_col;
                self.enter_insert();
            }
            "O" => {
                let off = self.buf.line_byte_offset(self.cur_line);
                let indent = self.indent_to_inherit();
                self.buf.apply(off, off, &format!("{}\n", indent));
                self.cur_col = indent.len();
                self.want_col = self.cur_col;
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
            // Replace mode — typed chars OVERWRITE existing text
            // until ESC. Toggle back to Insert mid-stream with <Ins>.
            "R" => self.enter_replace(),
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

            // (`r` — replace single char — is intercepted earlier in
            // handle_normal so the awaiting_char flag survives the
            // post-action `pending.clear()`.)

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
    /// with `:` run as ex commands (so a user can map `zz` to `:wq`).
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
        let mut i = 0;
        while i < keys.len() {
            let k = &keys[i];
            // When a macro fires `/` or `?` in Normal mode, vim drains
            // subsequent keys into the search pattern until <CR> (submit)
            // or <Esc> (cancel). The interactive `footer.ask()` path
            // would instead block on stdin and ignore the rest of the
            // macro — so consume the pattern here directly and call
            // `search.set()` without prompting.
            if (k == "/" || k == "?") && matches!(self.mode, Mode::Normal) {
                let dir = if k == "/" { Direction::Forward } else { Direction::Backward };
                let mut pattern = String::new();
                let mut cancelled = false;
                i += 1;
                while i < keys.len() {
                    let tk = &keys[i];
                    i += 1;
                    if tk == "ENTER" { break; }
                    if tk == "ESC" { cancelled = true; break; }
                    // BS / DEL inside the pattern erase the last char.
                    if tk == "BACKSPACE" || tk == "DEL" {
                        pattern.pop();
                        continue;
                    }
                    // Single-char tokens append literally; named-key
                    // tokens (arrows, etc.) are ignored.
                    if tk.chars().count() == 1 {
                        pattern.push_str(tk);
                    }
                }
                if !cancelled && !pattern.is_empty() {
                    self.search.set(&pattern, dir);
                    if let Some(byte) = self.search_next_at(self.cursor_byte(), dir) {
                        self.cursor_to_byte(byte);
                    } else {
                        self.set_status(&format!(" pattern not found: {}", pattern), 196);
                    }
                }
                continue;
            }
            quit = match self.mode {
                Mode::Normal      => self.handle_normal(k),
                Mode::Insert      => self.handle_insert(k),
                Mode::Replace     => self.handle_replace(k),
                Mode::Visual      |
                Mode::VisualLine  |
                Mode::VisualBlock => self.handle_visual(k),
                Mode::Command     => false,
            };
            if quit { break; }
            i += 1;
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
            Mode::Replace     => { self.handle_replace(key); }
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
                Mode::Normal  => { self.handle_normal(&key); }
                Mode::Insert  => { self.handle_insert(&key); }
                Mode::Replace => { self.handle_replace(&key); }
                Mode::Visual | Mode::VisualLine | Mode::VisualBlock => { self.handle_visual(&key); }
                Mode::Command => {}
            }
        }
        self.replay_depth -= 1;
    }

    /// Inclusive line range that should move as a unit when the cursor
    /// is at `line`. For a normal line, that's just `(line, line)`.
    /// For a closed-fold anchor, it spans the anchor + every hidden
    /// child up to the end of the fold (so `move_line_up/down` swap
    /// the entire collapsed block, not just the head line).
    fn fold_aware_block(&self, line: usize) -> (usize, usize) {
        let total = self.buf.line_count();
        if line >= total { return (line, line); }
        if self.folds.is_closed(line) {
            let all: Vec<String> = (0..total).map(|i| self.buf.line(i)).collect();
            let end = fold::fold_end(line, &all);
            (line, end.min(total - 1))
        } else {
            (line, line)
        }
    }

    /// Swap the block at the cursor with the block above. Fold-aware:
    /// either side can be a single line OR a closed-fold range; the
    /// swap treats whichever side is folded as one unit.
    fn move_line_up(&mut self) {
        if self.cur_line == 0 { return; }
        let (b_start, b_end) = self.fold_aware_block(self.cur_line);
        // The "block above" starts at b_start-1 and might itself be a
        // closed fold — walk back from b_start-1 to find its head.
        let upper_end = b_start - 1;
        let upper_start = self.fold_anchor_for_member(upper_end);
        self.swap_blocks(upper_start, upper_end, b_start, b_end);
    }

    /// Swap the block at the cursor with the block below. Fold-aware
    /// in the same way as `move_line_up`.
    fn move_line_down(&mut self) {
        let total = self.buf.line_count();
        let (a_start, a_end) = self.fold_aware_block(self.cur_line);
        if a_end + 1 >= total { return; }
        let lower_start = a_end + 1;
        let lower_end = self.fold_aware_block(lower_start).1;
        self.swap_blocks(a_start, a_end, lower_start, lower_end);
    }

    /// If `line` is the closed-fold head, return `line`. If it's
    /// hidden inside a closed fold, return that fold's head. Otherwise
    /// return `line` itself.
    fn fold_anchor_for_member(&self, line: usize) -> usize {
        let total = self.buf.line_count();
        if line >= total { return line; }
        let all: Vec<String> = (0..total).map(|i| self.buf.line(i)).collect();
        // Already a fold head?
        if self.folds.is_closed(line) { return line; }
        // Otherwise walk back looking for a closed fold whose body
        // covers this line. Stop at the first one — they don't nest
        // for our purposes (we just want the outermost visible head).
        for head in (0..line).rev() {
            if self.folds.is_closed(head) {
                let end = fold::fold_end(head, &all);
                if end >= line { return head; }
                break;
            }
        }
        line
    }

    /// Swap two adjacent line ranges `[a_start..=a_end]` and
    /// `[b_start..=b_end]` where `a_end + 1 == b_start`. Both ranges
    /// must be non-empty. Cursor lands on the new position of the
    /// originally-active block.
    fn swap_blocks(&mut self, a_start: usize, a_end: usize, b_start: usize, b_end: usize) {
        debug_assert_eq!(a_end + 1, b_start);
        let total = self.buf.line_count();
        if b_end >= total { return; }
        let start = self.buf.line_byte_offset(a_start);
        let end = if b_end + 1 < total {
            self.buf.line_byte_offset(b_end + 1)
        } else {
            self.buf.rope.len_bytes()
        };
        let block_has_trailing_nl = {
            let cs = self.buf.rope.byte_to_char(start);
            let ce = self.buf.rope.byte_to_char(end);
            let span: String = self.buf.rope.slice(cs..ce).into();
            span.ends_with('\n')
        };
        // Build text for each side.
        let collect = |lo: usize, hi: usize| -> String {
            let mut s = String::new();
            for i in lo..=hi {
                if !s.is_empty() { s.push('\n'); }
                s.push_str(&self.buf.line(i));
            }
            s
        };
        let upper = collect(a_start, a_end);
        let lower = collect(b_start, b_end);
        // Result: lower block first, then upper block.
        let mut rep = String::with_capacity(upper.len() + lower.len() + 4);
        rep.push_str(&lower);
        rep.push('\n');
        rep.push_str(&upper);
        if block_has_trailing_nl { rep.push('\n'); }
        // The relative offsets of the cursor's line within its block
        // are preserved. Compute where the cursor's line lands.
        let cur_was_in_upper = self.cur_line >= a_start && self.cur_line <= a_end;
        let cur_offset_in_block = if cur_was_in_upper {
            self.cur_line - a_start
        } else {
            self.cur_line - b_start
        };
        let upper_len = a_end - a_start + 1;
        let lower_len = b_end - b_start + 1;
        let new_cur = if cur_was_in_upper {
            // Upper block now starts at a_start + lower_len
            a_start + lower_len + cur_offset_in_block
        } else {
            // Lower block now starts at a_start
            a_start + cur_offset_in_block
        };

        self.buf.begin_compound();
        self.buf.apply(start, end, &rep);
        self.buf.end_compound();
        self.cur_line = new_cur.min(self.buf.line_count().saturating_sub(1));
        self.clamp_col_to_line();
        let _ = upper_len; // (suppress unused warning — kept for symmetry)
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
    ///
    /// Alignment is by DISPLAY column, not char index: a TAB is one
    /// character but `tabstop` columns wide, so char-index alignment put
    /// the wrong character under the cursor whenever the two lines were
    /// indented differently with tabs. Every other char still counts as
    /// one unit, so multi-byte text (æ ø å, emoji) lines up as the user
    /// sees it.
    fn copy_char_from(&mut self, dir: i32) {
        let src_idx: isize = self.cur_line as isize + dir as isize;
        if src_idx < 0 { return; }
        let src_idx = src_idx as usize;
        if src_idx >= self.buf.line_count() { return; }
        let ts = self.tabstop();
        let cur_line = self.buf.line(self.cur_line);
        let target = display_col(&cur_line, self.cur_col.min(cur_line.len()), ts);
        let src = self.buf.line(src_idx);
        let Some(ch) = char_at_display_col(&src, target, ts) else { return };
        let s = ch.to_string();
        let off = self.cursor_byte();
        self.buf.apply(off, off, &s);
        self.cur_col += s.len();
        self.want_col = self.cur_col;
        if self.capturing_insert { self.captured_insert.push_str(&s); }
    }

    /// Tab stop width for the current buffer. HyperList convention is
    /// 3 (matches hyperlist.vim's `setlocal tabstop=3`); everything
    /// else uses 8.
    fn tabstop(&self) -> usize {
        match &self.buf.kind {
            buffer::FileKind::Source(s) if s == "hl" || s == "woim" => 3,
            _ => 8,
        }
    }

    /// True iff the current buffer is a HyperList (.hl / .woim) file.
    /// Used to gate HyperList-specific keybindings like LEFT/RIGHT
    /// fold-toggle without polluting the generic motion handler.
    fn is_hyperlist(&self) -> bool {
        matches!(&self.buf.kind, buffer::FileKind::Source(s) if s == "hl" || s == "woim")
    }

    /// Leading-whitespace prefix of the current line (TAB/space/`*`),
    /// suitable for `o`/`O` to inherit on a new line. For HyperList
    /// buffers we honour the convention; for plain text we still
    /// indent to keep prose lists tidy.
    fn indent_to_inherit(&self) -> String {
        let line = self.buf.line(self.cur_line);
        line.chars()
            .take_while(|c| *c == '\t' || *c == ' ' || *c == '*')
            .collect()
    }

    /// Vim-leader (`\`) dispatch. All HyperList commands live behind
    /// `\` — see `\?` for the in-editor cheatsheet. Two-letter
    /// sequences (`\e?`, `\x?`) are routed via `handle_leader_sub`
    /// from the caller; this function only sees single-key tails.
    fn handle_leader(&mut self, key: &str) -> bool {
        let total = self.buf.line_count();
        let all: Vec<String> = (0..total).map(|i| self.buf.line(i)).collect();
        match key {
            // Fold levels.
            "0" => self.folds.set_level(0, &all),
            "1" => self.folds.set_level(1, &all),
            "2" => self.folds.set_level(2, &all),
            "3" => self.folds.set_level(3, &all),
            "4" => self.folds.set_level(4, &all),
            "5" => self.folds.set_level(5, &all),
            "6" => self.folds.set_level(6, &all),
            "7" => self.folds.set_level(7, &all),
            "8" => self.folds.set_level(8, &all),
            "9" => self.folds.set_level(9, &all),
            // Fold-all-open.
            "a" => self.folds.open_all(),
            // Checkboxes.
            "v" => self.toggle_checkbox(false),
            "V" => self.toggle_checkbox(true),
            "o" => self.mark_in_progress(),
            // Limelight: dim every paragraph except the cursor's.
            // Only meaningful in reading mode — outside it, source-mode
            // syntax colors are still applied to dimmed lines and the
            // effect is jarring rather than focusing.
            "h" => {
                if !self.reading_mode {
                    self.set_status(" \\h needs reading mode (`:read` or `zr`)", 244);
                } else {
                    self.paragraph_dim = !self.paragraph_dim;
                    self.set_status(if self.paragraph_dim { " highlight on" } else { " highlight off" }, 244);
                }
            }
            // Autonumber toggle.
            "n" => {
                self.autonumber = !self.autonumber;
                self.set_status(if self.autonumber { " autonumber on" } else { " autonumber off" }, 244);
            }
            // Renumber visual selection.
            "R" => self.renumber_visual(),
            // State / Transition underline cycle.
            "u" => {
                self.st_underline = (self.st_underline + 1) % 3;
                let label = match self.st_underline {
                    1 => "state items underlined",
                    2 => "transition items underlined",
                    _ => "underlining off",
                };
                self.set_status(&format!(" {}", label), 244);
            }
            // Sort visual block by indent.
            "s" => self.sort_visual_by_indent(),
            // Reference jump — auto-detects in-file ref / file path / URL.
            "r" => self.goto_reference(),
            // Calendar.
            "g" => self.calendar_add(),
            // Complexity.
            "c" => self.complexity_report(),
            // Colour the Visual selection (prism picks fg/bg). Stored as an
            // inline HTML span, so it survives a .md/.html save and exports
            // to docx/odt/pdf via soffice with the colour intact.
            "C" => self.color_visual(),
            // Set the font on the Visual selection (the `fonts` picker chooses
            // family + size). Stored as an inline HTML span like colour;
            // exports to docx/odt/pdf via soffice with the font intact.
            "F" => self.font_visual(),
            // Toggle concealment of colour/font span markup (reveals on the
            // cursor's line so it stays editable).
            "M" => self.toggle_markup(),
            // Presentation mode toggle (Up/Down → presentation_step).
            "p" => {
                self.presentation = !self.presentation;
                self.set_status(if self.presentation { " presentation ON" } else { " presentation off" }, 244);
            }
            // Show/hide-word aliases (\S = zs, \H = zh, \N = z0).
            "S" => self.showhide_word(true),
            "H" => self.showhide_word(false),
            "N" => {
                self.showhide = None;
                self.folds.set_level(99, &all);
                self.set_status(" show/hide cleared", 244);
            }
            // Cheatsheet popup.
            "?" => self.show_hl_cheatsheet(),
            // Word lookup: send the word under the cursor plus a few
            // lines of surrounding context to `claude -p` and show the
            // response in a popup. Vim's `K` is also wired up, but
            // `\w` is the discoverable variant from the leader
            // cheatsheet (mnemonic: **w**ord).
            "w" => self.lookup_word_with_claude(),
            // Next template element ("=$" → append).
            " " => self.jump_next_template(),
            _ => { self.set_status(&format!(" leader \\{}: unknown", key), 244); }
        }
        false
    }

    /// Two-letter leader dispatch for `\e?` (encryption) and `\x?`
    /// (exports). `group` is the first letter, `key` is the second.
    fn handle_leader_sub(&mut self, group: char, key: &str) -> bool {
        match (group, key) {
            ('e', "e") => self.encrypt_lines(!self.mode.is_visual()),
            ('e', "d") => self.decrypt_lines(!self.mode.is_visual()),
            ('e', "k") => self.rekey_buffer(),
            ('x', "h") => self.export_to("html"),
            ('x', "l") => self.export_to("latex"),
            ('x', "m") => self.export_to("markdown"),
            ('x', "p") => self.export_to("pdf"),
            ('x', "d") => self.export_to("docx"),
            ('x', "o") => self.export_to("odt"),
            ('e', "ESC") | ('x', "ESC") => self.set_status(" cancelled", 244),
            _ => self.set_status(&format!(" leader \\{}{}: unknown", group, key), 244),
        }
        false
    }

    /// `\ek` — re-encrypt the buffer with a new password. Asks for the
    /// old password first (to verify), then prompts for a new one. The
    /// buffer is re-encrypted in place; user `:w`s to commit.
    fn rekey_buffer(&mut self) {
        // Detect: are we currently looking at ciphertext, or at a
        // decrypted buffer that came from an encrypted file? In the
        // first case we need to decrypt then re-encrypt; in the second
        // we just swap the in-memory password.
        let starts_with_enc = self.buf
            .rope.byte_slice(0..self.buf.rope.len_bytes().min(4))
            .to_string()
            .starts_with("ENC:");
        if starts_with_enc {
            let old = match self.prompt_password("Old password: ") {
                Some(p) => p, None => { self.set_status(" cancelled", 244); return; }
            };
            let cipher = self.buf.rope.byte_slice(0..self.buf.rope.len_bytes()).to_string();
            let plain = match buffer::decrypt(&cipher, &old) {
                Ok(p) => p,
                Err(_) => { self.set_status(" wrong password", 196); return; }
            };
            let new_pw = match self.prompt_password_confirm("New password: ") {
                Some(p) => p, None => { self.set_status(" cancelled", 244); return; }
            };
            match buffer::encrypt(&plain, &new_pw) {
                Ok(c) => {
                    self.buf.begin_compound();
                    self.buf.apply(0, self.buf.rope.len_bytes(), &c);
                    self.buf.end_compound();
                    self.set_status(" rekeyed", 46);
                }
                Err(e) => self.set_status(&format!(" rekey failed: {}", e), 196),
            }
        } else {
            // Plain buffer: re-encrypt the whole thing with the new
            // password. Equivalent to `\eE` if we had one.
            self.encrypt_lines(true);
        }
    }

    /// `\?` cheatsheet popup. Modal; ESC dismisses.
    fn show_hl_cheatsheet(&mut self) {
        let popup_w = 64u16;
        let popup_h = 22u16;
        let mut popup = Popup::centered(popup_w, popup_h, 252, 236);
        let key = |k: &str| style::fg(k, 220);
        let mut lines: Vec<String> = Vec::new();
        lines.push(String::new());
        lines.push(format!("  {}", style::bold("HyperList — leader bindings (\\)")));
        lines.push(format!("  {}", style::fg(&"-".repeat(popup_w as usize - 4), 238)));
        lines.push(format!("  {}        fold to level 0..9",                key("\\0..\\9")));
        lines.push(format!("  {}           fold all open",                  key("\\a")));
        lines.push(format!("  {}      checkbox / + timestamp / in-progress", key("\\v \\V \\o")));
        lines.push(format!("  {}           autonumber toggle",              key("\\n")));
        lines.push(format!("  {}           renumber selection (visual)",    key("\\R")));
        lines.push(format!("  {}           sort by indent (visual)",        key("\\s")));
        lines.push(format!("  {}           state/transition underline",     key("\\u")));
        lines.push(format!("  {}           limelight highlight",            key("\\h")));
        lines.push(format!("  {}           reference jump (ref/file/URL)",  key("\\r")));
        lines.push(format!("  {}           presentation mode toggle",       key("\\p")));
        lines.push(format!("  {}           complexity report",              key("\\c")));
        lines.push(format!("  {}           colour selection (visual, prism)", key("\\C")));
        lines.push(format!("  {}           font on selection (visual, fonts)", key("\\F")));
        lines.push(format!("  {}           toggle colour/font markup", key("\\M")));
        lines.push(format!("  {}           calendar add (gcalcli)",         key("\\g")));
        lines.push(format!("  {}     show / hide / clear word",             key("\\S \\H \\N")));
        lines.push(format!("  {}        encrypt / decrypt / rekey",         key("\\ee \\ed \\ek")));
        lines.push(format!("  {} export HTML/LaTeX/MD/PDF/docx/odt", key("\\xh \\xl \\xm \\xp \\xd \\xo")));
        lines.push(format!("  {}", style::fg(&"-".repeat(popup_w as usize - 4), 238)));
        lines.push(format!("  {}  Close", key("ESC")));
        // Hide the terminal cursor — otherwise it stays parked on the
        // buffer beneath the popup and renders as a stray block (visible
        // through the popup's interior).
        Cursor::hide();
        popup.show(&lines.join("\n"));
        loop {
            let Some(k) = Input::getchr(None) else { break };
            if k == "ESC" || k == "q" { break; }
        }
        popup.dismiss(&mut [&mut self.header, &mut self.main_p, &mut self.footer]);
        Cursor::show();
        self.render_all();
    }

    /// `\v` (no_stamp) / `\V` (stamp) toggle a checkbox at the start of
    /// the cursor's item. State machine: nothing → `[_] `, `[_]` →
    /// `[x]` (with `\V`: also append `\` `YYYY-MM-DD HH.MM:`),
    /// `[x]` → `[_]` (and strip the timestamp if present).
    fn toggle_checkbox(&mut self, stamp: bool) {
        let line = self.buf.line(self.cur_line);
        let line_off = self.buf.line_byte_offset(self.cur_line);
        // Find the byte index of the first non-whitespace character.
        let body_start = line.bytes().position(|b| b != b'\t' && b != b' ' && b != b'*')
            .unwrap_or(line.len());
        let body = &line[body_start..];
        let abs = line_off + body_start;
        if body.starts_with("[O]") {
            // In-progress → done
            self.buf.apply(abs, abs + 3, "[x]");
        } else if body.starts_with("[_]") {
            let now = current_timestamp();
            let new = if stamp {
                format!("[x] {}: ", now)
            } else { "[x]".into() };
            self.buf.apply(abs, abs + 3, &new);
        } else if body.starts_with("[x]") {
            // Strip the optional " YYYY-MM-DD HH.MM:" timestamp suffix.
            let after = &body[3..];
            let mut consume = 3;
            if let Some(rest) = after.strip_prefix(' ') {
                if rest.len() >= 17
                    && rest.as_bytes()[4] == b'-' && rest.as_bytes()[7] == b'-'
                    && rest.as_bytes()[10] == b' '
                    && rest.as_bytes()[13] == b'.'
                    && rest.as_bytes()[16] == b':'
                {
                    consume += 1 + 17;
                }
            }
            self.buf.apply(abs, abs + consume, "[_]");
        } else {
            // No checkbox yet: prepend.
            self.buf.apply(abs, abs, "[_] ");
            self.cur_col += 4;
            self.want_col = self.cur_col;
        }
    }

    /// `\o` toggle in-progress checkbox `[O]`.
    fn mark_in_progress(&mut self) {
        let line = self.buf.line(self.cur_line);
        let line_off = self.buf.line_byte_offset(self.cur_line);
        let body_start = line.bytes().position(|b| b != b'\t' && b != b' ' && b != b'*')
            .unwrap_or(line.len());
        let body = &line[body_start..];
        let abs = line_off + body_start;
        if body.starts_with("[O]") {
            self.buf.apply(abs, abs + 3, "[_]");
        } else if body.starts_with("[_]") || body.starts_with("[x]") {
            self.buf.apply(abs, abs + 3, "[O]");
        } else {
            self.buf.apply(abs, abs, "[O] ");
            self.cur_col += 4;
            self.want_col = self.cur_col;
        }
    }

    /// `gr` — Goto Reference. Find the `<…>` reference under or after
    /// the cursor on the current line and jump to it. Supports:
    ///   - `<file:/path/...>` → opens the file via xdg-open (defers
    ///     to the user's default opener for the extension).
    ///   - `<+N>` / `<-N>` → relative line jump.
    ///   - `<a/b/c>` → path-style search for a multi-level descendant.
    ///   - `<Anything>` → simple text search.
    /// Sets the `'` mark at the current position before jumping so
    /// `''` can return.
    fn goto_reference(&mut self) {
        let line = self.buf.line(self.cur_line);
        let line_start = self.buf.line_byte_offset(self.cur_line);
        // Find the angle-bracketed reference at-or-after the cursor.
        let body = &line[self.cur_col.min(line.len())..];
        let abs_start_in_line = self.cur_col.min(line.len());
        let ref_full_opt = find_reference(line.as_str(), abs_start_in_line)
            .or_else(|| find_reference(body, 0));
        // Fallback: no <…> on the line — pick the first whitespace-
        // separated token at/after the cursor and try to open it as a
        // URL or file path. Lets `\r` work on bare URLs too.
        let ref_full = match ref_full_opt {
            Some(r) => r,
            None => {
                let token = body.split_whitespace().next()
                    .or_else(|| line.split_whitespace().next());
                if let Some(t) = token {
                    if looks_like_url(t) || looks_like_path(t) {
                        self.marks.insert('\'', self.cursor_byte());
                        self.open_path_external(t);
                        return;
                    }
                }
                self.set_status(" no reference on this line", 244);
                return;
            }
        };
        // Set mark `'` at current cursor.
        self.marks.insert('\'', self.cursor_byte());
        let inner = ref_full.trim_start_matches('<').trim_end_matches('>');
        if let Some(path) = inner.strip_prefix("file:") {
            self.open_path_external(path);
            return;
        }
        if looks_like_url(inner) {
            self.open_path_external(inner);
            return;
        }
        if let Some(rest) = inner.strip_prefix('+').or_else(|| inner.strip_prefix('-')) {
            if let Ok(n) = rest.parse::<i64>() {
                let signed = if inner.starts_with('-') { -n } else { n };
                let total = self.buf.line_count() as i64;
                let target = (self.cur_line as i64 + signed).clamp(0, total - 1) as usize;
                let line = self.buf.line(target);
                self.cur_line = target;
                self.cur_col = line.bytes()
                    .position(|b| b != b'\t' && b != b' ' && b != b'*')
                    .unwrap_or(0);
                self.want_col = self.cur_col;
                let _ = line_start;
                return;
            }
        }
        // Path-style or plain text search. Take the LAST segment after
        // any `/` and search for it line-by-line, restricting matches
        // to descendants of the previous segments. Simple version:
        // search whole buffer for the last segment.
        let target_text = inner.rsplit('/').next().unwrap_or(inner);
        for i in 0..self.buf.line_count() {
            if i == self.cur_line { continue; }
            let line = self.buf.line(i);
            if line.contains(target_text) {
                self.cur_line = i;
                // Land on first non-blank (skip leading TABs / `*`).
                self.cur_col = line.bytes()
                    .position(|b| b != b'\t' && b != b' ' && b != b'*')
                    .unwrap_or(0);
                self.want_col = self.cur_col;
                return;
            }
        }
        self.set_status(&format!(" reference not found: <{}>", inner), 196);
    }

    /// Spawn an external opener (xdg-open on Linux, open on macOS) for
    /// the given path. Expands leading `~`. Best-effort; failures just
    /// flash a status message.
    fn open_path_external(&mut self, path: &str) {
        let expanded = if let Some(rest) = path.strip_prefix("~/") {
            std::env::var_os("HOME")
                .map(|h| std::path::PathBuf::from(h).join(rest).to_string_lossy().into_owned())
                .unwrap_or_else(|| path.into())
        } else { path.into() };
        let opener = if cfg!(target_os = "macos") { "open" } else { "xdg-open" };
        let res = std::process::Command::new(opener)
            .arg(&expanded)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();
        match res {
            Ok(_)  => self.set_status(&format!(" opened {}", expanded), 244),
            Err(e) => self.set_status(&format!(" open failed: {}", e), 196),
        }
    }

    /// `\R` (visual): renumber the lines in the visual selection. Uses
    /// the FIRST selected line's number as the seed; only lines at the
    /// same indent get renumbered. Children stay put. Mirrors
    /// hyperlist.vim's Renumber() routine.
    fn renumber_visual(&mut self) {
        if !self.mode.is_visual() { return; }
        let l1 = self.visual_anchor_line.min(self.cur_line);
        let l2 = self.visual_anchor_line.max(self.cur_line);
        // Read first line. Determine indent + initial number prefix.
        let first = self.buf.line(l1);
        let (indent, _) = first.split_at(first.bytes().position(|b| b != b'\t' && b != b'*').unwrap_or(0));
        let body = &first[indent.len()..];
        // Extract leading number (e.g. "1.2.3 " or "5. " or "5 ") — capture the
        // path prefix and the trailing index that we'll increment.
        let (prefix, mut idx, trailing_period) = parse_number_prefix(body)
            .unwrap_or_else(|| ("".into(), 1, false));
        self.buf.begin_compound();
        for ln in l1..=l2 {
            let line = self.buf.line(ln);
            if !line.starts_with(indent) { continue; }
            let line_indent = line.bytes().take_while(|&b| b == b'\t' || b == b'*').count();
            if line_indent != indent.len() { continue; }
            // Replace any existing leading number (else inject one).
            let line_off = self.buf.line_byte_offset(ln);
            let body = &line[indent.len()..];
            let (existing_w, body_after_num) = strip_number_prefix(body);
            let new_num = format!("{}{}{}", prefix, idx, if trailing_period { "." } else { "" });
            let new_line_body = format!("{} {}", new_num, body_after_num.trim_start());
            let abs_start = line_off + indent.len();
            let abs_end = line_off + indent.len() + existing_w;
            self.buf.apply(abs_start, abs_end, &new_line_body[..new_line_body.len() - body_after_num.trim_start().len() - 1]);
            // Re-read the modified line to be safe; just bump idx and move on.
            idx += 1;
        }
        self.buf.end_compound();
        self.set_status(&format!(" renumbered {}..{}", l1 + 1, l2 + 1), 46);
    }

    /// `\s` (visual): sort the visually selected items alphabetically
    /// at the indent of the first selected line. Children come along
    /// with their parent. Mirrors hyperlist.vim's sort hack.
    fn sort_visual_by_indent(&mut self) {
        if !self.mode.is_visual() { return; }
        let l1 = self.visual_anchor_line.min(self.cur_line);
        let l2 = self.visual_anchor_line.max(self.cur_line);
        let total = self.buf.line_count();
        if l2 + 1 >= total {
            self.set_status(" last line cannot be at end of buffer", 196);
            return;
        }
        // All lines in span.
        let lines: Vec<String> = (l1..=l2).map(|i| self.buf.line(i)).collect();
        let target_indent = fold::fold_level(&lines[0]);
        // Group: each top-level item (at target_indent) starts a group;
        // lines at deeper indent belong to the previous group.
        let mut groups: Vec<Vec<String>> = Vec::new();
        for ln in &lines {
            let lvl = fold::fold_level(ln);
            if lvl < target_indent { continue; }
            if lvl == target_indent {
                groups.push(vec![ln.clone()]);
            } else if let Some(last) = groups.last_mut() {
                last.push(ln.clone());
            }
        }
        groups.sort_by(|a, b| a[0].cmp(&b[0]));
        let new_text = groups.into_iter()
            .flat_map(|g| g.into_iter())
            .collect::<Vec<_>>()
            .join("\n") + "\n";
        let start = self.buf.line_byte_offset(l1);
        let end = if l2 + 1 < total {
            self.buf.line_byte_offset(l2 + 1)
        } else {
            self.buf.rope.len_bytes()
        };
        self.buf.begin_compound();
        self.buf.apply(start, end, &new_text);
        self.buf.end_compound();
        self.set_status(" sorted", 46);
    }

    /// `\z` / `\Z` — encrypt the current line (and folded children) /
    /// the entire file via openssl AES-256-CBC + PBKDF2. Prompts for
    /// password (and confirmation). Doesn't touch the file on disk —
    /// the encrypted text replaces the buffer content; user `:w`s.
    fn encrypt_lines(&mut self, whole_file: bool) {
        let pw = match self.prompt_password_confirm("Encrypt password: ") {
            Some(p) => p,
            None => { self.set_status(" cancelled", 244); return; }
        };
        let total = self.buf.line_count();
        let (l1, l2) = if whole_file { (0, total.saturating_sub(1)) }
                       else if self.mode.is_visual() {
                           let a = self.visual_anchor_line.min(self.cur_line);
                           let b = self.visual_anchor_line.max(self.cur_line);
                           (a, b)
                       } else { (self.cur_line, self.cur_line) };
        let start = self.buf.line_byte_offset(l1);
        let end = if l2 + 1 < total { self.buf.line_byte_offset(l2 + 1) }
                  else { self.buf.rope.len_bytes() };
        let plain: String = self.buf.rope.byte_slice(start..end).to_string();
        match buffer::encrypt(&plain, &pw) {
            Ok(c) => {
                self.buf.begin_compound();
                self.buf.apply(start, end, &c);
                self.buf.end_compound();
                self.set_status(" encrypted", 46);
            }
            Err(e) => self.set_status(&format!(" encrypt failed: {}", e), 196),
        }
    }

    /// `\x` / `\X` — decrypt the current line (or visual) / whole file.
    fn decrypt_lines(&mut self, whole_file: bool) {
        let pw = match self.prompt_password("Decrypt password: ") {
            Some(p) => p,
            None => { self.set_status(" cancelled", 244); return; }
        };
        let total = self.buf.line_count();
        let (l1, l2) = if whole_file { (0, total.saturating_sub(1)) }
                       else if self.mode.is_visual() {
                           let a = self.visual_anchor_line.min(self.cur_line);
                           let b = self.visual_anchor_line.max(self.cur_line);
                           (a, b)
                       } else { (self.cur_line, self.cur_line) };
        let start = self.buf.line_byte_offset(l1);
        let end = if l2 + 1 < total { self.buf.line_byte_offset(l2 + 1) }
                  else { self.buf.rope.len_bytes() };
        let cipher: String = self.buf.rope.byte_slice(start..end).to_string();
        match buffer::decrypt(&cipher, &pw) {
            Ok(p) => {
                self.buf.begin_compound();
                self.buf.apply(start, end, &p);
                self.buf.end_compound();
                self.set_status(" decrypted", 46);
            }
            Err(e) => self.set_status(&format!(" decrypt failed: {}", e), 196),
        }
    }

    fn prompt_password(&mut self, label: &str) -> Option<String> {
        // Footer pane masks each typed char with `•` while `secret`
        // is set; cleared after so subsequent `:` prompts echo
        // normally.
        self.footer.secret = true;
        let s = self.footer.ask_with_bg(label, "", 17);
        self.footer.secret = false;
        self.render_footer();
        if s.is_empty() { None } else { Some(s) }
    }

    fn prompt_password_confirm(&mut self, label: &str) -> Option<String> {
        let p1 = self.prompt_password(label)?;
        let p2 = self.prompt_password("Confirm: ")?;
        if p1 != p2 {
            self.set_status(" passwords don't match", 196);
            return None;
        }
        Some(p1)
    }

    /// `\<SPACE>` — search forward for `=$` and append.
    fn jump_next_template(&mut self) {
        for i in self.cur_line..self.buf.line_count() {
            let line = self.buf.line(i);
            if line.trim_end().ends_with('=') {
                self.cur_line = i;
                self.cur_col = line.len();
                self.want_col = self.cur_col;
                self.enter_insert();
                return;
            }
        }
        self.set_status(" no template element below", 244);
    }

    /// `g<DOWN>` / `g<UP>` — presentation step. Move cursor by `dir`
    /// visible lines; close every fold above level corresponding to
    /// the target's indent so only it + ancestors remain visible.
    fn presentation_step(&mut self, dir: i32) {
        if dir > 0 { self.cur_line = self.next_visible_line_down(self.cur_line, 1); }
        else        { self.cur_line = self.next_visible_line_up(self.cur_line, 1); }
        let line = self.buf.line(self.cur_line);
        let target_lvl = fold::fold_level(&line);
        let total = self.buf.line_count();
        let all: Vec<String> = (0..total).map(|i| self.buf.line(i)).collect();
        // Close folds at the target level + 1 and below.
        self.folds.set_level(target_lvl + 1, &all);
        self.cur_col = 0;
        self.want_col = 0;
    }

    /// `zs` / `zh` — show / hide items containing the word under the
    /// cursor. Implementation:
    ///   - `zs` (show): keep lines that match OR have any descendant
    ///     that matches OR are an ancestor of a match. Fold every
    ///     foldable subtree whose range contains NO match.
    ///   - `zh` (hide): fold matching items themselves (closing them
    ///     so their children disappear and the head becomes unmatched
    ///     by virtue of being collapsed).
    /// `zs` / `zh` — show / hide lines containing the word under the
    /// cursor. Mirrors hyperlist.vim's foldexpr-based behaviour: each
    /// line is independently classified, and CONSECUTIVE non-shown
    /// lines collapse into a single fold whose head displays the
    /// first line of the run. Standalone non-shown lines stay
    /// visible (a one-line fold isn't worth closing).
    fn showhide_word(&mut self, show: bool) {
        let line = self.buf.line(self.cur_line);
        let off = self.cur_col.min(line.len());
        let bytes = line.as_bytes();
        let mut s = off;
        while s > 0 && (bytes[s - 1].is_ascii_alphanumeric() || bytes[s - 1] == b'_') { s -= 1; }
        let mut e = off;
        while e < bytes.len() && (bytes[e].is_ascii_alphanumeric() || bytes[e] == b'_') { e += 1; }
        if s == e { self.set_status(" no word at cursor", 244); return; }
        let word = line[s..e].to_string();
        self.showhide = Some((word.clone(), show));

        let total = self.buf.line_count();
        self.folds.clear();
        // Walk lines, collapse maximal runs of "should-hide" lines
        // into a fold whose head is the first line of the run.
        let mut run_start: Option<usize> = None;
        let mut close_run = |start: usize, end: usize, folds: &mut fold::Folds| {
            if end > start { folds.close_range(start, end); }
        };
        for i in 0..total {
            let l = self.buf.line(i);
            let matches = l.contains(&word);
            let hide_this = if show { !matches } else { matches };
            if hide_this {
                if run_start.is_none() { run_start = Some(i); }
            } else if let Some(start) = run_start.take() {
                close_run(start, i - 1, &mut self.folds);
            }
        }
        if let Some(start) = run_start {
            close_run(start, total - 1, &mut self.folds);
        }
        self.set_status(&format!(" {}: {}", if show { "show" } else { "hide" }, word), 244);
    }

    /// Insert-mode `<CR>` with autonumber on: clone the current line's
    /// indent + numbering, increment the trailing index, start a new
    /// item below.
    fn autonum_newline(&mut self) {
        let line = self.buf.line(self.cur_line);
        let indent: String = line.chars().take_while(|c| *c == '\t' || *c == '*').collect();
        let body = &line[indent.len()..];
        if let Some((prefix, idx, trail)) = parse_number_prefix(body) {
            let new_num = format!("{}{}{}", prefix, idx + 1, if trail { "." } else { "" });
            let off = self.buf.line_byte_offset(self.cur_line) + self.current_line_len();
            let inserted = format!("\n{}{} ", indent, new_num);
            let inserted_len = inserted.len();
            self.buf.apply(off, off, &inserted);
            self.cur_line += 1;
            self.cur_col = inserted_len - 1; // before the trailing space? prefer end-of-num
            self.cur_col = indent.len() + new_num.len() + 1;
            self.want_col = self.cur_col;
        } else {
            // Not a numbered line — fall back to plain newline + indent.
            let off = self.buf.line_byte_offset(self.cur_line) + self.current_line_len();
            let inserted = format!("\n{}", indent);
            self.buf.apply(off, off, &inserted);
            self.cur_line += 1;
            self.cur_col = indent.len();
            self.want_col = self.cur_col;
        }
    }

    /// Insert-mode `Ctrl-T`: indent right, append `.1` to the line's
    /// number prefix to make this a child level. (e.g. `1.2 foo` →
    /// indent + `1.2.1 foo`.)
    /// Insert-mode `Ctrl-T`: indent right and renumber as a child of
    /// what was the previous sibling. `1.2.` → `1.1.1.` (matches the
    /// vim plugin's mapping which DECREMENTS the trailing index first
    /// before appending `.1`).
    fn autonum_indent_in(&mut self) {
        let line = self.buf.line(self.cur_line);
        let indent: String = line.chars().take_while(|c| *c == '\t' || *c == '*').collect();
        let body = &line[indent.len()..];
        let line_off = self.buf.line_byte_offset(self.cur_line);

        self.buf.begin_compound();
        self.buf.apply(line_off, line_off, "\t");
        if let Some((prefix, idx, trail)) = parse_number_prefix(body) {
            let old = format!("{}{}{}", prefix, idx, if trail { "." } else { "" });
            let new_idx = idx.saturating_sub(1).max(1);
            let new = format!("{}{}.1{}", prefix, new_idx, if trail { "." } else { "" });
            let abs = line_off + 1 + indent.len(); // +1 for new TAB
            self.buf.apply(abs, abs + old.len(), &new);
            self.cur_col = indent.len() + 1 + new.len() + 1;
            self.want_col = self.cur_col;
        } else {
            self.cur_col += 1;
            self.want_col = self.cur_col;
        }
        self.buf.end_compound();
    }

    /// Insert-mode `Ctrl-D`: outdent and renumber as the next sibling
    /// of what was the parent. `1.1.1.` → `1.2.` (drop the trailing
    /// `.N` segment, increment what was the parent's index).
    fn autonum_indent_out(&mut self) {
        let line = self.buf.line(self.cur_line);
        let indent: String = line.chars().take_while(|c| *c == '\t' || *c == '*').collect();
        if indent.is_empty() { return; }
        let body = &line[indent.len()..];
        let line_off = self.buf.line_byte_offset(self.cur_line);

        self.buf.begin_compound();
        self.buf.apply(line_off, line_off + 1, "");
        if let Some((prefix, _idx, trail)) = parse_number_prefix(body) {
            // prefix is "1.1." or "1." or "" — drop the LAST dot to
            // access the parent index.
            let p = prefix.trim_end_matches('.');
            let (gp, parent_idx) = match p.rfind('.') {
                Some(d) => (&p[..d + 1], p[d + 1..].parse::<u64>().unwrap_or(0)),
                None if !p.is_empty() => ("", p.parse::<u64>().unwrap_or(0)),
                None => ("", 0),
            };
            let new = format!("{}{}{}", gp, parent_idx + 1, if trail { "." } else { "" });
            // Replace the entire old number-prefix (incl. trailing space).
            let (old_num_len, _) = strip_number_prefix(body);
            let abs = line_off + indent.len() - 1; // -1 for stripped TAB
            self.buf.apply(abs, abs + old_num_len.saturating_sub(1), &new);
            self.cur_col = (indent.len() - 1) + new.len() + 1;
            self.want_col = self.cur_col;
        } else {
            self.cur_col = self.cur_col.saturating_sub(1);
            self.want_col = self.cur_col;
        }
        self.buf.end_compound();
    }

    /// Insert-mode `Ctrl-T` outside autonumber: vim-style indent — insert
    /// one TAB at the start of the current line; the cursor rides along.
    fn insert_indent(&mut self) {
        let line_off = self.buf.line_byte_offset(self.cur_line);
        self.buf.apply(line_off, line_off, "\t");
        self.cur_col += 1;
        self.want_col = self.cur_col;
    }

    /// Insert-mode `Ctrl-D` outside autonumber: vim-style outdent — drop one
    /// leading TAB, or up to `tabstop` leading spaces, from the current line.
    fn insert_outdent(&mut self) {
        let line = self.buf.line(self.cur_line);
        let remove = if line.starts_with('\t') {
            1
        } else {
            let ts = self.tabstop();
            line.chars().take(ts).take_while(|c| *c == ' ').count()
        };
        if remove == 0 { return; }
        let line_off = self.buf.line_byte_offset(self.cur_line);
        self.buf.apply(line_off, line_off + remove, "");
        self.cur_col = self.cur_col.saturating_sub(remove);
        self.want_col = self.cur_col;
    }

    fn showhide_pattern(&mut self, pattern: &str, show: bool) {
        if pattern.is_empty() { self.set_status(" empty pattern", 244); return; }
        let re = match regex::Regex::new(pattern) {
            Ok(r) => r,
            Err(e) => { self.set_status(&format!(" bad regex: {}", e), 196); return; }
        };
        let total = self.buf.line_count();
        self.folds.clear();
        let mut run_start: Option<usize> = None;
        let mut close_run = |start: usize, end: usize, folds: &mut fold::Folds| {
            if end > start { folds.close_range(start, end); }
        };
        for i in 0..total {
            let l = self.buf.line(i);
            let m = re.is_match(&l);
            let hide_this = if show { !m } else { m };
            if hide_this {
                if run_start.is_none() { run_start = Some(i); }
            } else if let Some(start) = run_start.take() {
                close_run(start, i - 1, &mut self.folds);
            }
        }
        if let Some(start) = run_start {
            close_run(start, total - 1, &mut self.folds);
        }
        self.showhide = Some((pattern.into(), show));
        self.set_status(&format!(" {}: /{}/", if show { "show" } else { "hide" }, pattern), 244);
    }

    fn calendar_add(&mut self) {
        let mut text = String::new();
        for chunk in self.buf.rope.chunks() { text.push_str(chunk); }
        let report = calendar::add_future_events(
            &text,
            self.calendar.as_deref(),
            self.alldates);
        let mode = if self.calendar.is_some() { "gcalcli" } else { "ics files" };
        if report.errors.is_empty() {
            self.set_status(&format!(" {} events posted via {}", report.posted, mode), 46);
        } else {
            self.set_status(
                &format!(" {} posted, {} errors (first: {})", report.posted, report.errors.len(),
                    report.errors.first().cloned().unwrap_or_default()),
                178);
        }
    }

    fn complexity_report(&mut self) {
        let mut items = 0usize;
        let mut refs = 0usize;
        for i in 0..self.buf.line_count() {
            let line = self.buf.line(i);
            if line.trim().is_empty() { continue; }
            items += 1;
            // Count `<…>` references on the line.
            let mut p = 0;
            while let Some(start) = line[p..].find('<') {
                let abs = p + start;
                if let Some(rel) = line[abs..].find('>') {
                    refs += 1;
                    p = abs + rel + 1;
                } else { break; }
            }
        }
        self.set_status(&format!(" complexity: {} items + {} refs = {}",
            items, refs, items + refs), 46);
    }

    /// `\H` / `\L` / `\M` (and `:export FMT`) — render the buffer to
    /// HTML / LaTeX / Markdown, write the output next to the source
    /// file, and report the path. Buffer content untouched.
    fn export_to(&mut self, fmt: &str) {
        let mut text = String::new();
        for chunk in self.buf.rope.chunks() { text.push_str(chunk); }
        let title = self.buf.path.as_ref()
            .and_then(|p| p.file_stem().and_then(|s| s.to_str()))
            .unwrap_or("HyperList")
            .to_string();
        // PDF goes through LaTeX → pdflatex (auto-landscape for wide items).
        if matches!(fmt, "pdf" | "p") {
            let target = match self.buf.path.as_ref() {
                Some(p) => p.with_extension("pdf"),
                None    => std::path::PathBuf::from("scribe-export.pdf"),
            };
            let latex = export::to_latex(&text, &title);
            match export::latex_to_pdf(&latex, &target) {
                Ok(_)  => self.set_status(&format!(" exported → {}", target.display()), 46),
                Err(e) => self.set_status(&format!(" pdf export failed: {}", e), 196),
            }
            return;
        }
        // docx / odt go through LibreOffice headless (soffice) — the only
        // converter that keeps inline colour / highlight / font-size. md
        // buffers are first rendered to HTML by pandoc (the spans pass
        // through); html buffers feed soffice directly.
        if matches!(fmt, "docx" | "odt") {
            match self.export_office(fmt) {
                Ok(p)  => self.set_status(&format!(" exported → {}", p.display()), 46),
                Err(e) => self.set_status(&format!(" {} export failed: {}", fmt, e), 196),
            }
            return;
        }
        let (rendered, ext) = match fmt {
            "html" | "h" => (export::to_html(&text, &title), "html"),
            "latex" | "tex" | "l" => (export::to_latex(&text, &title), "tex"),
            "markdown" | "md" | "m" => (export::to_markdown(&text, &title), "md"),
            _ => { self.set_status(&format!(" unknown export format: {}", fmt), 196); return; }
        };
        let target = match self.buf.path.as_ref() {
            Some(p) => p.with_extension(ext),
            None    => std::path::PathBuf::from(format!("scribe-export.{}", ext)),
        };
        match std::fs::write(&target, rendered) {
            Ok(_)  => self.set_status(&format!(" exported → {}", target.display()), 46),
            Err(e) => self.set_status(&format!(" export failed: {}", e), 196),
        }
    }

    /// `\C` in Visual mode — wrap the selection in an inline colour span
    /// (`<span style="color:#..;background-color:#..">…</span>`). prism
    /// supplies fg/bg. A white background is treated as "no highlight" and
    /// omitted. Colours live in the text as HTML, so they survive a
    /// Markdown/HTML save and export to docx/odt/pdf via soffice.
    fn color_visual(&mut self) {
        if !self.mode.is_visual() {
            self.set_status(" \\C — select text in Visual mode first", 244);
            return;
        }
        let (lo, hi) = if matches!(self.mode, Mode::VisualLine) {
            self.visual_line_range()
        } else {
            self.visual_range()
        };
        self.mode = Mode::Normal;
        self.pending.clear();
        let Some((fg_hex, bg_hex)) = self.pick_color_prism() else {
            self.set_status(" prism not available", 196);
            self.render_all();
            return;
        };
        let s = self.buf.rope.to_string();
        if lo >= hi || hi > s.len() { self.render_all(); return; }
        let sel = &s[lo..hi];
        let mut decls = format!("color:{}", fg_hex);
        if bg_hex.to_ascii_lowercase() != "#ffffff" {
            decls.push_str(&format!(";background-color:{}", bg_hex));
        }
        let wrapped = format!("<span style=\"{}\">{}</span>", decls, sel);
        self.buf.begin_compound();
        self.buf.apply(lo, hi, &wrapped);
        self.buf.end_compound();
        let (line, col) = self.buf.byte_to_line_col(lo);
        self.cur_line = line;
        self.cur_col = col;
        self.want_col = col;
        self.set_status(" coloured (\\C)", 46);
        self.render_all();
    }

    /// Launch prism as an fg/bg picker; returns (fg_hex, bg_hex) like
    /// "#rrggbb", or None if prism couldn't run. Mirrors grid's picker:
    /// prism writes `fg=`/`bg=` to a temp file (`--out`) so its TUI and
    /// scribe's don't fight over the screen.
    fn pick_color_prism(&mut self) -> Option<(String, String)> {
        let outfile = format!("/tmp/scribe_pick_{}.txt", std::process::id());
        let _ = std::fs::remove_file(&outfile);
        Crust::cleanup();
        let status = std::process::Command::new("prism")
            .arg("--pair")
            .arg(format!("--out={}", outfile))
            .arg("#000000")
            .arg("#ffffff")
            .status();
        Crust::init();
        Crust::clear_screen();
        self.header.invalidate();
        self.main_p.invalidate();
        self.footer.invalidate();
        if status.is_err() { let _ = std::fs::remove_file(&outfile); return None; }
        let mut fg = "#000000".to_string();
        let mut bg = "#ffffff".to_string();
        if let Ok(text) = std::fs::read_to_string(&outfile) {
            for line in text.lines() {
                if let Some(h) = line.strip_prefix("fg=") { fg = h.trim().to_string(); }
                else if let Some(h) = line.strip_prefix("bg=") { bg = h.trim().to_string(); }
            }
        }
        let _ = std::fs::remove_file(&outfile);
        Some((fg, bg))
    }

    /// `\F` in Visual mode — wrap the selection in an inline font span
    /// (`<span style="font-family:'X'; font-size:Npt">…</span>`). The `fonts`
    /// picker supplies family + size. Like colour, it lives in the text as
    /// HTML and survives a Markdown/HTML save and export to docx/odt/pdf.
    fn font_visual(&mut self) {
        if !self.mode.is_visual() {
            self.set_status(" \\F — select text in Visual mode first", 244);
            return;
        }
        let (lo, hi) = if matches!(self.mode, Mode::VisualLine) {
            self.visual_line_range()
        } else {
            self.visual_range()
        };
        self.mode = Mode::Normal;
        self.pending.clear();
        let Some((family, size)) = self.pick_font() else {
            self.set_status(" font pick cancelled", 244);
            self.render_all();
            return;
        };
        let s = self.buf.rope.to_string();
        if lo >= hi || hi > s.len() { self.render_all(); return; }
        let sel = &s[lo..hi];
        let mut decls = format!("font-family:'{}'", family);
        if size > 0 { decls.push_str(&format!(";font-size:{}pt", size)); }
        let wrapped = format!("<span style=\"{}\">{}</span>", decls, sel);
        self.buf.begin_compound();
        self.buf.apply(lo, hi, &wrapped);
        self.buf.end_compound();
        let (line, col) = self.buf.byte_to_line_col(lo);
        self.cur_line = line;
        self.cur_col = col;
        self.want_col = col;
        self.set_status(&format!(" font: {} {}pt (\\F)", family, size), 46);
        self.render_all();
    }

    /// `\M` — toggle concealment of inline colour/font `<span>` markup. When
    /// on, the tags hide on every line except the cursor's (so the prose reads
    /// clean but the markup stays editable where the cursor is).
    fn toggle_markup(&mut self) {
        self.markup_concealed = !self.markup_concealed;
        if self.markup_concealed {
            self.set_status(" markup hidden (\\M) — reveals on the cursor line", 46);
        } else {
            self.set_status(" markup shown (\\M)", 244);
        }
        self.main_p.invalidate();
        self.render_all();
    }

    /// Launch the `fonts` picker; returns (family, size_pt), or None if the
    /// user cancelled or `fonts` isn't on PATH. Mirrors pick_color_prism: the
    /// picker writes `family=`/`size=` to a temp file (`--out`) so its TUI and
    /// scribe's don't fight over the screen.
    fn pick_font(&mut self) -> Option<(String, u32)> {
        let outfile = format!("/tmp/scribe_font_{}.txt", std::process::id());
        let _ = std::fs::remove_file(&outfile);
        Crust::cleanup();
        let status = std::process::Command::new("fonts")
            .arg(format!("--out={}", outfile))
            .status();
        Crust::init();
        Crust::clear_screen();
        self.header.invalidate();
        self.main_p.invalidate();
        self.footer.invalidate();
        if status.is_err() { let _ = std::fs::remove_file(&outfile); return None; }
        // A cancelled picker exits non-zero and writes nothing.
        let text = std::fs::read_to_string(&outfile).ok();
        let _ = std::fs::remove_file(&outfile);
        let text = text?;
        let mut family = String::new();
        let mut size = 0u32;
        for line in text.lines() {
            if let Some(v) = line.strip_prefix("family=") { family = v.trim().to_string(); }
            else if let Some(v) = line.strip_prefix("size=") { size = v.trim().parse().unwrap_or(0); }
        }
        if family.is_empty() { None } else { Some((family, size)) }
    }

    /// Build an HTML form of the buffer and convert it to docx/odt with
    /// LibreOffice headless. Returns the output path. md → html via pandoc
    /// (keeps the colour spans); html buffers are used as-is. A private
    /// soffice profile avoids clashing with a running LibreOffice instance.
    fn export_office(&self, fmt: &str) -> Result<std::path::PathBuf, String> {
        use std::process::{Command, Stdio};
        let src = self.buf.path.clone().ok_or_else(|| "save the file first".to_string())?;
        let ext = src.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase();
        let mut text = String::new();
        for chunk in self.buf.rope.chunks() { text.push_str(chunk); }

        let tmpdir = std::env::temp_dir();
        let stem = src.file_stem().and_then(|s| s.to_str()).unwrap_or("scribe-export");
        let tag = format!("{}-{}", stem, std::process::id());
        let html_path = tmpdir.join(format!("{}.html", tag));

        if matches!(ext.as_str(), "html" | "htm") {
            std::fs::write(&html_path, &text).map_err(|e| format!("write html: {}", e))?;
        } else {
            // pandoc markdown -> standalone html5; raw <span> passes through.
            let child = Command::new("pandoc")
                .args(["-f", "markdown", "-t", "html5", "--standalone"])
                .arg("-o").arg(&html_path)
                .stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::null())
                .spawn();
            let res = match child {
                Ok(mut c) => {
                    if let Some(si) = c.stdin.as_mut() {
                        use std::io::Write;
                        let _ = si.write_all(text.as_bytes());
                    }
                    c.wait()
                }
                Err(e) => return Err(format!("pandoc not available ({})", e)),
            };
            match res {
                Ok(s) if s.success() => {}
                _ => return Err("pandoc conversion failed".into()),
            }
        }

        let profile = format!("file://{}/scribe-soffice-{}",
            tmpdir.display(), std::process::id());
        let filter = if fmt == "docx" { "docx:MS Word 2007 XML" } else { "odt" };
        let run = Command::new("soffice")
            .arg("--headless")
            .arg(format!("-env:UserInstallation={}", profile))
            .arg("--convert-to").arg(filter)
            .arg("--outdir").arg(&tmpdir)
            .arg(&html_path)
            .stdout(Stdio::null()).stderr(Stdio::null())
            .status();
        let _ = std::fs::remove_file(&html_path);
        match run {
            Ok(s) if s.success() => {}
            Ok(_)  => return Err("soffice conversion failed".into()),
            Err(e) => return Err(format!("soffice (LibreOffice) not available ({})", e)),
        }
        let produced = tmpdir.join(format!("{}.{}", tag, fmt));
        let target = src.with_extension(fmt);
        std::fs::rename(&produced, &target)
            .or_else(|_| std::fs::copy(&produced, &target).map(|_| ()))
            .map_err(|e| format!("move output: {}", e))?;
        let _ = std::fs::remove_file(&produced);
        Ok(target)
    }

    /// True if the buffer carries an inline colour span scribe wrote.
    fn buffer_has_color(&self) -> bool {
        let s = self.buf.rope.to_string();
        s.contains("<span style=\"color:") || s.contains("<span style=\"background-color:")
    }

    /// True if the open file is already a colour-preserving markup format.
    fn path_is_markup(&self) -> bool {
        self.buf.path.as_ref()
            .and_then(|p| p.extension()).and_then(|e| e.to_str())
            .map(|e| matches!(e.to_ascii_lowercase().as_str(),
                "md" | "markdown" | "html" | "htm"))
            .unwrap_or(false)
    }

    /// Plain save with status. Returns true on success.
    fn save_plain(&mut self) -> bool {
        match self.buf.save() {
            Ok(_)  => { self.set_status(" written", 46); true }
            Err(e) => { self.set_status(&format!(" save failed: {}", e), 196); false }
        }
    }

    /// Save, guarding colours: if the buffer has colour spans but the file
    /// isn't .md/.html, ask whether to save as Markdown/HTML (keeping the
    /// colours) or strip them and save plain. Returns true when saved.
    fn save_guarded(&mut self) -> bool {
        if !self.buffer_has_color() || self.path_is_markup() {
            return self.save_plain();
        }
        let ans = self.footer_prompt(
            "Colours need .md/.html — save as [m]d / [h]tml, or [d]iscard? ");
        match ans.trim().chars().next() {
            Some('m') | Some('M') => self.save_as_ext("md"),
            Some('h') | Some('H') => self.save_as_ext("html"),
            Some('d') | Some('D') => { self.strip_color_spans(); self.save_plain() }
            _ => { self.set_status(" save cancelled", 244); false }
        }
    }

    /// Repoint the buffer at <stem>.<ext> and save there (keeps colours).
    fn save_as_ext(&mut self, ext: &str) -> bool {
        let new_path = match self.buf.path.as_ref() {
            Some(p) => p.with_extension(ext),
            None    => std::path::PathBuf::from(format!("scribe-export.{}", ext)),
        };
        self.buf.path = Some(new_path.clone());
        match self.buf.save() {
            Ok(_)  => { self.set_status(&format!(" written → {}", new_path.display()), 46); true }
            Err(e) => { self.set_status(&format!(" save failed: {}", e), 196); false }
        }
    }

    /// Remove scribe's colour spans, keeping their inner text. One undo step.
    fn strip_color_spans(&mut self) {
        let s = self.buf.rope.to_string();
        let re = regex::Regex::new(
            r#"(?s)<span style="(?:color|background-color):[^"]*">(.*?)</span>"#).unwrap();
        let stripped = re.replace_all(&s, "$1").into_owned();
        if stripped != s {
            self.buf.begin_compound();
            self.buf.apply(0, s.len(), &stripped);
            self.buf.end_compound();
        }
    }

    /// Step `count` visible lines down from `from`. Lines hidden by
    /// closed folds are skipped. Stops at the last visible line.
    fn next_visible_line_down(&self, from: usize, count: usize) -> usize {
        let total = self.buf.line_count();
        if total == 0 { return 0; }
        let all: Vec<String> = (0..total).map(|i| self.buf.line(i)).collect();
        let mut cur = from;
        let mut steps = 0;
        while steps < count {
            let mut next = cur + 1;
            while next < total && !self.folds.is_visible(next, &all) { next += 1; }
            if next >= total { break; }
            cur = next;
            steps += 1;
        }
        cur
    }

    /// Step `count` visible lines up from `from`. Symmetrical with
    /// next_visible_line_down. Stops at line 0.
    fn next_visible_line_up(&self, from: usize, count: usize) -> usize {
        let total = self.buf.line_count();
        if total == 0 { return 0; }
        let all: Vec<String> = (0..total).map(|i| self.buf.line(i)).collect();
        let mut cur = from;
        let mut steps = 0;
        while steps < count && cur > 0 {
            let mut next = cur - 1;
            while next > 0 && !self.folds.is_visible(next, &all) { next -= 1; }
            if !self.folds.is_visible(next, &all) { break; }
            cur = next;
            steps += 1;
        }
        cur
    }

    fn enter_insert(&mut self) {
        self.mode = Mode::Insert;
        self.capturing_insert = true;
        self.captured_insert.clear();
    }

    /// `R` from Normal — enter Replace mode. Same captured-keys
    /// machinery as Insert so dot-repeat replays the typed chars.
    fn enter_replace(&mut self) {
        self.mode = Mode::Replace;
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
        if matches!(kind, Mode::VisualBlock) {
            let line = self.buf.line(self.cur_line);
            let ts = self.tabstop();
            let v = display_col(&line, self.cur_col.min(line.len()), ts);
            self.vblock_anchor_vcol = v;
            self.vblock_cur_vcol = v;
        }
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
        // `r` <char>: replace every char in the selection. Checked
        // first so the replacement char isn't re-interpreted as a
        // visual command.
        if self.visual_replace_pending {
            self.visual_replace_pending = false;
            if key == "ESC" || key == "C-[" || key == "C-C" {
                self.mode = Mode::Normal;
                self.pending.clear();
                return false;
            }
            if let Some(ch) = key_to_literal_char(key) {
                self.apply_visual_replace(ch);
            } else {
                self.mode = Mode::Normal;
                self.pending.clear();
            }
            return false;
        }
        // Leader (`\`) prefix dispatch — works in Visual just like in
        // Normal so `\s` (sort), `\R` (renumber), `\z`/`\x`
        // (encrypt/decrypt visual range), etc. fire instead of `s`
        // being treated as substitute on the selection.
        if let Some(group) = self.leader_sub.take() {
            return self.handle_leader_sub(group, key);
        }
        if self.leader_prefix {
            self.leader_prefix = false;
            if key == "e" || key == "x" {
                self.leader_sub = Some(key.chars().next().unwrap());
                self.set_status(
                    if key == "e" { " \\e — e=encrypt  d=decrypt  k=rekey" }
                    else          { " \\x — h=HTML  l=LaTeX  m=Markdown  p=PDF  d=docx  o=odt" },
                    244);
                return false;
            }
            return self.handle_leader(key);
        }
        if key == "\\" { self.leader_prefix = true; return false; }
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
            "r" => {
                // Arm: next key is the replacement char (handled at the
                // top of handle_visual on the following call).
                self.visual_replace_pending = true;
                return false;
            }
            "y" | "Y" => {
                self.apply_visual_op('y');
                return false;
            }
            // `<` / `>` shift the selection. In vim these are always
            // linewise even when the visual selection is charwise —
            // partial-line indenting isn't a concept. apply_visual_op
            // handles the VisualLine / VisualBlock dispatch already;
            // for charwise Visual we widen the range to whole lines
            // here so the user sees the same behaviour vim ships.
            ">" | "<" => {
                let opc = key.chars().next().unwrap();
                if matches!(self.mode, Mode::Visual) {
                    // Force linewise: snap to line bounds and call
                    // execute_op_linewise directly so the charwise
                    // selection edge cases don't slip through.
                    self.pending.operator = Some(opc);
                    let cur = self.cursor_byte();
                    let lo_byte = cur.min(self.visual_anchor);
                    let hi_byte = cur.max(self.visual_anchor);
                    let l1 = self.buf.byte_to_line_col(lo_byte).0;
                    let l2 = self.buf.byte_to_line_col(hi_byte).0;
                    self.cur_line = l1;
                    self.execute_op_linewise(l1, l2 - l1);
                    self.mode = Mode::Normal;
                    self.pending.clear();
                } else {
                    self.apply_visual_op(opc);
                }
                return false;
            }
            "~" => {
                self.apply_visual_case(CaseOp::Toggle);
                return false;
            }
            "u" => {
                self.apply_visual_case(CaseOp::Lower);
                return false;
            }
            "U" => {
                self.apply_visual_case(CaseOp::Upper);
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

        // VisualBlock: h/l/j/k drive vcol/row directly so the block
        // stays a perfect rectangle through tabs and short lines. Other
        // motions (w/b/$/^/G/…) fall through to normal motion handling
        // and we re-derive vcol from the resulting cur_col afterwards.
        if matches!(self.mode, Mode::VisualBlock) {
            let n = self.pending.count();
            let consumed = match key {
                "h" | "LEFT" => {
                    self.vblock_cur_vcol = self.vblock_cur_vcol.saturating_sub(n);
                    self.sync_vblock_cursor();
                    true
                }
                "l" | "RIGHT" => {
                    self.vblock_cur_vcol = self.vblock_cur_vcol.saturating_add(n);
                    self.sync_vblock_cursor();
                    true
                }
                "j" | "DOWN" => {
                    let last = self.buf.line_count().saturating_sub(1);
                    self.cur_line = (self.cur_line + n).min(last);
                    self.sync_vblock_cursor();
                    true
                }
                "k" | "UP" => {
                    self.cur_line = self.cur_line.saturating_sub(n);
                    self.sync_vblock_cursor();
                    true
                }
                _ => false,
            };
            if consumed { self.pending.clear(); return false; }
        }

        // Selection wraps across line boundaries on `h`/`l` in
        // Visual / VisualLine. Vim defaults stop at the line edge
        // here, which is annoying when marking a sentence that
        // straddles a soft wrap: you can't extend past the line
        // end without a separate `j` step. Map `h` → `LEFT` and
        // `l` → `RIGHT` (already wrap-aware) in the non-block
        // visual modes only — VisualBlock still wants column-locked
        // semantics, handled above.
        let key = if matches!(self.mode, Mode::Visual | Mode::VisualLine) {
            match key { "h" => "LEFT", "l" => "RIGHT", k => k }
        } else { key };

        // Otherwise treat as motion to extend selection.
        if let Some(target) = self.parse_motion(key, self.pending.count()) {
            self.cursor_to_byte(target);
            // After non-h/l/j/k motion in VisualBlock, the user moved
            // by word/line/etc. — pin the new vcol to wherever cur_col
            // landed so the rectangle picks up the new column.
            if matches!(self.mode, Mode::VisualBlock) {
                let line = self.buf.line(self.cur_line);
                let ts = self.tabstop();
                self.vblock_cur_vcol = display_col(&line, self.cur_col.min(line.len()), ts);
            }
        }
        self.pending.clear();
        false
    }

    /// In VisualBlock, after changing `vblock_cur_vcol` or `cur_line`,
    /// snap `cur_col` to the byte at (or just inside) the desired vcol
    /// on the new line. The vcol field stays intact even if the line
    /// is shorter — that's how the rectangle keeps its right edge
    /// when the cursor passes through stubby lines.
    fn sync_vblock_cursor(&mut self) {
        let line = self.buf.line(self.cur_line);
        let ts = self.tabstop();
        let (b, _) = byte_at_or_past_col(&line, self.vblock_cur_vcol, ts);
        self.cur_col = b.min(line.len());
        self.want_col = self.cur_col;
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

    /// Lower / upper / toggle every char in the active visual selection.
    /// Linewise selection covers full lines; charwise is byte-exact.
    fn apply_visual_case(&mut self, op: CaseOp) {
        let (s, e) = match self.mode {
            Mode::Visual => self.visual_range(),
            Mode::VisualLine => self.visual_line_range(),
            _ => return,
        };
        let text: String = self.buf.rope.byte_slice(s..e).to_string();
        // Unicode-aware case mapping (not to_ascii_*): æøåÆØÅ and other
        // non-ASCII letters must fold too. char::to_lowercase /
        // to_uppercase yield iterators (a few chars expand, e.g. ß),
        // so build the string with `extend`.
        let mut transformed = String::with_capacity(text.len());
        for c in text.chars() {
            match op {
                CaseOp::Lower => transformed.extend(c.to_lowercase()),
                CaseOp::Upper => transformed.extend(c.to_uppercase()),
                CaseOp::Toggle => {
                    if c.is_uppercase() { transformed.extend(c.to_lowercase()); }
                    else if c.is_lowercase() { transformed.extend(c.to_uppercase()); }
                    else { transformed.push(c); }
                }
            }
        }
        self.buf.apply(s, e, &transformed);
        self.cursor_to_byte(s);
        self.mode = Mode::Normal;
    }

    /// Visual Block (Ctrl-v): apply op to each line at the same column range.
    fn apply_visual_block_op(&mut self, op: char) {
        let l1 = self.visual_anchor_line.min(self.cur_line);
        let l2 = self.visual_anchor_line.max(self.cur_line);
        // Display-column bounds — the block is a rectangle in screen
        // space, not in byte space. Per-line we resolve these to byte
        // ranges, accepting that a tab may be partially covered and
        // that short lines may contribute the empty string.
        let v1 = self.vblock_anchor_vcol.min(self.vblock_cur_vcol);
        let v2 = self.vblock_anchor_vcol.max(self.vblock_cur_vcol);
        let ts = self.tabstop();
        // Group all per-line edits into one undo node so a single `u`
        // reverses the entire block op.
        if op != 'y' { self.buf.begin_compound(); }
        let mut yanked: Vec<String> = Vec::new();
        // Walk lines from bottom up so earlier byte offsets remain valid.
        for line in (l1..=l2).rev() {
            let line_text = self.buf.line(line);
            let (start_byte, start_vcol) = byte_at_or_past_col(&line_text, v1, ts);
            let (end_byte, _)            = byte_at_or_past_col(&line_text, v2 + 1, ts);
            // Pad the yanked chunk on the left when the line is too
            // short to reach v1 — keeps the block rectangular when
            // the user later pastes it elsewhere.
            let left_pad = v1.saturating_sub(start_vcol);
            let chunk = if start_byte >= line_text.len() {
                " ".repeat(left_pad)
            } else {
                let mut s = " ".repeat(left_pad);
                s.push_str(&line_text[start_byte..end_byte]);
                s
            };
            yanked.push(chunk);
            if op != 'y' && end_byte > start_byte {
                let line_off = self.buf.line_byte_offset(line);
                self.buf.apply(line_off + start_byte, line_off + end_byte, "");
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
        // Snap cursor to the block's top-left corner in byte terms.
        self.cur_line = l1;
        let top_line = self.buf.line(self.cur_line);
        let (b, _) = byte_at_or_past_col(&top_line, v1, ts);
        self.cur_col = b.min(top_line.len());
        self.want_col = self.cur_col;
        if op == 'c' {
            self.enter_insert();
            // Arm block-insert replication: text typed on the top line
            // (l1) gets copied to l1+1..=l2 at column v1 on ESC. Empty
            // range (single-line block) → nothing to replicate.
            let lines: Vec<usize> = ((l1 + 1)..=l2).collect();
            if !lines.is_empty() {
                self.block_insert = Some(BlockInsert { vcol: v1, lines });
            }
        }
    }

    /// Copy `text` into each line of a block-insert at display column
    /// `vcol`. Called on ESC after a Visual Block `c`/`s`. Lines too
    /// short to reach `vcol` are left untouched (vim behaviour).
    fn replicate_block_insert(&mut self, vcol: usize, lines: &[usize], text: &str) {
        if text.is_empty() { return; }
        let ts = self.tabstop();
        self.buf.begin_compound();
        // Bottom-up so earlier line byte offsets stay valid as we edit.
        let mut sorted: Vec<usize> = lines.to_vec();
        sorted.sort_unstable();
        for &line in sorted.iter().rev() {
            if line >= self.buf.line_count() { continue; }
            let line_text = self.buf.line(line);
            let (byte, start_vcol) = byte_at_or_past_col(&line_text, vcol, ts);
            if start_vcol < vcol { continue; } // line ends before the block column
            let off = self.buf.line_byte_offset(line) + byte;
            self.buf.apply(off, off, text);
        }
        self.buf.end_compound();
    }

    /// Replace every character in the active visual selection with
    /// `ch` (vim's `r` in Visual / VisualLine / VisualBlock). Newlines
    /// in a charwise/linewise selection are preserved.
    fn apply_visual_replace(&mut self, ch: char) {
        let repl_str = ch.to_string();
        match self.mode {
            Mode::VisualBlock => {
                let l1 = self.visual_anchor_line.min(self.cur_line);
                let l2 = self.visual_anchor_line.max(self.cur_line);
                let v1 = self.vblock_anchor_vcol.min(self.vblock_cur_vcol);
                let v2 = self.vblock_anchor_vcol.max(self.vblock_cur_vcol);
                let ts = self.tabstop();
                self.buf.begin_compound();
                for line in (l1..=l2).rev() {
                    let line_text = self.buf.line(line);
                    let (sb, _) = byte_at_or_past_col(&line_text, v1, ts);
                    let (eb, _) = byte_at_or_past_col(&line_text, v2 + 1, ts);
                    if eb <= sb { continue; } // line doesn't reach the block
                    // Preserve cell count: one `ch` per existing char.
                    let n = line_text[sb..eb].chars().count();
                    let repl: String = std::iter::repeat(ch).take(n).collect();
                    let off = self.buf.line_byte_offset(line);
                    self.buf.apply(off + sb, off + eb, &repl);
                }
                self.buf.end_compound();
                self.cur_line = l1;
                let top = self.buf.line(self.cur_line);
                let (b, _) = byte_at_or_past_col(&top, v1, ts);
                self.cur_col = b.min(top.len());
                self.want_col = self.cur_col;
            }
            Mode::Visual | Mode::VisualLine => {
                let (s, e) = if matches!(self.mode, Mode::VisualLine) {
                    self.visual_line_range()
                } else {
                    self.visual_range()
                };
                let text = self.buf.rope.byte_slice(s..e).to_string();
                let repl: String = text.chars()
                    .map(|c| if c == '\n' { '\n' } else { ch }).collect();
                self.buf.apply(s, e, &repl);
                self.cursor_to_byte(s);
            }
            _ => { let _ = repl_str; }
        }
        self.mode = Mode::Normal;
        self.pending.clear();
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
                // Enter insert FIRST so col_cap allows landing one past
                // the last char. Otherwise `C` on "TEST" with cursor on
                // E deletes ESC(EST), leaves "T", then cursor_to_byte(1)
                // gets clamped to len-1=0 (Normal mode cap) and the
                // cursor sits on T instead of after it.
                self.enter_insert();
                self.cursor_to_byte(start);
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
        // Block paste: lay each line of the yank at the same DISPLAY
        // column on consecutive buffer lines, padding short lines with
        // spaces so the rectangle keeps its shape regardless of tabs
        // on the destination lines. Append new buffer lines if we run
        // out. Cursor lands at the top-left of the inserted block.
        if yank.kind == YankKind::Block {
            self.buf.begin_compound();
            let ts = self.tabstop();
            let lines: Vec<&str> = yank.text.split('\n').collect();
            // Target display column. `p` (after) inserts to the right
            // of the cursor cell; `P` (before) inserts at the cursor
            // cell. End-of-line sticks to the cell's display col.
            let cur_line_text = self.buf.line(self.cur_line);
            let cur_disp = display_col(&cur_line_text, self.cur_col.min(cur_line_text.len()), ts);
            let target_v = if after && self.cur_col < cur_line_text.len() {
                cur_disp + 1
            } else {
                cur_disp
            };
            for (i, chunk) in lines.iter().enumerate() {
                let mut chunk_n = String::new();
                for _ in 0..count.max(1) { chunk_n.push_str(chunk); }
                let bl = self.cur_line + i;
                if bl >= self.buf.line_count() {
                    // Append a brand new line. No tabs to worry about,
                    // so target_v == byte count of leading spaces.
                    let end = self.buf.rope.len_bytes();
                    let mut payload = String::new();
                    if !self.buf.rope.to_string().ends_with('\n') { payload.push('\n'); }
                    payload.push_str(&" ".repeat(target_v));
                    payload.push_str(&chunk_n);
                    self.buf.apply(end, end, &payload);
                    continue;
                }
                let line_text = self.buf.line(bl);
                let line_off = self.buf.line_byte_offset(bl);
                let line_disp_w = display_col(&line_text, line_text.len(), ts);
                if line_disp_w >= target_v {
                    // Destination already reaches target_v — insert
                    // chunk at byte that aligns with target_v. If
                    // target_v lands inside a tab's expansion, snap
                    // to the tab's start (vim behaviour).
                    let (b, _) = byte_at_or_past_col(&line_text, target_v, ts);
                    let insert_at = line_off + b;
                    self.buf.apply(insert_at, insert_at, &chunk_n);
                } else {
                    // Destination line shorter than target_v — pad
                    // with spaces from end-of-line up to target_v,
                    // then append chunk.
                    let pad_w = target_v - line_disp_w;
                    let mut payload = " ".repeat(pad_w);
                    payload.push_str(&chunk_n);
                    let end_byte = line_off + line_text.len();
                    self.buf.apply(end_byte, end_byte, &payload);
                }
            }
            // Cursor lands at the byte for target_v on the top line.
            let top_line = self.buf.line(self.cur_line);
            let (b, _) = byte_at_or_past_col(&top_line, target_v, ts);
            self.cur_col = b.min(top_line.len());
            self.want_col = self.cur_col;
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
    /// After the user just typed a boundary character (space, punct,
    /// enter — anything not in the abbrev-char set), look back at the
    /// preceding sequence of abbrev chars and, if it matches a stored
    /// trigger, replace it with the expansion. Cursor follows so the
    /// boundary char stays at the new end of the expanded text.
    /// Returns true if an expansion fired.
    fn try_expand_abbrev(&mut self, boundary_byte_len: usize) -> bool {
        if self.abbrev.is_empty() { return false; }
        let cur_off = self.cursor_byte();
        if cur_off < boundary_byte_len { return false; }
        let pre_boundary = cur_off - boundary_byte_len;
        let s = self.buf.rope.to_string();
        // Walk backwards from pre_boundary over abbrev chars to find
        // the trigger's start. Stop on the first non-abbrev byte.
        let mut start = pre_boundary;
        while start > 0 {
            // Step back to the start of the previous char.
            let mut p = start - 1;
            while p > 0 && !s.is_char_boundary(p) { p -= 1; }
            let ch = match s[p..start].chars().next() {
                Some(c) => c,
                None => break,
            };
            if !is_abbrev_char(ch) { break; }
            start = p;
        }
        if start == pre_boundary { return false; }
        let trigger = &s[start..pre_boundary];
        let expansion = match self.abbrev.get(trigger) {
            Some(e) => e.clone(),
            None => return false,
        };
        // Splice: replace [start, pre_boundary) with expansion. The
        // boundary char already in [pre_boundary, cur_off) stays in
        // place — re-attached at the new tail.
        self.buf.apply(start, pre_boundary, &expansion);
        let new_cursor = start + expansion.len() + boundary_byte_len;
        let (line, col) = self.buf.byte_to_line_col(new_cursor);
        self.cur_line = line;
        self.cur_col = col;
        self.want_col = self.cur_col;
        // Reflect the expanded text in the captured insert so dot
        // replays the expansion (not the trigger).
        if self.capturing_insert {
            // Find the most-recent occurrence of trigger in the
            // captured stream and replace the LAST one. Simplest
            // correct heuristic: rfind on the whole captured text.
            if let Some(pos) = self.captured_insert.rfind(trigger) {
                let end = pos + trigger.len();
                self.captured_insert.replace_range(pos..end, &expansion);
            }
        }
        true
    }

    fn handle_insert(&mut self, key: &str) -> bool {
        if let Some(q) = self.try_keymap("insert", key) { return q; }
        // <C-R> prefix: next keystroke is the register to paste from.
        // ESC / Ctrl-C cancels. Consumed before keymap-aware dispatch
        // so user-registered Insert keymaps can't accidentally swallow
        // a register letter.
        if self.insert_reg_prefix {
            self.insert_reg_prefix = false;
            if key == "ESC" || key == "C-C" { return false; }
            if let Some(c) = key.chars().next() {
                self.insert_from_register(c);
            }
            return false;
        }
        match key {
            // <Ins> toggles to Replace mode mid-stream (vim parity).
            "INS" => { self.mode = Mode::Replace; return false; }
            // <C-R>: arm the register-paste prefix (vim parity).
            "C-R" => { self.insert_reg_prefix = true; return false; }
            "ESC" | "C-[" | "C-C" => {
                self.mode = Mode::Normal;
                self.clamp_col_to_line();
                let block = self.block_insert.take();
                let mut block_text = String::new();
                if self.capturing_insert {
                    let captured = std::mem::take(&mut self.captured_insert);
                    self.capturing_insert = false;
                    if block.is_some() { block_text = captured.clone(); }
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
                // Block-insert replication: copy the top-line text onto
                // the rest of the block. vim breaks the block insert if
                // a newline was typed, so only single-line inserts
                // replicate.
                if let Some(bi) = block {
                    if !block_text.is_empty() && !block_text.contains('\n') {
                        self.replicate_block_insert(bi.vcol, &bi.lines, &block_text);
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
            // Page motion in Insert mode (vim parity — PageUp/PageDown
            // work mid-insert). Half-page steps via repeated move_up /
            // move_down so the insert-mode column clamp (one past EOL)
            // and want_col tracking match the single-arrow behaviour.
            "PgUP" => {
                let step = (self.main_p.h as usize) / 2;
                for _ in 0..step { self.move_up(); }
            }
            "PgDOWN" => {
                let step = (self.main_p.h as usize) / 2;
                for _ in 0..step { self.move_down(); }
            }
            // Ctrl-Up / Ctrl-Down move the current line (parity with
            // Normal mode). Capturing-insert recording sees this as
            // an opaque action; replay via dot still works because
            // the line motion is part of LastChange::Insert's text
            // boundary, not its content.
            "C-UP"   => self.move_line_up(),
            "C-DOWN" => self.move_line_down(),
            "HOME"  => { self.cur_col = 0; self.want_col = 0; }
            "END"   => { self.cur_col = self.col_cap(); self.want_col = self.cur_col; }
            // Ctrl-Home / Ctrl-End — first / last line of file, vim's
            // `gg` / `G` accessible without leaving the current mode.
            "C-HOME" => {
                self.cur_line = 0;
                self.cur_col = 0;
                self.want_col = 0;
            }
            "C-END"  => {
                self.cur_line = self.buf.line_count().saturating_sub(1);
                self.cur_col = self.current_line_len();
                self.want_col = self.cur_col;
            }
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
            // Vim's digraph-input key. Scribe overloads it to launch
            // a browseable picker (digraphs + emoji) rather than
            // strict Ctrl-K X Y two-char entry — the picker has a
            // search box that subsumes that use case.
            "C-K" => {
                let glyph = picker::pick(
                    picker::InitialTab::All,
                    &mut [&mut self.header, &mut self.main_p, &mut self.footer],
                );
                if let Some(g) = glyph {
                    self.insert_text_at_cursor(&g);
                }
                self.render_all();
            }
            "C-T" if self.autonumber => self.autonum_indent_in(),
            "C-D" if self.autonumber => self.autonum_indent_out(),
            // Vim insert-mode indent / outdent (plain buffers): one tab in,
            // one tab (or tabstop spaces) out, at the start of the line.
            "C-T" => self.insert_indent(),
            "C-D" => self.insert_outdent(),
            "ENTER" | "\n" | "\r" | "C-M" | "C-J" if self.autonumber => {
                self.autonum_newline();
            }
            "ENTER" | "\n" | "\r" | "C-M" | "C-J" => {
                let off = self.cursor_byte();
                self.buf.apply(off, off, "\n");
                self.cur_line += 1;
                self.cur_col = 0;
                self.want_col = 0;
                if self.capturing_insert { self.captured_insert.push('\n'); }
                // Newline is an abbrev boundary too — fire expansion
                // for the trigger that ended on the previous column.
                self.try_expand_abbrev(1);
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
                        // Abbreviation expansion: when the just-typed
                        // char is a boundary (anything not a-z, 0-9,
                        // `-`, `_`), check if the preceding word is a
                        // registered trigger and substitute.
                        if !is_abbrev_char(c) {
                            self.try_expand_abbrev(other.len());
                        }
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

    /// Insert at cursor from register `name` (Insert-mode `<C-R>{c}`
    /// dispatch). `=` evaluates an arithmetic expression typed in
    /// the footer prompt and inserts the result. `+` / `*` are the
    /// system-clipboard slots, the rest are named/numbered/unnamed
    /// slots as in Normal-mode paste.
    fn insert_from_register(&mut self, name: char) {
        let text = if name == '=' {
            match self.eval_expr_prompt() { Some(s) => s, None => return }
        } else {
            match self.regs.get(name) {
                Some(y) => y.text.clone(),
                None    => {
                    self.set_status(&format!(" register {} empty", name), 244);
                    return;
                }
            }
        };
        if text.is_empty() { return; }
        self.insert_text_at_cursor(&text);
    }

    /// Prompt for an arithmetic expression and return its formatted
    /// value. None on empty / invalid input (with a status hint in
    /// the latter case).
    fn eval_expr_prompt(&mut self) -> Option<String> {
        let raw = self.footer.ask_with_bg("=", "", 17);
        self.render_footer();
        let s = raw.trim();
        if s.is_empty() { return None; }
        match eval_math(s) {
            Some(v) => Some(fmt_math(v)),
            None    => {
                self.set_status(&format!(" bad expression: {}", s), 196);
                None
            }
        }
    }

    /// Splice `text` at the current cursor as one compound undo and
    /// advance the cursor past the insertion. Updates capturing_insert
    /// so a dot-repeat after a `c`-op replays the inserted text too.
    fn insert_text_at_cursor(&mut self, text: &str) {
        if text.is_empty() { return; }
        let off = self.cursor_byte();
        self.buf.begin_compound();
        self.buf.apply(off, off, text);
        self.buf.end_compound();
        let new_off = off + text.len();
        let (line, col) = self.buf.byte_to_line_col(new_off);
        self.cur_line = line;
        self.cur_col  = col;
        self.want_col = self.cur_col;
        if self.capturing_insert { self.captured_insert.push_str(text); }
    }

    // ── Replace mode ───────────────────────────────────────────────────
    /// Like Insert mode but typed printable chars OVERWRITE the char
    /// at the cursor (extending the line if cursor is past EOL).
    /// `<Ins>` toggles back to Insert. ESC / Ctrl-[ / Ctrl-C exit
    /// to Normal. Backspace moves the cursor left without restoring
    /// the original char (vim's exact "restore previous content"
    /// behavior would require tracking each overwritten byte; not
    /// worth the bookkeeping — `u` undoes the whole replace turn).
    fn handle_replace(&mut self, key: &str) -> bool {
        if let Some(q) = self.try_keymap("insert", key) { return q; }
        match key {
            "INS" => { self.mode = Mode::Insert; return false; }
            "ESC" | "C-[" | "C-C" => {
                self.mode = Mode::Normal;
                self.clamp_col_to_line();
                if self.capturing_insert {
                    let captured = std::mem::take(&mut self.captured_insert);
                    self.capturing_insert = false;
                    if !captured.is_empty() {
                        self.last_change = Some(LastChange::Insert {
                            text: captured,
                            append: false,
                        });
                    }
                }
                if self.spell_enabled { self.recheck_spell(); }
            }
            "LEFT"  => self.move_left_wrap(),
            "RIGHT" => self.move_right_wrap(),
            "UP"    => self.move_up(),
            "DOWN"  => self.move_down(),
            "HOME"  => { self.cur_col = 0; self.want_col = 0; }
            "END"   => { self.cur_col = self.col_cap(); self.want_col = self.cur_col; }
            // Ctrl-Home / Ctrl-End — first / last line of file, vim's
            // `gg` / `G` accessible without leaving the current mode.
            "C-HOME" => {
                self.cur_line = 0;
                self.cur_col = 0;
                self.want_col = 0;
            }
            "C-END"  => {
                self.cur_line = self.buf.line_count().saturating_sub(1);
                self.cur_col = self.current_line_len();
                self.want_col = self.cur_col;
            }
            "BACK" | "BACKSPACE" | "C-H" => {
                // Step the cursor back without restoring; user can `u`
                // to revert the whole replace turn.
                if self.cur_col > 0 {
                    let line = self.buf.line(self.cur_line);
                    let mut p = self.cur_col - 1;
                    while p > 0 && !line.is_char_boundary(p) { p -= 1; }
                    self.cur_col = p;
                    self.want_col = self.cur_col;
                } else if self.cur_line > 0 {
                    self.cur_line -= 1;
                    self.cur_col = self.col_cap();
                    self.want_col = self.cur_col;
                }
            }
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
                // Newline always inserts (no char to overwrite makes sense).
                let off = self.cursor_byte();
                self.buf.apply(off, off, "\n");
                self.cur_line += 1;
                self.cur_col = 0;
                self.want_col = 0;
                if self.capturing_insert { self.captured_insert.push('\n'); }
            }
            "TAB" | "\t" => {
                // Treat like a printable char — overwrites at cursor.
                self.replace_char_at_cursor("\t");
                if self.capturing_insert { self.captured_insert.push('\t'); }
            }
            other => {
                if other.chars().count() == 1 {
                    let c = other.chars().next().unwrap();
                    if !c.is_control() {
                        self.replace_char_at_cursor(other);
                        if self.capturing_insert { self.captured_insert.push_str(other); }
                    }
                }
            }
        }
        false
    }

    /// Overwrite the char at the cursor with `s` (which is one
    /// grapheme — single keystroke in practice). If the cursor is
    /// at or past EOL, append instead. Cursor moves to just past
    /// the inserted/replaced char.
    fn replace_char_at_cursor(&mut self, s: &str) {
        let line = self.buf.line(self.cur_line);
        let off = self.cursor_byte();
        if self.cur_col >= line.len() {
            // Past EOL — append.
            self.buf.apply(off, off, s);
        } else {
            // Find end of char at cursor, replace it.
            let mut end_col = self.cur_col + 1;
            while end_col < line.len() && !line.is_char_boundary(end_col) { end_col += 1; }
            let line_off = self.buf.line_byte_offset(self.cur_line);
            self.buf.apply(off, line_off + end_col, s);
        }
        self.cur_col += s.len();
        self.want_col = self.cur_col;
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
        // Hook tab-completion in for the colon prompt only — clear
        // after so other footer prompts (search, etc.) don't inherit
        // it.
        self.footer.completer = Some(complete_colon_command);
        let cmd = self.footer.ask_with_bg(":", "", 17);
        self.footer.completer = None;
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

        // Hide the terminal cursor — otherwise it stays parked on the
        // buffer beneath the popup and renders as a stray block
        // through the popup's interior.
        Cursor::hide();

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
        Cursor::show();
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

    /// Modal `:help` popup. Renders the embedded README (or HYPERLIST.md
    /// when `topic` is `hl` / `hyperlist`) with the markdown highlighter
    /// and shows it in a near-full-screen scrollable popup. ESC / `q`
    /// dismisses and returns to the current buffer untouched — no save,
    /// no buffer swap, no friction.
    fn open_help(&mut self, topic: &str) {
        const HELP_MAIN: &str = include_str!("../README.md");
        const HELP_HL:   &str = include_str!("../HYPERLIST.md");
        let text = match topic.trim().to_lowercase().as_str() {
            "hl" | "hyperlist" => HELP_HL,
            _ => HELP_MAIN,
        };
        // Run the markdown highlighter over the embedded text. The
        // `highlight_markdown` (not `_source`) variant formats tables
        // into aligned column rules, which is what we want for a
        // read-only viewer.
        let line_count = text.lines().count();
        let rendered = highlight::highlight_markdown(text, line_count + 1);

        // Near-full-screen popup. The README has tables sized up to
        // ~100 chars, so cap width there and let height take the rest.
        let (cols, rows) = Crust::terminal_size();
        let popup_w = (cols.saturating_sub(2)).min(110).max(50);
        // Height fits two rows of chrome top+bottom. After centering,
        // shift y down one more row so the popup's top border doesn't
        // sit immediately below the header (otherwise the two visually
        // "melt" together).
        let popup_h = (rows.saturating_sub(6)).max(12);
        let mut popup = Popup::centered(popup_w, popup_h, 252, 236);
        popup.pane.y = popup.pane.y.saturating_add(1);

        let topic_label = match topic.trim().to_lowercase().as_str() {
            "hl" | "hyperlist" => "HyperList",
            _ => "README",
        };
        let mut lines: Vec<String> = Vec::new();
        lines.push(format!("  {}",
            style::bold(&style::fg(&format!(":help — {} (ESC/q close, j/k scroll)", topic_label), 220))));
        lines.push(String::new());
        for ln in rendered.lines() {
            lines.push(ln.to_string());
        }

        Cursor::hide();
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
        Cursor::show();
        self.render_all();
    }

    /// `:keys` popup — comprehensive keybinding cheatsheet, grouped by
    /// category. Modal, scrollable, ESC / `q` to close. Built once per
    /// invocation so adding a binding only requires updating one spot
    /// in this function (not regenerating the README first).
    fn show_keys_popup(&mut self) {
        let (cols, rows) = Crust::terminal_size();
        let popup_w = (cols.saturating_sub(2)).min(78).max(50);
        // Height fits two rows of chrome top+bottom. After centering,
        // shift y down one more row so the popup's top border doesn't
        // sit immediately below the header (otherwise the two visually
        // "melt" together).
        let popup_h = (rows.saturating_sub(6)).max(12);
        let mut popup = Popup::centered(popup_w, popup_h, 252, 236);
        popup.pane.y = popup.pane.y.saturating_add(1);

        let head = |s: &str| style::bold(&style::fg(s, 81));
        let key  = |s: &str| style::fg(s, 220);
        let rule = style::fg(&"-".repeat(popup_w as usize - 4), 238);

        let mut lines: Vec<String> = Vec::new();
        lines.push(String::new());
        lines.push(format!("  {}",
            style::bold(&style::fg("Scribe — keybindings (:keys)", 220))));
        lines.push(format!("  {}", rule));

        // Pad the plain key string to a fixed visible width, then
        // apply the color. `{:<N}` would count ANSI escape bytes
        // toward the width and leave the column jagged; using
        // `crust::display_width` measures actual on-screen cells.
        let row = |k: &str, desc: &str| -> String {
            const KEY_COL: usize = 18;
            let pad = " ".repeat(KEY_COL.saturating_sub(crust::display_width(k)));
            format!("  {}{}  {}", key(k), pad, desc)
        };

        // MOTION
        lines.push(format!("  {}", head("MOTION")));
        for (k, d) in [
            ("h j k l",        "left / down / up / right"),
            ("0  ^  $",        "line start / first non-blank / end"),
            ("HOME / END",     "line start / end"),
            ("gg  G  12G",     "first / last line / line N"),
            ("w b e",          "next / prev word, end of word"),
            ("W B",            "WORD (whitespace-delimited)"),
            ("f{c} F{c}",      "jump on next / prev c on line"),
            ("t{c} T{c}",      "jump before next / prev c on line"),
            ("Ctrl-D / Ctrl-U","half-page down / up"),
            ("PgDn / PgUp",    "full page down / up"),
            ("*  #",           "search word under cursor"),
            ("n  N",           "next / prev search match"),
            ("K",              "Claude lookup for word under cursor"),
        ] { lines.push(row(k, d)); }
        lines.push(String::new());

        // MODES
        lines.push(format!("  {}", head("MODES")));
        for (k, d) in [
            ("i  a",           "insert before / after cursor"),
            ("I  A",           "insert at line start / end"),
            ("o  O",           "open new line below / above"),
            ("s  S",           "substitute char / line, enter Insert"),
            ("v  V  Ctrl-V",   "visual char / line / block"),
            (":",              "command mode"),
            ("Esc",            "return to Normal"),
        ] { lines.push(row(k, d)); }
        lines.push(String::new());

        // EDIT
        lines.push(format!("  {}", head("EDIT")));
        for (k, d) in [
            ("x  X",           "delete char fwd / back"),
            ("r{c}",           "replace char under cursor"),
            ("J  ~",           "join below / toggle case"),
            ("p  P",           "paste after / before"),
            ("u  Ctrl-R",      "undo / redo"),
            ("Ctrl-A / Ctrl-X","increment / decrement number or ISO date"),
            ("Ctrl-Up/Down",   "swap current line with above / below"),
            (".",              "dot-repeat last change"),
        ] { lines.push(row(k, d)); }
        lines.push(String::new());

        // OPERATORS
        lines.push(format!("  {}", head("OPERATORS  +  motion / text-object")));
        for (k, d) in [
            ("d  c  y",        "delete / change / yank"),
            (">  <",           "indent / outdent"),
            ("gq",             "text-wrap"),
            ("dd cc yy",       "linewise (also  >>  <<  gqq)"),
            ("D  C  Y",        "to end of line"),
            ("(examples)",     "5dw  d3w  cgg  yG  c$  >ap  gqap"),
        ] { lines.push(row(k, d)); }
        lines.push(String::new());

        // TEXT OBJECTS
        lines.push(format!("  {}", head("TEXT OBJECTS")));
        for (k, d) in [
            ("iw  aw",         "word (inner / around)"),
            ("i\"  a\"",        "string (also '  `)"),
            ("i(  a(",         "parens (also [ { <)"),
            ("ib  iB",         "shortcut for ()  / {}"),
            ("ip  ap",         "paragraph"),
        ] { lines.push(row(k, d)); }
        lines.push(String::new());

        // REGISTERS
        lines.push(format!("  {}", head("REGISTERS")));
        for (k, d) in [
            ("\"a ... \"z",     "named slots ( \"ay$ → yank into a )"),
            ("\"0",             "last yank only"),
            ("\"\"",             "unnamed (default for p / P)"),
            ("\"+  \"*",         "system clipboard (OSC 52)"),
            ("Ctrl-R {reg}",   "Insert-mode paste from register"),
            ("Ctrl-R =",       "Insert-mode eval expr and paste result"),
            (":reg",           "inspector popup"),
        ] { lines.push(row(k, d)); }
        lines.push(String::new());

        // SEARCH + SUBSTITUTE
        lines.push(format!("  {}", head("SEARCH / SUBSTITUTE")));
        for (k, d) in [
            ("/pat  ?pat",     "regex forward / backward"),
            (":s/p/r/[gi]",    "substitute on current line"),
            (":%s/p/r/[gi]",   "substitute on whole buffer"),
        ] { lines.push(row(k, d)); }
        lines.push(String::new());

        // MARKS
        lines.push(format!("  {}", head("MARKS")));
        for (k, d) in [
            ("m{a-z}",         "set mark"),
            ("'{a-z}",         "jump to mark (line start, first non-blank)"),
            ("`{a-z}",         "jump to mark (exact column)"),
        ] { lines.push(row(k, d)); }
        lines.push(String::new());

        // MACROS
        lines.push(format!("  {}", head("MACROS")));
        for (k, d) in [
            ("M{reg}",         "start recording; M to stop"),
            ("@{reg}  @@",     "replay (last)"),
        ] { lines.push(row(k, d)); }
        lines.push(String::new());

        // SPELL
        lines.push(format!("  {}", head("SPELL")));
        for (k, d) in [
            ("]s  [s",         "next / prev misspelling (also  zn / zp)"),
            ("z=",             "suggestions (numbered)"),
            ("zg",             "add word at cursor to personal dict"),
            ("zs  zh  z0",     "show / hide word, clear show-hide"),
            (":spell <LANG>",  "enable + set language atomically (en_US, nb_NO, …)"),
            (":spell",         "enable with the current language"),
            (":set spell",     "toggle (also  :set nospell)"),
            (":set spelllang=","switch dictionary without enabling"),
            ("(per-lang)",     "wire quick keys in scriberc [keymap]  →  :map"),
        ] { lines.push(row(k, d)); }
        lines.push(String::new());

        // FOLDS
        lines.push(format!("  {}", head("FOLDS")));
        for (k, d) in [
            ("zo  zc  za",     "open / close / toggle fold"),
            ("zR  zM",         "open all / close all"),
            ("zs  zh",         "fold by section / heading"),
        ] { lines.push(row(k, d)); }
        lines.push(String::new());

        // LEADER
        lines.push(format!("  {}", head("LEADER  \\   (HyperList — full list via \\?)")));
        for (k, d) in [
            ("\\?",            "HyperList leader cheatsheet popup"),
            ("\\w",            "Claude word lookup (same as K)"),
            ("\\v  \\V  \\o",  "checkbox / +timestamp / in-progress"),
            ("\\0 .. \\9",     "fold to level"),
            ("\\e[ekd]",       "encrypt / decrypt / rekey"),
            ("\\x[hlm]",       "export HTML / LaTeX / Markdown"),
        ] { lines.push(row(k, d)); }
        lines.push(String::new());

        // COMMANDS
        lines.push(format!("  {}", head("COMMANDS")));
        for (k, d) in [
            (":w  :wq  :q  :q!","save / save-quit / quit (! discards)"),
            (":e <file>",      "edit file"),
            (":help [topic]",  "README in a popup (topic: hl)"),
            (":keys",          "this popup"),
            (":map",           "your personal keymaps (scriberc [keymap])"),
            (":reg",           "registers inspector"),
            (":config",        "preferences popup"),
            (":chat",          "launch Claude session"),
            (":set ...",       "runtime settings (spell, lang, theme, …)"),
            (":export <fmt>",  "html / latex / markdown / pdf"),
        ] { lines.push(row(k, d)); }
        lines.push(String::new());

        // UI
        lines.push(format!("  {}", head("UI")));
        for (k, d) in [
            ("Ctrl-L",         "redraw"),
        ] { lines.push(row(k, d)); }

        lines.push(String::new());
        lines.push(format!("  {}", rule));
        lines.push(format!("  {}  {}  {}  {}",
            key("j/k"), key("PgUp/PgDn"), key("g/G"),
            style::fg("scroll   ESC / q  close", 244)));

        Cursor::hide();
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
        Cursor::show();
        self.render_all();
    }

    /// `:map` popup — list the user's personal keymaps loaded from the
    /// `[keymap]` section of `~/.config/scribe/scriberc`. Each map is
    /// shown as a `mode  lhs` header followed by its `rhs` on an
    /// indented next line so long RHS strings (typical for signature
    /// templates with embedded `<CR>` / `<Esc>` tokens) don't have to
    /// wrap inside a narrow column. Aliases: `:maps`, `:mappings`.
    fn show_maps_popup(&mut self) {
        let (cols, rows) = Crust::terminal_size();
        let popup_w = (cols.saturating_sub(2)).min(100).max(50);
        let popup_h = (rows.saturating_sub(6)).max(12);
        let mut popup = Popup::centered(popup_w, popup_h, 252, 236);
        popup.pane.y = popup.pane.y.saturating_add(1);

        let head = |s: &str| style::bold(&style::fg(s, 81));
        let rule = style::fg(&"-".repeat(popup_w as usize - 4), 238);

        let mut lines: Vec<String> = Vec::new();
        lines.push(String::new());
        lines.push(format!("  {}",
            style::bold(&style::fg("Scribe — your personal keymaps (:map)", 220))));
        lines.push(format!("  {}", rule));

        if self.keymaps.is_empty() {
            lines.push(format!("  {}",
                style::fg("(no user keymaps defined)", 244)));
            lines.push(String::new());
            lines.push(format!("  {}", head("Add bindings to your scriberc")));
            lines.push(format!("  {}", style::fg(
                "~/.config/scribe/scriberc — start the section with `[keymap]`,", 244)));
            lines.push(format!("  {}", style::fg(
                "then one mapping per line:", 244)));
            lines.push(String::new());
            lines.push(format!("  {}", style::fg(
                "    MODE  LHS  RHS", 81)));
            lines.push(String::new());
            lines.push(format!("  {}", style::fg(
                "MODE is normal / insert / visual (case-insensitive).", 244)));
            lines.push(format!("  {}", style::fg(
                "LHS uses macro notation: literal chars, plus <CR>, <Esc>,", 244)));
            lines.push(format!("  {}", style::fg(
                "<C-Space>, etc. RHS keeps internal spaces; an RHS that starts", 244)));
            lines.push(format!("  {}", style::fg(
                "with `:` is fed straight to the command executor.", 244)));
            lines.push(String::new());
            lines.push(format!("  {}", head("Example")));
            lines.push(format!("  {}",
                style::fg("    normal  ø  ddO<CR><CR><Up>", 81)));
        } else {
            // Group by mode so the user sees normal-mode maps together,
            // insert-mode together, etc.
            let modes = ["normal", "insert", "visual"];
            let mut shown_any_mode = false;
            for mode in &modes {
                let in_mode: Vec<&KeyMap> = self.keymaps.iter()
                    .filter(|m| m.mode.eq_ignore_ascii_case(mode))
                    .collect();
                if in_mode.is_empty() { continue; }
                if shown_any_mode { lines.push(String::new()); }
                shown_any_mode = true;
                lines.push(format!("  {}", head(&mode.to_uppercase())));
                for m in &in_mode {
                    let lhs_str = m.lhs.join("");
                    lines.push(format!("  {}    {}",
                        style::fg(&format!("{:<8}", lhs_str), 220),
                        style::fg(&m.rhs, 81)));
                }
            }
            // Catch any mode label not in the canonical three (defensive).
            let other: Vec<&KeyMap> = self.keymaps.iter()
                .filter(|m| !modes.iter().any(|x| m.mode.eq_ignore_ascii_case(x)))
                .collect();
            if !other.is_empty() {
                if shown_any_mode { lines.push(String::new()); }
                lines.push(format!("  {}", head("OTHER")));
                for m in &other {
                    let lhs_str = m.lhs.join("");
                    lines.push(format!("  {:<10}  {:<8}  {}",
                        style::fg(&m.mode, 244),
                        style::fg(&lhs_str, 220),
                        style::fg(&m.rhs, 81)));
                }
            }
        }

        lines.push(String::new());
        lines.push(format!("  {}", rule));
        lines.push(format!("  {}   {}",
            style::fg(&format!("{} mapping(s)", self.keymaps.len()), 244),
            style::fg("j/k scroll   ESC / q  close", 244)));

        Cursor::hide();
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
        Cursor::show();
        self.render_all();
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

        Cursor::hide();
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
        Cursor::show();
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
    /// Vim's `:r filename` — read file contents and insert AFTER the
    /// current line. Each line in the file becomes a new buffer line;
    /// the cursor moves to the first inserted line. Goes through
    /// `buf.apply` so the read is a single undo step.
    fn read_file_into_buffer(&mut self, raw_path: &str) {
        if raw_path.is_empty() {
            self.set_status(" :r needs a filename", 196);
            return;
        }
        // Expand `~/` → $HOME.
        let path: PathBuf = if let Some(rest) = raw_path.strip_prefix("~/") {
            let home = std::env::var_os("HOME")
                .map(PathBuf::from).unwrap_or_default();
            home.join(rest)
        } else if raw_path == "~" {
            std::env::var_os("HOME")
                .map(PathBuf::from).unwrap_or_default()
        } else {
            PathBuf::from(raw_path)
        };
        let mut text = match std::fs::read_to_string(&path) {
            Ok(s)  => s,
            Err(e) => {
                self.set_status(&format!(" :r failed: {}", e), 196);
                return;
            }
        };
        // Ensure inserted content ends with a newline so the read file's
        // last line doesn't merge with the buffer's next line.
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }

        // Insertion point: byte just past line N's trailing newline
        // (i.e. start of line N+1), so the file content lands as
        // brand-new lines immediately below the cursor's line. ropey's
        // line(N).len_bytes() includes the trailing \n when present.
        let cur_line = self.cur_line;
        let line_start = self.buf.line_byte_offset(cur_line);
        let line_len_bytes = self.buf.line(cur_line).len();
        let mut insert_pos = line_start + line_len_bytes;

        // If we're at the last line and the buffer doesn't end with a
        // newline, the insertion point sits flush with the last char —
        // prepend a newline to the read text so we don't merge.
        let buf_len = self.buf.rope.len_bytes();
        if insert_pos >= buf_len {
            insert_pos = buf_len;
            let ends_with_nl = if buf_len == 0 {
                true
            } else {
                let last_char_byte = buf_len - 1;
                self.buf.rope.byte_slice(last_char_byte..buf_len)
                    .chars().any(|c| c == '\n')
            };
            if !ends_with_nl && !text.is_empty() {
                text.insert(0, '\n');
            }
        }

        // Count inserted lines so we can report and so the cursor lands
        // on the first one.
        let inserted_lines = text.matches('\n').count();
        self.buf.apply(insert_pos, insert_pos, &text);

        // Move the cursor onto the first inserted line.
        let target_line = (cur_line + 1).min(self.buf.line_count().saturating_sub(1));
        self.cur_line = target_line;
        self.cur_col = 0;
        self.set_status(
            &format!(" \"{}\"  {}L read", path.display(), inserted_lines),
            244,
        );
    }

    fn execute_command(&mut self, cmd: &str) -> bool {
        // Keep the list in COLON_COMMANDS (below) in sync when you add
        // a new command here — that list drives tab completion.
        match cmd {
            // Picker — vim's `:digraphs` shows the digraph table; `:emoji`
            // jumps straight to the emoji tab. Both insert at the cursor
            // if anything is picked and re-enter Normal mode (or the
            // current mode, since the picker is modal on top).
            "digraphs" | "dig" => {
                let glyph = picker::pick(
                    picker::InitialTab::Digraphs,
                    &mut [&mut self.header, &mut self.main_p, &mut self.footer],
                );
                if let Some(g) = glyph {
                    self.insert_text_at_cursor(&g);
                }
                self.render_all();
                return false;
            }
            "emoji" => {
                let glyph = picker::pick(
                    picker::InitialTab::Emoji,
                    &mut [&mut self.header, &mut self.main_p, &mut self.footer],
                );
                if let Some(g) = glyph {
                    self.insert_text_at_cursor(&g);
                }
                self.render_all();
                return false;
            }
            "w" | "W" => {
                self.save_guarded();
                false
            }
            // Vim's `:w <path>` — write the current buffer to that
            // path without renaming the open file or clearing the
            // dirty flag (the current file is still unsaved). `~/`
            // is expanded to $HOME. Refuses on encrypted buffers
            // because writing cleartext to an arbitrary path defeats
            // the point.
            other if other.starts_with("w ") || other.starts_with("W ") => {
                let raw = &other[2..];
                let trimmed = raw.trim();
                if trimmed.is_empty() {
                    self.set_status(" :w needs a path", 196);
                    return false;
                }
                if self.buf.encrypted {
                    self.set_status(
                        " refusing :w <path> on encrypted buffer (would write cleartext)",
                        196);
                    return false;
                }
                let expanded = if let Some(rest) = trimmed.strip_prefix("~/") {
                    let home = std::env::var_os("HOME")
                        .map(std::path::PathBuf::from)
                        .unwrap_or_default();
                    home.join(rest)
                } else {
                    std::path::PathBuf::from(trimmed)
                };
                let mut s = String::new();
                for chunk in self.buf.rope.chunks() { s.push_str(chunk); }
                match std::fs::write(&expanded, s) {
                    Ok(_) => self.set_status(
                        &format!(" written to {}", expanded.display()), 46),
                    Err(e) => self.set_status(
                        &format!(" save failed: {}", e), 196),
                }
                false
            }
            "q"  => { if self.buf.dirty { self.set_status(" unsaved changes (use :q! to force)", 196); false } else { true } }
            "q!" => true,
            // `:Wq` / `:WQ` / `:wQ` accepted as aliases for `:wq` so a
            // sticky shift key (or muscle memory) doesn't bounce the user
            // out with "unknown: Wq".
            "wq" | "Wq" | "wQ" | "WQ" | "x" | "X" => {
                self.save_guarded()
            }
            "" => false,
            // Bare `:e` (vim's `:edit`) reloads the current file from disk.
            // Refuse if buffer is dirty unless forced (`:e!`), so the user
            // can't lose edits to a typo. The external-change reload prompt
            // in the main loop points users here when the on-disk file
            // differs from the buffer.
            "e" | "edit" => {
                if self.buf.dirty {
                    self.set_status(" unsaved changes (use :e! to force reload)", 196);
                } else if self.buf.path.is_some() {
                    if self.buf.reload().is_ok() {
                        let lc = self.buf.line_count().saturating_sub(1);
                        if self.cur_line > lc { self.cur_line = lc; }
                        self.clamp_col_to_line();
                        self.set_status(" reloaded", 2);
                    } else {
                        self.set_status(" reload failed", 196);
                    }
                } else {
                    self.set_status(" no file to reload", 196);
                }
                false
            }
            "e!" | "edit!" => {
                if self.buf.path.is_some() {
                    if self.buf.reload().is_ok() {
                        let lc = self.buf.line_count().saturating_sub(1);
                        if self.cur_line > lc { self.cur_line = lc; }
                        self.clamp_col_to_line();
                        self.set_status(" reloaded (force)", 2);
                    } else {
                        self.set_status(" reload failed", 196);
                    }
                } else {
                    self.set_status(" no file to reload", 196);
                }
                false
            }
            other if other.starts_with("e ") => {
                let path = other[2..].trim();
                if !path.is_empty() {
                    let p = PathBuf::from(path);
                    if buffer::is_encrypted_dotfile(&p) && p.exists() {
                        self.set_status(
                            " encrypted dotfile — open from command line so password prompt works",
                            196);
                    } else if let Ok(b) = Buffer::from_path(p) {
                        self.buf = b;
                        self.cur_line = 0;
                        self.cur_col = 0;
                        self.scroll = 0;
                        self.folds.clear();
                        self.restore_session();
                    } else {
                        self.set_status(" open failed", 196);
                    }
                }
                false
            }
            // Vim's `:r filename` — read the contents of a file and
            // insert them AFTER the current line. Each line of the file
            // lands as a new buffer line; the cursor moves to the first
            // inserted line. `~/` is expanded to $HOME. Also accepts
            // `:read filename` (the longer form). `:read` with no
            // argument continues to toggle reading mode (handled below).
            other if other.starts_with("r ")
                    || (other.starts_with("read ") && !other.eq("reading"))
                    || other.starts_with("read!") =>
            {
                let arg = if let Some(rest) = other.strip_prefix("read ") { rest }
                          else if let Some(rest) = other.strip_prefix("r ") { rest }
                          else { other.strip_prefix("read!").unwrap_or("") };
                self.read_file_into_buffer(arg.trim());
                false
            }
            "ab" | "abbrev" => {
                if self.abbrev.is_empty() {
                    self.set_status(" no abbreviations defined", 244);
                } else {
                    let mut names: Vec<&String> = self.abbrev.keys().collect();
                    names.sort();
                    let preview: Vec<String> = names.iter()
                        .take(6)
                        .map(|k| format!("{}→{}", k, self.abbrev[*k]))
                        .collect();
                    let suffix = if names.len() > 6 { format!(" (+{} more)", names.len() - 6) } else { String::new() };
                    self.set_status(&format!(" abbrev: {}{}", preview.join("  "), suffix), 244);
                }
                false
            }
            "abclear" | "abc" => {
                let n = self.abbrev.len();
                self.abbrev.clear();
                save_abbreviations(&self.abbrev);
                self.set_status(&format!(" cleared {n} abbreviations"), 244);
                false
            }
            other if other.starts_with("ab ") || other.starts_with("abbrev ") => {
                // `:ab trigger expansion words go here`. The first
                // token after the keyword is the trigger; everything
                // else is the expansion (verbatim, including spaces).
                let body = if let Some(rest) = other.strip_prefix("abbrev ") { rest }
                           else { other.strip_prefix("ab ").unwrap_or("") };
                let body = body.trim_start();
                if let Some((trigger, expansion)) = body.split_once(char::is_whitespace) {
                    let trigger = trigger.trim();
                    let expansion = expansion.trim_end_matches('\n');
                    if trigger.is_empty() {
                        self.set_status(" :ab needs a trigger", 196);
                    } else if !trigger.chars().all(is_abbrev_char) {
                        self.set_status(
                            " trigger must contain only letters, digits, '-', '_'",
                            196,
                        );
                    } else {
                        self.abbrev.insert(trigger.to_string(), expansion.to_string());
                        save_abbreviations(&self.abbrev);
                        self.set_status(&format!(" abbrev: {} → {}", trigger, expansion), 244);
                    }
                } else if !body.is_empty() {
                    // Single-token form `:ab trigger` shows current value
                    let key = body.trim();
                    match self.abbrev.get(key) {
                        Some(v) => self.set_status(&format!(" {} → {}", key, v), 244),
                        None => self.set_status(&format!(" no abbrev for {}", key), 244),
                    }
                }
                false
            }
            other if other.starts_with("una ") || other.starts_with("unabbrev ") => {
                let body = if let Some(rest) = other.strip_prefix("unabbrev ") { rest }
                           else { other.strip_prefix("una ").unwrap_or("") };
                let key = body.trim();
                if self.abbrev.remove(key).is_some() {
                    save_abbreviations(&self.abbrev);
                    self.set_status(&format!(" removed abbrev: {}", key), 244);
                } else {
                    self.set_status(&format!(" no abbrev for {}", key), 244);
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
            // `:spell <LANG>` — atomic enable + set lang. Equivalent to
            // `:set spelllang=LANG` followed by `:set spell` but in one
            // shot, which is what user keymaps want for a single-key
            // quick-toggle. `:spell` alone (no arg) just enables with
            // the current lang.
            "spell" => {
                self.spell_enable();
                if self.spell_enabled {
                    self.set_status(
                        &format!(" spell on ({}) — {} flagged",
                            self.spell_lang, self.misspellings.len()),
                        46);
                }
                false
            }
            other if other.starts_with("spell ") => {
                let lang = other[6..].trim();
                if lang.is_empty() {
                    self.spell_enable();
                } else {
                    self.quick_spell(lang);
                }
                false
            }
            "help" | "h" => {
                self.open_help("");
                false
            }
            other if other.starts_with("help ") || other.starts_with("h ") => {
                let topic = other.splitn(2, ' ').nth(1).unwrap_or("").trim();
                self.open_help(topic);
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
            "keys" | "keybindings" | "cheat" => {
                self.show_keys_popup();
                false
            }
            "map" | "maps" | "mappings" => {
                self.show_maps_popup();
                false
            }
            other if other.starts_with("show ") || other.starts_with("hide ") => {
                let show = other.starts_with("show ");
                let pat = other[5..].trim().to_string();
                self.showhide_pattern(&pat, show);
                false
            }
            other if other.starts_with("export ") => {
                let fmt = other[7..].trim().to_string();
                self.export_to(&fmt);
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
        let replacement_raw = parts[1];
        let flags = parts.get(2).copied().unwrap_or("");
        let global = flags.contains('g');
        let case_insensitive = flags.contains('i');
        // Expand backslash escapes in the replacement: `\t` → TAB,
        // `\n` → newline, `\r` → CR, `\\` → literal `\`. Capture
        // groups still use `$1`, `$2` (regex-crate convention).
        let replacement = expand_replacement_escapes(replacement_raw);
        let replacement = replacement.as_str();

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
    /// Term lookup. Picks what to send Claude based on mode:
    ///   * Visual / VisualLine — the current selection (the user
    ///     deliberately marked the phrase, send it whole). Lets the
    ///     user explain multi-word expressions, idioms, or full
    ///     sentences without leaving Normal mode just to widen the
    ///     `word_at_cursor` span.
    ///   * Normal — the WORD-style token under the cursor: the
    ///     contiguous non-whitespace span, with surrounding paired
    ///     punctuation trimmed. Catches compound terms like `F&O`,
    ///     `C++`, `user@host`, `bin/scribe` that `iskeyword`-style
    ///     extraction would split. Falls back to `word_at_cursor`
    ///     for spell-misflag matches.
    /// Always pairs the term with ±5 lines of surrounding context
    /// and asks for a one- or two-sentence explanation. The reply
    /// lands in a centred read-only popup; ESC / q / Enter dismisses.
    /// Bound to `\w` (leader; mnemonic: **w**ord) and `K` (vim
    /// muscle memory).
    fn lookup_word_with_claude(&mut self) {
        // Pull the term + a label for the popup heading. Visual modes
        // win even if the selection contains only a single word — the
        // user's explicit gesture beats heuristics.
        let (term, popup_label) = if matches!(self.mode, Mode::Visual | Mode::VisualLine) {
            let (a, b) = if matches!(self.mode, Mode::VisualLine) {
                self.visual_line_range()
            } else {
                self.visual_range()
            };
            let text: String = self.buf.rope.byte_slice(a..b).to_string();
            let trimmed = text.trim();
            if trimmed.is_empty() {
                self.set_status(" empty selection", 244);
                return;
            }
            let label = if trimmed.chars().count() > 40 {
                let mut t: String = trimmed.chars().take(37).collect();
                t.push_str("…");
                t
            } else {
                trimmed.to_string()
            };
            (trimmed.to_string(), label)
        } else {
            let Some(token) = self.token_at_cursor() else {
                self.set_status(" no token under cursor", 244);
                return;
            };
            let label = token.clone();
            (token, label)
        };

        let total = self.buf.line_count();
        if total == 0 { return; }
        let lo = self.cur_line.saturating_sub(5);
        let hi = (self.cur_line + 5).min(total.saturating_sub(1));
        let context: String = (lo..=hi)
            .map(|i| self.buf.line(i))
            .collect::<Vec<_>>()
            .join("\n");

        self.set_status(&format!(" looking up “{}”…", popup_label), 244);
        self.render_footer();
        use std::io::Write as _;
        let _ = std::io::stdout().flush();

        // Phrase the prompt so multi-word expressions and full
        // sentences read naturally; "the word X" works fine for a
        // single token and still parses correctly for "the phrase X".
        let prompt = format!(
            "Explain \"{term}\" as it is used in the snippet below. \
             One or two short sentences. Stay specific to *this* sense / \
             usage — if it's technical, give the technical reading; if \
             it's a name, identify it; if it's an idiom or phrase, gloss \
             the whole expression. No preamble, no quoting back the \
             snippet, no closing pleasantries.",
            term = term
        );
        let answer = match claude_run(&prompt, &context) {
            Ok(s) => s.trim().to_string(),
            Err(e) => {
                self.set_status(&format!(" claude: {}", e), 196);
                return;
            }
        };
        if answer.is_empty() {
            self.set_status(" claude returned empty response", 196);
            return;
        }

        // Size the popup to the response, capped to terminal bounds.
        let (cols, rows) = Crust::terminal_size();
        let pad: u16 = 4;
        let popup_w = cols.saturating_sub(pad).min(80).max(40);
        let inner_w = (popup_w as usize).saturating_sub(4).max(20);
        let wrapped = wrap_to_width(&answer, inner_w);
        let popup_h = (wrapped.len() as u16 + 6).min(rows.saturating_sub(pad)).max(8);
        let mut popup = Popup::centered(popup_w, popup_h, 252, 236);
        let mut lines: Vec<String> = Vec::new();
        lines.push(String::new());
        lines.push(format!("  {}", crust::style::bold(&crust::style::fg(&popup_label, 220))));
        lines.push(format!("  {}", crust::style::fg(&"-".repeat(inner_w), 238)));
        for ln in &wrapped {
            lines.push(format!("  {}", ln));
        }
        lines.push(String::new());
        lines.push(format!("  {}  Close", crust::style::fg("ESC / q", 244)));

        // Hide the terminal cursor while the popup is up. Without
        // this the cursor stays parked at the buffer position behind
        // the popup and renders as a stray red block — visible
        // through the popup's interior.
        Cursor::hide();
        popup.show(&lines.join("\n"));
        loop {
            let Some(k) = Input::getchr(None) else { break };
            if k == "ESC" || k == "q" || k == "ENTER" { break; }
        }
        popup.dismiss(&mut [&mut self.header, &mut self.main_p, &mut self.footer]);
        // Show the cursor again. `render_all` repositions it via
        // `position_cursor` which also emits `\x1b[?25h`, but the
        // explicit show here covers the (rare) case where rendering
        // is short-circuited by a pending mode change.
        Cursor::show();
        self.set_status("", 244);
        self.render_all();
    }

    /// WORD-style token at the cursor: the contiguous non-whitespace
    /// span around `cur_col`, stripped of paired punctuation that
    /// almost never belongs to the term being looked up
    /// (`,.;:!?)]}` at the tail, `([{` at the head). Used by `K` /
    /// `\w` so compound terms like `F&O`, `C++`, `user@host.com`,
    /// `bin/scribe` come through whole. Falls back to the narrower
    /// `word_at_cursor` when the WORD heuristic comes up empty
    /// (cursor on whitespace, etc.).
    fn token_at_cursor(&self) -> Option<String> {
        // Prefer a misspelling range if the cursor sits on one — the
        // spell checker already isolated the exact word the user is
        // likely staring at.
        if let Some(m) = self.misspelling_at_cursor() {
            return Some(m.word);
        }
        let line = self.buf.line(self.cur_line);
        if line.is_empty() { return self.word_at_cursor(); }
        let bytes = line.as_bytes();
        let cur = self.cur_col.min(bytes.len());
        // Walk backwards over non-whitespace bytes.
        let mut s = cur;
        while s > 0 && !bytes[s - 1].is_ascii_whitespace() { s -= 1; }
        let mut e = cur;
        while e < bytes.len() && !bytes[e].is_ascii_whitespace() { e += 1; }
        if s >= e { return self.word_at_cursor(); }
        while s > 0 && !line.is_char_boundary(s) { s -= 1; }
        while e < bytes.len() && !line.is_char_boundary(e) { e += 1; }
        let mut token = line[s..e].to_string();
        // Trim wrappers: outer parens / brackets / quotes / trailing
        // sentence punctuation. Stops as soon as a non-strippable
        // char appears, so `f(x)` stays `f(x)` (closing paren is
        // balanced by the opening one inside).
        while let Some(c) = token.chars().last() {
            if matches!(c, ',' | '.' | ';' | ':' | '!' | '?' | ')' | ']' | '}' | '"' | '\'') {
                token.pop();
            } else { break; }
        }
        while let Some(c) = token.chars().next() {
            if matches!(c, '(' | '[' | '{' | '"' | '\'') {
                token.remove(0);
            } else { break; }
        }
        if token.is_empty() { self.word_at_cursor() } else { Some(token) }
    }

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
/// Simple greedy word-wrap. Preserves explicit line breaks from the
/// input (newline → new wrapped line) and splits on whitespace within
/// each paragraph. Width is in visible columns; ANSI escapes are not
/// expected in the input (claude responses are plain text).
fn wrap_to_width(s: &str, width: usize) -> Vec<String> {
    let width = width.max(8);
    let mut out: Vec<String> = Vec::new();
    for paragraph in s.split('\n') {
        if paragraph.trim().is_empty() {
            out.push(String::new());
            continue;
        }
        let mut line = String::new();
        for word in paragraph.split_whitespace() {
            let wlen = word.chars().count();
            if line.is_empty() {
                // Long word that exceeds the column on its own: take
                // it whole and rely on the renderer's pane to clip.
                line.push_str(word);
            } else if line.chars().count() + 1 + wlen <= width {
                line.push(' ');
                line.push_str(word);
            } else {
                out.push(std::mem::take(&mut line));
                line.push_str(word);
            }
        }
        if !line.is_empty() { out.push(line); }
    }
    out
}

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
