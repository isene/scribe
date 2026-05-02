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

/// Pick a file kind from path / content. Email beats source detection so
/// that `.eml` and kastrup compose tempfiles get the email pane treatment
/// (header colors, quote levels, signature) instead of e.g. trying to
/// parse the body as some random language.
fn detect_kind(path: &PathBuf, content: &str) -> FileKind {
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
    if name.ends_with(".eml") || name.starts_with("kastrup_compose_") || name.starts_with("kastrup_body_") {
        return FileKind::Email;
    }
    let first = content.lines().find(|l| !l.is_empty()).unwrap_or("");
    if first.starts_with("From:") || first.starts_with("To:") || first.starts_with("Subject:") {
        return FileKind::Email;
    }
    // Source detection: ask the highlight crate whether the extension is
    // known. The String stored in FileKind::Source is the lowercased
    // extension — the renderer passes it back to highlight::highlight()
    // to dispatch on language.
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        let lower = ext.to_lowercase();
        if highlight::lang_known(&lower).is_some() {
            return FileKind::Source(lower);
        }
    }
    FileKind::Plain
}

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
    /// Multiple edits per node so multi-step operations (block paste, block
    /// delete, etc.) undo as one atomic action. Most nodes hold a single
    /// edit; compound nodes hold one per micro-edit.
    edits: Vec<Edit>,
    parent: Option<usize>,
    children: Vec<usize>,
}

/// Detected file kind. The renderer dispatches per variant:
/// * `Plain`   — no styling beyond the pane default
/// * `Email`   — header / quote / signature coloring + inline tokens
/// * `Source`  — syntect-based syntax highlighting; the inner String is
///   the syntax name (e.g. "Rust", "Markdown", "Bash") that
///   `highlight::find_syntax_by_name` resolves at render time.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FileKind {
    Plain,
    Email,
    Source(String),
}

pub struct Buffer {
    pub rope: Rope,
    pub path: Option<PathBuf>,
    pub dirty: bool,
    pub kind: FileKind,
    nodes: Vec<UndoNode>,
    head: Option<usize>,
    /// When > 0, `apply()` accumulates edits into `pending_compound` instead
    /// of finalising one node per call. `end_compound()` (when the depth
    /// reaches 0) commits the accumulated edits as a single node.
    compound_depth: usize,
    pending_compound: Vec<Edit>,
}

impl Buffer {
    pub fn empty() -> Self {
        Self {
            rope: Rope::new(),
            path: None, dirty: false, kind: FileKind::Plain,
            nodes: Vec::new(), head: None,
            compound_depth: 0, pending_compound: Vec::new(),
        }
    }

    /// Build an in-memory buffer from a string. Used by `:help` (which
    /// embeds the README at compile time) and by other internal scratch
    /// buffers. No path → `:w` would error or save to the user's CWD;
    /// callers that don't want save-by-accident should set `dirty=false`
    /// after construction (already the default for help).
    pub fn from_str(text: &str, kind: FileKind) -> Self {
        Self {
            rope: Rope::from_str(text),
            path: None, dirty: false, kind,
            nodes: Vec::new(), head: None,
            compound_depth: 0, pending_compound: Vec::new(),
        }
    }

    pub fn from_path(path: PathBuf) -> std::io::Result<Self> {
        let s = std::fs::read_to_string(&path).unwrap_or_default();
        let kind = detect_kind(&path, &s);
        Ok(Self {
            rope: Rope::from_str(&s),
            path: Some(path), dirty: false, kind,
            nodes: Vec::new(), head: None,
            compound_depth: 0, pending_compound: Vec::new(),
        })
    }

    /// Begin grouping subsequent `apply()` calls into a single undo node.
    /// Calls nest; only the outermost `end_compound` finalises.
    pub fn begin_compound(&mut self) { self.compound_depth += 1; }

    /// Commit the pending compound edits as one undo node. No-op if outside
    /// a compound or no edits accumulated.
    pub fn end_compound(&mut self) {
        if self.compound_depth == 0 { return; }
        self.compound_depth -= 1;
        if self.compound_depth == 0 && !self.pending_compound.is_empty() {
            let edits = std::mem::take(&mut self.pending_compound);
            let node = UndoNode { edits, parent: self.head, children: Vec::new() };
            let idx = self.nodes.len();
            self.nodes.push(node);
            if let Some(p) = self.head { self.nodes[p].children.push(idx); }
            self.head = Some(idx);
        }
    }

    pub fn save(&mut self) -> std::io::Result<()> {
        let Some(path) = self.path.clone() else {
            return Err(std::io::Error::new(std::io::ErrorKind::Other, "no file"));
        };
        // Belt + braces: snapshot the current on-disk content to
        // `<path>.scribe-bak` before overwriting. Survives accidental
        // `:wq` after a destructive `:claude` turn — the original file is
        // one rename away. Failure to write the backup is non-fatal; the
        // save still proceeds.
        if path.exists() {
            let mut bak = path.clone().into_os_string();
            bak.push(".scribe-bak");
            let _ = std::fs::copy(&path, std::path::PathBuf::from(bak));
        }
        let mut s = String::new();
        for chunk in self.rope.chunks() { s.push_str(chunk); }
        std::fs::write(&path, s)?;
        self.dirty = false;
        Ok(())
    }

    /// Apply an edit and record it on the undo tree. When inside a compound
    /// (`begin_compound` has been called), accumulate into pending_compound
    /// instead of creating a node per call.
    pub fn apply(&mut self, start: usize, end: usize, replacement: &str) {
        let original: String = self.rope.byte_slice(start..end).to_string();
        let edit = Edit { start, end, replacement: replacement.into(), original };
        let start_char = self.rope.byte_to_char(start);
        let end_char = self.rope.byte_to_char(end);
        self.rope.remove(start_char..end_char);
        self.rope.insert(start_char, replacement);
        self.dirty = true;
        if self.compound_depth > 0 {
            self.pending_compound.push(edit);
        } else {
            let node = UndoNode { edits: vec![edit], parent: self.head, children: Vec::new() };
            let idx = self.nodes.len();
            self.nodes.push(node);
            if let Some(p) = self.head { self.nodes[p].children.push(idx); }
            self.head = Some(idx);
        }
    }

    /// Undo the current head's edits (reverse-order across the node's edits
    /// for compound undo). Returns the byte offset where the cursor should
    /// land (start of the first edit's restored region), or None if nothing
    /// to undo.
    pub fn undo(&mut self) -> Option<usize> {
        let head = self.head?;
        let node = self.nodes[head].clone();
        for e in node.edits.iter().rev() {
            let new_end = e.start + e.replacement.len();
            let start_char = self.rope.byte_to_char(e.start);
            let end_char = self.rope.byte_to_char(new_end);
            self.rope.remove(start_char..end_char);
            self.rope.insert(start_char, &e.original);
        }
        self.head = node.parent;
        self.dirty = true;
        Some(node.edits.first().map(|e| e.start).unwrap_or(0))
    }

    /// Redo: walk to the most-recently-added child of head. Re-applies all
    /// edits in original order. Returns the byte offset where the cursor
    /// should land, or None if no redo branch.
    pub fn redo(&mut self) -> Option<usize> {
        let target = match self.head {
            Some(h) => self.nodes[h].children.last().copied(),
            None => self.nodes.iter().enumerate().find(|(_, n)| n.parent.is_none()).map(|(i, _)| i),
        };
        let target = target?;
        let node = self.nodes[target].clone();
        for e in &node.edits {
            let start_char = self.rope.byte_to_char(e.start);
            let end_char = self.rope.byte_to_char(e.end);
            self.rope.remove(start_char..end_char);
            self.rope.insert(start_char, &e.replacement);
        }
        self.head = Some(target);
        self.dirty = true;
        Some(node.edits.last().map(|e| e.start + e.replacement.len()).unwrap_or(0))
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
