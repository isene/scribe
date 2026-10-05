//! The overview: the text files of one folder as cards, the way a notes
//! app lays out its notes. `go` or `:overview` opens it, and so does
//! starting scribe on a folder. Nothing here runs before that, and the
//! folder is read once each time the overview opens.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crust::{display_width, pad_display, style, truncate_ansi, Cursor, Input};

use crate::pics;

/// A file with a zero byte this early is not text, and gets no card.
const HEAD: u64 = 1024;
/// As much of a file as is read for its card, its tags and the filter.
const MOST: u64 = 64 * 1024;
/// Lines of text kept for a card.
const LINES: usize = 12;
/// Rows of a card: a border, the name, seven rows of text, a border.
const CARD_H: usize = 10;
/// A card is at least this many cells wide.
const CARD_W: usize = 38;
/// Files named as notes show without their ending.
const NOTE_EXT: [&str; 5] = ["md", "markdown", "txt", "text", "hl"];

/// One text file, as much of it as a card shows.
pub struct Card {
    pub path: PathBuf,
    pub title: String,
    /// The first lines of text, without picture links and the line of tags.
    pub lines: Vec<String>,
    pub tags: Vec<String>,
    /// The first picture the note shows, as the note names it.
    pub picture: Option<String>,
    pub pinned: bool,
    /// Name and text in lower case, for the filter.
    lower: String,
    modified: SystemTime,
}

/// The text files in `dir`: pinned notes first, then the newest first.
pub fn load(dir: &Path) -> Vec<Card> {
    let Ok(entries) = std::fs::read_dir(dir) else { return Vec::new() };
    let mut cards: Vec<Card> = entries.flatten().filter_map(|e| read(&e)).collect();
    cards.sort_by(|a, b| {
        b.pinned.cmp(&a.pinned).then(b.modified.cmp(&a.modified)).then(a.title.cmp(&b.title))
    });
    cards
}

/// The card of one folder entry, or `None` for a folder, a hidden file
/// and a file that is not text.
fn read(entry: &std::fs::DirEntry) -> Option<Card> {
    let path = entry.path();
    let name = path.file_name()?.to_str()?;
    // Hidden files, and the copies an editor leaves beside a file.
    if name.starts_with('.') || name.ends_with('~') || name.ends_with(".scribe-bak") { return None; }
    // Asked before the file is opened: opening a pipe would wait for ever.
    let kind = entry.file_type().ok()?;
    let plain = kind.is_file() || (kind.is_symlink() && std::fs::metadata(&path).is_ok_and(|m| m.is_file()));
    if !plain { return None; }
    let mut file = std::fs::File::open(&path).ok()?;
    let modified = file.metadata().ok()?.modified().ok()?;
    let mut bytes = Vec::new();
    file.by_ref().take(HEAD).read_to_end(&mut bytes).ok()?;
    if bytes.contains(&0) { return None; }
    if bytes.len() as u64 == HEAD {
        file.take(MOST - HEAD).read_to_end(&mut bytes).ok()?;
    }
    let text = String::from_utf8_lossy(&bytes);
    let ending = path.extension().and_then(|e| e.to_str()).unwrap_or("").to_lowercase();
    let title = if NOTE_EXT.contains(&ending.as_str()) {
        path.file_stem()?.to_str()?.to_string()
    } else {
        name.to_string()
    };
    let tags = tags_of(&text);
    Some(Card {
        lines: lines_of(&text),
        picture: text.lines().find_map(pics::link).map(String::from),
        pinned: tags.iter().any(|t| t.eq_ignore_ascii_case("pinned")),
        lower: format!("{}\n{}", title, text).to_lowercase(),
        tags,
        title,
        modified,
        path,
    })
}

/// The tags of a note. A tag is `#` and a word, at the start of a line or
/// after a space: `#idea`. `# Heading` and `example.com/#top` are not tags.
pub fn tags_of(text: &str) -> Vec<String> {
    let mut tags: Vec<String> = Vec::new();
    let mut after_space = true;
    for (at, c) in text.char_indices() {
        if c == '#' && after_space {
            let rest = &text[at + 1..];
            let end = rest.find(|c: char| !(c.is_alphanumeric() || c == '_' || c == '-')).unwrap_or(rest.len());
            let word = &rest[..end];
            let known = tags.iter().any(|t| t.to_lowercase() == word.to_lowercase());
            if word.starts_with(char::is_alphabetic) && !known { tags.push(word.to_string()); }
        }
        after_space = c.is_whitespace();
    }
    tags
}

/// A line that is tags and nothing else: `#work #idea`.
fn only_tags(line: &str) -> bool {
    line.split_whitespace().all(|w| w.starts_with('#') && w.len() > 1)
}

/// The first lines of a note for its card: no empty lines, no picture
/// links, no line of tags. A tab becomes two spaces.
fn lines_of(text: &str) -> Vec<String> {
    text.lines()
        .filter(|l| !l.trim().is_empty() && !l.trim_start().starts_with("![") && !only_tags(l))
        .take(LINES)
        .map(|l| l.trim_end().replace('\t', "  ").chars().filter(|c| !c.is_control()).collect())
        .collect()
}

impl Card {
    /// True when the name or the text has `query` in it. `query` is in lower case.
    pub fn matches(&self, query: &str) -> bool {
        self.lower.contains(query)
    }
}

/// How many cards go side by side in `w` cells, and the cells each one gets.
pub fn columns(w: usize) -> (usize, usize) {
    let n = (w / CARD_W).max(1);
    (n, w / n)
}

/// `line` broken at its spaces into rows no wider than `width` cells.
/// What it is indented by stays on the first row.
fn wrap(line: &str, width: usize) -> Vec<String> {
    let indent = (line.len() - line.trim_start().len()).min(width / 2);
    let mut rows: Vec<String> = Vec::new();
    let mut row = " ".repeat(indent);
    let mut empty = true;
    for word in line.split_whitespace() {
        if !empty && display_width(&row) + 1 + display_width(word) > width {
            rows.push(std::mem::take(&mut row));
            empty = true;
        }
        if !empty { row.push(' '); }
        row.push_str(word);
        empty = false;
    }
    rows.push(row);
    rows
}

/// `s` in exactly `w` cells: cut with an ellipsis, or filled with spaces.
fn fit(s: &str, w: usize) -> String {
    if display_width(s) > w { pad_display(&truncate_ansi(s, w), w) } else { pad_display(s, w) }
}

/// The rows of one card, each `w` cells wide. The first `picture` rows
/// under the name are left empty, for a picture to be put there.
pub fn draw(card: &Card, w: usize, h: usize, selected: bool, picture: usize) -> Vec<String> {
    let inner = w.saturating_sub(4);
    let edge = |s: &str| if selected { style::bold(&style::fg(s, 220)) } else { style::fg(s, 240) };
    let row = |text: String| format!("{} {} {}", edge("│"), text, edge("│"));
    let mut out = vec![edge(&format!("╭{}╮", "─".repeat(w.saturating_sub(2))))];
    out.push(row(style::bold(&fit(&card.title, inner))));

    let body = h.saturating_sub(3);
    let tag_row = usize::from(!card.tags.is_empty() && body > picture);
    let room = body.saturating_sub(picture + tag_row);
    let mut text: Vec<String> = Vec::new();
    for line in &card.lines {
        text.extend(wrap(line, inner));
        if text.len() > room { break; }
    }
    let cut = text.len() > room;
    text.truncate(room);
    if cut {
        if let Some(last) = text.last_mut() {
            if display_width(last) < inner { last.push('…') } else { *last = truncate_ansi(last, inner.saturating_sub(1)) }
        }
    }
    for _ in 0..picture.min(body) { out.push(row(" ".repeat(inner))); }
    for i in 0..room {
        out.push(row(style::fg(&fit(text.get(i).map_or("", String::as_str), inner), 250)));
    }
    if tag_row == 1 {
        let tags: Vec<String> = card.tags.iter().map(|t| format!("#{}", t)).collect();
        out.push(row(style::fg(&fit(&tags.join(" "), inner), 81)));
    }
    out.push(edge(&format!("╰{}╯", "─".repeat(w.saturating_sub(2)))));
    out
}

/// Show the text files of `dir` as cards and let the user pick one.
/// Returns the file to open, or `None` when the user backs out.
pub fn pick(app: &mut crate::App, dir: &Path) -> Option<PathBuf> {
    let cards = load(dir);
    let mut shown: Vec<usize> = (0..cards.len()).collect();
    let mut query = String::new();
    // Start on the card of the file in the buffer.
    let open = app.buf.path.as_ref().and_then(|p| p.canonicalize().ok());
    let mut sel = open
        .and_then(|o| cards.iter().position(|c| c.path.canonicalize().is_ok_and(|p| p == o)))
        .unwrap_or(0);
    let mut top = 0usize;
    let place = dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf());
    let home = std::env::var("HOME").ok().and_then(|h| place.strip_prefix(h).ok().map(Path::to_path_buf));
    let place = home.map_or_else(|| place.display().to_string(), |rest| format!("~/{}", rest.display()));
    Cursor::hide();
    let chosen = loop {
        let (w, h) = (app.main_p.w as usize, app.main_p.h as usize);
        let (per_row, cell_w) = columns(w);
        // A card and the gap to its right neighbour fill one cell of the grid.
        let card_w = if per_row > 1 { cell_w - 1 } else { cell_w };
        let card_h = CARD_H.min(h).max(4);
        let rows = (h / card_h).max(1);
        sel = sel.min(shown.len().saturating_sub(1));
        if sel / per_row < top { top = sel / per_row; }
        if sel / per_row >= top + rows { top = sel / per_row + 1 - rows; }

        let inner = card_w.saturating_sub(4);
        // A picture leaves two rows of the card for text.
        let picture_box = card_h.saturating_sub(5).max(1);
        let mut pictures: Vec<pics::Shown> = Vec::new();
        let mut out = String::new();
        for r in top..top + rows {
            let mut drawn: Vec<Vec<String>> = Vec::new();
            for c in 0..per_row {
                let Some(&at) = shown.get(r * per_row + c) else { break };
                let card = &cards[at];
                // One picture file goes on one card: showing it again moves it.
                let file = card.picture.as_ref().filter(|_| app.pictures).map(|p| dir.join(p))
                    .filter(|f| !pictures.iter().any(|p| Path::new(&p.path) == f.as_path()));
                let size = file.as_ref().and_then(|f| pics::size(f.to_str()?, inner as u16, picture_box as u16));
                if let (Some(file), Some((pic_w, pic_h))) = (&file, size) {
                    pictures.push(pics::Shown {
                        path: file.to_string_lossy().into_owned(),
                        x: app.main_p.x + (c * cell_w) as u16 + 2 + (inner as u16).saturating_sub(pic_w) / 2,
                        y: app.main_p.y + ((r - top) * card_h) as u16 + 2,
                        cols: pic_w,
                        rows: pic_h,
                        visible: pic_h,
                    });
                }
                drawn.push(draw(card, card_w, card_h, r * per_row + c == sel, size.map_or(0, |s| s.1 as usize)));
            }
            for line in 0..card_h {
                for card in &drawn {
                    out.push_str(&card[line]);
                    if per_row > 1 { out.push(' '); }
                }
                out.push('\n');
            }
        }
        if shown.is_empty() {
            out = format!("\n  {}", if query.is_empty() { "No text files here." } else { "No file has that in it." });
        }
        let count = if query.is_empty() {
            format!("{} files", cards.len())
        } else {
            format!("{} of {} with \"{}\"", shown.len(), cards.len(), query)
        };
        app.header.say(&style::bold(&format!(" {}  ({})", place, count)));
        app.main_p.set_text(&out);
        app.main_p.refresh();
        app.footer.say(&style::fg(" h j k l move   Enter open   / find   q back", 244));
        pics::place(pictures);

        let Some(key) = Input::getchr(None) else { break None };
        let last = shown.len().saturating_sub(1);
        match key.as_str() {
            "ENTER" => break shown.get(sel).map(|&at| cards[at].path.clone()),
            "q" => break None,
            "ESC" if query.is_empty() => break None,
            "ESC" => { query.clear(); shown = (0..cards.len()).collect(); sel = 0; }
            "h" | "LEFT" => sel = sel.saturating_sub(1),
            "l" | "RIGHT" => sel = (sel + 1).min(last),
            "k" | "UP" => sel = sel.saturating_sub(per_row),
            "j" | "DOWN" => if sel + per_row <= last { sel += per_row } else if sel / per_row < last / per_row { sel = last },
            "g" | "HOME" => sel = 0,
            "G" | "END" => sel = last,
            "PgUP" => sel = sel.saturating_sub(per_row * rows),
            "PgDOWN" => sel = (sel + per_row * rows).min(last),
            "/" => {
                query = app.footer.ask_with_bg(" /", "", 17).trim().to_lowercase();
                Cursor::hide();
                shown = (0..cards.len()).filter(|&at| cards[at].matches(&query)).collect();
                sel = 0;
            }
            "RESIZE" => app.handle_resize(),
            _ => {}
        }
    };
    pics::hide();
    Cursor::show();
    chosen
}

#[cfg(test)]
mod tests {
    use super::*;

    fn card(title: &str, text: &str) -> Card {
        let tags = tags_of(text);
        Card {
            path: PathBuf::from(title),
            title: title.to_string(),
            lines: lines_of(text),
            picture: None,
            pinned: false,
            lower: text.to_lowercase(),
            tags,
            modified: SystemTime::UNIX_EPOCH,
        }
    }

    #[test]
    fn a_tag_is_a_word_after_a_hash_at_the_start_of_a_word() {
        assert_eq!(tags_of("#idea and #Work-2, not # Heading or a.com/#top\n#idea #ærlig"), ["idea", "Work-2", "ærlig"]);
        assert_eq!(tags_of("C# and #1 and ##"), Vec::<String>::new());
    }

    #[test]
    fn a_card_leaves_out_empty_lines_picture_links_and_the_line_of_tags() {
        let c = card("n", "![](img/a.jpg)\n\nFirst\n\tSecond\n\n#one #two\n");
        assert_eq!(c.lines, ["First", "  Second"]);
        assert_eq!(c.tags, ["one", "two"]);
    }

    #[test]
    fn every_row_of_a_card_is_as_wide_as_the_card() {
        let long = "word ".repeat(80);
        for text in ["", "short", long.as_str(), "日本語のテキスト 日本語のテキスト 日本語のテキスト 日本語のテキスト", "a\n#tag"] {
            for picture in [0, 3, 9] {
                let rows = draw(&card("A title that is far too long to fit on one row of a card", text), 30, 10, true, picture);
                assert_eq!(rows.len(), 10);
                for row in &rows { assert_eq!(display_width(row), 30, "{:?}", row); }
            }
        }
    }

    #[test]
    fn text_that_does_not_fit_ends_in_an_ellipsis() {
        let rows = draw(&card("t", &"one two three four five six seven\n".repeat(9)), 20, 6, false, 0);
        assert!(crust::strip_ansi(&rows[4]).contains('…'));
        assert!(!crust::strip_ansi(&draw(&card("t", "one"), 20, 6, false, 0)[2]).contains('…'));
    }

    #[test]
    fn cards_fill_the_width() {
        assert_eq!(columns(190), (5, 38));
        assert_eq!(columns(80), (2, 40));
        assert_eq!(columns(20), (1, 20));
    }
}
