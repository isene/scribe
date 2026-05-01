//! Spellcheck via a long-lived `hunspell -a` subprocess (Aspell-pipe
//! protocol). Lifecycle:
//!
//!   1. `Spell::spawn(lang)` forks `hunspell -d <lang> -i UTF-8 -a`. The
//!      first line of hunspell output is a banner (`@(#) International ...`)
//!      that we drain. Subsequent input is line-buffered: we send `^<text>`
//!      (the `^` prefix tells hunspell to treat the whole line as text and
//!      ignore any leading aspell command chars), and read response lines
//!      until a blank line marks end-of-input.
//!
//!   2. `Spell::check_lines(&[String])` returns `Vec<MisspellRange>` with
//!      byte offsets into the joined buffer. Each line is scanned in turn
//!      and word offsets summed across lines using the buffer's line byte
//!      offsets.
//!
//!   3. `Spell::shutdown()` drops stdin (hunspell exits on EOF). The Drop
//!      impl handles forgotten shutdowns.
//!
//! Hunspell `-a` response codes (one per word):
//!   `*`        word recognised
//!   `+ STEM`   recognised by affix rules
//!   `-`        compound recognised
//!   `# MISS N` no suggestions; word starts at column N (1-based)
//!   `& MISS C N: s1, s2, ...`  C suggestions, word at column N (1-based)
//!
//! Personal dictionary: kept out of hunspell, in `personal: HashSet`. After
//! parsing, words present there are filtered out before returning.

use std::collections::HashSet;
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

/// One misspelled word's location, in absolute buffer byte offsets.
#[derive(Clone, Debug)]
pub struct MisspellRange {
    pub start: usize,
    pub end: usize,
    pub word: String,
    pub suggestions: Vec<String>,
}

pub struct Spell {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    pub personal: HashSet<String>,
}

impl Spell {
    /// Spawn `hunspell -d <lang> -i UTF-8 -a` and consume the banner. Returns
    /// None if hunspell isn't on PATH or the language isn't installed.
    pub fn spawn(lang: &str) -> Option<Self> {
        let mut child = Command::new("hunspell")
            .args(["-d", lang, "-i", "UTF-8", "-a"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .ok()?;
        let stdin = child.stdin.take()?;
        let stdout = BufReader::new(child.stdout.take()?);
        let mut s = Self { child, stdin, stdout, personal: HashSet::new() };
        // Drain the banner line; if hunspell aborted (missing dict) the
        // BufReader returns 0 bytes and we treat that as failure.
        let mut banner = String::new();
        if s.stdout.read_line(&mut banner).ok()? == 0 {
            return None;
        }
        Some(s)
    }

    /// Add a personal word (in-memory only; caller persists).
    pub fn add_personal(&mut self, word: &str) {
        self.personal.insert(word.to_lowercase());
    }

    pub fn load_personal(&mut self, words: impl IntoIterator<Item = String>) {
        for w in words { self.add_personal(&w); }
    }

    /// Check a single line. `base_byte` is that line's absolute byte offset
    /// in the buffer. Returns ranges for misspelled words, with word offsets
    /// adjusted to absolute byte offsets and the suggestions list per word.
    fn check_line(&mut self, line: &str, base_byte: usize) -> Vec<MisspellRange> {
        let mut out = Vec::new();
        if line.trim().is_empty() {
            return out;
        }
        // Send the line prefixed with `^` so hunspell ignores any leading
        // aspell command chars in user text.
        if writeln!(self.stdin, "^{}", line).is_err() { return out; }
        if self.stdin.flush().is_err() { return out; }

        // Read response lines until a blank line (end-of-input marker).
        loop {
            let mut buf = String::new();
            if self.stdout.read_line(&mut buf).unwrap_or(0) == 0 { break; }
            let trimmed = buf.trim_end_matches(|c| c == '\n' || c == '\r');
            if trimmed.is_empty() { break; }
            let first = trimmed.as_bytes().first().copied().unwrap_or(0);
            match first {
                b'*' | b'+' | b'-' => {} // recognised, skip
                b'#' => {
                    // `# WORD COLUMN`
                    if let Some((word, col)) = parse_hash(trimmed) {
                        if !self.personal.contains(&word.to_lowercase()) {
                            let start = base_byte + col_to_byte(line, col);
                            let end = start + word.len();
                            out.push(MisspellRange { start, end, word, suggestions: Vec::new() });
                        }
                    }
                }
                b'&' => {
                    // `& WORD COUNT COLUMN: s1, s2, ...`
                    if let Some((word, col, sugg)) = parse_amp(trimmed) {
                        if !self.personal.contains(&word.to_lowercase()) {
                            let start = base_byte + col_to_byte(line, col);
                            let end = start + word.len();
                            out.push(MisspellRange { start, end, word, suggestions: sugg });
                        }
                    }
                }
                _ => {} // ignore unknown markers
            }
        }
        out
    }

    /// Check a sequence of (line, base_byte) pairs. Caller chooses which lines
    /// to scan (e.g. skipping email headers / quoted-reply blocks).
    pub fn check_lines(&mut self, lines: &[(String, usize)]) -> Vec<MisspellRange> {
        let mut all = Vec::new();
        for (line, base) in lines {
            all.extend(self.check_line(line, *base));
        }
        all
    }
}

impl Drop for Spell {
    fn drop(&mut self) {
        // Closing stdin signals EOF; hunspell exits cleanly.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Parse a `#` line: `# WORD COLUMN` (column is 1-based char index).
fn parse_hash(line: &str) -> Option<(String, usize)> {
    let rest = line.strip_prefix("# ")?;
    let mut it = rest.rsplitn(2, ' ');
    let col_str = it.next()?;
    let word = it.next()?.to_string();
    let col: usize = col_str.parse().ok()?;
    Some((word, col))
}

/// Parse a `&` line: `& WORD COUNT COLUMN: s1, s2, ...`.
fn parse_amp(line: &str) -> Option<(String, usize, Vec<String>)> {
    let rest = line.strip_prefix("& ")?;
    // Split on the colon that separates header from suggestion list.
    let (head, sugg_part) = rest.split_once(": ").unwrap_or((rest, ""));
    let parts: Vec<&str> = head.split_whitespace().collect();
    if parts.len() < 3 { return None; }
    // Last two tokens are count + column; everything before is the word
    // (hunspell never emits multi-word entries, but be defensive).
    let col: usize = parts[parts.len() - 1].parse().ok()?;
    let _count: usize = parts[parts.len() - 2].parse().ok()?;
    let word = parts[..parts.len() - 2].join(" ");
    let suggestions: Vec<String> = sugg_part
        .split(", ")
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .collect();
    Some((word, col, suggestions))
}

/// Hunspell columns are 1-based char counts. Convert to 0-based byte offset
/// within the line so we can map to absolute buffer bytes.
fn col_to_byte(line: &str, col_1based: usize) -> usize {
    let target = col_1based.saturating_sub(1);
    let mut count = 0;
    for (b, _) in line.char_indices() {
        if count == target { return b; }
        count += 1;
    }
    line.len()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test] fn parse_hash_basic() {
        let (w, c) = parse_hash("# zzzqq 5").unwrap();
        assert_eq!(w, "zzzqq"); assert_eq!(c, 5);
    }
    #[test] fn parse_amp_basic() {
        let (w, c, s) = parse_amp("& mispeled 5 3: misled, dispelled, misplaced, mistyped, misspelled").unwrap();
        assert_eq!(w, "mispeled"); assert_eq!(c, 3);
        assert_eq!(s.len(), 5);
        assert_eq!(s[4], "misspelled");
    }
    #[test] fn col_byte_ascii() { assert_eq!(col_to_byte("hello", 3), 2); }
    #[test] fn col_byte_utf8() {
        // "kåre" — 'å' is 2 bytes; col 3 should be byte index of 'r'.
        assert_eq!(col_to_byte("kåre", 3), 3);
    }
}
