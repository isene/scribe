//! Digraph + emoji picker popup. Vim's `:digraphs` browseable
//! interactively; rcurses' emoji picker carried over to the Rust
//! side. Single popup, category tabs, type-to-filter.
//!
//! Single-pick: Enter inserts the highlighted glyph and closes;
//! ESC closes without inserting.

use crust::{Cursor, Input, Pane, Popup, style};

use crate::digraphs::{DIGRAPHS, Digraph};
use crate::emoji_data::EMOJI_CATEGORIES;

/// Popup panel background (256-color index). Used both to build the
/// popup and to restore the bg after a truecolor-bg selected tab, so
/// the rest of the tab strip keeps the panel colour.
const POPUP_BG: u16 = 236;

/// A single item in the picker (one per row).
struct Entry<'a> {
    glyph: &'a str,
    /// Two-char digraph code (Vim mnemonic) — empty for emojis.
    code: &'a str,
    /// Searchable label (digraph Unicode name or emoji label).
    label: &'a str,
}

/// Top-level grouping. Tab cycles through these.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    All,
    Digraphs,
    Emoji(usize),  // index into EMOJI_CATEGORIES
}

impl Tab {
    fn title(self) -> String {
        match self {
            Tab::All       => "All".into(),
            Tab::Digraphs  => "Digraphs".into(),
            Tab::Emoji(i)  => EMOJI_CATEGORIES.get(i).map(|c| c.name.to_string())
                                .unwrap_or_else(|| "?".into()),
        }
    }
    fn next(self) -> Tab {
        let max_emoji = EMOJI_CATEGORIES.len();
        match self {
            Tab::All        => Tab::Digraphs,
            Tab::Digraphs   => if max_emoji > 0 { Tab::Emoji(0) } else { Tab::All },
            Tab::Emoji(i)   => if i + 1 < max_emoji { Tab::Emoji(i + 1) } else { Tab::All },
        }
    }
    fn prev(self) -> Tab {
        let max_emoji = EMOJI_CATEGORIES.len();
        match self {
            Tab::All        => if max_emoji > 0 { Tab::Emoji(max_emoji - 1) } else { Tab::Digraphs },
            Tab::Digraphs   => Tab::All,
            Tab::Emoji(0)   => Tab::Digraphs,
            Tab::Emoji(i)   => Tab::Emoji(i - 1),
        }
    }
}

/// Build the entry list for a given tab.
fn entries_for(tab: Tab) -> Vec<Entry<'static>> {
    let mut out: Vec<Entry<'static>> = Vec::new();
    let push_digraphs = |out: &mut Vec<Entry<'static>>| {
        for d in DIGRAPHS {
            out.push(Entry { glyph: d.glyph, code: d.code, label: d.name });
        }
    };
    let push_emoji = |out: &mut Vec<Entry<'static>>, cat_idx: Option<usize>| {
        for (i, cat) in EMOJI_CATEGORIES.iter().enumerate() {
            if let Some(want) = cat_idx { if i != want { continue; } }
            for (glyph, label) in cat.items {
                out.push(Entry { glyph, code: "", label });
            }
        }
    };
    match tab {
        Tab::All       => { push_digraphs(&mut out); push_emoji(&mut out, None); }
        Tab::Digraphs  => push_digraphs(&mut out),
        Tab::Emoji(i)  => push_emoji(&mut out, Some(i)),
    }
    out
}

/// Filter case-insensitively: a query matches if every whitespace-
/// separated word in it appears (substring) in either the code or
/// the label. Empty query matches all.
fn matches(entry: &Entry, query: &str) -> bool {
    if query.is_empty() { return true; }
    let code = entry.code.to_lowercase();
    let label = entry.label.to_lowercase();
    for word in query.split_whitespace() {
        let w = word.to_lowercase();
        if !code.contains(&w) && !label.contains(&w) { return false; }
    }
    true
}

/// Run the picker. Returns the chosen glyph or `None` on cancel.
///
/// `refresh_panes` is the list of panes underneath the popup that need
/// to be repainted after the popup goes away — without this, the
/// caller's `render_all()` diff-renders against stale `prev_frame` and
/// leaves the chrome blank. `popup.dismiss()` resets prev_frame on
/// each pane (via `full_refresh`), so the next `say()` repaints.
pub fn pick(initial_tab: InitialTab, refresh_panes: &mut [&mut Pane]) -> Option<String> {
    let popup_w: u16 = 72;
    let popup_h: u16 = 22;
    let mut popup = Popup::centered(popup_w, popup_h, 252, POPUP_BG);

    let mut tab: Tab = match initial_tab {
        InitialTab::All       => Tab::All,
        InitialTab::Digraphs  => Tab::Digraphs,
        InitialTab::Emoji     => if EMOJI_CATEGORIES.is_empty() { Tab::All } else { Tab::Emoji(0) },
    };
    let mut query = String::new();
    let mut cursor: usize = 0;
    let mut scroll: usize = 0;
    let body_rows = (popup_h as usize).saturating_sub(6);  // chrome takes 6 rows

    // Hide the terminal cursor so it doesn't blink through the popup.
    Cursor::hide();

    let result: Option<String> = loop {
        // Build filtered entry list for the current tab + query.
        let all = entries_for(tab);
        let filtered: Vec<&Entry> = all.iter().filter(|e| matches(e, &query)).collect();
        if cursor >= filtered.len() && !filtered.is_empty() { cursor = filtered.len() - 1; }
        if filtered.is_empty() { cursor = 0; }
        // Scroll window keeps cursor visible.
        if cursor < scroll { scroll = cursor; }
        if cursor >= scroll + body_rows { scroll = cursor + 1 - body_rows; }

        // ---- Render ----
        let mut lines: Vec<String> = Vec::new();
        lines.push(String::new());
        // Tab strip. The selected tab gets an orange bg; the trick is
        // that `style::bg_rgb` closes with `\x1b[49m` (reset bg to the
        // TERMINAL default), not to the popup's bg. On a line that
        // continues past the highlighted tab (the wrapped second row),
        // the following tabs then render on the terminal default —
        // visible as a dark box. So after the orange tab we explicitly
        // restore the panel bg (236, matching Popup::centered below).
        let tab_label = |label: &str, selected: bool| -> String {
            if selected {
                format!("{}\x1b[48;5;{}m",
                    style::bg_rgb(&style::fg(label, 16), "f74c00"), POPUP_BG)
            } else {
                style::fg(label, 244)
            }
        };
        let mut tab_strip = String::from("  ");
        for &(t, _shortcut) in &[
            (Tab::All, "A"), (Tab::Digraphs, "D"),
        ] {
            let label = format!(" {} ", t.title());
            tab_strip.push_str(&tab_label(&label, t == tab));
            tab_strip.push(' ');
        }
        for (i, cat) in EMOJI_CATEGORIES.iter().enumerate() {
            let label = format!(" {} ", cat.name);
            tab_strip.push_str(&tab_label(&label, tab == Tab::Emoji(i)));
            tab_strip.push(' ');
        }
        lines.push(tab_strip);
        lines.push(style::fg(&format!("  {}", "─".repeat(popup_w as usize - 4)), 238));
        // Search line
        let search_label = style::fg(" search:  ", 81);
        lines.push(format!("{}{}", search_label, &query));
        lines.push(style::fg(&format!("  {}", "─".repeat(popup_w as usize - 4)), 238));

        // Body rows
        if filtered.is_empty() {
            lines.push(String::new());
            lines.push(style::fg("    (no matches)", 244));
        } else {
            for vr in 0..body_rows {
                let idx = scroll + vr;
                if idx >= filtered.len() { break; }
                let e = filtered[idx];
                // Force a neutral white around the glyph. Glass's
                // color-emoji path (CBDT via Noto Color Emoji) isn't
                // wired up for non-BMP codepoints in this build, so
                // emojis fall back to monochrome outline rendering
                // in the current fg. Plain `style::native` (CSI 39)
                // landed in glass's default fg which is a bluish
                // grey — making every emoji look uniformly blue.
                // White is the least bad alternative until glass's
                // CBDT path is fixed; digraphs (and emoji color in
                // the main buffer) are unaffected.
                let glyph_cell = format!(" {} ", style::fg(&format!("{:<2}", e.glyph), 255));
                let code_cell  = if e.code.is_empty() {
                    format!(" {:<4} ", "")
                } else {
                    style::fg(&format!(" {:<4} ", e.code), 220)
                };
                let label_w = (popup_w as usize).saturating_sub(14);
                let label_trim: String = e.label.chars().take(label_w).collect();
                let label_cell = style::fg(&label_trim, 252);
                let row = format!("  {}{} {}", glyph_cell, code_cell, label_cell);
                let row_styled = if idx == cursor {
                    style::bg_rgb(&format!(" {} ", row.trim_end()), "3a3a4e")
                } else {
                    format!(" {}", row)
                };
                lines.push(row_styled);
            }
        }
        // Pad to fill
        while lines.len() < popup_h as usize - 1 { lines.push(String::new()); }
        lines.push(style::fg(
            "  Tab cycle cat · type to filter · Enter insert · ESC cancel",
            240,
        ));

        popup.show(&lines.join("\n"));

        // ---- Input ----
        let Some(k) = Input::getchr(None) else { continue };
        match k.as_str() {
            "ESC" | "C-C" => break None,
            "ENTER" | "\n" | "\r" | "C-M" | "C-J" => {
                if let Some(e) = filtered.get(cursor) {
                    break Some(e.glyph.to_string());
                }
            }
            "TAB"        => { tab = tab.next(); cursor = 0; scroll = 0; }
            "S-TAB" | "BACK_TAB" => { tab = tab.prev(); cursor = 0; scroll = 0; }
            "j" | "DOWN" => {
                if !filtered.is_empty() && cursor + 1 < filtered.len() { cursor += 1; }
            }
            "k" | "UP"   => { cursor = cursor.saturating_sub(1); }
            "PgDOWN" | " " => {
                cursor = (cursor + body_rows).min(filtered.len().saturating_sub(1));
            }
            "PgUP"       => { cursor = cursor.saturating_sub(body_rows); }
            "HOME"       => { cursor = 0; }
            "END"        => { cursor = filtered.len().saturating_sub(1); }
            "BACK" | "BACKSPACE" | "C-H" => {
                if !query.is_empty() {
                    let len = query.len();
                    let mut start = len - 1;
                    while start > 0 && !query.is_char_boundary(start) { start -= 1; }
                    query.truncate(start);
                    cursor = 0; scroll = 0;
                }
            }
            other => {
                if other.chars().count() == 1 {
                    let c = other.chars().next().unwrap();
                    if !c.is_control() {
                        query.push(c);
                        cursor = 0; scroll = 0;
                    }
                }
            }
        }
    };

    // Dismiss clears the popup area AND full_refreshes the underlying
    // panes, resetting their prev_frame so the caller's render_all()
    // sees a clean slate and paints the chrome back in.
    popup.dismiss(refresh_panes);
    Cursor::show();
    result
}

#[derive(Clone, Copy)]
pub enum InitialTab {
    All,
    Digraphs,
    Emoji,
}
