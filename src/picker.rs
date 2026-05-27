//! Digraph + emoji picker popup. Vim's `:digraphs` browseable
//! interactively; rcurses' emoji picker carried over to the Rust
//! side. Single popup, category tabs, type-to-filter.
//!
//! Multi-pick: stays open after Enter so the user can queue several
//! glyphs in one session. ESC closes and returns everything queued
//! (in pick order). The caller inserts them all at once after the
//! popup clears — they appear at the cursor on the same line in the
//! order picked.

use crust::{Cursor, Input, Popup, style};

use crate::digraphs::{DIGRAPHS, Digraph};
use crate::emoji_data::EMOJI_CATEGORIES;

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

/// Run the picker. Stays open across multiple Enter presses, each
/// of which queues the highlighted glyph. Returns the queued glyphs
/// in pick order when the user presses ESC. Empty vec = cancelled
/// without picking anything.
pub fn pick(initial_tab: InitialTab) -> Vec<String> {
    let popup_w: u16 = 72;
    let popup_h: u16 = 22;
    let mut popup = Popup::centered(popup_w, popup_h, 252, 236);

    let mut tab: Tab = match initial_tab {
        InitialTab::All       => Tab::All,
        InitialTab::Digraphs  => Tab::Digraphs,
        InitialTab::Emoji     => if EMOJI_CATEGORIES.is_empty() { Tab::All } else { Tab::Emoji(0) },
    };
    let mut query = String::new();
    let mut cursor: usize = 0;
    let mut scroll: usize = 0;
    let mut picks: Vec<String> = Vec::new();
    let body_rows = (popup_h as usize).saturating_sub(6);  // chrome takes 6 rows

    // Hide the terminal cursor so it doesn't blink through the popup.
    Cursor::hide();

    loop {
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
        // Tab strip
        let mut tab_strip = String::from("  ");
        for &(t, _shortcut) in &[
            (Tab::All, "A"), (Tab::Digraphs, "D"),
        ] {
            let label = format!(" {} ", t.title());
            let styled = if t == tab {
                style::bg_rgb(&style::fg(&label, 16), "f74c00")
            } else {
                style::fg(&label, 244)
            };
            tab_strip.push_str(&styled);
            tab_strip.push(' ');
        }
        for (i, cat) in EMOJI_CATEGORIES.iter().enumerate() {
            let label = format!(" {} ", cat.name);
            let styled = if tab == Tab::Emoji(i) {
                style::bg_rgb(&style::fg(&label, 16), "f74c00")
            } else {
                style::fg(&label, 244)
            };
            tab_strip.push_str(&styled);
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
                let glyph_cell = format!(" {:<2} ", e.glyph);
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
        // Footer: show queued picks (truncated to fit) + hint.
        let footer = if picks.is_empty() {
            style::fg(
                "  Tab cycle · type to filter · Enter queue · ESC done",
                240,
            )
        } else {
            // Show the running queue so the user knows what'll get
            // inserted on ESC. Truncate to fit the popup width.
            let mut queue = String::new();
            for g in &picks {
                queue.push_str(g);
                queue.push(' ');
            }
            let max_q = (popup_w as usize).saturating_sub(28);
            let q_disp: String = queue.chars().take(max_q).collect();
            format!("  {}  {}",
                style::fg(&format!("queued ({}):", picks.len()), 81),
                style::fg(&q_disp, 220))
        };
        lines.push(footer);

        popup.show(&lines.join("\n"));

        // ---- Input ----
        let Some(k) = Input::getchr(None) else { continue };
        match k.as_str() {
            "ESC" | "C-C" => break,
            "ENTER" | "\n" | "\r" | "C-M" | "C-J" => {
                if let Some(e) = filtered.get(cursor) {
                    // Queue the pick; stay open so the user can
                    // keep picking. The buffer behind only updates
                    // when the popup is dismissed (ESC).
                    picks.push(e.glyph.to_string());
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
                } else if !picks.is_empty() {
                    // No query to delete from → pop the last queued
                    // pick instead. Lets the user undo a wrong
                    // Enter without leaving the popup.
                    picks.pop();
                }
            }
            other => {
                // Single printable char → append to query.
                if other.chars().count() == 1 {
                    let c = other.chars().next().unwrap();
                    if !c.is_control() {
                        query.push(c);
                        cursor = 0; scroll = 0;
                    }
                }
            }
        }
    }

    // Clear the popup region so the caller's render_all() doesn't
    // need to paint over leftover popup pixels.
    popup.pane.clear();
    Cursor::show();
    picks
}

#[derive(Clone, Copy)]
pub enum InitialTab {
    All,
    Digraphs,
    Emoji,
}
