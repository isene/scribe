//! Indent-based folding for HyperList (`.hl`) and any other syntax that
//! uses indentation to denote structure.
//!
//! Model: a fold spans a line and all consecutive following lines that
//! are indented further than it. A fold is "closed" when its body is
//! collapsed; `Folds::is_visible(line)` returns false for hidden lines.
//! Closing/opening a fold doesn't move text — it's purely a render-time
//! filter.
//!
//! `fold_level(line)` is the indent level: each leading TAB or `*` adds
//! one level. (HyperList accepts both for legacy reasons.)
//!
//! Usage:
//!   * `Folds::toggle_at(line, lines)` — `<SPACE>` behavior. If the line
//!     starts a fold (next line is more deeply indented), toggles
//!     between collapsed and expanded.
//!   * `Folds::toggle_recursive_at(line, lines)` — `<C-SPACE>`. Toggles
//!     this fold AND every nested fold inside it.
//!   * `Folds::set_level(n, lines)` — `\0` … `\f`. Closes every fold
//!     deeper than `n` levels in.
//!
//! Storage: a `HashSet<usize>` of START line indexes that are currently
//! closed. Cheap lookup, simple state.

// Folds exposes a complete open/close/count API; the binary currently
// uses a subset, so allow the unused public methods.
#![allow(dead_code)]

use std::collections::HashSet;

#[derive(Default, Clone, Debug)]
pub struct Folds {
    closed: HashSet<usize>,
    /// Explicit `(start, end)` ranges added by zs/zh/show/hide. Each
    /// range hides lines `start+1..=end` while the head at `start`
    /// remains visible. Stored separately from indent-derived folds
    /// so they coexist (you can space-toggle within a show/hide
    /// view).
    explicit: Vec<(usize, usize)>,
}

/// Indent level of `line`: count of leading TAB or `*` chars. Empty
/// lines and lines beginning with neither return 0.
pub fn fold_level(line: &str) -> usize {
    line.chars()
        .take_while(|c| *c == '\t' || *c == '*')
        .count()
}

/// Index of the last line in the fold starting at `start` — i.e. the
/// last line whose indent is strictly greater than line `start`'s.
/// Skips blank lines (they're considered part of whatever fold contains
/// them). Returns `start` itself if the next line isn't a child.
pub fn fold_end(start: usize, lines: &[String]) -> usize {
    if start >= lines.len() { return start; }
    let parent_lvl = fold_level(&lines[start]);
    let mut end = start;
    let mut i = start + 1;
    while i < lines.len() {
        let line = &lines[i];
        if line.trim().is_empty() {
            // Blank — peek ahead to see if a child follows.
            let mut j = i + 1;
            while j < lines.len() && lines[j].trim().is_empty() { j += 1; }
            if j < lines.len() && fold_level(&lines[j]) > parent_lvl {
                end = j;
                i = j + 1;
                continue;
            }
            break;
        }
        if fold_level(line) > parent_lvl {
            end = i;
            i += 1;
        } else {
            break;
        }
    }
    end
}

/// True iff `start` has at least one child line (i.e. is foldable).
pub fn is_foldable(start: usize, lines: &[String]) -> bool {
    fold_end(start, lines) > start
}

/// Walk upward from `line` to find its immediate parent — the nearest
/// preceding non-blank line whose indent level is strictly less.
/// Returns None for top-level lines (level 0) and when no parent
/// exists.
pub fn find_parent(line: usize, lines: &[String]) -> Option<usize> {
    if line == 0 || line >= lines.len() { return None; }
    let lvl = fold_level(&lines[line]);
    if lvl == 0 { return None; }
    let mut i = line;
    while i > 0 {
        i -= 1;
        if lines[i].trim().is_empty() { continue; }
        if fold_level(&lines[i]) < lvl { return Some(i); }
    }
    None
}

impl Folds {
    pub fn new() -> Self { Self::default() }

    pub fn clear(&mut self) { self.closed.clear(); self.explicit.clear(); }

    /// Add a force-close fold over `[start..=end]` (head visible,
    /// rest hidden). Used by zs/zh to collapse runs of non-matching
    /// lines.
    pub fn close_range(&mut self, start: usize, end: usize) {
        if end > start { self.explicit.push((start, end)); }
    }

    /// Total number of currently-closed folds.
    pub fn count(&self) -> usize { self.closed.len() }

    /// True iff the fold starting at `line` is currently closed.
    /// (Distinct from `is_visible`, which asks the inverse question
    /// about a child line.)
    pub fn is_closed(&self, line: usize) -> bool { self.closed.contains(&line) }

    /// Force-close the fold at `line`. No-op if it's already closed
    /// or not foldable — callers should check `is_foldable` first.
    pub fn close(&mut self, line: usize) { self.closed.insert(line); }

    /// Force-open the fold at `line`. No-op if it's not closed.
    pub fn open(&mut self, line: usize) { self.closed.remove(&line); }

    /// True if `line` is hidden by some closed fold above it.
    pub fn is_visible(&self, line: usize, lines: &[String]) -> bool {
        // Explicit range hides lines strictly inside (start..=end].
        for &(s, e) in &self.explicit {
            if line > s && line <= e { return false; }
        }
        if self.closed.is_empty() { return true; }
        for &s in &self.closed {
            if s < line && fold_end(s, lines) >= line {
                return false;
            }
        }
        true
    }

    /// Iterate every closed fold start whose hidden range contains
    /// `line`. Innermost first (largest start).
    pub fn closed_folds_containing(&self, line: usize, lines: &[String]) -> Vec<usize> {
        let mut hits: Vec<usize> = self.closed.iter()
            .copied()
            .filter(|&s| s < line && fold_end(s, lines) >= line)
            .collect();
        hits.sort_by(|a, b| b.cmp(a));
        hits
    }

    /// `<SPACE>`: toggle the fold at `line`.
    ///
    /// - On a foldable line (has children): flip its closed state.
    /// - On a leaf inside a closed fold: open the innermost
    ///   containing fold (escape upward).
    /// - On a leaf with no closed ancestor: fold the immediate
    ///   parent — handy for "done with this branch, collapse it"
    ///   without having to navigate back to the parent line first.
    pub fn toggle_at(&mut self, line: usize, lines: &[String]) {
        if !is_foldable(line, lines) {
            if let Some(&s) = self.closed_folds_containing(line, lines).first() {
                self.closed.remove(&s);
                return;
            }
            if let Some(p) = find_parent(line, lines) {
                self.closed.insert(p);
            }
            return;
        }
        if self.closed.contains(&line) {
            self.closed.remove(&line);
        } else {
            self.closed.insert(line);
        }
    }

    /// `<C-SPACE>`: toggle the fold at `line` AND every nested fold
    /// strictly inside it. If the outer fold is currently open, closes
    /// the outer and every descendant. If currently closed, opens
    /// everything back up.
    pub fn toggle_recursive_at(&mut self, line: usize, lines: &[String]) {
        if !is_foldable(line, lines) {
            if let Some(&s) = self.closed_folds_containing(line, lines).first() {
                self.toggle_recursive_at(s, lines);
            }
            return;
        }
        let end = fold_end(line, lines);
        let was_closed = self.closed.contains(&line);
        if was_closed {
            // Open: clear every closed fold whose start is in [line..=end].
            self.closed.retain(|&s| !(s >= line && s <= end));
        } else {
            // Close: insert this AND every child that's foldable.
            self.closed.insert(line);
            let mut i = line + 1;
            while i <= end {
                if is_foldable(i, lines) {
                    self.closed.insert(i);
                }
                i += 1;
            }
        }
    }

    /// `\N`: globally close every fold deeper than `level` (in levels;
    /// the line's indent count). After this call, every fold whose
    /// start has indent >= `level` is closed; folds shallower than
    /// `level` are open.
    ///
    /// `level=0` opens all folds; large `level` (e.g. 15) closes
    /// nothing. The vim convention is "show up to N levels expanded".
    pub fn set_level(&mut self, level: usize, lines: &[String]) {
        self.closed.clear();
        for (i, line) in lines.iter().enumerate() {
            if !is_foldable(i, lines) { continue; }
            // Fold at line `i` has indent = fold_level(line). When the
            // user says "show level 3", folds whose START indent is >=3
            // should be closed (their bodies are at depth > 3).
            if fold_level(line) >= level {
                self.closed.insert(i);
            }
        }
    }

    /// Open every fold (alias for `set_level(usize::MAX)` semantics).
    pub fn open_all(&mut self) { self.closed.clear(); }

    /// Shift fold state after a structural edit: `delta` lines were
    /// added (positive) or removed (negative) starting just after
    /// `at_line`. Closed-fold starts and explicit ranges move with the
    /// text they were attached to; fold heads that sat inside a deleted
    /// span are dropped. Without this, every insert/delete above a
    /// closed fold left the stored line index pointing one line off —
    /// most visibly, a freshly inserted item "inherited" the next
    /// sibling's closed fold and collapsed the moment it gained a child.
    pub fn shift_lines(&mut self, at_line: usize, delta: isize) {
        if delta == 0 { return; }
        let moved = |s: usize| -> Option<usize> {
            if s <= at_line { return Some(s); }
            if delta < 0 {
                let cut = (-delta) as usize;
                if s <= at_line + cut { return None; }      // inside deleted span
                Some(s - cut)
            } else {
                Some(s + delta as usize)
            }
        };
        self.closed = self.closed.iter().filter_map(|&s| moved(s)).collect();
        self.explicit = self.explicit.iter()
            .filter_map(|&(s, e)| match (moved(s), moved(e)) {
                (Some(s2), Some(e2)) if e2 > s2 => Some((s2, e2)),
                _ => None,
            })
            .collect();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(s: &str) -> Vec<String> {
        s.lines().map(|l| l.to_string()).collect()
    }

    #[test]
    fn fold_level_counts_tabs_and_asterisks() {
        assert_eq!(fold_level("foo"), 0);
        assert_eq!(fold_level("\tfoo"), 1);
        assert_eq!(fold_level("\t\tfoo"), 2);
        assert_eq!(fold_level("**foo"), 2);
        assert_eq!(fold_level("\t*foo"), 2);
    }

    #[test]
    fn fold_end_basic() {
        let l = lines("a\n\tb\n\t\tc\n\td\ne");
        assert_eq!(fold_end(0, &l), 3);
        assert_eq!(fold_end(1, &l), 2);
        assert_eq!(fold_end(4, &l), 4);
    }

    #[test]
    fn toggle_hides_children() {
        let l = lines("a\n\tb\n\tc\nd");
        let mut f = Folds::new();
        f.toggle_at(0, &l);
        assert!(f.is_visible(0, &l));
        assert!(!f.is_visible(1, &l));
        assert!(!f.is_visible(2, &l));
        assert!(f.is_visible(3, &l));
        f.toggle_at(0, &l);
        assert!(f.is_visible(1, &l));
    }

    #[test]
    fn shift_lines_keeps_folds_attached() {
        // Collapsed-to-level-2 tree; insert a new level-2 line between
        // two collapsed items, then give it a child. The child must
        // stay visible — the bug was that the next sibling's closed
        // index landed on the new item after the insert.
        let before = lines("a\n\tb1\n\t\tc1\n\tb2\n\t\tc2");
        let mut f = Folds::new();
        f.set_level(1, &before);          // b1 and b2 collapsed
        assert!(!f.is_visible(2, &before));
        // Insert a new "\tnew" after b1's fold (index 3), then a child.
        let after = lines("a\n\tb1\n\t\tc1\n\tnew\n\t\tchild\n\tb2\n\t\tc2");
        f.shift_lines(2, 2);              // two lines added after line 2
        assert!(f.is_visible(3, &after), "new item visible");
        assert!(f.is_visible(4, &after), "typed child stays visible");
        assert!(!f.is_visible(6, &after), "b2 stays collapsed");
        // And deletion shifts back: remove the two inserted lines.
        f.shift_lines(2, -2);
        assert!(!f.is_visible(2, &before));
        assert!(!f.is_visible(4, &before));
    }

    #[test]
    fn set_level_hides_deep() {
        let l = lines("a\n\tb\n\t\tc\n\td\ne");
        let mut f = Folds::new();
        f.set_level(1, &l);
        assert!(f.is_visible(0, &l));   // depth 0 always visible
        assert!(f.is_visible(1, &l));   // depth 1 — visible at level 1
        assert!(!f.is_visible(2, &l));  // depth 2 — hidden
        assert!(f.is_visible(3, &l));   // depth 1 — visible
        f.set_level(0, &l);
        // level 0: only level-0 lines visible
        assert!(f.is_visible(0, &l));
        assert!(!f.is_visible(1, &l));
    }
}
