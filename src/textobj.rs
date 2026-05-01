//! Text objects. Each takes a `Buffer` and a cursor byte offset, returns the
//! byte range to operate on. `inner` excludes the delimiters; `around`
//! includes them (and trailing whitespace for word objects).

use crate::buffer::Buffer;
use crate::motion::{classify, CharClass};

fn rope_string(buf: &Buffer) -> String { buf.rope.to_string() }

/// `iw` — inner word. Returns the byte range of the word containing `cur`.
pub fn inner_word(buf: &Buffer, cur: usize) -> Option<(usize, usize)> {
    let s = rope_string(buf);
    if cur >= s.len() { return None; }
    let c = s[cur..].chars().next()?;
    let cls = classify(c);
    if cls == CharClass::Space { return None; }
    let mut start = cur;
    while start > 0 {
        let mut p = start - 1;
        while p > 0 && !s.is_char_boundary(p) { p -= 1; }
        let pc = s[p..].chars().next().unwrap();
        if classify(pc) != cls { break; }
        start = p;
    }
    let mut end = cur;
    while end < s.len() {
        let ch = s[end..].chars().next().unwrap();
        if classify(ch) != cls { break; }
        end += ch.len_utf8();
    }
    Some((start, end))
}

/// `aw` — outer word: include trailing whitespace (or leading if at EOL).
pub fn around_word(buf: &Buffer, cur: usize) -> Option<(usize, usize)> {
    let (start, mut end) = inner_word(buf, cur)?;
    let s = rope_string(buf);
    // Extend end through trailing spaces (not newlines).
    while end < s.len() {
        let ch = s[end..].chars().next().unwrap();
        if ch == ' ' || ch == '\t' { end += ch.len_utf8(); }
        else { break; }
    }
    Some((start, end))
}

/// Inner range between matching pair `open`/`close` containing `cur`.
/// Excludes the delimiters.
pub fn inner_pair(buf: &Buffer, cur: usize, open: char, close: char) -> Option<(usize, usize)> {
    let s = rope_string(buf);
    // Find nearest unmatched `open` before cur.
    let mut depth = 0i32;
    let mut start: Option<usize> = None;
    let mut i = cur;
    while i > 0 {
        let mut p = i - 1;
        while p > 0 && !s.is_char_boundary(p) { p -= 1; }
        let ch = s[p..].chars().next()?;
        if ch == close { depth += 1; }
        else if ch == open {
            if depth == 0 { start = Some(p); break; }
            depth -= 1;
        }
        i = p;
    }
    let start = start?;
    let inner_start = start + open.len_utf8();
    // Find matching close after inner_start.
    let mut depth = 0i32;
    let mut j = inner_start;
    let mut close_at: Option<usize> = None;
    while j < s.len() {
        let ch = s[j..].chars().next()?;
        if ch == open { depth += 1; }
        else if ch == close {
            if depth == 0 { close_at = Some(j); break; }
            depth -= 1;
        }
        j += ch.len_utf8();
    }
    let close_at = close_at?;
    Some((inner_start, close_at))
}

pub fn around_pair(buf: &Buffer, cur: usize, open: char, close: char) -> Option<(usize, usize)> {
    let (a, b) = inner_pair(buf, cur, open, close)?;
    let start = a - open.len_utf8();
    let end = b + close.len_utf8();
    Some((start, end))
}

/// Inner quoted span — `delim` is the same char for open and close.
/// Heuristic: pick the closest quote pair on the current line.
pub fn inner_quote(buf: &Buffer, cur: usize, delim: char) -> Option<(usize, usize)> {
    let line = buf.rope.byte_to_line(cur);
    let line_start = buf.line_byte_offset(line);
    let line_text = buf.line(line);
    let pos = cur - line_start;
    // Collect all delim positions on this line.
    let positions: Vec<usize> = line_text.char_indices()
        .filter_map(|(i, c)| if c == delim { Some(i) } else { None })
        .collect();
    if positions.len() < 2 { return None; }
    // Find a pair (a, b) with a <= pos < b, picking the closest.
    let mut best: Option<(usize, usize)> = None;
    let mut iter = positions.windows(2);
    while let Some(w) = iter.next() {
        let (a, b) = (w[0], w[1]);
        if a <= pos && pos <= b {
            best = Some((a, b));
            break;
        }
    }
    let (a, b) = best?;
    Some((line_start + a + delim.len_utf8(), line_start + b))
}

pub fn around_quote(buf: &Buffer, cur: usize, delim: char) -> Option<(usize, usize)> {
    let (a, b) = inner_quote(buf, cur, delim)?;
    Some((a - delim.len_utf8(), b + delim.len_utf8()))
}

/// `ip` — inner paragraph. Run of non-blank lines containing `cur`.
pub fn inner_paragraph(buf: &Buffer, cur: usize) -> Option<(usize, usize)> {
    let line = buf.rope.byte_to_line(cur);
    if buf.line(line).trim().is_empty() {
        // On a blank: paragraph is the run of blank lines.
        let mut top = line;
        while top > 0 && buf.line(top - 1).trim().is_empty() { top -= 1; }
        let mut bot = line;
        while bot + 1 < buf.line_count() && buf.line(bot + 1).trim().is_empty() { bot += 1; }
        let start = buf.line_byte_offset(top);
        let end = if bot + 1 >= buf.line_count() {
            buf.rope.len_bytes()
        } else { buf.line_byte_offset(bot + 1) };
        return Some((start, end));
    }
    let mut top = line;
    while top > 0 && !buf.line(top - 1).trim().is_empty() { top -= 1; }
    let mut bot = line;
    while bot + 1 < buf.line_count() && !buf.line(bot + 1).trim().is_empty() { bot += 1; }
    let start = buf.line_byte_offset(top);
    let end = if bot + 1 >= buf.line_count() {
        buf.rope.len_bytes()
    } else { buf.line_byte_offset(bot + 1) };
    Some((start, end))
}

pub fn around_paragraph(buf: &Buffer, cur: usize) -> Option<(usize, usize)> {
    let (start, mut end) = inner_paragraph(buf, cur)?;
    // Extend through trailing blank lines.
    let mut line = buf.rope.byte_to_line(end.saturating_sub(1));
    while line + 1 < buf.line_count() && buf.line(line + 1).trim().is_empty() { line += 1; }
    end = if line + 1 >= buf.line_count() {
        buf.rope.len_bytes()
    } else { buf.line_byte_offset(line + 1) };
    Some((start, end))
}
