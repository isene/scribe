//! Forward / backward search through the buffer using the regex crate.
//! Cached compiled pattern + last direction so `n` / `N` work without
//! re-prompting.

use regex::Regex;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction { Forward, Backward }

pub struct SearchState {
    pub pattern: String,
    pub regex: Option<Regex>,
    pub direction: Direction,
}

impl SearchState {
    pub fn new() -> Self {
        Self { pattern: String::new(), regex: None, direction: Direction::Forward }
    }

    pub fn set(&mut self, pattern: &str, dir: Direction) {
        self.pattern = pattern.into();
        self.regex = Regex::new(pattern).ok();
        self.direction = dir;
    }

    /// Find the next match starting at byte offset `from`, in the chosen
    /// direction. Wraps around the buffer end → start.
    pub fn find(&self, haystack: &str, from: usize, dir: Direction) -> Option<(usize, usize)> {
        let re = self.regex.as_ref()?;
        match dir {
            Direction::Forward => {
                if let Some(m) = re.find_at(haystack, from.min(haystack.len())) {
                    return Some((m.start(), m.end()));
                }
                // Wrap to start.
                if let Some(m) = re.find(haystack) { return Some((m.start(), m.end())); }
                None
            }
            Direction::Backward => {
                let limit = from.min(haystack.len());
                let prefix = &haystack[..limit];
                let mut last: Option<(usize, usize)> = None;
                for m in re.find_iter(prefix) { last = Some((m.start(), m.end())); }
                if let Some(x) = last { return Some(x); }
                // Wrap.
                let mut last: Option<(usize, usize)> = None;
                for m in re.find_iter(haystack) { last = Some((m.start(), m.end())); }
                last
            }
        }
    }
}
