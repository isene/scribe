//! Text buffer backed by a Rope (ropey crate) with an undo TREE.
//!
//! Why a rope: O(log n) insert/delete on huge files, O(log n) line/byte/char
//! conversions. Don't write your own piece-table; ropey is a battle-tested
//! library used in production editors.
//!
//! Why a tree (not a stack): undo + redo + new edit creates a branch. We
//! preserve all branches so the user can navigate to "what I had 5 minutes
//! ago" after a wrong-direction undo.

use ropey::Rope;
use std::path::PathBuf;

/// A single edit: replace `range` bytes with `replacement`.
#[derive(Clone, Debug)]
pub struct Edit {
    pub start: usize,
    pub end: usize,
    pub replacement: String,
    /// What was at `start..end` before the edit (so we can undo).
    pub original: String,
}

#[derive(Clone, Debug)]
struct UndoNode {
    edit: Edit,
    parent: Option<usize>,
    children: Vec<usize>,
}

pub struct Buffer {
    pub rope: Rope,
    pub path: Option<PathBuf>,
    pub dirty: bool,
    nodes: Vec<UndoNode>,
    /// Index of the node that represents the current state. None = pristine.
    head: Option<usize>,
}

impl Buffer {
    pub fn empty() -> Self {
        Self {
            rope: Rope::new(),
            path: None, dirty: false,
            nodes: Vec::new(), head: None,
        }
    }

    pub fn from_path(path: PathBuf) -> std::io::Result<Self> {
        let s = std::fs::read_to_string(&path).unwrap_or_default();
        Ok(Self {
            rope: Rope::from_str(&s),
            path: Some(path), dirty: false,
            nodes: Vec::new(), head: None,
        })
    }

    pub fn save(&mut self) -> std::io::Result<()> {
        let Some(path) = self.path.clone() else {
            return Err(std::io::Error::new(std::io::ErrorKind::Other, "no file"));
        };
        let mut s = String::new();
        for chunk in self.rope.chunks() { s.push_str(chunk); }
        std::fs::write(&path, s)?;
        self.dirty = false;
        Ok(())
    }

    /// Apply an edit and record it on the undo tree.
    pub fn apply(&mut self, start: usize, end: usize, replacement: &str) {
        let original: String = self.rope.byte_slice(start..end).to_string();
        let edit = Edit { start, end, replacement: replacement.into(), original };
        // Apply to rope.
        let start_char = self.rope.byte_to_char(start);
        let end_char = self.rope.byte_to_char(end);
        self.rope.remove(start_char..end_char);
        self.rope.insert(start_char, replacement);
        self.dirty = true;
        // Record.
        let node = UndoNode { edit, parent: self.head, children: Vec::new() };
        let idx = self.nodes.len();
        self.nodes.push(node);
        if let Some(p) = self.head {
            self.nodes[p].children.push(idx);
        }
        self.head = Some(idx);
    }

    /// Undo the current head's edit. Returns the byte offset where the cursor
    /// should land (start of the restored original text), or None if nothing
    /// to undo.
    pub fn undo(&mut self) -> Option<usize> {
        let head = self.head?;
        let node = self.nodes[head].clone();
        // Reverse the edit.
        let new_end = node.edit.start + node.edit.replacement.len();
        let start_char = self.rope.byte_to_char(node.edit.start);
        let end_char = self.rope.byte_to_char(new_end);
        self.rope.remove(start_char..end_char);
        self.rope.insert(start_char, &node.edit.original);
        self.head = node.parent;
        self.dirty = true;
        Some(node.edit.start)
    }

    /// Redo: walk to the most-recently-added child of head. Returns the byte
    /// offset where the cursor should land (just after the re-applied edit),
    /// or None if no redo branch.
    pub fn redo(&mut self) -> Option<usize> {
        let target = match self.head {
            Some(h) => self.nodes[h].children.last().copied(),
            None => self.nodes.iter().enumerate().find(|(_, n)| n.parent.is_none()).map(|(i, _)| i),
        };
        let target = target?;
        let node = self.nodes[target].clone();
        let start_char = self.rope.byte_to_char(node.edit.start);
        let end_char = self.rope.byte_to_char(node.edit.end);
        self.rope.remove(start_char..end_char);
        self.rope.insert(start_char, &node.edit.replacement);
        self.head = Some(target);
        self.dirty = true;
        Some(node.edit.start + node.edit.replacement.len())
    }

    pub fn line_count(&self) -> usize { self.rope.len_lines() }
    pub fn line(&self, idx: usize) -> String {
        if idx >= self.rope.len_lines() { return String::new(); }
        let line = self.rope.line(idx);
        let mut s: String = line.into();
        if s.ends_with('\n') { s.pop(); }
        s
    }
    pub fn line_byte_offset(&self, line: usize) -> usize {
        if line >= self.rope.len_lines() {
            return self.rope.len_bytes();
        }
        self.rope.line_to_byte(line)
    }
    pub fn byte_to_line_col(&self, byte: usize) -> (usize, usize) {
        let line = self.rope.byte_to_line(byte);
        let line_start = self.rope.line_to_byte(line);
        (line, byte - line_start)
    }
}
