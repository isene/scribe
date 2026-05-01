//! Pure motion primitives. All take a `Buffer` and a starting byte offset,
//! return the destination byte offset (charwise) or `None` (no movement).
//!
//! Word semantics follow vim:
//!   - "small word" (w/b/e): runs of `\w` chars OR runs of punctuation.
//!     Whitespace separates.
//!   - "WORD" (W/B/E): non-whitespace sequences.

use crate::buffer::Buffer;

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum CharClass {
    Word,        // alphanumeric or _
    Punct,       // visible non-word
    Space,       // whitespace including newline
}

pub fn classify(c: char) -> CharClass {
    if c.is_alphanumeric() || c == '_' { CharClass::Word }
    else if c.is_whitespace()          { CharClass::Space }
    else                               { CharClass::Punct }
}

/// Whole rope as a String. Cheap for small buffers; rope iter for big.
fn rope_string(buf: &Buffer) -> String { buf.rope.to_string() }

/// Find the next char-boundary at or after `idx`.
fn next_boundary(s: &str, idx: usize) -> usize {
    let mut i = idx;
    while i < s.len() && !s.is_char_boundary(i) { i += 1; }
    i
}

fn prev_boundary(s: &str, idx: usize) -> usize {
    if idx == 0 { return 0; }
    let mut i = idx - 1;
    while i > 0 && !s.is_char_boundary(i) { i -= 1; }
    i
}

/// `w`: move to the start of the next word (small-word).
pub fn word_forward(buf: &Buffer, mut start: usize) -> usize {
    let s = rope_string(buf);
    if start >= s.len() { return s.len(); }
    let cur = s[start..].chars().next().map(classify).unwrap_or(CharClass::Space);
    // Skip rest of current run (if not whitespace).
    if cur != CharClass::Space {
        while start < s.len() {
            let c = s[start..].chars().next().unwrap();
            if classify(c) != cur { break; }
            start = next_boundary(&s, start + c.len_utf8());
        }
    }
    // Skip whitespace (incl. newlines).
    while start < s.len() {
        let c = s[start..].chars().next().unwrap();
        if classify(c) != CharClass::Space { break; }
        start = next_boundary(&s, start + c.len_utf8());
    }
    start
}

/// `b`: move to the start of the previous word.
pub fn word_backward(buf: &Buffer, mut start: usize) -> usize {
    let s = rope_string(buf);
    if start == 0 { return 0; }
    // Step back over the current char.
    start = prev_boundary(&s, start);
    // Skip whitespace going backward.
    while start > 0 {
        let c = s[start..].chars().next().unwrap();
        if classify(c) != CharClass::Space { break; }
        start = prev_boundary(&s, start);
    }
    if start == 0 { return 0; }
    // Now find the start of the run containing start.
    let cur = s[start..].chars().next().map(classify).unwrap_or(CharClass::Space);
    while start > 0 {
        let prev = prev_boundary(&s, start);
        let pc = s[prev..].chars().next().unwrap();
        if classify(pc) != cur { break; }
        start = prev;
    }
    start
}

/// `e`: move to the end of the current/next word (returns the byte INDEX of
/// the last char of the word, NOT one-past-end — vim's e lands ON the char).
pub fn word_end(buf: &Buffer, mut start: usize) -> usize {
    let s = rope_string(buf);
    if start >= s.len() { return s.len(); }
    // Step forward one so we don't get stuck on a word's last char.
    let first = s[start..].chars().next().unwrap();
    start = next_boundary(&s, start + first.len_utf8());
    // Skip whitespace.
    while start < s.len() {
        let c = s[start..].chars().next().unwrap();
        if classify(c) != CharClass::Space { break; }
        start = next_boundary(&s, start + c.len_utf8());
    }
    if start >= s.len() { return s.len().saturating_sub(1); }
    let cur = s[start..].chars().next().map(classify).unwrap_or(CharClass::Space);
    // Walk until run ends; return last in-run index.
    let mut last = start;
    while start < s.len() {
        let c = s[start..].chars().next().unwrap();
        if classify(c) != cur { break; }
        last = start;
        start = next_boundary(&s, start + c.len_utf8());
    }
    last
}

/// `W`: WORD forward — same as word_forward but only whitespace separates.
pub fn big_word_forward(buf: &Buffer, mut start: usize) -> usize {
    let s = rope_string(buf);
    while start < s.len() {
        let c = s[start..].chars().next().unwrap();
        if classify(c) == CharClass::Space { break; }
        start = next_boundary(&s, start + c.len_utf8());
    }
    while start < s.len() {
        let c = s[start..].chars().next().unwrap();
        if classify(c) != CharClass::Space { break; }
        start = next_boundary(&s, start + c.len_utf8());
    }
    start
}

/// `B`: WORD backward.
pub fn big_word_backward(buf: &Buffer, mut start: usize) -> usize {
    let s = rope_string(buf);
    if start == 0 { return 0; }
    start = prev_boundary(&s, start);
    while start > 0 {
        let c = s[start..].chars().next().unwrap();
        if classify(c) != CharClass::Space { break; }
        start = prev_boundary(&s, start);
    }
    if start == 0 { return 0; }
    while start > 0 {
        let prev = prev_boundary(&s, start);
        let pc = s[prev..].chars().next().unwrap();
        if classify(pc) == CharClass::Space { break; }
        start = prev;
    }
    start
}

/// Beginning of current line (column 0).
pub fn line_start(buf: &Buffer, byte: usize) -> usize {
    let line = buf.rope.byte_to_line(byte);
    buf.line_byte_offset(line)
}

/// First non-whitespace char of current line.
pub fn line_first_nonblank(buf: &Buffer, byte: usize) -> usize {
    let line = buf.rope.byte_to_line(byte);
    let off = buf.line_byte_offset(line);
    let l = buf.line(line);
    let mut i = 0;
    for c in l.chars() {
        if !c.is_whitespace() { return off + i; }
        i += c.len_utf8();
    }
    off + i
}

/// End of current line (one past the last char, BEFORE the newline).
pub fn line_end(buf: &Buffer, byte: usize) -> usize {
    let line = buf.rope.byte_to_line(byte);
    let off = buf.line_byte_offset(line);
    off + buf.line(line).len()
}

/// Find char `target` forward on the current line. Returns Some(byte) where
/// the char is found, or None.
pub fn find_forward(buf: &Buffer, byte: usize, target: char) -> Option<usize> {
    let line = buf.rope.byte_to_line(byte);
    let off = buf.line_byte_offset(line);
    let l = buf.line(line);
    let cur_col = byte.saturating_sub(off);
    let mut i = cur_col + 1;
    while i < l.len() {
        if !l.is_char_boundary(i) { i += 1; continue; }
        if let Some(c) = l[i..].chars().next() {
            if c == target { return Some(off + i); }
            i += c.len_utf8();
        } else { break; }
    }
    None
}

pub fn find_backward(buf: &Buffer, byte: usize, target: char) -> Option<usize> {
    let line = buf.rope.byte_to_line(byte);
    let off = buf.line_byte_offset(line);
    let l = buf.line(line);
    let cur_col = byte.saturating_sub(off);
    if cur_col == 0 { return None; }
    let prefix = &l[..cur_col];
    prefix.char_indices().rev().find_map(|(i, c)| if c == target { Some(off + i) } else { None })
}
